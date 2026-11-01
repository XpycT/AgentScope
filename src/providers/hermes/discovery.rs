//! Native Hermes home/profile resolution and bounded, cheap session metadata.
//!
//! A home is a directory containing `state.db`; an unscoped home also enumerates
//! `profiles/<name>/state.db` (sorted, with no fixed truncation). A home that is
//! itself a profile is read alone and never widened to its siblings. Aliases of
//! one physical database collapse to the first path. Discovery opens each
//! database read-only, probes the shape once, and resolves session metadata in
//! resumable slices; a locked database yields to the others and is retried.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::ffi::OsStr;
use std::fs;
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::super::sqlite_guard::{interrupted, map_error};
use super::super::{DiscoveryDelta, FORGOTTEN_CAP, Forgotten, ProviderError};
use super::lineage::{HermesMarkers, HermesRowMeta};
use super::schema::{self, EFFORT_MAX, HermesShape, MAX_CONFIG_BYTES, epoch_ms, short};
use crate::config::{Config, HarnessConfig};
use crate::domain::{
    ActivityStamp, AggregationClass, Harness, Lineage, ModelDetail, RefreshBudget, ReleaseState,
    ScanProgress, SessionKey, SessionLink, SessionMetadata, SourceKey, Timestamp, Warning,
    WarningKey, WarningKind,
};

/// Rows fetched per SQL batch; the work budget may be smaller.
const BATCH: usize = 64;
/// Estimated fixed overhead of one tracked session's bookkeeping.
const KNOWN_OVERHEAD: usize = 64;

/// One discovered database path. An explicit path warns when it cannot be
/// opened; a platform-default path that is absent stays silent.
struct Candidate {
    path: PathBuf,
}

/// Resumable discovery over every configured Hermes home/profile.
pub struct HermesCatalog {
    sources: Vec<Source>,
    known: BoundedKnown,
    /// Published sessions displaced from `known` by the metadata ceiling. Bounded;
    /// kept only to report a confirmed deletion instead of forgetting silently.
    forgotten: Forgotten<SessionKey, ()>,
    /// Keys tracked this pass by `known` or `forgotten` across every source.
    /// `forgotten` is shared, so displacement cleanup must target this set too.
    seen: HashSet<SessionKey>,
    /// Largest `seen` length observed in the current pass.
    peak_seen: usize,
    busy_ms: u64,
    /// Next source to offer work, for fair round-robin slicing.
    next: usize,
    scanned: usize,
}

struct Source {
    path: PathBuf,
    connection: Option<Connection>,
    shape: HermesShape,
    pass: Option<Pass>,
    /// Row count of the current pass; never reused across passes.
    total: Option<usize>,
    /// An open/read failure settled this generation; retried next generation.
    failed: bool,
    unsupported: bool,
    count_skipped: bool,
    /// The retained connection must reprobe the schema at this pass boundary.
    reprobe: bool,
    warned_unsupported: bool,
    warned_missing: bool,
    warned_cells: bool,
}

#[derive(Default)]
struct Pass {
    cursor: String,
}

struct Step {
    deadline: Instant,
    records: usize,
    bytes: usize,
    examined: usize,
    limited_warned: bool,
}

impl Source {
    fn new(candidate: Candidate) -> Self {
        Self {
            path: candidate.path,
            connection: None,
            shape: HermesShape::default(),
            pass: None,
            total: None,
            failed: false,
            unsupported: false,
            count_skipped: false,
            reprobe: false,
            warned_unsupported: false,
            warned_missing: false,
            warned_cells: false,
        }
    }

    /// Nothing more this source can contribute to the current pass.
    fn settled(&self) -> bool {
        self.unsupported || self.failed || self.pass.is_none()
    }
}

fn warning_for(path: &Path, kind: WarningKind, message: String) -> Warning {
    Warning {
        key: WarningKey {
            harness: Harness::Hermes,
            source: format!("{}#discovery", path.display()),
            kind,
        },
        message,
        count: 1,
    }
}

impl HermesCatalog {
    /// Resolves candidate databases from the shared, already-precedenced config.
    /// No database is opened here.
    pub fn new(config: &Config) -> Self {
        let busy_ms = config.refresh_busy_timeout_ms;
        let sources = resolve_sources(&config.harnesses.hermes)
            .into_iter()
            .map(Source::new)
            .collect();
        Self {
            sources,
            known: BoundedKnown::new(config.max_metadata_sources, config.max_metadata_bytes),
            forgotten: Forgotten::new(FORGOTTEN_CAP),
            seen: HashSet::new(),
            peak_seen: 0,
            busy_ms,
            next: 0,
            scanned: 0,
        }
    }

    /// Databases this catalog resolved (and deduplicated), in read order.
    pub fn databases(&self) -> Vec<PathBuf> {
        self.sources.iter().map(|s| s.path.clone()).collect()
    }

