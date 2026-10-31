//! OpenCode SQLite adapter: read-only connection, schema detection, roster scan.
//!
//! Access policy (ADR-0002): the database is opened `SQLITE_OPEN_READ_ONLY` with
//! `query_only`, and an authorizer allows only `SELECT` of allowlisted tables and
//! columns. SQLite may create/update its transient `-shm`/`-wal` sidecars; this
//! module never writes, deletes, repairs, or copies any provider file, and never
//! uses `immutable=1`. Account, credential, share, and key-value tables are denied
//! at the connection boundary, and `share_url` is never selectable.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use serde_json::Value;

use super::{
    Admit, DiscoveryDelta, FORGOTTEN_CAP, Forgotten, HydrationDelta, JobLedger, ProviderError,
    ProviderReader, ReadJobId, ReadProgress, Retention, SourceRecord, WorkProgress,
};
use crate::config::Config;
use crate::domain::{
    ActivityStamp, Actor, AggregationClass, BoundedText, DiagnosticEntry, Event, EventKind,
    Harness, InputAccounting, Lifecycle, Lineage, LocatorKind, ModelDetail, MoneyFact, MoneySource,
    OutputAccounting, PageAnchor, PageDirection, PageRequest, ProcessAssociation, Provenance,
    RecordedMoney, RefreshBudget, ReleaseState, RequestMetadata, ScanProgress, SessionDetailPage,
    SessionKey, SessionLink, SessionMetadata, SessionMetrics, SessionSummary, SourceKey,
    SourceLocator, Timestamp, TokenAccounting, TokenCategories, ToolInteraction, ToolStatus,
    TranscriptPage, UsageObservation, UsageScope, Warning, WarningKey, WarningKind,
};
use crate::ingest::text::{display_safe, preview};
use crate::metrics::session_timing;
use crate::usage::{DerivedUsage, OpenCodeDerivation, opencode_summary};

const TITLE_BYTES: usize = 1024;
const TITLE_COLS: usize = 80;
const DEFAULT_BUSY_MS: u64 = 2000;
/// Newest message rows consulted for a session's latest activity.
const ACTIVITY_WINDOW: usize = 256;
/// Message rows counted before the per-session activity count saturates.
const COUNT_CAP: usize = 100_000;
/// Marks the error a progress-handler interrupt produces.
const INTERRUPTED: &str = "interrupted at the work-slice deadline";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schema {
    /// `session_v2` + `session_message` generation.
    V2,
    /// `session` + `message` + `part`, only when v2 is absent or empty.
    Legacy,
    Unsupported,
}

/// Tables and the only columns of each that SQL may read. Anything else is denied.
const ALLOWED: &[(&str, &[&str])] = &[
    (
        "session_v2",
        &[
            "id",
            "title",
            "directory",
            "project_id",
            "parent_id",
            "fork_session_id",
            "model",
            "version",
            "time_created",
            "time_updated",
            "time_archived",
            "tokens_input",
            "tokens_output",
            "tokens_reasoning",
            "tokens_cache_read",
            "tokens_cache_write",
            "cost",
            "idle_outcome",
            "agent",
        ],
    ),
    (
        "session_message",
        &[
            "id",
            "session_id",
            "type",
            "data",
            "time_created",
            "time_updated",
        ],
    ),
    ("project", &["id", "name"]),
    (
        "session",
        &[
            "id",
            "title",
            "directory",
            "project_id",
            "parent_id",
            "version",
            "time_created",
            "time_updated",
            "time_archived",
        ],
    ),
    (
        "message",
        &["id", "session_id", "time_created", "time_updated", "data"],
    ),
    (
        "part",
        &[
            "id",
            "message_id",
            "session_id",
            "time_created",
            "time_updated",
            "data",
        ],
    ),
    // Only the table-existence count used to force the WAL/shm handshake on open.
    ("sqlite_master", &["name"]),
    ("sqlite_schema", &["name"]),
];

/// Aggregates only; no extension, file, or text-producing functions.
const ALLOWED_FUNCTIONS: &[&str] = &["count", "max", "min", "substr", "length", "coalesce"];

fn authorize(ctx: AuthContext<'_>) -> Authorization {
    match ctx.action {
        AuthAction::Select => Authorization::Allow,
        AuthAction::Read {
            table_name,
            column_name,
        } => {
            let allowed = ALLOWED.iter().any(|(table, columns)| {
                table.eq_ignore_ascii_case(table_name)
                    && (column_name.is_empty()
                        || columns.iter().any(|c| c.eq_ignore_ascii_case(column_name)))
            });
            if allowed {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name } => {
            if ALLOWED_FUNCTIONS
                .iter()
                .any(|f| f.eq_ignore_ascii_case(function_name))
            {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Deny,
    }
}

fn map_error(error: rusqlite::Error) -> ProviderError {
    match &error {
        rusqlite::Error::SqliteFailure(failure, _)
        | rusqlite::Error::SqlInputError { error: failure, .. } => match failure.code {
            ErrorCode::DatabaseBusy
            | ErrorCode::DatabaseLocked
            | ErrorCode::CannotOpen
            | ErrorCode::ReadOnly
            | ErrorCode::SystemIoFailure
            | ErrorCode::DiskFull
            | ErrorCode::OutOfMemory
            | ErrorCode::FileLockingProtocolFailed
            | ErrorCode::NoLargeFileSupport => ProviderError::Unavailable(error.to_string()),
            ErrorCode::OperationInterrupted => {
                ProviderError::Unavailable(format!("OpenCode query {INTERRUPTED}"))
            }
            ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt => {
                ProviderError::Parse(error.to_string())
            }
            _ => ProviderError::Internal(error.to_string()),
        },
        _ => ProviderError::Internal(error.to_string()),
    }
}

/// Opens `path` read-only behind the authorizer. The first schema read happens
/// here so a WAL that needs a sidecar SQLite cannot create surfaces as
/// `Unavailable` immediately rather than on a later query.
pub fn open_read_only(path: &Path) -> Result<Connection, ProviderError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let mut conn = Connection::open_with_flags(path, flags).map_err(map_error)?;
    conn.busy_timeout(Duration::from_millis(DEFAULT_BUSY_MS))
        .map_err(map_error)?;
    conn.pragma_update(None, "query_only", true)
        .map_err(map_error)?;
    conn.authorizer(Some(authorize)).map_err(map_error)?;
    conn.query_row("SELECT count(*) FROM sqlite_master", [], |r| {
        r.get::<_, i64>(0)
    })
    .map_err(map_error)?;
    Ok(conn)
}

const V2_SESSION_COLUMNS: &str = "id, title, directory, project_id, parent_id, fork_session_id, \
     model, version, time_created, time_updated, time_archived";
const LEGACY_SESSION_COLUMNS: &str = "id, title, directory, project_id, parent_id, version, time_created, time_updated, time_archived";

/// True when `sql` prepares. A SQLite "no such table/column" error means the shape
/// is absent; authorization and I/O failures are real errors.
fn shape_present(conn: &Connection, sql: &str) -> Result<bool, ProviderError> {
    match conn.prepare(sql) {
        Ok(_) => Ok(true),
        Err(
            rusqlite::Error::SqliteFailure(f, _) | rusqlite::Error::SqlInputError { error: f, .. },
        ) if f.code == ErrorCode::Unknown => Ok(false),
        Err(e) => Err(map_error(e)),
    }
}

fn has_rows(conn: &Connection, table: &str) -> Result<bool, ProviderError> {
    conn.query_row(&format!("SELECT EXISTS(SELECT 1 FROM {table})"), [], |r| {
        r.get::<_, bool>(0)
    })
    .map_err(map_error)
}

/// v2 when its tables are recognised and populated; legacy when v2 is absent or
/// empty and the legacy shape is recognised; otherwise unsupported. Never guesses.
pub fn detect_schema(conn: &Connection) -> Result<Schema, ProviderError> {
    let v2 = shape_present(
        conn,
        &format!("SELECT {V2_SESSION_COLUMNS} FROM session_v2 LIMIT 0"),
    )? && shape_present(
        conn,
        "SELECT id, session_id, time_created, time_updated FROM session_message LIMIT 0",
    )?;
    let legacy = shape_present(
        conn,
        &format!("SELECT {LEGACY_SESSION_COLUMNS} FROM session LIMIT 0"),
    )? && shape_present(
        conn,
        "SELECT id, session_id, time_created, time_updated FROM message LIMIT 0",
    )? && shape_present(conn, "SELECT id, message_id, session_id FROM part LIMIT 0")?;
    if v2 {
        let populated = has_rows(conn, "session_v2")? || has_rows(conn, "session_message")?;
        if populated || !legacy || !has_rows(conn, "session")? {
            return Ok(Schema::V2);
        }
        return Ok(Schema::Legacy);
    }
    Ok(if legacy {
        Schema::Legacy
    } else {
        Schema::Unsupported
    })
}

/// Optional columns/tables whose presence is probed once per connection.
#[derive(Clone, Copy, Default)]
struct Shape {
    project: bool,
    /// Presence of each `USAGE_COLUMNS` entry in `session_v2`, probed independently.
    usage: [bool; 6],
    /// `session_v2.idle_outcome`.
    idle: bool,
    /// `session_v2.agent` (the OpenCode agent/role recorded for the session).
    agent: bool,
}

const USAGE_COLUMNS: [&str; 6] = [
    "tokens_input",
    "tokens_output",
    "tokens_reasoning",
    "tokens_cache_read",
    "tokens_cache_write",
    "cost",
];

#[derive(Clone, Copy)]
struct Tables {
    session: &'static str,
    message: &'static str,
    /// Table `transcript_page` reads: `session_message` (v2) or `part` (legacy).
    transcript: &'static str,
    legacy: bool,
    /// `session` column expressions that legacy databases lack.
    model: &'static str,
    fork: &'static str,
}

impl Tables {
    fn for_schema(schema: Schema) -> Option<Self> {
        match schema {
            Schema::V2 => Some(Self {
                session: "session_v2",
                message: "session_message",
                transcript: MESSAGE_TABLE,
                legacy: false,
                model: "s.model",
                fork: "s.fork_session_id",
            }),
            Schema::Legacy => Some(Self {
                session: "session",
                message: "message",
                transcript: "part",
                legacy: true,
                model: "NULL",
                fork: "NULL",
            }),
            Schema::Unsupported => None,
        }
    }
}

#[derive(Clone)]
struct Row {
    id: String,
    title: Option<String>,
    directory: Option<String>,
    project_name: Option<String>,
    parent: Option<String>,
    parent_exists: bool,
    fork: Option<String>,
    fork_exists: bool,
    agent: Option<String>,
    model: Option<String>,
    version: Option<String>,
    created: Option<i64>,
    updated: i64,
    archived: Option<i64>,
    message_updated: Option<i64>,
    tokens: TokenCategories,
    cost: Option<f64>,
    idle_outcome: Option<String>,
    activity: usize,
}

/// Work allowance of one slice of a row pager: counts rows examined and bytes read,
/// whether or not they produce events.
struct PageSlice {
    max_records: usize,
    max_bytes: usize,
    deadline: Instant,
    examined: usize,
    read: usize,
    stopped: bool,
    /// Last row fully processed; the next slice resumes after it.
    last: Option<(i64, String)>,
}

impl PageSlice {
    fn spent(&self) -> bool {
        self.examined >= self.max_records
            || self.read >= self.max_bytes
            || Instant::now() >= self.deadline
    }
}

/// Continuation of one resumable `details_page` request scan.
#[derive(Default)]
struct DetailJob {
    /// Last row examined; the next slice resumes after it.
    cursor: Option<(i64, String)>,
    /// Last row that produced an observation (the next page's anchor).
    last: Option<(i64, String)>,
    out: Vec<UsageObservation>,
    bytes: usize,
    examined: usize,
    read: usize,
    /// The request scan finished (`more` says whether another page remains).
    requests_done: bool,
    more: bool,
    /// Recent events collected so far, and the last row the recent pager examined.
    recent: Vec<Event>,
    recent_cursor: Option<(i64, String)>,
    recent_bytes: usize,
    recent_truncated: bool,
    recent_done: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum RequestsStatus {
    /// Every row was examined.
    Done,
    /// The page filled; more requests remain.
    More,
    /// The work allowance ended first; call again.
    Slice,
}

/// Progress of one resumable `count_events_after`.
struct CountJob {
    /// Last row consumed; the next slice resumes after it.
    cursor: (i64, String),
    count: usize,
}

struct Pass {
    exhausted: bool,
    cursor: Option<(i64, String)>,
    seen: HashSet<String>,
    processed: usize,
    total: usize,
    oversized: usize,
    /// Hydrated sessions whose message activity is rechecked once rows are exhausted.
    recheck: Option<VecDeque<String>>,
    /// Forgotten sessions still to be checked for deletion, resumably.
    sweep: Option<VecDeque<String>>,
}

/// What discovery remembers per session: a digest of its metadata row (to emit only
/// changes) and the recorded version (provenance of record locators).
struct KnownRow {
    digest: u64,
    version: Option<String>,
}

fn digest(row: &Row) -> u64 {
    let mut hasher = DefaultHasher::new();
    (
        &row.title,
        &row.directory,
        &row.project_name,
        &row.parent,
        row.parent_exists,
        &row.fork,
        row.fork_exists,
    )
        .hash(&mut hasher);
    (
        &row.agent,
        &row.model,
        &row.version,
        row.created,
        row.updated,
        row.archived,
    )
        .hash(&mut hasher);
    hasher.finish()
}

pub struct OpenCodeScanner {
    config: Config,
    path: PathBuf,
    connection: Option<Connection>,
    schema: Schema,
    shape: Shape,
    pass: Option<Pass>,
    known: HashMap<String, KnownRow>,
    /// Newest-first retention that bounds `known` to the metadata ceiling.
    retention: Retention<String>,
    /// Evicted sessions that may still be frozen into a roster: watched for deletion.
    forgotten: Forgotten<String, ()>,
    /// Findings of a failed discovery slice, delivered with the next one.
    carry_sessions: Vec<SessionMetadata>,
    carry_removed: Vec<SessionKey>,
    /// Sessions displaced at the ceiling since the last delta, and whether it was hit.
    displaced: Vec<String>,
    limited: bool,
    /// Per-session lifecycle/usage evidence keyed by the change stamp it was read at,
    /// with a recency tick. Bounded by `max_cached_sessions`.
    evidence: HashMap<String, (Stamp, Evidence, u64)>,
    /// Evidence collections interrupted by the work budget, keyed by session ID.
    jobs: HashMap<String, Job>,
    ledger: JobLedger,
    /// Sessions whose rows changed since evidence was last read; drained per delta.
    invalidated: Vec<SessionKey>,
    /// Resumable `count_events_after` continuations per `(session, job)`.
    counts: RefCell<HashMap<(SessionKey, ReadJobId), CountJob>>,
    /// Resumable `details_page` scans per `(session, job)`.
    details: RefCell<HashMap<(SessionKey, ReadJobId), DetailJob>>,
    tick: u64,
}

impl OpenCodeScanner {
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        let path = config.harnesses.opencode.db.clone().ok_or_else(|| {
            ProviderError::Unavailable("no OpenCode database path configured".into())
        })?;
        let mut scanner = Self {
            config: config.clone(),
            path,
            connection: None,
            schema: Schema::Unsupported,
            shape: Shape::default(),
            pass: None,
            known: HashMap::new(),
            retention: Retention::new(config.max_metadata_sources, config.max_metadata_bytes),
            forgotten: Forgotten::new(FORGOTTEN_CAP),
            carry_sessions: Vec::new(),
            carry_removed: Vec::new(),
            displaced: Vec::new(),
            limited: false,
            evidence: HashMap::new(),
            jobs: HashMap::new(),
            ledger: JobLedger::default(),
            invalidated: Vec::new(),
            counts: RefCell::new(HashMap::new()),
            details: RefCell::new(HashMap::new()),
            tick: 0,
        };
        scanner.ensure_open()?;
        Ok(scanner)
    }

    /// Sessions this adapter currently tracks (bounded by the metadata ceiling).
    pub fn tracked_sources(&self) -> usize {
        self.retention.len()
    }

    /// Sessions with cached evidence (bounded by `max_cached_sessions`).
    pub fn cached_sessions(&self) -> usize {
        self.evidence.len()
    }

    /// Evidence collections still in flight.
    pub fn job_state(&self) -> usize {
        self.jobs
            .len()
            .max(self.ledger.len())
            .max(self.counts.borrow().len())
            .max(self.details.borrow().len())
    }

    fn ensure_open(&mut self) -> Result<(), ProviderError> {
        if self.connection.is_some() {
            return Ok(());
        }
        let conn = open_read_only(&self.path)?;
        conn.busy_timeout(Duration::from_millis(self.config.refresh_busy_timeout_ms))
            .map_err(map_error)?;
        self.schema = detect_schema(&conn)?;
        let v2 = self.schema == Schema::V2;
        let mut usage = [false; 6];
        for (present, column) in usage.iter_mut().zip(USAGE_COLUMNS) {
            *present =
                v2 && shape_present(&conn, &format!("SELECT {column} FROM session_v2 LIMIT 0"))?;
        }
        self.shape = Shape {
            project: shape_present(&conn, "SELECT id, name FROM project LIMIT 0")?,
            usage,
            idle: v2 && shape_present(&conn, "SELECT idle_outcome FROM session_v2 LIMIT 0")?,
            agent: v2 && shape_present(&conn, "SELECT agent FROM session_v2 LIMIT 0")?,
        };
        self.connection = Some(conn);
        Ok(())
    }

    fn warning(&self, label: &str, kind: WarningKind, message: &str) -> Warning {
        Warning {
            key: WarningKey {
                harness: Harness::OpenCode,
                source: format!("{}#{label}", self.path.display()),
                kind,
            },
            message: message.into(),
            count: 1,
        }
    }

    /// Opens (if needed), bounds the slice's SQLite work to `budget`, and runs `f`.
    /// The busy timeout never exceeds the slice, and a progress handler interrupts a
    /// query that outlives it, so an aggregate cannot hold the worker past one slice.
    /// Any failure that is not a plain refusal drops the connection for a reopen.
    fn sliced<T>(
        &mut self,
        budget: RefreshBudget,
        f: impl FnOnce(&mut Self, &Connection, Instant) -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        let slice = Duration::from_millis(budget.max_millis.max(1));
        let busy = slice.min(Duration::from_millis(self.config.refresh_busy_timeout_ms));
        let result = self.ensure_open().and_then(|()| {
            let mut conn = self.connection.take().expect("opened");
            let deadline = Instant::now() + slice;
            let outcome = conn
                .busy_timeout(busy.max(Duration::from_millis(1)))
                .map_err(map_error)
                .and_then(|()| {
                    conn.progress_handler(1000, Some(move || Instant::now() >= deadline))
                        .map_err(map_error)?;
                    let out = f(self, &conn, deadline);
                    let _ = conn.progress_handler(0, None::<fn() -> bool>);
                    out
                });
            self.connection = Some(conn);
            outcome
        });
        match &result {
            Ok(_) => {}
            // The slice ended mid-query: keep the connection and allow more time next call.
            Err(ProviderError::Unavailable(m)) if m.contains(INTERRUPTED) => {
                self.pass = None;
            }
            Err(e) if !matches!(e, ProviderError::Unsupported(_)) => {
                self.connection = None;
                self.pass = None;
            }
            Err(_) => {}
        }
        result
    }

    fn meta_of(&self, row: &Row, tables: &Tables) -> SessionMetadata {
        let model = row
            .model
            .as_deref()
            .and_then(|raw| opencode_model_detail(raw, &self.config.pricing));
        let link = |id: &Option<String>, exists: bool| {
            id.as_ref().map(|id| {
                if exists {
                    SessionLink::Resolved(key(&self.path, tables, id))
                } else {
                    SessionLink::Unresolved(id.clone())
                }
            })
        };
        let directory = row.directory.as_deref().filter(|d| !d.is_empty());
        let project = row.project_name.as_deref().and_then(short).or_else(|| {
            directory
                .map(Path::new)
                .and_then(Path::file_name)
                .and_then(|n| short(&n.to_string_lossy()))
        });
        SessionMetadata {
            key: key(&self.path, tables, &row.id),
            title: row
                .title
                .as_deref()
                .and_then(short)
                .unwrap_or_else(|| row.id.clone()),
            title_derived: false,
            project,
            cwd: directory.map(PathBuf::from),
            model,
            origins: vec![],
            created_at: row.created,
            activity: ActivityStamp {
                at: Some(row.updated.max(row.message_updated.unwrap_or(i64::MIN))),
                class: AggregationClass::Reported,
            },
            lineage: Lineage {
                parent: link(&row.parent, row.parent_exists),
                fork: link(&row.fork, row.fork_exists),
                agent_type: row.agent.as_deref().and_then(short),
                ..Lineage::default()
            },
            members: vec![],
            archived: row.archived.is_some(),
            released: ReleaseState::Unknown,
            source_available: true,
            harness_version: row.version.as_deref().and_then(short),
        }
    }

    /// `None` when the retention caps rejected the session; otherwise whether it is new
    /// or changed since last reported.
    fn remember(&mut self, row: &Row) -> Option<bool> {
        let weight = 128 + row_bytes(row);
        match self.retention.admit(&row.id, Some(row.updated), weight) {
            Admit::Dropped | Admit::Displaced => {
                // A tracked session that no longer fits is forgotten, not removed: it stays
                // in the database and readable, and is watched for deletion.
                if self.known.remove(&row.id).is_some() {
                    self.forgotten.insert(row.id.clone(), ());
                }
                self.evidence.remove(&row.id);
                self.jobs.remove(&row.id);
                self.displaced.push(row.id.clone());
                self.limited = true;
                return None;
            }
            Admit::Kept { evicted } => {
                // Bookkeeping only: evicted sessions stay in the database and readable.
                self.forgotten.remove(&row.id);
                for id in evicted {
                    self.known.remove(&id);
                    self.evidence.remove(&id);
                    self.jobs.remove(&id);
                    self.forgotten.insert(id.clone(), ());
                    self.displaced.push(id);
                    self.limited = true;
                }
            }
        }
        let digest = digest(row);
        let version = row.version.as_deref().and_then(short);
        match self.known.get_mut(&row.id) {
            Some(known) if known.digest == digest => Some(false),
            Some(known) => {
                known.digest = digest;
                known.version = version;
                Some(true)
            }
            None => {
                self.known
                    .insert(row.id.clone(), KnownRow { digest, version });
                Some(true)
            }
        }
    }

    /// One bounded slice of metadata discovery: session rows newest `time_updated`
    /// first, read through allowlisted columns only (no message aggregates, no
    /// usage), then a recheck of the hydrated sessions' message activity.
    fn discover_step(
        &mut self,
        conn: &Connection,
        budget: RefreshBudget,
        started: Instant,
        sessions: &mut Vec<SessionMetadata>,
        removed: &mut Vec<SessionKey>,
    ) -> Result<DiscoveryDelta, ProviderError> {
        let mut warnings = Vec::new();
        let done_scan = |scanned: usize, total: Option<usize>, done: bool| ScanProgress {
            harness: Harness::OpenCode,
            scanned,
            total,
            done,
        };
        let Some(tables) = Tables::for_schema(self.schema) else {
            warnings.push(self.warning(
                "schema",
                WarningKind::Unsupported,
                "unsupported OpenCode database schema; no sessions listed",
            ));
            // Findings carried from an earlier failed slice are delivered, never dropped.
            return Ok(DiscoveryDelta {
                sessions: std::mem::take(sessions),
                removed: std::mem::take(removed),
                invalidated: std::mem::take(&mut self.invalidated),
                warnings,
                scan: done_scan(0, Some(0), true),
            });
        };
        if self.schema == Schema::Legacy
            && shape_present(conn, "SELECT id FROM session_v2 LIMIT 0")?
        {
            warnings.push(self.warning(
                "schema",
                WarningKind::Unsupported,
                "v2 tables are empty; reading legacy session tables",
            ));
        }
        let mut pass = match self.pass.take() {
            Some(pass) => pass,
            None => {
                let count = |sql: String| -> Result<usize, ProviderError> {
                    let n: i64 = conn.query_row(&sql, [], |r| r.get(0)).map_err(map_error)?;
                    Ok(usize::try_from(n).unwrap_or(0))
                };
                let all = count(format!("SELECT count(*) FROM {}", tables.session))?;
                let total = count(format!(
                    "SELECT count(*) FROM {} s WHERE {}",
                    tables.session,
                    eligible_ids(&tables)
                ))?;
                Pass {
                    exhausted: false,
                    cursor: None,
                    seen: HashSet::new(),
                    processed: 0,
                    total,
                    oversized: all - total.min(all),
                    recheck: None,
                    sweep: None,
                }
            }
        };
        // A zero allowance reads nothing and cannot finish the pass.
        if budget.max_records == 0 || budget.max_bytes == 0 || budget.max_millis == 0 {
            let scan = done_scan(pass.processed, None, false);
            self.pass = Some(pass);
            return Ok(DiscoveryDelta {
                sessions: std::mem::take(sessions),
                removed: std::mem::take(removed),
                invalidated: Vec::new(),
                warnings,
                scan,
            });
        }
        if !pass.exhausted {
            let limit = budget.max_records;
            let (rows, ended) = read_rows(
                conn,
                &tables,
                self.shape,
                Rows::After {
                    cursor: pass.cursor.as_ref(),
                    limit,
                },
                Detail::Light,
                &budget,
                started,
            )?;
            pass.exhausted |= ended;
            pass.processed += rows.len();
            for row in rows {
                pass.cursor = Some((row.updated, row.id.clone()));
                // Pass bookkeeping is bounded to admitted sources: a rejected or
                // displaced session is not tracked (it stays readable in the database).
                let admitted = self.remember(&row);
                if admitted.is_some() {
                    pass.seen.insert(row.id.clone());
                }
                for id in std::mem::take(&mut self.displaced) {
                    pass.seen.remove(&id);
                }
                if admitted == Some(true) {
                    sessions.push(self.meta_of(&row, &tables));
                }
            }
        }
        if self.limited {
            let mut w = self.warning(
                "metadata-limit",
                WarningKind::Unsupported,
                "Discovery limited: only the newest OpenCode sessions are tracked; \
                 raise max_metadata_sources to see more",
            );
            w.key.source = "metadata-limit".into();
            warnings.push(w);
        }
        if pass.exhausted {
            let recheck = pass
                .recheck
                .get_or_insert_with(|| self.evidence.keys().cloned().collect::<VecDeque<String>>());
            let mut checked = 0usize;
            while checked < budget.max_records
                && started.elapsed() < Duration::from_millis(budget.max_millis)
                && let Some(id) = recheck.pop_front()
            {
                checked += 1;
                let Some((stamp, _, _)) = self.evidence.get(&id) else {
                    continue;
                };
                let stamp = *stamp;
                let (rows, _) = read_rows(
                    conn,
                    &tables,
                    self.shape,
                    Rows::One(&id),
                    Detail::Full,
                    &budget,
                    started,
                )?;
                let Some(row) = rows.into_iter().next() else {
                    continue;
                };
                if (row.updated, row.message_updated) != stamp {
                    self.invalidated.push(key(&self.path, &tables, &row.id));
                    sessions.push(self.meta_of(&row, &tables));
                }
            }
        }
        if pass.exhausted && pass.recheck.as_ref().is_some_and(VecDeque::is_empty) {
            // Forgotten sessions are not tracked in `known`; a budgeted, resumable
            // existence sweep lets a genuine deletion of a frozen row reach the roster.
            let sweep = pass
                .sweep
                .get_or_insert_with(|| self.forgotten.keys().into_iter().collect());
            let mut checked = 0usize;
            while checked < budget.max_records
                && started.elapsed() < Duration::from_millis(budget.max_millis)
                && let Some(id) = sweep.front().cloned()
            {
                checked += 1;
                let exists = conn
                    .query_row(
                        &format!(
                            "SELECT EXISTS(SELECT 1 FROM {} WHERE id = ?1)",
                            tables.session
                        ),
                        params![id],
                        |r| r.get::<_, bool>(0),
                    )
                    .map_err(map_error)?;
                sweep.pop_front();
                if !exists {
                    self.forgotten.remove(&id);
                    removed.push(key(&self.path, &tables, &id));
                }
            }
        }
        let done = pass.exhausted
            && pass.recheck.as_ref().is_some_and(VecDeque::is_empty)
            && pass.sweep.as_ref().is_some_and(VecDeque::is_empty);
        if done {
            let missing: Vec<String> = self
                .known
                .keys()
                .filter(|id| !pass.seen.contains(*id))
                .cloned()
                .collect();
            // A row that merely moved in the ordering mid-pass is kept while it exists.
            for id in missing {
                let exists = conn
                    .query_row(
                        &format!(
                            "SELECT EXISTS(SELECT 1 FROM {} WHERE id = ?1)",
                            tables.session
                        ),
                        params![id],
                        |r| r.get::<_, bool>(0),
                    )
                    .map_err(map_error)?;
                if !exists {
                    self.retention.remove(&id);
                    self.known.remove(&id);
                    self.evidence.remove(&id);
                    self.jobs.remove(&id);
                    removed.push(key(&self.path, &tables, &id));
                }
            }
            if pass.oversized > 0 {
                let mut w = self.warning(
                    "oversized-id",
                    WarningKind::Unsupported,
                    "sessions skipped: identifier too long to key exactly",
                );
                w.count = u32::try_from(pass.oversized).unwrap_or(u32::MAX);
                warnings.push(w);
            }
        }
        let scan = done_scan(pass.processed, done.then_some(pass.total), done);
        if !done {
            self.pass = Some(pass);
        }
        Ok(DiscoveryDelta {
            sessions: std::mem::take(sessions),
            removed: std::mem::take(removed),
            invalidated: std::mem::take(&mut self.invalidated),
            warnings,
            scan,
        })
    }

    fn summarize(&self, row: &Row, tables: &Tables, evidence: &Evidence) -> SessionSummary {
        let details: Vec<ModelDetail> = row
            .model
            .as_deref()
            .and_then(|raw| opencode_model_detail(raw, &self.config.pricing))
            .into_iter()
            .collect();
        let usage = opencode_summary(
            details.first(),
            &row.tokens,
            row.cost,
            evidence.derived.as_ref(),
            &self.config.pricing,
            self.config.reference_policy(Harness::OpenCode),
        );
        SessionSummary {
            meta: self.meta_of(row, tables),
            usage,
            // `time_created` is recorded. Prompt occupancy, turn spans and compaction
            // markers exist only inside `session_message` bodies; the bounded roster
            // pass streams message rows for lifecycle/derivation evidence, not a full
            // per-message metrics sweep, so those facts stay Unknown rather than
            // sampled inconsistently per session.
            metrics: SessionMetrics {
                timing: session_timing(row.created, None, &[], false, None),
                ..SessionMetrics::default()
            },
            lifecycle: evidence.lifecycle,
            unfinished_work: evidence.unfinished,
            last_event_at: None,
            last_completion: evidence.last_completion,
            // Provider activity count (messages for v2, legacy messages for the
            // legacy schema), not exact displayed rows: OpenCode rows expand into
            // a different number of events. It is monotonic across appends and
            // feeds only the `N new` arrival delta.
            displayed_event_count: row.activity,
            hidden_records: 0,
            server_tools: None,
            process: ProcessAssociation::NotChecked,
            warnings: vec![],
        }
    }

    /// Validates `key` against this database and reads its full row.
    fn session_row(
        &self,
        conn: &Connection,
        key: &SessionKey,
        budget: &RefreshBudget,
        started: Instant,
    ) -> Result<(Tables, Row), ProviderError> {
        let Some(tables) = Tables::for_schema(self.schema) else {
            return Err(ProviderError::Unsupported(
                "unsupported OpenCode database schema".into(),
            ));
        };
        let valid = key.harness == Harness::OpenCode
            && matches!(&key.source, SourceKey::DbRow { database, table, id }
                if *database == self.path && table == tables.session && *id == key.native_id);
        if !valid {
            return Err(ProviderError::Unsupported("not an OpenCode session".into()));
        }
        let (rows, _) = read_rows(
            conn,
            &tables,
            self.shape,
            Rows::One(&key.native_id),
            Detail::Full,
            budget,
            started,
        )?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| ProviderError::Unavailable("unknown OpenCode session".into()))?;
        Ok((tables, row))
    }

    /// Bounds cached evidence after a hydration completes.
    fn trim_evidence(&mut self, keep: &str) {
        let cap = self.config.max_cached_sessions.max(1);
        while self.evidence.len() > cap {
            let victim = self
                .evidence
                .iter()
                .filter(|(id, _)| id.as_str() != keep && !self.jobs.contains_key(*id))
                .min_by_key(|(_, (_, _, used))| *used)
                .map(|(id, _)| id.clone());
            match victim {
                Some(id) => {
                    self.evidence.remove(&id);
                }
                None => break,
            }
        }
    }

    fn hydrate_step(
        &mut self,
        conn: &Connection,
        job: ReadJobId,
        key: &SessionKey,
        budget: RefreshBudget,
        started: Instant,
    ) -> Result<HydrationDelta, ProviderError> {
        self.tick += 1;
        let (tables, mut work) = match self.jobs.remove(&key.native_id) {
            Some(work) => {
                let tables = Tables::for_schema(self.schema).ok_or_else(|| {
                    ProviderError::Unsupported("unsupported OpenCode database schema".into())
                })?;
                (tables, work)
            }
            None => {
                let (tables, row) = self.session_row(conn, key, &budget, started)?;
                let stamp = (row.updated, row.message_updated);
                if let Some((at, cached, used)) = self.evidence.get_mut(&row.id)
                    && *at == stamp
                {
                    *used = self.tick;
                    let cached = cached.clone();
                    self.ledger.release(key, job);
                    let summary = self.summarize(&row, &tables, &cached);
                    return Ok(HydrationDelta {
                        summary,
                        done: true,
                    });
                }
                (tables, Job::new(row))
            }
        };
        let record_cap = self.config.max_record_bytes;
        let mut allow = Allowance {
            records: budget.max_records,
            bytes: budget.max_bytes,
            deadline: started + Duration::from_millis(budget.max_millis),
        };
        match advance(conn, &tables, &mut work, &mut allow, record_cap)? {
            Some(evidence) => {
                let job_state = work;
                let summary = self.summarize(&job_state.row, &tables, &evidence);
                if self
                    .evidence
                    .get(&job_state.row.id)
                    .is_some_and(|(prior, _, _)| *prior != job_state.stamp)
                {
                    self.invalidated.push(summary.meta.key.clone());
                }
                self.evidence.insert(
                    job_state.row.id.clone(),
                    (job_state.stamp, evidence, self.tick),
                );
                self.trim_evidence(&job_state.row.id);
                self.ledger.release(key, job);
                Ok(HydrationDelta {
                    summary,
                    done: true,
                })
            }
            None => {
                let meta = self.meta_of(&work.row, &tables);
                self.ledger.join(key, job);
                self.jobs.insert(key.native_id.clone(), work);
                Ok(HydrationDelta {
                    summary: SessionSummary::unhydrated(meta),
                    done: false,
                })
            }
        }
    }
}

fn key(database: &Path, tables: &Tables, id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::OpenCode,
        native_id: id.to_owned(),
        source: SourceKey::DbRow {
            database: database.to_path_buf(),
            table: tables.session.to_owned(),
            id: id.to_owned(),
        },
    }
}