    /// Sessions whose metadata digest is still tracked (bounded by the ceiling).
    pub fn tracked_sources(&self) -> usize {
        self.known.len()
    }

    /// Published sessions displaced from the ceiling and still watched for deletion.
    pub fn forgotten_sources(&self) -> usize {
        self.forgotten.len()
    }

    /// Largest pass seen set observed; bounded by tracked + forgotten entries.
    pub fn peak_seen(&self) -> usize {
        self.peak_seen
    }

    /// The metadata last reported for `key`, while its row is still tracked.
    pub fn reported(&self, key: &SessionKey) -> Option<&SessionMetadata> {
        self.known.entries.get(key).map(|entry| &entry.meta)
    }

    /// The relation inputs of every tracked row (bounded by the metadata ceiling).
    pub fn rows(&self) -> Vec<&HermesRowMeta> {
        self.known
            .entries
            .values()
            .map(|entry| &entry.row)
            .collect()
    }

    /// Databases that failed (or had an unsupported shape) in the latest pass.
    /// Their last-good rows stay tracked and are retried on the next pass.
    pub fn failed_databases(&self) -> Vec<PathBuf> {
        self.sources
            .iter()
            .filter(|source| source.failed || source.unsupported)
            .map(|source| source.path.clone())
            .collect()
    }

    /// One bounded slice of discovery. Finishing a pass (`scan.done`) reports
    /// removals; the next call starts a new pass. A database that is locked,
    /// corrupt, or unsupported settles with a content-free warning so healthy
    /// databases keep flowing and the failed one is retried next pass.
    pub fn step(
        &mut self,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        let mut delta = DiscoveryDelta {
            sessions: Vec::new(),
            removed: Vec::new(),
            invalidated: Vec::new(),
            warnings: Vec::new(),
            scan: ScanProgress {
                harness: Harness::Hermes,
                scanned: 0,
                total: None,
                done: false,
            },
        };
        if self.sources.iter().all(Source::settled) {
            self.begin_pass();
        }
        let mut step = Step {
            deadline: Instant::now() + Duration::from_millis(budget.max_millis.max(1)),
            records: budget.max_records.max(1),
            bytes: budget.max_bytes.max(1),
            examined: 0,
            limited_warned: false,
        };

        while Instant::now() < step.deadline && step.records > 0 && step.bytes > 0 {
            let Some(index) = self.next_unsettled() else {
                break;
            };
            let mut books = Bookkeeping {
                known: &mut self.known,
                forgotten: &mut self.forgotten,
                seen: &mut self.seen,
                peak_seen: &mut self.peak_seen,
            };
            let result = work(
                &mut self.sources[index],
                &mut books,
                self.busy_ms,
                &mut step,
                &mut delta,
            );
            match result {
                Ok(true) => finish(
                    &mut self.sources[index],
                    &mut self.known,
                    &mut self.forgotten,
                    &self.seen,
                    &mut delta,
                ),
                Ok(false) => {}
                Err(error) if interrupted(&error) => {
                    // The slice ended mid-statement: keep the pass and connection.
                }
                Err(error) => {
                    let source = &mut self.sources[index];
                    if !source.unsupported {
                        delta.warnings.push(warning_for(
                            &source.path,
                            error_kind(&error),
                            format!("database unavailable: {error}"),
                        ));
                    }
                    source.connection = None;
                    source.pass = None;
                    source.failed = true;
                    // A stale count from a previous pass is not evidence about an
                    // unavailable database.
                    source.total = None;
                }
            }
        }

        self.scanned += step.examined;
        delta.scan.scanned = self.scanned;
        delta.scan.total = self.scan_total();
        delta.scan.done = self.sources.iter().all(Source::settled);
        Ok(delta)
    }

    fn begin_pass(&mut self) {
        self.scanned = 0;
        self.seen.clear();
        self.peak_seen = 0;
        for source in &mut self.sources {
            source.failed = false;
            source.count_skipped = false;
            // A count from the previous pass is stale: inserts and deletes change it.
            source.total = None;
            // A database the producer may have migrated since the last pass is
            // probed again; the warning itself is not repeated.
            source.unsupported = false;
            source.reprobe = source.connection.is_some();
            source.pass = Some(Pass::default());
        }
    }

    fn next_unsettled(&mut self) -> Option<usize> {
        let count = self.sources.len();
        for offset in 0..count {
            let index = (self.next + offset) % count;
            if !self.sources[index].settled() {
                self.next = (index + 1) % count;
                return Some(index);
            }
        }
        None
    }

    /// Sum of every known row count, or `None` while one is still unknown.
    fn scan_total(&self) -> Option<usize> {
        let mut sum = 0;
        for source in &self.sources {
            if source.unsupported {
                continue;
            }
            sum += source.total?;
        }
        Some(sum)
    }
}

fn error_kind(error: &ProviderError) -> WarningKind {
    match error {
        ProviderError::Io(_) => WarningKind::Io,
        ProviderError::Parse(_) => WarningKind::Parse,
        ProviderError::Unsupported(_) => WarningKind::Unsupported,
        ProviderError::Unavailable(_) => WarningKind::Unavailable,
        ProviderError::Internal(_) => WarningKind::Io,
    }
}

// ---- native home and profile resolution -------------------------------------

fn resolve_sources(config: &HarnessConfig) -> Vec<Candidate> {
    let mut candidates = Vec::new();
    // An explicit database names exactly one file; a home names its own
    // `state.db` and, when unscoped, its profile databases.
    if config.root.is_none()
        && let Some(db) = &config.db
    {
        candidates.push(Candidate { path: db.clone() });
    }
    if let Some(home) = &config.root {
        home_candidates(home, false, &mut candidates);
    }
    for home in &config.extra_roots {
        home_candidates(home, true, &mut candidates);
    }
    dedupe(candidates)
}

fn home_candidates(home: &Path, explicit: bool, out: &mut Vec<Candidate>) {
    // A database file is never a home, even when named as an extra root.
    if home.is_file() {
        return;
    }
    let db = home.join("state.db");
    if db.is_file() || (explicit && !home.exists()) {
        out.push(Candidate { path: db });
    }
    // A configured profile home is read alone; only an unscoped home widens.
    if is_profile_home(home) {
        return;
    }
    let profiles = home.join("profiles");
    let Ok(entries) = fs::read_dir(&profiles) else {
        return;
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_dir()))
        .map(|entry| entry.path())
        .collect();
    dirs.sort();
    for profile in dirs {
        let db = profile.join("state.db");
        if db.is_file() {
            out.push(Candidate { path: db });
        }
    }
}

fn is_profile_home(home: &Path) -> bool {
    home.parent()
        .and_then(Path::file_name)
        .is_some_and(|name| name == OsStr::new("profiles"))
}

/// Keeps the first path of each physical database. Canonical paths catch `.`,
/// case and separator aliases; `same-file` catches links canonicalization misses.
fn dedupe(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut kept: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        let resolved = fs::canonicalize(&candidate.path).ok();
        let duplicate = kept.iter().any(|other| {
            other.path == candidate.path
                || resolved
                    .as_ref()
                    .zip(fs::canonicalize(&other.path).ok())
                    .is_some_and(|(a, b)| a.as_path() == b.as_path())
                || same_file::is_same_file(&other.path, &candidate.path).unwrap_or(false)
        });
        if !duplicate {
            kept.push(candidate);
        }
    }
    kept
}

// ---- one bounded batch ------------------------------------------------------

/// One session row projected by the fixed discovery query. Only allowlisted
/// columns and fixed JSON keys are read; `model_config` itself never leaves SQL.
struct MetaRow {
    id: String,
    parent: Option<String>,
    parent_exists: bool,
    title: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    config_type: Option<String>,
    config_bytes: Option<i64>,
    config_valid: Option<i64>,
    enabled: Option<String>,
    effort: Option<String>,
    started: Option<f64>,
    started_type: Option<String>,
    last_activity: Option<f64>,
    last_activity_type: Option<String>,
    archived: Option<i64>,
    latest_message: Option<f64>,
    source: Option<String>,
    end_reason: Option<String>,
    ended: Option<f64>,
    delegate_from: Option<String>,
    branched_from: Option<String>,
    reset_from: Option<String>,
    bytes: usize,
}

impl MetaRow {
    fn read(row: &rusqlite::Row<'_>) -> Result<Self, rusqlite::Error> {
        let id: Option<String> = row.get(0)?;
        let parent: Option<String> = row.get(1)?;
        let parent_exists: i64 = row.get(2)?;
        let title: Option<String> = row.get(3)?;
        let model: Option<String> = row.get(4)?;
        let provider: Option<String> = row.get(5)?;
        let config_type: Option<String> = row.get(6)?;
        let config_bytes: Option<i64> = row.get(7)?;
        let config_valid: Option<i64> = row.get(8)?;
        let enabled: Option<String> = row.get(9)?;
        let effort: Option<String> = row.get(10)?;
        let started: Option<f64> = row.get(11)?;
        let started_type: Option<String> = row.get(12)?;
        let last_activity: Option<f64> = row.get(13)?;
        let last_activity_type: Option<String> = row.get(14)?;
        let archived: Option<i64> = row.get(15)?;
        let latest_message: Option<f64> = row.get(16)?;
        let source: Option<String> = row.get(17)?;
        let end_reason: Option<String> = row.get(18)?;
        let ended: Option<f64> = row.get(19)?;
        let delegate_from: Option<String> = row.get(20)?;
        let branched_from: Option<String> = row.get(21)?;
        let reset_from: Option<String> = row.get(22)?;
        let config_bytes = config_bytes.unwrap_or(0).max(0) as usize;
        let bytes = KNOWN_OVERHEAD
            + id.as_ref().map_or(0, String::len)
            + parent.as_ref().map_or(0, String::len)
            + title.as_ref().map_or(0, String::len)
            + model.as_ref().map_or(0, String::len)
            + provider.as_ref().map_or(0, String::len)
            + config_bytes;
        Ok(Self {
            id: id.unwrap_or_default(),
            parent,
            parent_exists: parent_exists != 0,
            title,
            model,
            provider,
            config_type,
            config_bytes: Some(config_bytes as i64),
            config_valid,
            enabled,
            effort,
            started,
            started_type,
            last_activity,
            last_activity_type,
            archived,
            latest_message,
            source,
            end_reason,
            ended,
            delegate_from,
            branched_from,
            reset_from,
            bytes,
        })
    }