/// Collapsed, redacted, bounded one-line text; `None` when nothing is left.
fn short(text: &str) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let out = preview(&collapsed, TITLE_BYTES, TITLE_COLS).preview;
    (!out.trim().is_empty()).then_some(out)
}

/// The `(billing provider, model id)` an assistant message names, if any. The two
/// documented shapes are dispatched, never mixed: v2 nests `model: {id, providerID}`;
/// legacy messages are flat `modelID` / `providerID`.
fn message_model(v: &Value) -> Option<(Option<String>, String)> {
    let text = |v: &Value, k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    };
    match v.get("model") {
        Some(m) if m.is_object() => Some((text(m, "providerID"), text(m, "id")?)),
        _ => Some((text(v, "providerID"), text(v, "modelID")?)),
    }
}

/// `model` is JSON (`{"id", "providerID", "variant"}`); a bare string is its own id.
///
/// OpenCode records reasoning effort as the model `variant`, but custom providers
/// and model overrides can reuse any variant name for another purpose. The raw
/// value always stays in `variant`; it is also reported as `effort` only when the
/// exact `(providerID, model id)` is a detected model route in the local price
/// catalog and the spelling is an observed thinking level. A missing provider,
/// unknown/custom model, or `default`/`none` leaves effort `Unknown`.
fn opencode_model_detail(raw: &str, prices: &crate::pricing::PriceSchedule) -> Option<ModelDetail> {
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(map)) => {
            let text = |name: &str| map.get(name).and_then(Value::as_str).and_then(short);
            let id = text("id")?;
            let provider = text("providerID");
            let variant = text("variant");
            let effort = variant.as_deref().and_then(thinking_effort).filter(|_| {
                provider
                    .as_deref()
                    .is_some_and(|p| prices.bundled_route(p, &id))
            });
            Some(ModelDetail {
                effort,
                variant,
                id,
                provider,
                configured_effort: None,
            })
        }
        Ok(Value::String(s)) => short(&s).map(bare_model),
        Ok(_) => None,
        Err(_) => {
            let t = raw.trim_start();
            if t.starts_with(['{', '[', '"']) {
                None
            } else {
                short(raw).map(bare_model)
            }
        }
    }
}

/// Observed `variant` spellings that the OpenCode model catalog defines as reasoning
/// levels. Anything else stays a raw variant.
const THINKING_LEVELS: &[&str] = &["minimal", "low", "medium", "high", "xhigh", "max"];

fn thinking_effort(variant: &str) -> Option<String> {
    THINKING_LEVELS
        .contains(&variant)
        .then(|| variant.to_owned())
}

/// The native child session a tool call provably ran, from the documented tool
/// metadata shape: v2 `subagent` records `sessionID`; legacy `task` records
/// `sessionId`. Tool names and keys are matched exactly (case-sensitive), the value
/// must be a plausible identifier, and output text is never consulted.
pub(crate) fn opencode_child_link(
    tool_name: &str,
    metadata: &Value,
    shape: Schema,
) -> Option<String> {
    let key = match shape {
        Schema::V2 if tool_name == "subagent" => "sessionID",
        Schema::Legacy if tool_name == "task" => "sessionId",
        _ => return None,
    };
    let id = metadata.get(key)?.as_str()?;
    let valid = !id.is_empty()
        && id.len() <= ID_MAX
        && !id.chars().any(|c| c.is_control() || c.is_whitespace());
    valid.then(|| id.to_owned())
}

fn bare_model(id: String) -> ModelDetail {
    ModelDetail {
        id,
        provider: None,
        variant: None,
        effort: None,
        configured_effort: None,
    }
}

/// Display strings are cut in SQL so one oversized value is never materialised whole.
/// Identifiers are never cut: identity, cache keys, and keyset cursors need exact
/// values, so sessions whose id/parent/fork exceed `ID_MAX` chars are skipped (and
/// counted in a warning) instead of being truncated into colliding keys.
const ID_MAX: usize = 4096;
const MODEL_MAX: usize = 65_536;
const TEXT_CHARS: usize = 1024;
const PATH_CHARS: usize = 4096;
const VERSION_CHARS: usize = 128;

/// SQL predicate (alias `s`) selecting sessions with exact, bounded identifiers.
fn eligible_ids(tables: &Tables) -> String {
    let mut sql =
        format!("length(s.id) <= {ID_MAX} AND coalesce(length(s.parent_id), 0) <= {ID_MAX}");
    if tables.fork != "NULL" {
        sql.push_str(&format!(
            " AND coalesce(length({}), 0) <= {ID_MAX}",
            tables.fork
        ));
    }
    sql
}

fn row_bytes(row: &Row) -> usize {
    let opt = |s: &Option<String>| s.as_ref().map_or(0, String::len);
    64 + row.id.len()
        + opt(&row.title)
        + opt(&row.directory)
        + opt(&row.project_name)
        + opt(&row.parent)
        + opt(&row.fork)
        + opt(&row.agent)
        + opt(&row.model)
        + opt(&row.version)
}

/// Which session rows a read selects.
enum Rows<'a> {
    /// Keyset page after `cursor`, newest `time_updated` first.
    After {
        cursor: Option<&'a (i64, String)>,
        limit: usize,
    },
    /// One session by exact ID.
    One(&'a str),
}

/// How much of a session row a read pulls. `Light` is metadata only: no message
/// aggregates, usage columns, or activity count, so it never touches message tables.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Detail {
    Light,
    Full,
}