    /// True when a cell used for timestamp/config evidence has a storage type
    /// the producer never writes, JSON that does not parse, or a config larger
    /// than the fixed parse bound.
    fn nonsense(&self) -> bool {
        let numeric = |kind: Option<&str>| matches!(kind, None | Some("null" | "integer" | "real"));
        !numeric(self.started_type.as_deref())
            || !numeric(self.last_activity_type.as_deref())
            || matches!(
                self.config_type.as_deref(),
                Some("blob" | "integer" | "real")
            )
            || self.config_valid == Some(0)
            || self
                .config_bytes
                .is_some_and(|bytes| bytes > MAX_CONFIG_BYTES as i64)
    }
}

/// Fixed bounded projection. Optional columns are substituted with `NULL` and
/// every used cell is behind a `typeof`/`json_valid` guard, so a wrong type or a
/// non-JSON config cell stays `Unknown` instead of failing the row.
fn row_sql(shape: &HermesShape) -> String {
    let text = |present: bool, expr: &str| {
        if present {
            format!("CASE WHEN typeof({expr}) = 'text' THEN {expr} END")
        } else {
            "NULL".to_string()
        }
    };
    let number = |present: bool, expr: &str| {
        if present {
            format!("CASE WHEN typeof({expr}) IN ('integer','real') THEN {expr} END")
        } else {
            "NULL".to_string()
        }
    };
    let kind = |present: bool, expr: &str| {
        if present {
            format!("typeof({expr})")
        } else {
            "NULL".to_string()
        }
    };
    let title = text(shape.title, "SUBSTR(s.title, 1, 1024)");
    let model = text(shape.model, "SUBSTR(s.model, 1, 256)");
    let provider = text(shape.billing_provider, "SUBSTR(s.billing_provider, 1, 256)");
    let config_type = kind(shape.model_config, "s.model_config");
    // Size is checked before any JSON parsing: an oversized (or non-text) cell
    // never reaches json_valid/json_type/json_extract.
    let sized = format!(
        "typeof(s.model_config) = 'text' AND length(CAST(s.model_config AS BLOB)) <= {MAX_CONFIG_BYTES}"
    );
    let config_bytes = if shape.model_config {
        "CASE WHEN typeof(s.model_config) IN ('text','blob') \
         THEN length(CAST(s.model_config AS BLOB)) END"
            .to_string()
    } else {
        "NULL".to_string()
    };
    let config_valid = if shape.model_config {
        format!("CASE WHEN {sized} THEN json_valid(s.model_config) END")
    } else {
        "NULL".to_string()
    };
    let enabled = if shape.model_config {
        format!(
            "CASE WHEN {sized} AND json_valid(s.model_config) \
             THEN json_type(s.model_config, '$.reasoning_config.enabled') END"
        )
    } else {
        "NULL".to_string()
    };
    let effort = if shape.model_config {
        format!(
            "CASE WHEN {sized} AND json_valid(s.model_config) \
               AND json_type(s.model_config, '$.reasoning_config') = 'object' \
               AND json_type(s.model_config, '$.reasoning_config.enabled') = 'true' \
               AND json_type(s.model_config, '$.reasoning_config.effort') = 'text' \
             THEN SUBSTR(json_extract(s.model_config, '$.reasoning_config.effort'), 1, 64) END"
        )
    } else {
        "NULL".to_string()
    };
    let started = number(true, "s.started_at");
    let started_type = kind(true, "s.started_at");
    let ended = number(shape.ended, "s.ended_at");
    let source = text(shape.source, "SUBSTR(s.source, 1, 64)");
    let end_reason = text(shape.end_reason, "SUBSTR(s.end_reason, 1, 64)");
    let marker = |key: &str| {
        if shape.model_config {
            format!(
                "CASE WHEN {sized} AND json_valid(s.model_config) \
                 AND json_type(s.model_config, '$.{key}') = 'text' \
                 THEN SUBSTR(json_extract(s.model_config, '$.{key}'), 1, 256) END"
            )
        } else {
            "NULL".to_string()
        }
    };
    let delegate = marker("_delegate_from");
    let branched = marker("_branched_from");
    let reset = marker("_reset_from");
    let last_activity = number(shape.last_activity, "s.last_activity_at");
    let last_activity_type = kind(shape.last_activity, "s.last_activity_at");
    let archived = if shape.archived {
        "CASE WHEN typeof(s.archived) = 'integer' THEN s.archived \
           WHEN typeof(s.archived) = 'real' THEN CAST(s.archived AS INTEGER) END"
            .to_string()
    } else {
        "NULL".to_string()
    };
    let id = "CASE WHEN typeof(s.id) = 'text' THEN s.id END";
    let parent = text(shape.parent, "s.parent_session_id");
    let parent_exists = if shape.parent {
        "CASE WHEN typeof(s.parent_session_id) = 'text' AND s.parent_session_id <> '' \
           THEN EXISTS(SELECT 1 FROM sessions p WHERE p.id = s.parent_session_id) ELSE 0 END"
            .to_string()
    } else {
        "0".to_string()
    };
    // Bounded indexed latest-message evidence; `MAX` over a session uses the
    // `(session_id, timestamp)` index in the producer schema.
    let latest = "(SELECT MAX(m.timestamp) FROM messages m \
         WHERE m.session_id = s.id AND typeof(m.timestamp) IN ('integer','real'))";
    format!(
        "SELECT {id}, {parent}, {parent_exists}, {title}, {model}, {provider}, \
         {config_type}, {config_bytes}, {config_valid}, {enabled}, {effort}, {started}, \
         {started_type}, {last_activity}, {last_activity_type}, {archived}, {latest}, \
         {source}, {end_reason}, {ended}, {delegate}, {branched}, {reset} \
         FROM sessions s \
         WHERE typeof(s.id) = 'text' AND s.id <> '' AND s.id > ?1 \
         ORDER BY s.id LIMIT ?2",
        source = source,
        end_reason = end_reason,
        ended = ended,
        delegate = delegate,
        branched = branched,
        reset = reset,
    )
}