/// Reads up to `limit` rows, stopping early once the byte or time allowance is
/// spent. A zero byte/time allowance reads nothing; otherwise the first row is
/// always taken so a pass makes progress (its size is bounded by the column caps).
/// Returns the rows and whether the result set ended (no more rows to read).
fn read_rows(
    conn: &Connection,
    tables: &Tables,
    shape: Shape,
    select: Rows<'_>,
    detail: Detail,
    budget: &RefreshBudget,
    started: Instant,
) -> Result<(Vec<Row>, bool), ProviderError> {
    if budget.max_bytes == 0 || budget.max_millis == 0 {
        return Ok((Vec::new(), false));
    }
    let deadline = Duration::from_millis(budget.max_millis);
    let (project_name, project_join) = if shape.project {
        (
            format!("substr(p.name, 1, {TEXT_CHARS})"),
            "LEFT JOIN project p ON p.id = s.project_id",
        )
    } else {
        ("NULL".to_owned(), "")
    };
    let v2 = !tables.legacy;
    let full = detail == Detail::Full;
    let usage_cols = USAGE_COLUMNS
        .iter()
        .zip(shape.usage)
        .map(|(column, present)| {
            if full && present && v2 {
                format!("s.{column}")
            } else {
                "NULL".to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let idle_col = if full && shape.idle && v2 {
        "substr(s.idle_outcome, 1, 64)"
    } else {
        "NULL"
    };
    let (message_updated, activity) = if full {
        (
            // Bounded by construction so one query can never outlive a work slice: the
            // newest message activity is the max over the newest {ACTIVITY_WINDOW} rows,
            // and the count saturates at {COUNT_CAP} + 1.
            format!(
                "(SELECT max(w.time_updated) FROM (SELECT time_updated FROM {m} \
                 WHERE session_id = s.id ORDER BY time_created DESC, id DESC \
                 LIMIT {ACTIVITY_WINDOW}) w)",
                m = tables.message
            ),
            format!(
                "(SELECT count(*) FROM (SELECT 1 FROM {m} WHERE session_id = s.id \
                 LIMIT {limit}) c)",
                m = tables.message,
                limit = COUNT_CAP + 1
            ),
        )
    } else {
        ("NULL".to_owned(), "0".to_owned())
    };
    let fork = if tables.fork == "NULL" {
        "NULL".to_owned()
    } else {
        tables.fork.to_owned()
    };
    // Whole JSON or nothing: a cut fragment must never be read as a model id.
    let model = if tables.model == "NULL" {
        "NULL".to_owned()
    } else {
        format!(
            "CASE WHEN length({m}) <= {MODEL_MAX} THEN {m} END",
            m = tables.model
        )
    };
    // The agent is a display string: cut it in SQL, and never select it from a
    // database shape that does not have the column.
    let agent = if !tables.legacy && shape.agent {
        format!("substr(s.agent, 1, {TEXT_CHARS})")
    } else {
        "NULL".to_owned()
    };
    let eligible = eligible_ids(tables);
    let (filter, tail) = match select {
        Rows::After { .. } => (
            "(s.time_updated < ?1 OR (s.time_updated = ?1 AND s.id < ?2))",
            "ORDER BY s.time_updated DESC, s.id DESC LIMIT ?3",
        ),
        Rows::One(_) => ("s.id = ?1", "LIMIT 1"),
    };
    let sql = format!(
        "SELECT s.id, substr(s.title, 1, {TEXT_CHARS}), \
            substr(s.directory, 1, {PATH_CHARS}), {project_name}, \
            s.parent_id, \
            EXISTS(SELECT 1 FROM {t} x WHERE x.id = s.parent_id), {fork}, \
            EXISTS(SELECT 1 FROM {t} y WHERE y.id = {fork}), {agent}, {model}, \
            substr(s.version, 1, {VERSION_CHARS}), \
            s.time_created, s.time_updated, s.time_archived, \
            {message_updated}, \
            {usage_cols}, {idle_col}, \
            {activity} \
         FROM {t} s {project_join} \
         WHERE {eligible} AND {filter} {tail}",
        t = tables.session,
    );
    let mut stmt = conn.prepare(&sql).map_err(map_error)?;
    let (mut query, limit) = match select {
        Rows::After { cursor, limit } => {
            let (after_time, after_id) = match cursor {
                Some((time, id)) => (*time, id.as_str()),
                None => (i64::MAX, ""),
            };
            let limit_sql = i64::try_from(limit).unwrap_or(i64::MAX);
            (
                stmt.query(params![after_time, after_id, limit_sql])
                    .map_err(map_error)?,
                limit,
            )
        }
        Rows::One(id) => (stmt.query(params![id]).map_err(map_error)?, 1),
    };
    let mut rows = Vec::new();
    let mut bytes = 0usize;
    while let Some(r) = query.next().map_err(map_error)? {
        let row = Row {
            id: r.get(0).map_err(map_error)?,
            title: r.get(1).map_err(map_error)?,
            directory: r.get(2).map_err(map_error)?,
            project_name: r.get(3).map_err(map_error)?,
            parent: r.get(4).map_err(map_error)?,
            parent_exists: r.get(5).map_err(map_error)?,
            fork: r.get(6).map_err(map_error)?,
            fork_exists: r.get(7).map_err(map_error)?,
            agent: r.get(8).map_err(map_error)?,
            model: r.get(9).map_err(map_error)?,
            version: r.get(10).map_err(map_error)?,
            created: r.get(11).map_err(map_error)?,
            updated: r.get(12).map_err(map_error)?,
            archived: r.get(13).map_err(map_error)?,
            message_updated: r.get(14).map_err(map_error)?,
            tokens: TokenCategories {
                input: uint(r.get_ref(15).map_err(map_error)?),
                output: uint(r.get_ref(16).map_err(map_error)?),
                reasoning: uint(r.get_ref(17).map_err(map_error)?),
                cache_read: uint(r.get_ref(18).map_err(map_error)?),
                cache_write: uint(r.get_ref(19).map_err(map_error)?),
                ..TokenCategories::default()
            },
            cost: float(r.get_ref(20).map_err(map_error)?),
            idle_outcome: lossy(r.get_ref(21).map_err(map_error)?),
            activity: usize::try_from(r.get::<_, i64>(22).map_err(map_error)?).unwrap_or(0),
        };
        bytes = bytes.saturating_add(row_bytes(&row));
        rows.push(row);
        if rows.len() == limit {
            return Ok((rows, false));
        }
        if bytes >= budget.max_bytes || started.elapsed() >= deadline {
            return Ok((rows, false));
        }
    }
    Ok((rows, true))
}

// ---------------------------------------------------------------------------
// Transcript: `session_message` mapping, keyset paging, source records.
// ---------------------------------------------------------------------------

const MESSAGE_TABLE: &str = "session_message";
const PREVIEW_CAP: usize = 2048;
const DIAG_CAP: usize = 200;
const TYPE_CHARS: usize = 64;

/// One `session_message` row. `data` is `None` when the column is NULL or longer
/// than the record cap (`data_len` then tells which); the oversized body is never
/// read into memory.
struct RawRow {
    id: String,
    created: i64,
    updated: i64,
    ty: String,
    data: Option<String>,
    data_len: Option<i64>,
    /// Legacy part rows only: the owning message's `role` (empty when unknown).
    role: Option<String>,
    /// The id exceeds `ID_MAX`; `id` is only its first `ID_MAX + 1` chars and the row
    /// cannot be keyed.
    oversized: bool,
}

/// The row's change marker. `time_updated` moves whenever the provider rewrites a
/// row in place, so a locator minted for older content no longer matches.
fn generation(row: &RawRow) -> u64 {
    u64::try_from(row.updated).unwrap_or(0)
}

fn lossy(value: rusqlite::types::ValueRef<'_>) -> Option<String> {
    match value {
        rusqlite::types::ValueRef::Text(b) | rusqlite::types::ValueRef::Blob(b) => {
            Some(String::from_utf8_lossy(b).into_owned())
        }
        _ => None,
    }
}

/// What `stream_rows` reads and where it starts.
struct Scan<'a> {
    tables: &'a Tables,
    session: &'a str,
    record_cap: usize,
    direction: PageDirection,
    from: Option<(i64, &'a str)>,
    /// Legacy only: read `part` rows (with the owning message's role) rather than
    /// `message` rows.
    parts: bool,
    /// v2 only: restrict to `assistant` rows.
    assistant_only: bool,
    /// Message rows whose id is too long to key are returned (id cut to a prefix,
    /// `oversized`) instead of filtered out, so callers can see they were omitted.
    flag_oversized: bool,
}

/// Streams rows of one session in `(time_created, id)` order, starting at (and
/// including) the `from` row when given. Keyset comparison on the
/// `(session_id, time_created, id)` index; no `OFFSET`, no row accumulation: the
/// callback sees one row at a time and returns `false` to stop.
fn stream_rows(
    conn: &Connection,
    scan: Scan<'_>,
    mut each: impl FnMut(RawRow) -> Result<bool, ProviderError>,
) -> Result<(), ProviderError> {
    let (cmp, order) = match scan.direction {
        PageDirection::Forward => (">=", "ASC"),
        PageDirection::Backward => ("<=", "DESC"),
    };
    let legacy = scan.tables.legacy;
    let prefix = if scan.parts { "p." } else { "" };
    // Only static fragments are spliced into the statement; every value is bound.
    let bound = if scan.from.is_some() {
        format!(" AND ({prefix}time_created, {prefix}id) {cmp} (?4, ?5)")
    } else {
        String::new()
    };
    let sql = if scan.parts {
        format!(
            "SELECT p.id, p.time_created, p.time_updated, NULL, \
             CASE WHEN length(CAST(p.data AS BLOB)) <= ?2 THEN p.data END, \
             length(CAST(p.data AS BLOB)), \
             CASE WHEN length(p.message_id) <= {ID_MAX} THEN p.message_id END, \
             CASE WHEN length(CAST(m.data AS BLOB)) <= ?2 THEN m.data END, 0 \
             FROM part p LEFT JOIN message m ON m.id = p.message_id \
             WHERE p.session_id = ?1 AND length(p.id) <= ?3{bound} \
             ORDER BY p.time_created {order}, p.id {order}"
        )
    } else {
        let ty = if legacy {
            "NULL".to_owned()
        } else {
            format!("substr(type, 1, {TYPE_CHARS})")
        };
        let only = if scan.assistant_only && !legacy {
            " AND type = 'assistant'"
        } else {
            ""
        };
        let (id_col, id_filter, oversized) = if scan.flag_oversized {
            (
                "CASE WHEN length(id) <= ?3 THEN id ELSE substr(id, 1, ?3 + 1) END",
                "",
                "length(id) > ?3",
            )
        } else {
            ("id", " AND length(id) <= ?3", "0")
        };
        format!(
            "SELECT {id_col}, time_created, time_updated, {ty}, \
             CASE WHEN length(CAST(data AS BLOB)) <= ?2 THEN data END, \
             length(CAST(data AS BLOB)), NULL, NULL, {oversized} \
             FROM {table} \
             WHERE session_id = ?1{id_filter}{only}{bound} \
             ORDER BY time_created {order}, id {order}",
            table = scan.tables.message
        )
    };
    let cap = i64::try_from(scan.record_cap).unwrap_or(i64::MAX);
    let max_id = i64::try_from(ID_MAX).unwrap_or(i64::MAX);
    let mut statement = conn.prepare(&sql).map_err(map_error)?;
    let mut rows = match scan.from {
        Some((created, id)) => statement.query(params![scan.session, cap, max_id, created, id]),
        None => statement.query(params![scan.session, cap, max_id]),
    }
    .map_err(map_error)?;
    // Consecutive parts share a message; resolve its role once.
    let mut last_role: Option<(String, String)> = None;
    while let Some(row) = rows.next().map_err(map_error)? {
        let role = if scan.parts {
            let message_id: Option<String> = row.get(6).map_err(map_error)?;
            let cached = message_id
                .as_ref()
                .and_then(|id| last_role.as_ref().filter(|(last, _)| last == id))
                .map(|(_, role)| role.clone());
            Some(match cached {
                Some(role) => role,
                None => {
                    let role: String = row
                        .get_ref(7)
                        .map_err(map_error)
                        .map(lossy)?
                        .and_then(|d| serde_json::from_str::<Value>(&d).ok())
                        .and_then(|v| str_field(&v, "role").map(|r| r.chars().take(32).collect()))
                        .unwrap_or_default();
                    if let Some(id) = message_id {
                        last_role = Some((id, role.clone()));
                    }
                    role
                }
            })
        } else {
            None
        };
        let raw = RawRow {
            id: row.get(0).map_err(map_error)?,
            created: row.get(1).map_err(map_error)?,
            updated: row.get(2).map_err(map_error)?,
            ty: row
                .get_ref(3)
                .map_err(map_error)
                .map(lossy)?
                .unwrap_or_default(),
            data: row.get_ref(4).map_err(map_error).map(lossy)?,
            data_len: row.get(5).map_err(map_error)?,
            role,
            oversized: row.get(8).map_err(map_error)?,
        };
        if !each(raw)? {
            break;
        }
    }
    Ok(())
}

fn uint(value: rusqlite::types::ValueRef<'_>) -> Option<u64> {
    match value {
        rusqlite::types::ValueRef::Integer(i) => u64::try_from(i).ok(),
        rusqlite::types::ValueRef::Real(f)
            if f.is_finite() && f >= 0.0 && f.fract() == 0.0 && f < 1.0e19 =>
        {
            Some(f as u64)
        }
        _ => None,
    }
}

/// A finite, non-negative number; a reported `0` is kept.
fn float(value: rusqlite::types::ValueRef<'_>) -> Option<f64> {
    match value {
        rusqlite::types::ValueRef::Integer(i) if i >= 0 => Some(i as f64),
        rusqlite::types::ValueRef::Real(f) if f.is_finite() && f >= 0.0 => Some(f),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Lifecycle and derived usage evidence: resumable, budgeted jobs.
// ---------------------------------------------------------------------------

/// Rows examined (newest first) while looking for the latest completed turn.
const LIFECYCLE_ROWS: usize = 256;
/// Rows read (of any kind) when deriving usage from messages.
const DERIVE_ROWS: usize = 100_000;

type Stamp = (i64, Option<i64>);

#[derive(Clone, Debug, PartialEq)]
struct Evidence {
    lifecycle: Lifecycle,
    last_completion: Option<Timestamp>,
    unfinished: bool,
    derived: Option<DerivedUsage>,
}

enum Turn {
    Complete(Timestamp),
    Open,
    Other,
}

fn row_value(row: &RawRow) -> Option<Value> {
    row.data
        .as_deref()
        .and_then(|d| serde_json::from_str::<Value>(d).ok())
}

/// v2 rows carry `type`; legacy message rows carry `role` in their data.
fn row_kind<'a>(row: &'a RawRow, value: Option<&'a Value>) -> Option<&'a str> {
    if row.ty.is_empty() {
        value.and_then(|v| str_field(v, "role"))
    } else {
        Some(row.ty.as_str())
    }
}

/// Turn evidence of one message. A user prompt or `finish: tool-calls` opens
/// work; `finish: stop` or a readable `idle` row (outcome absent or `succeeded`)
/// completes a turn. `unknown`/`length`/null finishes, other or unreadable idle
/// rows, and tool errors are neutral: never a session failure.
fn classify(row: &RawRow) -> Turn {
    let value = row_value(row);
    let text = |name: &str| {
        value
            .as_ref()
            .and_then(|v| str_field(v, name))
            .filter(|s| !s.is_empty())
    };
    match row_kind(row, value.as_ref()) {
        Some("user") => Turn::Open,
        Some("idle") => match &value {
            Some(Value::Object(map)) => match map.get("outcome") {
                None | Some(Value::Null) => Turn::Complete(row.created),
                Some(Value::String(s)) if s == "succeeded" => Turn::Complete(row.created),
                _ => Turn::Other,
            },
            _ => Turn::Other,
        },
        Some("assistant") => match text("finish") {
            Some("stop") => Turn::Complete(
                value
                    .as_ref()
                    .and_then(|v| v.get("time")?.get("completed")?.as_i64())
                    .unwrap_or(row.updated),
            ),
            Some("tool-calls") => Turn::Open,
            _ => Turn::Other,
        },
        _ => Turn::Other,
    }
}

/// `tokens` of an assistant message: `input`, `output`, `reasoning`, `cache.read`,
/// `cache.write`. Absent categories stay `None`.
fn message_tokens(value: &Value) -> TokenCategories {
    let tokens = value.get("tokens");
    let field = |v: Option<&Value>, name: &str| v?.get(name)?.as_u64();
    let cache = tokens.and_then(|t| t.get("cache"));
    TokenCategories {
        input: field(tokens, "input"),
        output: field(tokens, "output"),
        reasoning: field(tokens, "reasoning"),
        cache_read: field(cache, "read"),
        cache_write: field(cache, "write"),
        ..TokenCategories::default()
    }
}

fn has_session_totals(row: &Row) -> bool {
    let t = &row.tokens;
    [t.input, t.output, t.reasoning, t.cache_read, t.cache_write]
        .iter()
        .any(Option::is_some)
}

/// What one refresh step may still spend on evidence.
struct Allowance {
    records: usize,
    bytes: usize,
    deadline: Instant,
}

impl Allowance {
    fn exhausted(&self) -> bool {
        self.records == 0 || self.bytes == 0 || Instant::now() >= self.deadline
    }

    /// Every row read counts, whatever its type or validity.
    fn spend(&mut self, row: &RawRow) {
        let cost = 64 + row.id.len() + row.ty.len() + row.data.as_ref().map_or(0, String::len);
        self.records = self.records.saturating_sub(1);
        self.bytes = self.bytes.saturating_sub(cost);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Lifecycle,
    Derive,
}

/// Resumable evidence collection for one session at one change stamp.
struct Job {
    row: Row,
    stamp: Stamp,
    stage: Stage,
    open: bool,
    completion: Option<Timestamp>,
    life_seen: usize,
    life_cursor: Option<(i64, String)>,
    derivation: Option<OpenCodeDerivation>,
    derive_rows: usize,
    derive_cursor: Option<(i64, String)>,
    /// The cursor is a consumed row (skip it on resume), not a synthetic prefix
    /// bound (a row equal to it has not been seen).
    derive_cursor_is_row: bool,
    omitted: bool,
}

impl Job {
    fn new(row: Row) -> Self {
        let stamp = (row.updated, row.message_updated);
        let derive = !(has_session_totals(&row) && row.cost.is_some());
        Self {
            row,
            stamp,
            stage: Stage::Lifecycle,
            open: false,
            completion: None,
            life_seen: 0,
            life_cursor: None,
            derivation: derive.then(OpenCodeDerivation::new),
            derive_rows: 0,
            derive_cursor: None,
            derive_cursor_is_row: false,
            omitted: false,
        }
    }
}

/// Advances `job` until it finishes (`Some`) or the allowance runs out (`None`,
/// state kept for the next call). At least one row is read per call when rows remain.
fn advance(
    conn: &Connection,
    tables: &Tables,
    job: &mut Job,
    allow: &mut Allowance,
    record_cap: usize,
) -> Result<Option<Evidence>, ProviderError> {
    let session = job.row.id.clone();
    if job.stage == Stage::Lifecycle {
        let resume = job.life_cursor.clone();
        let (mut open, mut completion) = (job.open, job.completion);
        let (mut seen, mut cursor) = (job.life_seen, job.life_cursor.clone());
        let mut more = false;
        stream_rows(
            conn,
            Scan {
                tables,
                session: &session,
                record_cap,
                direction: PageDirection::Backward,
                from: resume.as_ref().map(|(c, i)| (*c, i.as_str())),
                parts: false,
                assistant_only: false,
                flag_oversized: false,
            },
            |r| {
                if resume
                    .as_ref()
                    .is_some_and(|(c, i)| r.created == *c && r.id == *i)
                {
                    return Ok(true);
                }
                seen += 1;
                allow.spend(&r);
                cursor = Some((r.created, r.id.clone()));
                match classify(&r) {
                    Turn::Complete(at) => {
                        completion = Some(at);
                        return Ok(false);
                    }
                    Turn::Open => open = true,
                    Turn::Other => {}
                }
                if seen >= LIFECYCLE_ROWS {
                    return Ok(false);
                }
                more = allow.exhausted();
                Ok(!more)
            },
        )?;
        job.open = open;
        job.completion = completion;
        job.life_seen = seen;
        job.life_cursor = cursor;
        if more && completion.is_none() && seen < LIFECYCLE_ROWS {
            return Ok(None);
        }
        job.stage = Stage::Derive;
        if job.derivation.is_some() && allow.exhausted() {
            return Ok(None);
        }
    }

    if let Some(mut derivation) = job.derivation.take() {
        let resume = job.derive_cursor.clone();
        let skip_resume = job.derive_cursor_is_row;
        let mut cursor_is_row = skip_resume;
        let (mut rows, mut cursor, mut omitted) =
            (job.derive_rows, job.derive_cursor.clone(), job.omitted);
        let mut more = false;
        let mut capped = false;
        stream_rows(
            conn,
            Scan {
                tables,
                session: &session,
                record_cap,
                direction: PageDirection::Forward,
                from: resume.as_ref().map(|(c, i)| (*c, i.as_str())),
                parts: false,
                assistant_only: true,
                flag_oversized: true,
            },
            |r| {
                if !r.oversized
                    && skip_resume
                    && resume
                        .as_ref()
                        .is_some_and(|(c, i)| r.created == *c && r.id == *i)
                {
                    return Ok(true);
                }
                rows += 1;
                allow.spend(&r);
                let value = row_value(&r);
                let kind = row_kind(&r, value.as_ref());
                if r.oversized {
                    // An id too long to key exactly cannot be counted once. Legacy
                    // scans read every role, so the skipped prefix group may hold
                    // assistant rows that were never examined.
                    omitted |= tables.legacy || kind.is_none_or(|k| k == "assistant");
                    // Continue past the whole group of ids sharing this prefix (all
                    // oversized) with a true upper bound; the full id is not kept.
                    match prefix_successor(&r.id) {
                        Some(next) => {
                            cursor = Some((r.created, next));
                            cursor_is_row = false;
                        }
                        None => return Ok(false),
                    }
                } else {
                    cursor = Some((r.created, r.id.clone()));
                    cursor_is_row = true;
                    if kind == Some("assistant") {
                        let tokens = value.as_ref().map(message_tokens);
                        let cost = value
                            .as_ref()
                            .and_then(|v| v.get("cost")?.as_f64())
                            .filter(|c| c.is_finite() && *c >= 0.0);
                        derivation.add(
                            &r.id,
                            tokens.as_ref(),
                            cost,
                            value.as_ref().and_then(message_model),
                        );
                    }
                }
                if rows >= DERIVE_ROWS {
                    capped = true;
                    return Ok(false);
                }
                more = allow.exhausted();
                Ok(!more)
            },
        )?;
        job.derive_rows = rows;
        job.derive_cursor = cursor;
        job.derive_cursor_is_row = cursor_is_row;
        job.omitted = omitted;
        if capped {
            derivation.mark_truncated();
        } else if more {
            job.derivation = Some(derivation);
            return Ok(None);
        }
        if omitted {
            derivation.mark_truncated();
        }
        return Ok(Some(finish_evidence(job, Some(derivation.finish()))));
    }
    Ok(Some(finish_evidence(job, None)))
}

/// Smallest string above every string that starts with `prefix` (byte order of
/// UTF-8 equals code point order): the last char is incremented, dropping trailing
/// maximal chars. `None` when no such bound exists.
fn prefix_successor(prefix: &str) -> Option<String> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(last) = chars.pop() {
        let next = match last as u32 + 1 {
            0xD800 => 0xE000,
            n => n,
        };
        if let Some(c) = char::from_u32(next) {
            chars.push(c);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

fn finish_evidence(job: &Job, derived: Option<DerivedUsage>) -> Evidence {
    let mut completion = job.completion;
    let mut lifecycle = if job.open {
        Lifecycle::Ongoing
    } else if completion.is_some() {
        Lifecycle::Completed
    } else {
        Lifecycle::Unknown
    };
    // The session-level outcome only fills in when no message row says anything
    // about the latest turn. Its time is the session row's reported `time_updated`.
    if lifecycle == Lifecycle::Unknown && job.row.idle_outcome.as_deref() == Some("succeeded") {
        lifecycle = Lifecycle::Completed;
        completion = Some(job.row.updated);
    }
    Evidence {
        lifecycle,
        last_completion: completion,
        unfinished: job.open,
        derived,
    }
}

enum Mapped {
    Event(Box<Event>),
    Diag(Box<DiagnosticEntry>),
}

/// Maps rows to events and diagnostics. `cap` bounds every preview.
struct Mapper<'a> {
    key: &'a SessionKey,
    /// Table named by locators: `session_message` (v2) or `part` (legacy).
    table: &'a str,
    version: Option<&'a str>,
    /// Schema generation, used to pick the documented child-link metadata shape.
    shape: Schema,
    cap: usize,
}

/// Text of a JSON value: strings verbatim, anything else compact JSON.
fn value_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        other => Some(other.to_string()),
    }
}

/// Tool content is often `[{"type":"text","text":...}]`; show the text, not the JSON.
fn content_text(value: &Value) -> Option<String> {
    if let Value::Array(items) = value {
        let texts: Vec<&str> = items
            .iter()
            .filter_map(|i| i.get("text").and_then(Value::as_str))
            .collect();
        if !texts.is_empty() && texts.len() == items.len() {
            return Some(texts.join("\n"));
        }
    }
    value_text(value)
}

fn str_field<'v>(value: &'v Value, name: &str) -> Option<&'v str> {
    value.get(name).and_then(Value::as_str)
}

impl Mapper<'_> {
    fn locator(&self, row: &RawRow, part: usize) -> SourceLocator {
        SourceLocator {
            session: self.key.clone(),
            generation: Some(generation(row)),
            kind: LocatorKind::DbRow {
                table: self.table.to_owned(),
                row_id: row.id.clone(),
                part_index: Some(part),
            },
        }
    }

    fn bounded(&self, text: &str, locator: &SourceLocator) -> BoundedText {
        let mut bounded = preview(text, self.cap, self.cap);
        bounded.locator = Some(locator.clone());
        bounded
    }

    fn diag(&self, row: &RawRow, part: usize, label: &str) -> (usize, Mapped) {
        (
            part,
            Mapped::Diag(Box::new(DiagnosticEntry {
                locator: self.locator(row, part),
                label: label.to_owned(),
                hidden: true,
            })),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn event(
        &self,
        row: &RawRow,
        part: usize,
        record_type: String,
        kind: EventKind,
        actor: Actor,
        text: Option<String>,
        tool: Option<ToolInteraction>,
    ) -> (usize, Mapped) {
        let locator = self.locator(row, part);
        let text = text
            .filter(|t| !t.is_empty())
            .map(|t| self.bounded(&t, &locator));
        (
            part,
            Mapped::Event(Box::new(Event {
                id: format!("{}#{part}", row.id),
                session: self.key.clone(),
                timestamp: Some(row.created),
                source_order: part as u64,
                kind,
                actor,
                text,
                tool,
                usage: None,
                provenance: Provenance {
                    harness: Harness::OpenCode,
                    source: self.key.source.clone(),
                    locator,
                    record_type,
                    harness_version: self.version.map(str::to_owned),
                },
            })),
        )
    }

    /// Items of one row in part order. A row that displays nothing yields one
    /// "source-only" diagnostic so it stays reachable in the source view.
    fn map_row(&self, row: &RawRow, record_cap: usize) -> Vec<(usize, Mapped)> {
        let Some(data) = &row.data else {
            let label = if row.data_len.is_some_and(|n| n > record_cap as i64) {
                "oversized record"
            } else {
                "source-only record (no data)"
            };
            return vec![self.diag(row, 0, label)];
        };
        let Ok(value @ Value::Object(_)) = serde_json::from_str::<Value>(data) else {
            return vec![self.diag(row, 0, "malformed record")];
        };
        if let Some(role) = &row.role {
            return self.map_legacy(row, role, &value);
        }
        let ty = row.ty.as_str();
        let one = |kind, actor, text: Option<String>| {
            self.event(row, 0, ty.to_owned(), kind, actor, text, None)
        };
        let mut items: Vec<(usize, Mapped)> = Vec::new();
        match ty {
            "user" => {
                if let Some(text) = str_field(&value, "text").filter(|t| !t.is_empty()) {
                    items.push(one(
                        EventKind::UserPrompt,
                        Actor::User,
                        Some(text.to_owned()),
                    ));
                }
                let mut part = 1;
                for (label, field) in [("file", "files"), ("agent", "agents")] {
                    for item in value
                        .get(field)
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        let name = ["name", "filename", "path", "uri", "url"]
                            .iter()
                            .find_map(|k| str_field(item, k))
                            .map(str::to_owned)
                            .or_else(|| value_text(item))
                            .unwrap_or_default();
                        items.push(self.event(
                            row,
                            part,
                            ty.to_owned(),
                            EventKind::System,
                            Actor::User,
                            Some(format!("{label}: {name}")),
                            None,
                        ));
                        part += 1;
                    }
                }
            }
            "system" | "synthetic" => {
                if let Some(text) = str_field(&value, "text").filter(|t| !t.is_empty()) {
                    items.push(one(EventKind::System, Actor::System, Some(text.to_owned())));
                }
            }
            "compaction" => {
                let text = ["summary", "text"]
                    .iter()
                    .find_map(|k| str_field(&value, k))
                    .filter(|t| !t.is_empty())
                    .unwrap_or("Context compacted");
                items.push(one(
                    EventKind::Compaction,
                    Actor::System,
                    Some(text.to_owned()),
                ));
            }
            "idle" => {
                let text = match str_field(&value, "outcome").filter(|o| !o.is_empty()) {
                    Some(outcome) => format!("Idle \u{2014} {outcome}"),
                    None => "Idle".to_owned(),
                };
                items.push(one(EventKind::Lifecycle, Actor::System, Some(text)));
            }
            "assistant" => {
                if let Some(parts) = value.get("content").and_then(Value::as_array) {
                    for (part, item) in parts.iter().enumerate() {
                        items.extend(self.assistant_part(row, part, item));
                    }
                }
            }
            _ => return vec![self.diag(row, 0, "unknown record type")],
        }
        if items.is_empty() {
            items.push(self.diag(row, 0, "source-only record (nothing to display)"));
        }
        items
    }

    /// Legacy `part` row: `type` lives in the data, `role` comes from the message.
    fn map_legacy(&self, row: &RawRow, role: &str, value: &Value) -> Vec<(usize, Mapped)> {
        let ty = str_field(value, "type").unwrap_or_default();
        let record_type = format!("part/{ty}");
        let text = str_field(value, "text").filter(|t| !t.is_empty());
        let one = |kind, actor, text: Option<String>| {
            vec![self.event(row, 0, record_type.clone(), kind, actor, text, None)]
        };
        let nothing = || vec![self.diag(row, 0, "source-only record (nothing to display)")];
        match ty {
            "text" => match (text, role) {
                (Some(t), "user") => one(EventKind::UserPrompt, Actor::User, Some(t.to_owned())),
                (Some(t), "assistant") => one(
                    EventKind::AssistantMessage,
                    Actor::Assistant,
                    Some(t.to_owned()),
                ),
                (Some(t), _) => one(EventKind::System, Actor::System, Some(t.to_owned())),
                (None, _) => nothing(),
            },
            "reasoning" => match text {
                Some(t) => one(EventKind::Reasoning, Actor::Assistant, Some(t.to_owned())),
                None => nothing(),
            },
            "compaction" => one(
                EventKind::Compaction,
                Actor::System,
                Some("Context compacted".to_owned()),
            ),
            "tool" => vec![self.legacy_tool(row, value)],
            "file" | "patch" | "agent" | "subtask" => {
                let mut rest = value.clone();
                if let Value::Object(map) = &mut rest {
                    map.remove("type");
                }
                one(
                    EventKind::System,
                    Actor::System,
                    Some(format!("{ty}: {rest}")),
                )
            }
            "step-start" | "step-finish" | "snapshot" | "retry" => {
                vec![self.diag(row, 0, "step marker")]
            }
            _ => vec![self.diag(row, 0, "unknown part type")],
        }
    }

    fn legacy_tool(&self, row: &RawRow, item: &Value) -> (usize, Mapped) {
        let locator = self.locator(row, 0);
        let null = Value::Null;
        let state = item.get("state").unwrap_or(&null);
        let status = match str_field(state, "status") {
            Some("completed") => ToolStatus::Completed,
            Some("error") => ToolStatus::Error,
            _ => ToolStatus::Unknown,
        };
        let time = state.get("time").unwrap_or(&null);
        let stamp = |name: &str| time.get(name).and_then(Value::as_i64);
        let metadata = state.get("metadata").unwrap_or(&null);
        let exit_code = ["exit", "exitCode", "exit_code"]
            .iter()
            .find_map(|k| metadata.get(*k).and_then(Value::as_i64))
            .and_then(|n| i32::try_from(n).ok());
        let name = str_field(item, "tool")
            .filter(|n| !n.is_empty())
            .unwrap_or("tool");
        let tool = ToolInteraction {
            name: preview(name, self.cap, self.cap).preview,
            call_id: str_field(item, "callID").map(str::to_owned),
            input: state
                .get("input")
                .and_then(value_text)
                .map(|t| self.bounded(&t, &locator)),
            output: state
                .get("output")
                .or_else(|| state.get("error"))
                .and_then(content_text)
                .map(|t| self.bounded(&t, &locator)),
            status,
            exit_code,
            started_at: stamp("start"),
            completed_at: stamp("end"),
            orphan: false,
            child_session: opencode_child_link(name, metadata, self.shape)
                .map(SessionLink::Unresolved),
        };
        self.event(
            row,
            0,
            "part/tool".to_owned(),
            EventKind::Tool,
            Actor::Tool,
            None,
            Some(tool),
        )
    }

    fn assistant_part(&self, row: &RawRow, part: usize, item: &Value) -> Option<(usize, Mapped)> {
        let record_type = |inner: &str| format!("assistant/{inner}");
        let text_of = |name: &str| str_field(item, name).filter(|t| !t.is_empty());
        match str_field(item, "type") {
            Some("text") => text_of("text").map(|t| {
                self.event(
                    row,
                    part,
                    record_type("text"),
                    EventKind::AssistantMessage,
                    Actor::Assistant,
                    Some(t.to_owned()),
                    None,
                )
            }),
            Some("reasoning") => text_of("text").map(|t| {
                self.event(
                    row,
                    part,
                    record_type("reasoning"),
                    EventKind::Reasoning,
                    Actor::Assistant,
                    Some(t.to_owned()),
                    None,
                )
            }),
            Some("tool") => Some(self.tool_part(row, part, item)),
            Some(label @ ("patch" | "file" | "agent" | "subtask")) => {
                let mut rest = item.clone();
                if let Value::Object(map) = &mut rest {
                    map.remove("type");
                }
                Some(self.event(
                    row,
                    part,
                    record_type(label),
                    EventKind::System,
                    Actor::System,
                    Some(format!("{label}: {rest}")),
                    None,
                ))
            }
            _ => Some(self.diag(row, part, "unknown part type")),
        }
    }

    fn tool_part(&self, row: &RawRow, part: usize, item: &Value) -> (usize, Mapped) {
        let locator = self.locator(row, part);
        let null = Value::Null;
        let state = item.get("state").unwrap_or(&null);
        let status = match str_field(state, "status") {
            Some("completed") => ToolStatus::Completed,
            Some("error") => ToolStatus::Error,
            _ => ToolStatus::Unknown,
        };
        let time = item.get("time").unwrap_or(&null);
        let stamp = |name: &str| time.get(name).and_then(Value::as_i64);
        let metadata = state.get("metadata").unwrap_or(&null);
        let exit_code = ["exit", "exitCode", "exit_code"]
            .iter()
            .find_map(|k| metadata.get(*k).and_then(Value::as_i64))
            .and_then(|n| i32::try_from(n).ok());
        let name = str_field(item, "name")
            .filter(|n| !n.is_empty())
            .unwrap_or("tool");
        let tool = ToolInteraction {
            name: preview(name, self.cap, self.cap).preview,
            call_id: str_field(item, "id").map(str::to_owned),
            input: state
                .get("input")
                .and_then(value_text)
                .map(|t| self.bounded(&t, &locator)),
            output: state
                .get("content")
                .and_then(content_text)
                .map(|t| self.bounded(&t, &locator)),
            status,
            exit_code,
            started_at: stamp("created").or_else(|| stamp("ran")),
            completed_at: stamp("completed"),
            orphan: false,
            child_session: opencode_child_link(name, metadata, self.shape)
                .map(SessionLink::Unresolved),
        };
        self.event(
            row,
            part,
            "assistant/tool".to_owned(),
            EventKind::Tool,
            Actor::Tool,
            None,
            Some(tool),
        )
    }
}

/// Anchor whose derived `Ord` (timestamp, source_order, stable_id) equals the SQL
/// order `(time_created, id, part)`: `source_order` is a constant 0 and `stable_id`
/// is the lowercase hex of the row id's bytes (byte order is preserved, any id is
/// representable), `#`, and the zero-padded part index. `#` sorts below every hex
/// digit, so an id that prefixes another still orders first, as in SQL.
fn anchor_of(e: &Event, row_id: &str) -> PageAnchor {
    let mut stable_id = String::with_capacity(row_id.len() * 2 + 11);
    for byte in row_id.bytes() {
        stable_id.push_str(&format!("{byte:02x}"));
    }
    stable_id.push_str(&format!("#{:010}", e.source_order));
    PageAnchor {
        timestamp: e.timestamp,
        source_order: 0,
        stable_id,
    }
}

/// Bytes an event contributes to the page budget: its bounded previews.
fn event_cost(e: &Event) -> usize {
    let len = |t: &Option<BoundedText>| t.as_ref().map_or(0, |b| b.preview.len());
    let link = |t: &ToolInteraction| match &t.child_session {
        Some(SessionLink::Unresolved(id)) => id.len(),
        Some(SessionLink::Resolved(key)) => key.native_id.len(),
        Some(SessionLink::Ambiguous) | None => 0,
    };
    len(&e.text)
        + e.tool.as_ref().map_or(0, |t| {
            t.name.len() + len(&t.input) + len(&t.output) + link(t)
        })
}

fn event_truncated(e: &Event) -> bool {
    let cut = |t: &Option<BoundedText>| t.as_ref().is_some_and(|b| b.truncated);
    cut(&e.text)
        || e.tool
            .as_ref()
            .is_some_and(|t| cut(&t.input) || cut(&t.output))
}

fn push_diag(into: &mut Vec<DiagnosticEntry>, entry: DiagnosticEntry, truncated: &mut bool) {
    if into.len() < DIAG_CAP {
        into.push(entry);
    } else {
        *truncated = true;
    }
}

/// Splits an anchor into `(time_created, row id, part index)`.
fn anchor_parts(anchor: &PageAnchor) -> Result<(i64, String, usize), ProviderError> {
    let bad = || ProviderError::Unsupported("not an OpenCode anchor".into());
    let created = anchor.timestamp.ok_or_else(bad)?;
    let (hex, part) = anchor.stable_id.rsplit_once('#').ok_or_else(bad)?;
    if anchor.source_order != 0 || part.len() != 10 || hex.len() % 2 != 0 || !hex.is_ascii() {
        return Err(bad());
    }
    let bytes = (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16))
        .collect::<Result<Vec<u8>, _>>()
        .map_err(|_| bad())?;
    let id = String::from_utf8(bytes).map_err(|_| bad())?;
    Ok((created, id, part.parse().map_err(|_| bad())?))
}

impl OpenCodeScanner {
    fn set_busy_timeout(&self, millis: u64) -> Result<(), ProviderError> {
        match &self.connection {
            Some(conn) => conn
                .busy_timeout(Duration::from_millis(millis))
                .map_err(map_error),
            None => Ok(()),
        }
    }

    /// Opens (if needed) with the page timeout, runs `f`, and drops the connection
    /// on any failure that is not a plain refusal so the next call reopens.
    fn guarded<T>(
        &mut self,
        f: impl FnOnce(&Self, &Connection) -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        let page_ms = self.config.page_busy_timeout_ms;
        let result = self
            .ensure_open()
            .and_then(|()| self.set_busy_timeout(page_ms))
            .and_then(|()| {
                let conn = self.connection.as_ref().expect("opened");
                f(self, conn)
            });
        if matches!(&result, Err(e) if !matches!(e, ProviderError::Unsupported(_))) {
            self.connection = None;
        }
        result
    }

    /// Validates a session key and that this schema has a readable transcript.
    fn check_session(&self, conn: &Connection, key: &SessionKey) -> Result<Tables, ProviderError> {
        let Some(tables) = Tables::for_schema(self.schema) else {
            return Err(ProviderError::Unsupported(
                "unsupported OpenCode database schema".into(),
            ));
        };
        let valid = key.harness == Harness::OpenCode
            && matches!(&key.source, SourceKey::DbRow { database, table, id }
                if *database == self.path && table == tables.session && *id == key.native_id);
        if !valid {
            return Err(ProviderError::Unsupported("not an OpenCode session".into()));
        }
        let exists: i64 = conn
            .query_row(
                &format!("SELECT count(*) FROM {} WHERE id = ?1", tables.session),
                params![key.native_id],
                |r| r.get(0),
            )
            .map_err(map_error)?;
        if exists == 0 {
            return Err(ProviderError::Unavailable(
                "unknown OpenCode session".into(),
            ));
        }
        Ok(tables)
    }

    fn version_of(&self, key: &SessionKey) -> Option<&str> {
        self.known
            .get(&key.native_id)
            .and_then(|k| k.version.as_deref())
    }

    #[allow(clippy::too_many_arguments)]
    fn page(
        &self,
        conn: &Connection,
        key: &SessionKey,
        anchor: Option<&PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
        resume: Option<(i64, String)>,
        mut slice: Option<&mut PageSlice>,
    ) -> Result<TranscriptPage, ProviderError> {
        let tables = self.check_session(conn, key)?;
        let at = anchor.map(anchor_parts).transpose()?;
        // A resumed pager continues after the last row it fully processed.
        let resume_row = resume.clone();
        let at = match resume {
            Some((created, id)) => Some((created, id, 0usize)),
            None => at,
        };
        let record_cap = self.config.max_record_bytes;
        let mapper = Mapper {
            key,
            table: tables.transcript,
            version: self.version_of(key),
            shape: self.schema,
            cap: (request.max_bytes / 4).clamp(1, PREVIEW_CAP),
        };
        let max_events = request.max_events.max(1);
        let forward = direction == PageDirection::Forward;
        let mut events: Vec<(Event, String)> = Vec::new();
        let mut diags = Vec::new();
        // Forward: diagnostics waiting for the next event. Backward: the page's own.
        // A diagnostic belongs to the page of the next event after it (or the last
        // page), so both directions partition diagnostics identically.
        let mut pending = Vec::new();
        let mut truncated = false;
        let mut bytes = 0usize;
        let mut has_more = false;
        stream_rows(
            conn,
            Scan {
                tables: &tables,
                session: &key.native_id,
                record_cap,
                direction,
                from: at.as_ref().map(|(created, id, _)| (*created, id.as_str())),
                parts: tables.legacy,
                assistant_only: false,
                flag_oversized: false,
            },
            |row| {
                // The row a previous slice already processed is skipped before any charge.
                if resume_row
                    .as_ref()
                    .is_some_and(|(c, i)| row.created == *c && row.id == *i)
                {
                    return Ok(true);
                }
                if let Some(s) = slice.as_deref_mut() {
                    if s.examined > 0 && s.spent() {
                        s.stopped = true;
                        return Ok(false);
                    }
                    s.examined += 1;
                    s.read = s.read.saturating_add(
                        usize::try_from(row.data_len.unwrap_or(0)).unwrap_or(usize::MAX),
                    );
                }
                let (row_created, row_id) = (row.created, row.id.clone());
                let mut items = mapper.map_row(&row, record_cap);
                if !forward {
                    items.reverse();
                }
                for (part, mapped) in items {
                    // The anchor's own row resumes after (forward) / before
                    // (backward) the anchored part.
                    if let Some((created, id, at_part)) = &at
                        && row.created == *created
                        && row.id == *id
                        && (if forward {
                            part <= *at_part
                        } else {
                            part >= *at_part
                        })
                    {
                        continue;
                    }
                    let keep_diag = forward || anchor.is_none() || !events.is_empty();
                    match mapped {
                        Mapped::Diag(d) => {
                            if keep_diag {
                                let sink = if forward { &mut pending } else { &mut diags };
                                push_diag(sink, *d, &mut truncated);
                            }
                        }
                        Mapped::Event(event) => {
                            let cost = event_cost(&event);
                            if !events.is_empty()
                                && (events.len() >= max_events || bytes + cost > request.max_bytes)
                            {
                                has_more = true;
                                return Ok(false);
                            }
                            if forward {
                                for d in pending.drain(..) {
                                    push_diag(&mut diags, d, &mut truncated);
                                }
                            }
                            bytes += cost;
                            events.push((*event, row.id.clone()));
                        }
                    }
                }
                if let Some(s) = slice.as_deref_mut() {
                    s.last = Some((row_created, row_id));
                }
                Ok(true)
            },
        )?;
        if forward && !has_more {
            for d in pending {
                push_diag(&mut diags, d, &mut truncated);
            }
        }
        if !forward {
            events.reverse();
            diags.reverse();
        }
        truncated |= events.iter().any(|(e, _)| event_truncated(e));
        let next = if forward {
            events.last()
        } else {
            events.first()
        }
        .map(|(e, id)| anchor_of(e, id));
        Ok(TranscriptPage {
            events: events.into_iter().map(|(e, _)| e).collect(),
            diagnostics: diags,
            next,
            has_more,
            truncated,
        })
    }

    /// One bounded slice of counting events after `anchor`: at most `budget.max_records`
    /// rows, `budget.max_bytes` of row data and `budget.max_millis`, then `Pending`.
    fn count_slice(
        &self,
        conn: &Connection,
        job: ReadJobId,
        key: &SessionKey,
        anchor: &PageAnchor,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError> {
        let tables = self.check_session(conn, key)?;
        let (created, at_id, at_part) = anchor_parts(anchor)?;
        let record_cap = self.config.max_record_bytes;
        let mapper = Mapper {
            key,
            table: tables.transcript,
            version: None,
            shape: self.schema,
            cap: 1,
        };
        let slot = (key.clone(), job);
        let resumed = self
            .counts
            .borrow()
            .get(&slot)
            .map(|j| (j.cursor.clone(), j.count));
        let (from, mut count) = match &resumed {
            Some(((c, id), n)) => ((*c, id.clone()), *n),
            None => ((created, at_id.clone()), 0),
        };
        let started = Instant::now();
        let (mut rows, mut bytes) = (0usize, 0usize);
        let mut last = from.clone();
        let mut stopped = false;
        stream_rows(
            conn,
            Scan {
                tables: &tables,
                session: &key.native_id,
                record_cap,
                direction: PageDirection::Forward,
                from: Some((from.0, from.1.as_str())),
                parts: tables.legacy,
                assistant_only: false,
                flag_oversized: false,
            },
            |row| {
                if resumed.is_some() && row.created == from.0 && row.id == from.1 {
                    return Ok(true); // consumed by the previous slice
                }
                if rows > 0
                    && (rows >= budget.max_records
                        || bytes >= budget.max_bytes
                        || started.elapsed().as_millis() >= u128::from(budget.max_millis))
                {
                    stopped = true;
                    return Ok(false);
                }
                let same = resumed.is_none() && row.created == created && row.id == at_id;
                count += mapper
                    .map_row(&row, record_cap)
                    .iter()
                    .filter(|(part, mapped)| {
                        matches!(mapped, Mapped::Event(_)) && !(same && *part <= at_part)
                    })
                    .count();
                rows += 1;
                bytes = bytes.saturating_add(
                    usize::try_from(row.data_len.unwrap_or(0)).unwrap_or(usize::MAX),
                );
                last = (row.created, row.id.clone());
                Ok(true)
            },
        )?;
        if stopped {
            self.counts.borrow_mut().insert(
                slot,
                CountJob {
                    cursor: last,
                    count,
                },
            );
            Ok(ReadProgress::Pending(WorkProgress {
                records: rows,
                expanded_bytes: bytes,
            }))
        } else {
            self.counts.borrow_mut().remove(&slot);
            Ok(ReadProgress::Ready(count))
        }
    }

    /// Recorded request observations (assistant messages with token evidence), newest
    /// first, resuming from `job.cursor`. One call examines at most `budget.max_records`
    /// rows / `budget.max_bytes` of row data / `budget.max_millis`, whether or not they
    /// yield observations; it reports `Slice` when it stopped on that allowance.
    #[allow(clippy::too_many_arguments)]
    fn requests_slice(
        &self,
        conn: &Connection,
        key: &SessionKey,
        tables: &Tables,
        job: &mut DetailJob,
        limit: usize,
        max_bytes: usize,
        budget: RefreshBudget,
    ) -> Result<RequestsStatus, ProviderError> {
        let record_cap = self.config.max_record_bytes;
        let started = Instant::now();
        let (mut examined, mut read) = (0usize, 0usize);
        let start = job.cursor.clone();
        let mut status = RequestsStatus::Done;
        stream_rows(
            conn,
            Scan {
                tables,
                session: &key.native_id,
                record_cap,
                direction: PageDirection::Backward,
                from: start.as_ref().map(|(c, i)| (*c, i.as_str())),
                parts: false,
                // Not filtered in SQL: every row advances the cursor and spends the
                // allowance, so a long run of other rows is crossed in slices.
                assistant_only: false,
                flag_oversized: false,
            },
            |row| {
                if start
                    .as_ref()
                    .is_some_and(|(c, i)| row.created == *c && row.id == *i)
                {
                    return Ok(true);
                }
                if examined > 0
                    && (examined >= budget.max_records
                        || read >= budget.max_bytes
                        || started.elapsed().as_millis() >= u128::from(budget.max_millis))
                {
                    status = RequestsStatus::Slice;
                    return Ok(false);
                }
                examined += 1;
                read = read.saturating_add(
                    usize::try_from(row.data_len.unwrap_or(0)).unwrap_or(usize::MAX),
                );
                job.cursor = Some((row.created, row.id.clone()));
                if row.ty != "assistant" {
                    return Ok(true);
                }
                let Some(value) = row_value(&row) else {
                    return Ok(true);
                };
                if value.get("tokens").is_none() {
                    return Ok(true);
                }
                let model = message_model(&value).map(|(provider, id)| ModelDetail {
                    id,
                    provider,
                    variant: None,
                    effort: None,
                    configured_effort: None,
                });
                let cost = value.get("cost").and_then(Value::as_f64);
                let size = 256 + row.id.len() + model.as_ref().map_or(0, |m| m.id.len());
                if !job.out.is_empty()
                    && (job.out.len() >= limit || job.bytes.saturating_add(size) > max_bytes)
                {
                    status = RequestsStatus::More;
                    return Ok(false);
                }
                job.bytes += size;
                let locator = SourceLocator {
                    session: key.clone(),
                    generation: Some(generation(&row)),
                    kind: LocatorKind::DbRow {
                        table: tables.transcript.to_owned(),
                        row_id: row.id.clone(),
                        part_index: Some(0),
                    },
                };
                job.out.push(UsageObservation {
                    model,
                    scope: UsageScope::Message,
                    categories: message_tokens(&value),
                    accounting: TokenAccounting {
                        input: InputAccounting::AdditiveCaches,
                        output: OutputAccounting::SeparateReasoning,
                    },
                    request: Some(RequestMetadata {
                        id: Some(row.id.clone()),
                        completed_at: value
                            .get("time")
                            .and_then(|t| t.get("completed"))
                            .and_then(Value::as_i64),
                        ..RequestMetadata::default()
                    }),
                    money: RecordedMoney {
                        reported_api: cost.map(|usd| MoneyFact {
                            usd,
                            source: MoneySource::ReportedApi,
                            as_of: None,
                        }),
                        ..RecordedMoney::default()
                    },
                    provenance: Provenance {
                        harness: Harness::OpenCode,
                        source: key.source.clone(),
                        locator,
                        record_type: "assistant".into(),
                        harness_version: self.version_of(key).map(str::to_owned),
                    },
                });
                job.last = Some((row.created, row.id.clone()));
                Ok(true)
            },
        )?;
        if status == RequestsStatus::More {
            // The row that did not fit is not consumed: the next page starts after the
            // last emitted request.
            job.cursor = job.last.clone();
        }
        job.examined += examined;
        job.read += read;
        Ok(status)
    }

    fn record(
        &self,
        conn: &Connection,
        locator: &SourceLocator,
    ) -> Result<SourceRecord, ProviderError> {
        let LocatorKind::DbRow {
            table,
            row_id,
            part_index,
        } = &locator.kind
        else {
            return Err(ProviderError::Unsupported(
                "not an OpenCode row locator".into(),
            ));
        };
        let tables = self.check_session(conn, &locator.session)?;
        if table != tables.transcript {
            return Err(ProviderError::Unsupported(
                "locator names a table outside the OpenCode allowlist".into(),
            ));
        }
        let session = &locator.session.native_id;
        let record_cap = self.config.max_record_bytes;
        let cap = i64::try_from(record_cap).unwrap_or(i64::MAX);
        let ty = if tables.legacy {
            "NULL".to_owned()
        } else {
            format!("substr(type, 1, {TYPE_CHARS})")
        };
        let found = conn
            .query_row(
                &format!(
                    "SELECT session_id, time_created, time_updated, {ty}, \
                     CASE WHEN length(CAST(data AS BLOB)) <= ?2 THEN data END, \
                     length(CAST(data AS BLOB)) \
                     FROM {table} WHERE id = ?1 AND length(id) <= ?3",
                    table = tables.transcript
                ),
                params![row_id, cap, i64::try_from(ID_MAX).unwrap_or(i64::MAX)],
                |r| {
                    let ty = lossy(r.get_ref(3)?).unwrap_or_default();
                    let data = lossy(r.get_ref(4)?);
                    Ok((
                        r.get::<_, String>(0)?,
                        RawRow {
                            id: row_id.clone(),
                            created: r.get(1)?,
                            updated: r.get(2)?,
                            ty,
                            data,
                            data_len: r.get(5)?,
                            oversized: false,
                            // Only the part's own data is shown; the role is not needed.
                            role: tables.legacy.then(String::new),
                        },
                    ))
                },
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(map_error(other)),
            })?;
        let Some((owner, row)) = found else {
            return Err(ProviderError::Unavailable(
                "source row no longer exists".into(),
            ));
        };
        if owner != *session {
            return Err(ProviderError::Unsupported(
                "locator does not belong to this session".into(),
            ));
        }
        if locator.generation != Some(generation(&row)) {
            return Err(ProviderError::Unavailable(
                "stale source locator: the row was updated".into(),
            ));
        }
        let mapper = Mapper {
            key: &locator.session,
            table: tables.transcript,
            version: None,
            shape: self.schema,
            cap: 1,
        };
        if let Some(part) = part_index
            && !mapper
                .map_row(&row, record_cap)
                .iter()
                .any(|(p, _)| p == part)
        {
            return Err(ProviderError::Unavailable(
                "locator names a part the record does not have".into(),
            ));
        }
        let Some(raw) = row.data else {
            let reason = if row.data_len.is_some() {
                format!("record exceeds the {record_cap}-byte limit")
            } else {
                "record has no data".to_owned()
            };
            return Ok(SourceRecord {
                raw: String::new(),
                display: String::new(),
                unavailable: Some(reason),
            });
        };
        let display = display_safe(&raw);
        Ok(SourceRecord {
            raw,
            display,
            unavailable: None,
        })
    }
}

impl ProviderReader for OpenCodeScanner {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn discover(
        &mut self,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        let started = Instant::now();
        // Findings are kept outside the slice, so an interrupted or failed slice still
        // delivers (or retains) every session and removal it already consumed.
        let mut sessions = std::mem::take(&mut self.carry_sessions);
        let mut removed = std::mem::take(&mut self.carry_removed);
        let result = self.sliced(budget, |this, conn, _| {
            this.discover_step(conn, budget, started, &mut sessions, &mut removed)
        });
        match result {
            // The slice ended inside one query: report progress and try again with
            // more time, never a failure.
            Err(ProviderError::Unavailable(m)) if m.contains(INTERRUPTED) => Ok(DiscoveryDelta {
                sessions,
                removed,
                invalidated: std::mem::take(&mut self.invalidated),
                warnings: vec![],
                scan: ScanProgress {
                    harness: Harness::OpenCode,
                    scanned: 0,
                    total: None,
                    done: false,
                },
            }),
            Err(error) => {
                self.carry_sessions = sessions;
                self.carry_removed = removed;
                Err(error)
            }
            ok => ok,
        }
    }

    fn hydrate(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        let started = Instant::now();
        let result = self.sliced(budget, |this, conn, _| {
            this.hydrate_step(conn, job, key, budget, started)
        });
        if result.is_err() {
            self.ledger.release(key, job);
        }
        result
    }

    fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId) {
        self.counts.borrow_mut().remove(&(key.clone(), job));
        self.details.borrow_mut().remove(&(key.clone(), job));
        if self.ledger.release(key, job) {
            // The last consumer is gone: abandoned evidence collection goes, cached
            // completed evidence stays.
            self.jobs.remove(&key.native_id);
        }
    }

    fn details_page(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, ProviderError> {
        // Request observations come from assistant messages that recorded `tokens`
        // (v2 schema); the combined record and byte budget is shared with the recent
        // events of the first page. OpenCode records no compaction markers in its
        // session rows, so `compactions` is always empty. A request anchor
        // (`request:<id>`) continues into older requests only. The scan is resumable:
        // every call examines a bounded number of rows and answers `Pending` until
        // the page is complete.
        let max = request.max_events.max(1);
        let after = match &anchor {
            None => None,
            Some(a) => {
                let id = a
                    .stable_id
                    .strip_prefix("request:")
                    .ok_or_else(|| ProviderError::Unsupported("not a request anchor".into()))?;
                let created = a
                    .timestamp
                    .ok_or_else(|| ProviderError::Unsupported("not a request anchor".into()))?;
                Some((created, id.to_owned()))
            }
        };
        let first = anchor.is_none();
        let slot = (key.clone(), job);
        let result = self.sliced(budget, |this, conn, deadline| {
            let tables = this.check_session(conn, key)?;
            let mut state = this
                .details
                .borrow_mut()
                .remove(&slot)
                .unwrap_or(DetailJob {
                    cursor: after.clone(),
                    recent_done: !first,
                    ..DetailJob::default()
                });
            let (examined_before, read_before) = (state.examined, state.read);
            let (n, bytes) = if first {
                (max.div_ceil(2), request.max_bytes / 2)
            } else {
                (max, request.max_bytes)
            };
            // Every exit that is not a finished page puts the continuation back, so an
            // interrupted or exhausted slice resumes where it stopped.
            let park = |state: DetailJob, this: &Self| {
                let progress = WorkProgress {
                    records: state.examined - examined_before,
                    expanded_bytes: state.read - read_before,
                };
                this.details.borrow_mut().insert(slot.clone(), state);
                progress
            };
            let mut status = RequestsStatus::Done;
            if !state.requests_done && !tables.legacy {
                match this.requests_slice(conn, key, &tables, &mut state, n, bytes.max(1), budget) {
                    Ok(s) => status = s,
                    Err(e) => {
                        park(state, this);
                        return Err(e);
                    }
                }
                if status == RequestsStatus::Slice {
                    return Ok(ReadProgress::Pending(park(state, this)));
                }
            }
            if !state.requests_done {
                state.requests_done = true;
                state.more = status == RequestsStatus::More;
            }
            if first && !state.recent_done {
                let left = max.saturating_sub(state.out.len());
                let cap_bytes = request.max_bytes / 2;
                // The page is finished once its record or byte allowance is used up,
                // however many slices it took to collect.
                if left == 0
                    || state.recent.len() >= left
                    || (!state.recent.is_empty() && state.recent_bytes >= cap_bytes)
                {
                    state.recent_truncated |= !state.recent.is_empty();
                    state.recent_done = true;
                } else {
                    // The recent pager shares this call's remaining row allowance.
                    let used = state.examined - examined_before;
                    let used_bytes = state.read - read_before;
                    if used >= budget.max_records.max(1) || used_bytes >= budget.max_bytes.max(1) {
                        return Ok(ReadProgress::Pending(park(state, this)));
                    }
                    let mut slice = PageSlice {
                        max_records: budget.max_records.max(1) - used,
                        max_bytes: budget.max_bytes.max(1) - used_bytes,
                        deadline,
                        examined: 0,
                        read: 0,
                        stopped: false,
                        last: None,
                    };
                    let events = match this.page(
                        conn,
                        key,
                        None,
                        PageDirection::Backward,
                        PageRequest {
                            max_events: left - state.recent.len(),
                            max_bytes: cap_bytes.saturating_sub(state.recent_bytes).max(1),
                        },
                        state.recent_cursor.clone(),
                        Some(&mut slice),
                    ) {
                        Ok(events) => events,
                        Err(e) => {
                            park(state, this);
                            return Err(e);
                        }
                    };
                    state.examined += slice.examined;
                    state.read += slice.read;
                    state.recent_truncated |= events.truncated;
                    // Events past the accumulated allowance are not taken (a
                    // continuation always returns at least one event).
                    let mut full = false;
                    for event in events.events {
                        let cost = event_cost(&event);
                        if !state.recent.is_empty()
                            && (state.recent.len() >= left
                                || state.recent_bytes.saturating_add(cost) > cap_bytes)
                        {
                            full = true;
                            break;
                        }
                        state.recent_bytes += cost;
                        state.recent.push(event);
                    }
                    if full || state.recent.len() >= left {
                        state.recent_truncated |= events.has_more || full || slice.stopped;
                        state.recent_done = true;
                    } else if slice.stopped {
                        state.recent_cursor = slice.last.or(state.recent_cursor.take());
                        return Ok(ReadProgress::Pending(park(state, this)));
                    } else {
                        state.recent_truncated |= events.has_more;
                        state.recent_done = true;
                    }
                }
            }
            let mut page = SessionDetailPage {
                requests: std::mem::take(&mut state.out),
                truncated: state.recent_truncated,
                ..SessionDetailPage::default()
            };
            page.recent = std::mem::take(&mut state.recent);
            if state.more
                && let Some(last) = state.last
            {
                page.has_more = true;
                page.next = Some(PageAnchor {
                    timestamp: Some(last.0),
                    source_order: 0,
                    stable_id: format!("request:{}", last.1),
                });
            }
            Ok(ReadProgress::Ready(page))
        });
        // An interrupted slice is progress, not a failure: the parked state resumes.
        let result = match result {
            Err(ProviderError::Unavailable(m)) if m.contains(INTERRUPTED) => {
                Ok(ReadProgress::Pending(WorkProgress::default()))
            }
            other => other,
        };
        match &result {
            Ok(ReadProgress::Pending(_)) => self.ledger.join(key, job),
            _ => {
                self.details.borrow_mut().remove(&slot);
                self.ledger.release(key, job);
            }
        }
        result
    }

    fn transcript_page(
        &mut self,
        _job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
        self.guarded(|this, conn| {
            this.page(conn, key, anchor.as_ref(), direction, request, None, None)
        })
        .map(ReadProgress::Ready)
    }

    fn count_events_after(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: &PageAnchor,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError> {
        let result = self.guarded(|this, conn| this.count_slice(conn, job, key, anchor, budget));
        match &result {
            Ok(ReadProgress::Pending(_)) => self.ledger.join(key, job),
            _ => {
                self.counts.borrow_mut().remove(&(key.clone(), job));
                self.ledger.release(key, job);
            }
        }
        result
    }

    fn source_record(
        &mut self,
        _job: ReadJobId,
        locator: &SourceLocator,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        self.guarded(|this, conn| this.record(conn, locator))
            .map(ReadProgress::Ready)
    }
}

/// OpenCode reader that stays registered when the database cannot be opened yet
/// (locked, still being created, sidecar unavailable). Each discovery and request
/// retries opening, so a transient failure at startup is not permanent.
pub struct RetryingOpenCode {
    config: Config,
    scanner: Option<OpenCodeScanner>,
}

impl RetryingOpenCode {
    /// Tries to open once; the error, if any, is returned so the caller can note it.
    pub fn open(config: &Config) -> (Self, Option<ProviderError>) {
        let (scanner, error) = match OpenCodeScanner::new(config) {
            Ok(scanner) => (Some(scanner), None),
            Err(error) => (None, Some(error)),
        };
        (
            Self {
                config: config.clone(),
                scanner,
            },
            error,
        )
    }

    fn scanner(&mut self) -> Result<&mut OpenCodeScanner, ProviderError> {
        if self.scanner.is_none() {
            self.scanner = Some(OpenCodeScanner::new(&self.config)?);
        }
        Ok(self.scanner.as_mut().expect("opened"))
    }
}

impl ProviderReader for RetryingOpenCode {
    fn harness(&self) -> Harness {
        Harness::OpenCode
    }

    fn discover(
        &mut self,
        now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        self.scanner()?.discover(now, budget)
    }

    fn hydrate(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        self.scanner()?.hydrate(job, key, now, budget)
    }

    fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId) {
        if let Some(scanner) = self.scanner.as_mut() {
            scanner.cancel_read(key, job);
        }
    }

    fn details_page(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, ProviderError> {
        self.scanner()?
            .details_page(job, key, anchor, request, budget)
    }

    fn transcript_page(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
        self.scanner()?
            .transcript_page(job, key, anchor, direction, request, budget)
    }

    fn count_events_after(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: &PageAnchor,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError> {
        self.scanner()?.count_events_after(job, key, anchor, budget)
    }

    fn source_record(
        &mut self,
        job: ReadJobId,
        locator: &SourceLocator,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        self.scanner()?.source_record(job, locator, budget)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deletion consumed by an earlier failed slice is still delivered, exactly once,
    /// when the database is then found to have an unsupported schema.
    #[test]
    fn carried_removals_survive_an_unsupported_schema_delta() {
        let dir = std::env::temp_dir().join(format!("agentscope-carry-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("opencode.db");
        let _ = std::fs::remove_file(&db);
        let writer = Connection::open(&db).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE session_v2(id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT,
                    fork_session_id TEXT, title TEXT, directory TEXT, model TEXT, version TEXT,
                    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                    time_archived INTEGER);
                 CREATE TABLE session_message(id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
                    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
                 INSERT INTO session_v2(id, time_created, time_updated) VALUES ('keep', 1, 2);",
            )
            .unwrap();
        let cli = crate::config::CliOverrides {
            opencode_db: Some(db.clone()),
            ..Default::default()
        };
        let config =
            Config::load_with_env(&cli, &crate::config::Env::new()).expect("valid test config");
        let mut scanner = OpenCodeScanner::new(&config).unwrap();
        let budget = RefreshBudget {
            max_records: 1000,
            max_bytes: 1 << 20,
            max_millis: 60_000,
        };
        let gone = SessionKey {
            harness: Harness::OpenCode,
            native_id: "gone".into(),
            source: SourceKey::DbRow {
                database: db.clone(),
                table: "session_v2".into(),
                id: "gone".into(),
            },
        };
        // The findings of a slice that consumed a deletion and then failed.
        scanner.carry_removed.push(gone.clone());
        // The database is replaced by an unrecognised schema before the next slice.
        writer
            .execute_batch(
                "ALTER TABLE session_v2 RENAME TO session_old;
                 ALTER TABLE session_message RENAME TO message_old;",
            )
            .unwrap();
        scanner.connection = None;
        let first = scanner.discover(0, budget).unwrap();
        assert_eq!(first.removed, vec![gone], "the carried deletion arrives");
        assert!(first.scan.done);
        let second = scanner.discover(0, budget).unwrap();
        assert!(second.removed.is_empty(), "and only once");
        drop(writer);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