fn configured_effort(enabled: Option<&str>, effort: Option<&str>) -> Option<String> {
    match enabled? {
        // Verified producer shape: `{"enabled": false}` is an explicit off.
        "false" => Some("off".to_string()),
        "true" => effort
            .map(str::trim)
            .filter(|level| !level.is_empty() && level.len() <= EFFORT_MAX)
            .map(str::to_owned),
        // `null`, a non-boolean, or an absent key is not an explicit setting.
        _ => None,
    }
}

fn metadata(database: &Path, row: &MetaRow) -> Option<SessionMetadata> {
    if row.id.is_empty() {
        return None;
    }
    let key = SessionKey {
        harness: Harness::Hermes,
        native_id: row.id.clone(),
        source: SourceKey::DbRow {
            database: database.to_path_buf(),
            table: "sessions".to_string(),
            id: row.id.clone(),
        },
    };
    let recorded_title = row.title.as_deref().and_then(short);
    let title_derived = recorded_title.is_none();
    let title = recorded_title.unwrap_or_else(|| row.id.clone());
    let configured = configured_effort(row.enabled.as_deref(), row.effort.as_deref());
    let model = row.model.as_deref().and_then(short).map(|id| ModelDetail {
        id,
        provider: row.provider.as_deref().and_then(short),
        variant: None,
        effort: None,
        configured_effort: configured,
    });
    let started = epoch_ms(row.started);
    let activity = [epoch_ms(row.last_activity), epoch_ms(row.latest_message)]
        .into_iter()
        .flatten()
        .max()
        .or(started);
    let parent = row.parent.as_deref().filter(|parent| !parent.is_empty());
    Some(SessionMetadata {
        key,
        title,
        title_derived,
        project: None,
        cwd: None,
        model,
        origins: Vec::new(),
        created_at: started,
        activity: ActivityStamp {
            at: activity,
            class: if activity.is_some() {
                AggregationClass::Reported
            } else {
                AggregationClass::Unknown
            },
        },
        lineage: Lineage {
            parent: parent.map(|parent| {
                if row.parent_exists {
                    SessionLink::Resolved(SessionKey {
                        harness: Harness::Hermes,
                        native_id: parent.to_string(),
                        source: SourceKey::DbRow {
                            database: database.to_path_buf(),
                            table: "sessions".to_string(),
                            id: parent.to_string(),
                        },
                    })
                } else {
                    SessionLink::Unresolved(parent.to_string())
                }
            }),
            ..Lineage::default()
        },
        members: Vec::new(),
        archived: row.archived.is_some_and(|archived| archived != 0),
        released: ReleaseState::Unknown,
        source_available: true,
        harness_version: None,
    })
}

/// The exact relation inputs of one discovered row, kept beside its metadata so
/// the scanner can resolve compression chains without a second database scan.
fn row_meta(database: &Path, row: &MetaRow) -> HermesRowMeta {
    let mut meta = HermesRowMeta::new(database, &row.id);
    meta.parent = row.parent.clone().filter(|parent| !parent.is_empty());
    meta.source = row.source.clone();
    meta.started = epoch_ms(row.started);
    meta.ended = epoch_ms(row.ended);
    meta.end_reason = row.end_reason.clone();
    meta.markers = HermesMarkers {
        delegate_from: row.delegate_from.clone().filter(|id| !id.is_empty()),
        branched_from: row.branched_from.clone().filter(|id| !id.is_empty()),
        reset_from: row.reset_from.clone().filter(|id| !id.is_empty()),
    };
    meta
}

fn digest(meta: &SessionMetadata) -> u64 {
    let mut hasher = DefaultHasher::new();
    meta.title.hash(&mut hasher);
    meta.title_derived.hash(&mut hasher);
    meta.model
        .as_ref()
        .map(|m| (&m.id, &m.provider, &m.configured_effort))
        .hash(&mut hasher);
    meta.created_at.hash(&mut hasher);
    meta.activity.at.hash(&mut hasher);
    meta.archived.hash(&mut hasher);
    meta.lineage
        .parent
        .as_ref()
        .map(|link| match link {
            SessionLink::Resolved(key) => (true, key.native_id.clone()),
            SessionLink::Unresolved(id) => (false, id.clone()),
            SessionLink::Ambiguous => (false, String::new()),
        })
        .hash(&mut hasher);
    hasher.finish()
}

/// Busy timeout that fits the remaining slice, optionally bounded by config.
fn busy_for(step: &Step, busy_ms: u64) -> u64 {
    busy_for_deadline(step.deadline, busy_ms)
}

/// Busy timeout shared with hydration: never wait past the slice deadline.
pub(crate) fn busy_for_deadline(deadline: Instant, busy_ms: u64) -> u64 {
    deadline
        .saturating_duration_since(Instant::now())
        .as_millis()
        .min(u128::from(busy_ms.max(1)))
        .max(1) as u64
}

pub(crate) fn set_busy(conn: &Connection, busy_ms: u64) -> Result<(), ProviderError> {
    conn.busy_timeout(Duration::from_millis(busy_ms.max(1)))
        .map_err(map_error)
}

/// Records a detected shape. `false` means the minimum supported shape is absent.
fn accept_shape(source: &mut Source, shape: HermesShape, delta: &mut DiscoveryDelta) -> bool {
    source.shape = shape;
    if !shape.supported {
        source.unsupported = true;
        source.pass = None;
        if !source.warned_unsupported {
            source.warned_unsupported = true;
            delta.warnings.push(warning_for(
                &source.path,
                WarningKind::Unsupported,
                "database has no supported sessions/messages schema".to_string(),
            ));
        }
        return false;
    }
    if !source.warned_missing {
        let missing = shape.missing_optional();
        if missing > 0 {
            source.warned_missing = true;
            delta.warnings.push(warning_for(
                &source.path,
                WarningKind::Unsupported,
                format!("{missing} optional column(s) are absent; those fields stay Unknown"),
            ));
        }
    }
    true
}

/// Bounded bookkeeping handed to one batch read. `seen` is catalog-wide because
/// the forgotten watch is shared across databases.
struct Bookkeeping<'a> {
    known: &'a mut BoundedKnown,
    forgotten: &'a mut Forgotten<SessionKey, ()>,
    seen: &'a mut HashSet<SessionKey>,
    peak_seen: &'a mut usize,
}

/// Reads at most one bounded batch from `source`, appending changed metadata to
/// `delta`. Returns `true` when the source's pass is complete.
///
/// `books.seen` records only keys still tracked by `known` or `forgotten`, so its
/// size is bounded by the metadata ceiling plus the forgotten watch, never by the
/// number of rows in a database.
fn work(
    source: &mut Source,
    books: &mut Bookkeeping<'_>,
    busy_ms: u64,
    step: &mut Step,
    delta: &mut DiscoveryDelta,
) -> Result<bool, ProviderError> {
    if source.connection.is_none() {
        let conn = schema::open_with(&source.path, busy_for(step, busy_ms))?;
        let shape = schema::detect_shape(&conn)?;
        let supported = accept_shape(source, shape, delta);
        source.reprobe = false;
        if !supported {
            return Ok(true);
        }
        source.connection = Some(conn);
    } else if source.reprobe {
        // A producer migration in place is picked up without reopening, and the
        // retained connection never keeps a stale reduced-shape flag.
        let conn = source
            .connection
            .as_mut()
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        set_busy(conn, busy_for(step, busy_ms))?;
        let shape = schema::detect_shape(conn)?;
        let supported = accept_shape(source, shape, delta);
        source.reprobe = false;
        if !supported {
            source.connection = None;
            return Ok(true);
        }
    }
    if source.unsupported || source.pass.is_none() {
        return Ok(true);
    }

    if source.total.is_none() && !source.count_skipped {
        let conn = source
            .connection
            .as_mut()
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        set_busy(conn, busy_for(step, busy_ms))?;
        let counted = with_deadline(conn, step.deadline, |conn| {
            conn.query_row("SELECT count(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
        });
        match counted {
            Ok(total) => source.total = Some(total.max(0) as usize),
            // A count that does not fit this slice must not starve the rows; the
            // pass proceeds with an honest unknown total and retries next pass.
            Err(error) if interrupted(&error) => source.count_skipped = true,
            Err(error) => return Err(error),
        }
    }

    let database = source.path.clone();
    let sql = row_sql(&source.shape);
    let limit = step.records.clamp(1, BATCH) as i64;
    let connection = source
        .connection
        .as_mut()
        .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
    // The connection may have been opened for a much longer slice; the busy wait
    // must fit *this* slice or it cannot be interrupted by the progress handler.
    set_busy(connection, busy_for(step, busy_ms))?;
    let pass = source
        .pass
        .as_mut()
        .ok_or_else(|| ProviderError::Internal("hermes pass missing".into()))?;
    let cursor = pass.cursor.clone();
    let mut fetched = 0usize;
    let mut stopped = false;
    let mut nonsense = false;
    let warned = source.warned_cells;

    let outcome = with_deadline(connection, step.deadline, |conn| {
        let mut statement = conn.prepare_cached(&sql)?;
        let mut rows = statement.query(rusqlite::params![cursor, limit])?;
        while let Some(row) = rows.next()? {
            let row = MetaRow::read(row)?;
            fetched += 1;
            step.examined += 1;
            step.records = step.records.saturating_sub(1);
            step.bytes = step.bytes.saturating_sub(row.bytes);
            nonsense |= row.nonsense();
            let Some(meta) = metadata(&database, &row) else {
                continue;
            };
            let facts = row_meta(&database, &row);
            pass.cursor = meta.key.native_id.clone();
            let change = books.known.insert(
                &meta.key,
                digest(&meta),
                meta.activity.at,
                meta.clone(),
                facts,
            );
            // Displaced entries stay watched (bounded) so a later confirmed
            // deletion is still reported. The watch is shared across databases,
            // so cleanup targets the shared seen set, not the current source.
            for evicted in &change.evicted {
                if let Some(dropped) = books.forgotten.insert_displacing(evicted.clone(), ()) {
                    books.seen.remove(&dropped);
                }
            }
            if books.known.contains(&meta.key) {
                books.forgotten.remove(&meta.key);
            }
            if books.known.contains(&meta.key) || books.forgotten.get(&meta.key).is_some() {
                books.seen.insert(meta.key.clone());
            }
            *books.peak_seen = (*books.peak_seen).max(books.seen.len());
            if change.limited && !step.limited_warned {
                step.limited_warned = true;
                delta.warnings.push(warning_for(
                    &database,
                    WarningKind::Unsupported,
                    "metadata limit reached; some bookkeeping was evicted".to_string(),
                ));
            }
            if change.changed {
                delta.sessions.push(meta);
            }
            if step.records == 0 || step.bytes == 0 {
                stopped = true;
                break;
            }
        }
        Ok(())
    });
    if nonsense && !warned {
        source.warned_cells = true;
        delta.warnings.push(warning_for(
            &source.path,
            WarningKind::Unsupported,
            "some stored cells have an unexpected type, invalid JSON, or an oversized \
             model_config; affected fields stay Unknown"
                .to_string(),
        ));
    }
    outcome?;
    Ok(fetched < limit as usize && !stopped)
}

/// Runs one SQLite read with a progress handler pinned to the slice deadline so
/// a large count or batch cannot hold the worker past it.
fn with_deadline<T>(
    conn: &mut Connection,
    deadline: Instant,
    run: impl FnOnce(&Connection) -> Result<T, rusqlite::Error>,
) -> Result<T, ProviderError> {
    with_deadline_bound(conn, deadline, |conn| run(conn).map_err(map_error))
}

/// Generic deadline-bound SQLite work for tests/hydration whose row parsing
/// reports [`ProviderError`] rather than a raw SQLite error.
pub(crate) fn with_deadline_bound<T>(
    conn: &mut Connection,
    deadline: Instant,
    run: impl FnOnce(&Connection) -> Result<T, ProviderError>,
) -> Result<T, ProviderError> {
    conn.progress_handler(1000, Some(move || Instant::now() >= deadline))
        .map_err(map_error)?;
    let result = run(conn);
    let _ = conn.progress_handler(0, None::<fn() -> bool>);
    result
}

fn finish(
    source: &mut Source,
    known: &mut BoundedKnown,
    forgotten: &mut Forgotten<SessionKey, ()>,
    seen: &HashSet<SessionKey>,
    delta: &mut DiscoveryDelta,
) {
    if source.pass.take().is_none() {
        return;
    }
    // Only a completed pass can prove absence. Tracked-but-unseen keys are
    // confirmed deletions; the same bounded filter covers displaced sessions.
    let mut tracked: Vec<SessionKey> = known
        .keys()
        .filter(|key| database(key) == source.path.as_path())
        .cloned()
        .collect();
    tracked.extend(
        forgotten
            .keys()
            .into_iter()
            .filter(|key| database(key) == source.path.as_path()),
    );
    for key in tracked {
        if !seen.contains(&key) {
            known.remove(&key);
            forgotten.remove(&key);
            delta.removed.push(key);
        }
    }
}

fn database(key: &SessionKey) -> &Path {
    match &key.source {
        SourceKey::DbRow { database, .. } => database.as_path(),
        SourceKey::File(path) => path.as_path(),
    }
}

// ---- bounded discovery bookkeeping ------------------------------------------

struct KnownEntry {
    digest: u64,
    at: Option<Timestamp>,
    seq: u64,
    bytes: usize,
    meta: SessionMetadata,
    row: HermesRowMeta,
}

struct Change {
    changed: bool,
    limited: bool,
    /// Entries displaced (or immediately displaced) by this insertion, including
    /// the inserted key itself when its weight cannot fit the byte cap.
    evicted: Vec<SessionKey>,
}

/// Newest-first bounded record of reported sessions. Eviction only forgets
/// bookkeeping; the session remains in its database and readable.
struct BoundedKnown {
    cap: usize,
    cap_bytes: usize,
    bytes: usize,
    next_seq: u64,
    entries: HashMap<SessionKey, KnownEntry>,
    order: BTreeSet<(Option<Timestamp>, u64)>,
    by_seq: HashMap<u64, SessionKey>,
}

impl BoundedKnown {
    fn new(cap: usize, cap_bytes: usize) -> Self {
        Self {
            cap: cap.max(1),
            cap_bytes: cap_bytes.max(1),
            bytes: 0,
            next_seq: 0,
            entries: HashMap::new(),
            order: BTreeSet::new(),
            by_seq: HashMap::new(),
        }
    }

    fn weight(key: &SessionKey) -> usize {
        KNOWN_OVERHEAD + key.native_id.len() + database(key).as_os_str().len()
    }

    fn insert(
        &mut self,
        key: &SessionKey,
        digest: u64,
        at: Option<Timestamp>,
        meta: SessionMetadata,
        row: HermesRowMeta,
    ) -> Change {
        let changed = self
            .entries
            .get(key)
            .is_none_or(|entry| entry.digest != digest);
        if let Some(old) = self.entries.remove(key) {
            self.order.remove(&(old.at, old.seq));
            self.by_seq.remove(&old.seq);
            self.bytes = self.bytes.saturating_sub(old.bytes);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        let bytes = Self::weight(key);
        self.entries.insert(
            key.clone(),
            KnownEntry {
                digest,
                at,
                seq,
                bytes,
                meta,
                row,
            },
        );
        self.order.insert((at, seq));
        self.by_seq.insert(seq, key.clone());
        self.bytes += bytes;
        let mut limited = false;
        let mut evicted = Vec::new();
        while self.entries.len() > self.cap
            || (self.bytes > self.cap_bytes && self.entries.len() > 1)
        {
            let Some((_, seq)) = self.order.pop_first() else {
                break;
            };
            if let Some(key) = self.by_seq.remove(&seq)
                && let Some(entry) = self.entries.remove(&key)
            {
                self.bytes = self.bytes.saturating_sub(entry.bytes);
                evicted.push(key);
            }
            limited = true;
        }
        Change {
            changed,
            limited,
            evicted,
        }
    }

    fn contains(&self, key: &SessionKey) -> bool {
        self.entries.contains_key(key)
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn remove(&mut self, key: &SessionKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.order.remove(&(entry.at, entry.seq));
            self.by_seq.remove(&entry.seq);
            self.bytes = self.bytes.saturating_sub(entry.bytes);
        }
    }

    fn keys(&self) -> impl Iterator<Item = &SessionKey> {
        self.entries.keys()
    }
}
