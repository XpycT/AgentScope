//! The Hermes [`ProviderReader`]: profile discovery through [`HermesCatalog`],
//! chain-local hydration through [`HermesHydration`], and the paged transcript /
//! compaction projection in [`super::messages`].
//!
//! Discovery keeps one logical session per compression chain. A member asked for
//! by key resolves to that one root, so a continuation child never becomes a second
//! root. Database handles are bounded and reused; a replaced file or an in-place
//! schema change bumps the source generation, which makes stale page and source
//! continuations reject instead of serving bytes from another generation.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::discovery::{HermesCatalog, busy_for_deadline, set_busy, with_deadline_bound};
use super::hydration::HermesHydration;
use super::lineage::HermesLogicalSession;
use super::messages::{self, Projection};
use super::schema::{self, HermesShape};
use crate::config::Config;
use crate::domain::{
    ActivityStamp, AggregationClass, Harness, PageAnchor, PageDirection, PageRequest,
    RefreshBudget, ScanProgress, SessionDetailPage, SessionKey, SessionMetadata, SessionSummary,
    SourceKey, SourceLocator, Timestamp, TranscriptPage,
};
use crate::providers::{
    DiscoveryDelta, HydrationDelta, JobLedger, ProviderError, ProviderReader, ReadJobId,
    ReadProgress, SourceRecord, WorkProgress, share_details_budget,
};

/// Open database handles kept at once.
const MAX_DBS: usize = 8;
/// Transcript projections kept at once.
const MAX_TRANSCRIPTS: usize = 8;
/// Jobs whose generation was already checked (bounded; a reset only rechecks).
const CHECKED_CAP: usize = 4096;

struct Db {
    connection: Connection,
    shape: HermesShape,
    generation: u64,
    handle: Option<same_file::Handle>,
    /// Size and modification time of the WAL (or main file when no WAL exists)
    /// when this connection was opened. A change means another connection may
    /// have migrated the schema, which a retained SQLite handle does not always
    /// observe until it is reopened.
    wal: Option<(u64, Option<std::time::SystemTime>)>,
    used: u64,
}

struct HydrationEntry {
    hydrator: HermesHydration,
    generation: u64,
}

struct Transcript {
    projection: Projection,
    members: Vec<SessionKey>,
    generation: u64,
    /// Cheap content stamp; a changed value restarts the projection.
    stamp: u64,
    used: u64,
}

/// Hermes sessions and native compression chains as a [`ProviderReader`].
pub struct HermesScanner {
    config: Config,
    catalog: HermesCatalog,
    ledger: JobLedger,
    dbs: HashMap<PathBuf, Db>,
    hydrations: HashMap<PathBuf, HydrationEntry>,
    transcripts: HashMap<SessionKey, Transcript>,
    /// Last published logical metadata, by logical key.
    published: HashMap<SessionKey, SessionMetadata>,
    published_digests: HashMap<SessionKey, u64>,
    published_members: HashMap<SessionKey, Vec<SessionKey>>,
    /// Every physical member key of a logical session (and the logical key itself).
    member_of: HashMap<SessionKey, SessionKey>,
    pending_removed: Vec<SessionKey>,
    checked: HashSet<(SessionKey, ReadJobId)>,
    /// Source generation per database path, kept independently of the bounded
    /// connection cache so eviction and reopening never invalidates a valid page.
    generations: HashMap<PathBuf, (u64, HermesShape, Option<std::sync::Arc<same_file::Handle>>)>,
    /// Generation each continuation anchor was issued under, keyed by logical
    /// session and anchor identity so a new worker job still sees it.
    page_generation: HashMap<(SessionKey, String), u64>,
    next_generation: u64,
    tick: u64,
}

impl HermesScanner {
    /// Cheap: no database is opened until the first `discover`.
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        Ok(Self {
            config: config.clone(),
            catalog: HermesCatalog::new(config),
            ledger: JobLedger::default(),
            dbs: HashMap::new(),
            hydrations: HashMap::new(),
            transcripts: HashMap::new(),
            published: HashMap::new(),
            published_digests: HashMap::new(),
            published_members: HashMap::new(),
            member_of: HashMap::new(),
            pending_removed: Vec::new(),
            checked: HashSet::new(),
            generations: HashMap::new(),
            page_generation: HashMap::new(),
            next_generation: 1,
            tick: 0,
        })
    }

    /// Databases the catalog resolved (deduplicated), in read order.
    pub fn databases(&self) -> Vec<PathBuf> {
        self.catalog.databases()
    }

    /// Logical sessions published by the last completed pass.
    pub fn published_sessions(&self) -> usize {
        self.published.len()
    }

    /// `(session, job)` pairs still holding partial work.
    pub fn job_state(&self) -> usize {
        self.ledger.len()
    }

    fn resolve(&self, key: &SessionKey) -> Result<SessionKey, ProviderError> {
        if key.harness != Harness::Hermes {
            return Err(ProviderError::Unsupported("not a Hermes session".into()));
        }
        if self.published.contains_key(key) {
            return Ok(key.clone());
        }
        self.member_of
            .get(key)
            .cloned()
            .ok_or_else(|| ProviderError::Unavailable("unknown Hermes session".into()))
    }

    fn meta(&self, logical: &SessionKey) -> Result<SessionMetadata, ProviderError> {
        self.published
            .get(logical)
            .cloned()
            .ok_or_else(|| ProviderError::Unavailable("unknown Hermes session".into()))
    }

    fn drop_db(&mut self, path: &Path) {
        self.dbs.remove(path);
        self.hydrations.remove(path);
        self.transcripts.retain(|_, transcript| {
            transcript
                .members
                .first()
                .and_then(|key| database(key))
                .is_none_or(|db| db != path)
        });
    }

    fn wal_stamp(path: &Path) -> Option<(u64, Option<std::time::SystemTime>)> {
        let mut name = path.as_os_str().to_owned();
        name.push("-wal");
        let wal = PathBuf::from(name);
        let target = if wal.is_file() {
            wal
        } else {
            path.to_path_buf()
        };
        let metadata = std::fs::metadata(target).ok()?;
        Some((metadata.len(), metadata.modified().ok()))
    }

    /// Opens (or reuses) one database, noticing an in-place replacement first.
    fn ensure_db(
        &mut self,
        path: &Path,
        deadline: Instant,
    ) -> Result<(u64, HermesShape), ProviderError> {
        self.tick += 1;
        let identity = same_file::Handle::from_path(path).ok();
        let wal = Self::wal_stamp(path);
        if let Some(db) = self.dbs.get_mut(path) {
            if db.handle != identity && identity.is_some() {
                self.generations.remove(path);
                self.drop_db(path);
            } else if db.wal != wal {
                // Another connection wrote since this handle was opened. A
                // retained read-only handle can keep its old schema cache in
                // WAL mode, so reopen and keep the generation only when the
                // probed shape is unchanged.
                let old_generation = db.generation;
                let old_shape = db.shape;
                self.drop_db(path);
                let (generation, shape) = self.open_fresh(path, deadline, identity, wal)?;
                let kept = if shape == old_shape {
                    if let Some(db) = self.dbs.get_mut(path) {
                        db.generation = old_generation;
                    }
                    // The persistent record is the authority after eviction.
                    let handle = same_file::Handle::from_path(path)
                        .ok()
                        .map(std::sync::Arc::new);
                    self.generations
                        .insert(path.to_path_buf(), (old_generation, old_shape, handle));
                    old_generation
                } else {
                    generation
                };
                return Ok((kept, shape));
            } else {
                db.used = self.tick;
                return Ok((db.generation, db.shape));
            }
        }
        self.open_fresh(path, deadline, identity, wal)
    }

    fn open_fresh(
        &mut self,
        path: &Path,
        deadline: Instant,
        identity: Option<same_file::Handle>,
        wal: Option<(u64, Option<std::time::SystemTime>)>,
    ) -> Result<(u64, HermesShape), ProviderError> {
        let busy = busy_for_deadline(deadline, self.config.page_busy_timeout_ms);
        let mut connection = schema::open_with_deadline(path, busy, deadline)?;
        let shape = with_deadline_bound(&mut connection, deadline, schema::detect_shape)?;
        if !shape.supported {
            return Err(ProviderError::Unsupported(
                "database has no supported sessions/messages schema".into(),
            ));
        }
        if self.dbs.len() >= MAX_DBS {
            let victim = self
                .dbs
                .iter()
                .min_by_key(|(_, db)| db.used)
                .map(|(path, _)| path.clone());
            if let Some(victim) = victim {
                self.drop_db(&victim);
            }
        }
        // A DB reopened after cache eviction keeps its generation unless the
        // file identity or probed shape actually changed.
        let map_handle = same_file::Handle::from_path(path)
            .ok()
            .map(std::sync::Arc::new);
        let reused =
            self.generations
                .get(path)
                .and_then(|(generation, known_shape, known_handle)| {
                    let same_identity = match (known_handle.as_deref(), map_handle.as_deref()) {
                        (Some(a), Some(b)) => a == b,
                        (None, None) => true,
                        _ => false,
                    };
                    (same_identity && *known_shape == shape).then_some(*generation)
                });
        let generation = reused.unwrap_or_else(|| {
            let generation = self.next_generation;
            self.next_generation += 1;
            generation
        });
        if self.generations.len() >= 4096 && !self.generations.contains_key(path) {
            self.generations.clear();
        }
        self.generations
            .insert(path.to_path_buf(), (generation, shape, map_handle));
        self.dbs.insert(
            path.to_path_buf(),
            Db {
                connection,
                shape,
                generation,
                handle: identity,
                wal,
                used: self.tick,
            },
        );
        Ok((generation, shape))
    }

    /// Cheap per-chain change stamp: row count, newest id, and the display flag
    /// sums. An append, compaction or rewind changes one of them; a source with
    /// none unchanged is reused without rescanning.
    fn message_stamp(
        &mut self,
        path: &Path,
        shape: HermesShape,
        members: &[SessionKey],
        deadline: Instant,
    ) -> Result<Option<u64>, ProviderError> {
        use std::hash::{DefaultHasher, Hash, Hasher};
        if members.is_empty() {
            return Ok(Some(0));
        }
        let marks = (1..=members.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sum = |present: bool, column: &str| {
            if present {
                format!(
                    "COALESCE(SUM(CASE WHEN typeof({column}) = 'integer' THEN {column} ELSE 0 END), 0)"
                )
            } else {
                "0".to_string()
            }
        };
        let sql = format!(
            "SELECT count(*), COALESCE(MAX(id), -1), {}, {}, {} \
             FROM messages WHERE session_id IN ({marks})",
            sum(shape.message_active, "active"),
            sum(shape.message_compacted, "compacted"),
            sum(shape.message_summary, "_compressed_summary"),
        );
        let values: Vec<rusqlite::types::Value> = members
            .iter()
            .map(|member| rusqlite::types::Value::Text(member.native_id.clone()))
            .collect();
        let busy = busy_for_deadline(deadline, self.config.page_busy_timeout_ms);
        let db = self
            .dbs
            .get_mut(path)
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        set_busy(&db.connection, busy)?;
        let mut values_out = None;
        let outcome = with_deadline_bound(&mut db.connection, deadline, |conn| {
            let mut statement = conn
                .prepare_cached(&sql)
                .map_err(crate::providers::sqlite_guard::map_error)?;
            let row = statement
                .query_row(rusqlite::params_from_iter(values), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, i64>(4)?,
                    ))
                })
                .map_err(crate::providers::sqlite_guard::map_error)?;
            values_out = Some(row);
            Ok(())
        });
        match outcome {
            Ok(()) => {}
            Err(error) if crate::providers::sqlite_guard::interrupted(&error) => return Ok(None),
            Err(error) => return Err(error),
        }
        let mut hasher = DefaultHasher::new();
        values_out.hash(&mut hasher);
        Ok(Some(hasher.finish()))
    }

    /// Confirms the source shape once per `(session, job)` and bumps the
    /// generation if the producer migrated the schema in place.
    fn check_job(
        &mut self,
        job: ReadJobId,
        logical: &SessionKey,
        path: &Path,
        deadline: Instant,
    ) -> Result<(u64, HermesShape), ProviderError> {
        let (mut generation, mut shape) = self.ensure_db(path, deadline)?;
        let marker = (logical.clone(), job);
        if self.checked.insert(marker.clone()) {
            let busy = busy_for_deadline(deadline, self.config.page_busy_timeout_ms);
            let db = self
                .dbs
                .get_mut(path)
                .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
            set_busy(&db.connection, busy)?;
            let current = with_deadline_bound(&mut db.connection, deadline, schema::detect_shape)?;
            if !current.supported {
                self.drop_db(path);
                return Err(ProviderError::Unsupported(
                    "database no longer has a supported schema".into(),
                ));
            }
            if current != db.shape {
                db.shape = current;
                generation = self.next_generation;
                self.next_generation += 1;
                db.generation = generation;
                shape = current;
                self.transcripts.remove(logical);
                self.hydrations.remove(path);
            }
            if self.checked.len() > CHECKED_CAP {
                self.checked.clear();
                self.checked.insert(marker);
            }
        }
        // Whatever path changed or retained the authoritative generation, the
        // persistent record must agree so a later cache eviction reuses it.
        let handle = same_file::Handle::from_path(path)
            .ok()
            .map(std::sync::Arc::new);
        self.generations
            .insert(path.to_path_buf(), (generation, shape, handle));
        Ok((generation, shape))
    }

    fn member_keys(meta: &SessionMetadata) -> Vec<SessionKey> {
        if meta.members.is_empty() {
            vec![meta.key.clone()]
        } else {
            meta.members.clone()
        }
    }

    /// Makes the transcript projection current, or reports the work this slice did.
    fn ensure_transcript(
        &mut self,
        job: ReadJobId,
        logical: &SessionKey,
        budget: RefreshBudget,
    ) -> Result<Option<WorkProgress>, ProviderError> {
        let meta = self.meta(logical)?;
        let members = Self::member_keys(&meta);
        let path = members
            .first()
            .and_then(|key| database(key))
            .ok_or_else(|| ProviderError::Internal("hermes session has no database".into()))?
            .to_path_buf();
        let deadline = Instant::now() + Duration::from_millis(budget.max_millis.max(1));
        let (generation, shape) = self.check_job(job, logical, &path, deadline)?;
        let Some(stamp) = self.message_stamp(&path, shape, &members, deadline)? else {
            self.ledger.join(logical, job);
            return Ok(Some(WorkProgress::default()));
        };
        if self.transcripts.get(logical).is_some_and(|state| {
            state.members != members || state.generation != generation || state.stamp != stamp
        }) {
            self.transcripts.remove(logical);
        }
        if !self.transcripts.contains_key(logical) {
            if self.transcripts.len() >= MAX_TRANSCRIPTS {
                let victim = self
                    .transcripts
                    .iter()
                    .filter(|(key, _)| *key != logical && !self.ledger.has_consumer(key))
                    .min_by_key(|(_, state)| state.used)
                    .map(|(key, _)| key.clone());
                if let Some(victim) = victim {
                    self.transcripts.remove(&victim);
                }
            }
            self.transcripts.insert(
                logical.clone(),
                Transcript {
                    projection: Projection::new(&members, &path, generation, shape, None),
                    members,
                    generation,
                    stamp,
                    used: self.tick,
                },
            );
        }
        self.tick += 1;
        let mut transcript = self
            .transcripts
            .remove(logical)
            .ok_or_else(|| ProviderError::Internal("transcript state vanished".into()))?;
        transcript.used = self.tick;
        let result = {
            let db = self
                .dbs
                .get_mut(&path)
                .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
            transcript.projection.step(&mut db.connection, budget)
        };
        self.transcripts.insert(logical.clone(), transcript);
        match result {
            Ok(ReadProgress::Pending(progress)) => {
                self.ledger.join(logical, job);
                Ok(Some(progress))
            }
            Ok(ReadProgress::Ready(_)) => Ok(None),
            Err(error) => {
                self.transcripts.remove(logical);
                Err(error)
            }
        }
    }

    /// A job that is no longer `Pending` holds nothing.
    fn settle<T>(
        &mut self,
        key: &SessionKey,
        job: ReadJobId,
        result: &Result<ReadProgress<T>, ProviderError>,
    ) {
        if !matches!(result, Ok(ReadProgress::Pending(_))) {
            self.ledger.release(key, job);
            self.checked.remove(&(key.clone(), job));
        }
    }

    fn page_ready(
        &mut self,
        job: ReadJobId,
        logical: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
        if let Some(progress) = self.ensure_transcript(job, logical, budget)? {
            return Ok(ReadProgress::Pending(progress));
        }
        let started = Instant::now();
        let mut transcript = self
            .transcripts
            .remove(logical)
            .ok_or_else(|| ProviderError::Internal("transcript state vanished".into()))?;
        if let Some(anchor) = anchor.as_ref() {
            let expected = self
                .page_generation
                .get(&(logical.clone(), anchor.stable_id.clone()))
                .copied();
            if expected.is_some_and(|generation| generation != transcript.generation) {
                self.transcripts.insert(logical.clone(), transcript);
                return Err(ProviderError::Unsupported(
                    "transcript changed since this page was read; reopen the session".into(),
                ));
            }
        }
        let path = transcript
            .members
            .first()
            .and_then(|key| database(key))
            .ok_or_else(|| ProviderError::Internal("hermes session has no database".into()))?
            .to_path_buf();
        let result = {
            let db = self
                .dbs
                .get(&path)
                .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
            transcript.projection.page(
                &db.connection,
                logical,
                anchor.as_ref(),
                direction,
                request,
                budget,
                started,
            )
        };
        let generation = transcript.generation;
        self.transcripts.insert(logical.clone(), transcript);
        let page = result?;
        if let Some(next) = page.next.as_ref().filter(|_| page.has_more) {
            if self.page_generation.len() >= 4096 {
                self.page_generation.clear();
            }
            self.page_generation
                .insert((logical.clone(), next.stable_id.clone()), generation);
        }
        Ok(ReadProgress::Ready(page))
    }

    fn compactions(
        &mut self,
        path: &Path,
        shape: HermesShape,
        generation: u64,
        members: &[SessionKey],
    ) -> Result<(Vec<crate::domain::CompactionObservation>, bool), ProviderError> {
        let rows: Vec<super::lineage::HermesRowMeta> = self
            .catalog
            .rows()
            .into_iter()
            .filter(|row| members.contains(&row.key))
            .cloned()
            .collect();
        let prices = self.config.pricing.clone();
        let db = self
            .dbs
            .get_mut(path)
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        let scan = messages::read_compactions(
            &db.connection,
            shape,
            path,
            members,
            &rows,
            None,
            messages::CARRIER_SCAN,
            Some(generation),
        )?;
        let mut observations = scan.observations;
        // The ledger is enumerated in one bounded page for the details view; a
        // larger ledger marks the page truncated instead of materializing it.
        let page = messages::compression_ledger(
            &mut db.connection,
            shape,
            path,
            &prices,
            messages::LedgerRequest {
                members,
                after: -1,
                limit: messages::CARRIER_SCAN,
                max_bytes: usize::MAX,
                deadline: None,
                generation: Some(generation),
            },
        )?;
        let (ledger, more) = match page {
            ReadProgress::Ready(page) => (page.observations, page.more),
            ReadProgress::Pending(_) => (Vec::new(), true),
        };
        observations.extend(ledger);
        observations.sort_by(|a, b| (a.timestamp, &a.id).cmp(&(b.timestamp, &b.id)));
        Ok((observations, more))
    }

    fn details_ready(
        &mut self,
        job: ReadJobId,
        logical: &SessionKey,
        anchor: Option<PageAnchor>,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, ProviderError> {
        let meta = self.meta(logical)?;
        let members = Self::member_keys(&meta);
        let path = members
            .first()
            .and_then(|key| database(key))
            .ok_or_else(|| ProviderError::Internal("hermes session has no database".into()))?
            .to_path_buf();
        if let Some(progress) = self.ensure_transcript(job, logical, budget)? {
            return Ok(ReadProgress::Pending(progress));
        }
        let generation = self
            .transcripts
            .get(logical)
            .map(|state| state.generation)
            .ok_or_else(|| ProviderError::Internal("transcript state vanished".into()))?;
        let shape = self
            .dbs
            .get(&path)
            .map(|db| db.shape)
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        let (compactions, cut, events) = if anchor.is_none() {
            let (all, ledger_more) = self.compactions(&path, shape, generation, &members)?;
            let (keep, events) = share_details_budget(request, all.len(), false);
            let skip = all.len().saturating_sub(keep);
            (all[skip..].to_vec(), skip > 0 || ledger_more, events)
        } else {
            (Vec::new(), false, share_details_budget(request, 0, false).1)
        };
        let page = match self.page_ready(
            job,
            logical,
            anchor,
            PageDirection::Backward,
            events,
            budget,
        )? {
            ReadProgress::Pending(progress) => return Ok(ReadProgress::Pending(progress)),
            ReadProgress::Ready(page) => page,
        };
        Ok(ReadProgress::Ready(SessionDetailPage {
            // Hermes persists aggregate ledger rows, not individual requests.
            requests: Vec::new(),
            compactions,
            recent: page.events,
            next: page.next,
            has_more: page.has_more,
            truncated: page.truncated || cut,
        }))
    }

    fn source_ready(
        &mut self,
        job: ReadJobId,
        locator: &SourceLocator,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        let path = database(&locator.session)
            .ok_or_else(|| ProviderError::Unsupported("not a Hermes source locator".into()))?
            .to_path_buf();
        let deadline = Instant::now() + Duration::from_millis(budget.max_millis.max(1));
        let logical = self.resolve(&locator.session)?;
        let (generation, shape) = self.check_job(job, &logical, &path, deadline)?;
        if locator
            .generation
            .is_some_and(|generated| generated != generation)
        {
            return Err(ProviderError::Unsupported(
                "source changed since this record was listed; reopen the session".into(),
            ));
        }
        let (table, row_id) = match &locator.kind {
            crate::domain::LocatorKind::DbRow { table, row_id, .. } => {
                (table.clone(), row_id.clone())
            }
            _ => {
                return Err(ProviderError::Unsupported(
                    "not a Hermes database locator".into(),
                ));
            }
        };
        let db = self
            .dbs
            .get(&path)
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        let record = messages::read_source_record(
            &db.connection,
            shape,
            &table,
            &row_id,
            self.config.max_record_bytes,
            Some(generation),
            &locator.session,
        )?;
        Ok(ReadProgress::Ready(record))
    }
}

fn database(key: &SessionKey) -> Option<&Path> {
    match &key.source {
        SourceKey::DbRow { database, .. } => Some(database.as_path()),
        SourceKey::File(path) => Some(path.as_path()),
    }
}

fn digest(meta: &SessionMetadata) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    meta.key.native_id.hash(&mut hasher);
    meta.title.hash(&mut hasher);
    meta.title_derived.hash(&mut hasher);
    meta.model
        .as_ref()
        .map(|model| (&model.id, &model.provider, &model.configured_effort))
        .hash(&mut hasher);
    meta.created_at.hash(&mut hasher);
    meta.activity.at.hash(&mut hasher);
    meta.archived.hash(&mut hasher);
    meta.lineage
        .parent
        .as_ref()
        .map(|link| match link {
            crate::domain::SessionLink::Resolved(key) => (true, key.native_id.clone()),
            crate::domain::SessionLink::Unresolved(id) => (false, id.clone()),
            crate::domain::SessionLink::Ambiguous => (false, String::new()),
        })
        .hash(&mut hasher);
    meta.lineage
        .fork
        .as_ref()
        .map(|link| match link {
            crate::domain::SessionLink::Resolved(key) => (true, key.native_id.clone()),
            crate::domain::SessionLink::Unresolved(id) => (false, id.clone()),
            crate::domain::SessionLink::Ambiguous => (false, String::new()),
        })
        .hash(&mut hasher);
    meta.members
        .iter()
        .map(|member| &member.native_id)
        .collect::<Vec<_>>()
        .hash(&mut hasher);
    hasher.finish()
}

/// Merges the physical member metadata into one logical session row.
fn logical_meta(
    logical: &HermesLogicalSession,
    raw: &HashMap<SessionKey, SessionMetadata>,
) -> Option<SessionMetadata> {
    let root = raw.get(logical.members.first()?)?;
    let tip = logical
        .members
        .last()
        .and_then(|key| raw.get(key))
        .unwrap_or(root);
    let mut meta = root.clone();
    meta.model = tip.model.clone().or_else(|| meta.model.clone());
    meta.archived = tip.archived;
    let activity = logical
        .members
        .iter()
        .filter_map(|member| raw.get(member).and_then(|meta| meta.activity.at))
        .max();
    meta.activity = ActivityStamp {
        at: activity,
        class: if activity.is_some() {
            AggregationClass::Reported
        } else {
            AggregationClass::Unknown
        },
    };
    meta.members = logical.members.clone();
    logical.apply(&mut meta);
    Some(meta)
}

impl ProviderReader for HermesScanner {
    fn harness(&self) -> Harness {
        Harness::Hermes
    }

    fn discover(
        &mut self,
        now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        let delta = self.catalog.step(now, budget)?;
        self.pending_removed.extend(delta.removed);
        let mut out = DiscoveryDelta {
            sessions: Vec::new(),
            removed: Vec::new(),
            invalidated: Vec::new(),
            warnings: delta.warnings,
            scan: delta.scan,
        };
        if !delta.scan.done {
            return Ok(out);
        }
        let failed = self.catalog.failed_databases();
        let rows: Vec<super::lineage::HermesRowMeta> =
            self.catalog.rows().into_iter().cloned().collect();
        let mut raw: HashMap<SessionKey, SessionMetadata> = HashMap::new();
        for row in &rows {
            if let Some(meta) = self.catalog.reported(&row.key) {
                raw.insert(row.key.clone(), meta.clone());
            }
        }
        let lineage = super::lineage::resolve_lineage(&rows);
        let mut next: HashMap<SessionKey, SessionMetadata> = HashMap::new();
        let mut next_members: HashMap<SessionKey, Vec<SessionKey>> = HashMap::new();
        let mut member_of: HashMap<SessionKey, SessionKey> = HashMap::new();
        for session in &lineage.sessions {
            let Some(meta) = logical_meta(session, &raw) else {
                continue;
            };
            next.insert(meta.key.clone(), meta.clone());
            next_members.insert(meta.key.clone(), session.members.clone());
            for member in &session.members {
                member_of.insert(member.clone(), meta.key.clone());
            }
            member_of.insert(meta.key.clone(), meta.key.clone());
        }
        // Folded members stop being roots; a member that was published earlier is
        // removed even though its row still exists.
        let mut removed: HashSet<SessionKey> = HashSet::new();
        for key in self.published.keys() {
            if next.contains_key(key) {
                continue;
            }
            if member_of
                .get(key)
                .is_some_and(|logical| logical != key && next.contains_key(logical))
            {
                removed.insert(key.clone());
                continue;
            }
            let Some(members) = self.published_members.get(key) else {
                continue;
            };
            let unavailable =
                database(key).is_some_and(|db| failed.iter().any(|failed| failed == db));
            if unavailable {
                // Last-good metadata for a locked/unavailable profile is retained.
                next.insert(key.clone(), self.published[key].clone());
                next_members.insert(key.clone(), members.clone());
                for member in members {
                    member_of.insert(member.clone(), key.clone());
                }
                continue;
            }
            if members
                .iter()
                .any(|member| self.pending_removed.contains(member))
            {
                removed.insert(key.clone());
            }
        }
        for meta in next.values() {
            let key = meta.key.clone();
            let changed = self
                .published_digests
                .get(&key)
                .is_none_or(|known| *known != digest(meta));
            if changed {
                out.sessions.push(meta.clone());
            }
            let previous = self.published_members.get(&key);
            if previous.is_some_and(|members| next_members.get(&key) != Some(members)) {
                out.invalidated.push(key);
            }
        }
        out.removed = removed
            .into_iter()
            .filter(|key| !next.contains_key(key))
            .collect();
        for key in &out.removed {
            self.published.remove(key);
            self.published_digests.remove(key);
            self.published_members.remove(key);
        }
        for (key, meta) in &next {
            self.published_digests.insert(key.clone(), digest(meta));
        }
        self.published = next;
        self.published_members = next_members;
        self.member_of = member_of;
        self.pending_removed.clear();
        out.scan = ScanProgress {
            harness: Harness::Hermes,
            scanned: delta.scan.scanned,
            total: delta.scan.total,
            done: true,
        };
        Ok(out)
    }

    fn hydrate(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        let logical = self.resolve(key)?;
        let meta = self.meta(&logical)?;
        let members = Self::member_keys(&meta);
        let path = members
            .first()
            .and_then(|key| database(key))
            .ok_or_else(|| ProviderError::Internal("hermes session has no database".into()))?
            .to_path_buf();
        let deadline = Instant::now() + Duration::from_millis(budget.max_millis.max(1));
        let (generation, _) = self.check_job(job, &logical, &path, deadline)?;
        self.ledger.join(&logical, job);
        if !matches!(self.hydrations.get(&path), Some(entry) if entry.generation == generation) {
            self.hydrations.insert(
                path.clone(),
                HydrationEntry {
                    hydrator: HermesHydration::new(&path, self.config.pricing.clone()),
                    generation,
                },
            );
        }
        let hydrator = &mut self
            .hydrations
            .get_mut(&path)
            .ok_or_else(|| ProviderError::Internal("hydration state vanished".into()))?
            .hydrator;
        let primed = hydrator.prime(job, &logical, &members, budget)?;
        if !primed {
            return Ok(HydrationDelta {
                summary: SessionSummary::unhydrated(meta),
                done: false,
            });
        }
        let result = hydrator.step(job, &logical, now, budget);
        match result {
            Ok(delta) if !delta.done => Ok(delta),
            Ok(delta) => {
                self.ledger.release(&logical, job);
                self.checked.remove(&(logical.clone(), job));
                Ok(delta)
            }
            Err(error) => {
                if let Some(entry) = self.hydrations.get_mut(&path) {
                    entry.hydrator.cancel_read(&logical, job);
                }
                self.ledger.release(&logical, job);
                self.checked.remove(&(logical.clone(), job));
                Err(error)
            }
        }
    }

    fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId) {
        let logical = self.resolve(key).unwrap_or_else(|_| key.clone());
        let entry = self
            .published
            .get(&logical)
            .and_then(|meta| Self::member_keys(meta).first().cloned())
            .and_then(|key| database(&key).map(Path::to_path_buf))
            .and_then(|path| self.hydrations.get_mut(&path));
        if let Some(entry) = entry {
            entry.hydrator.cancel_read(&logical, job);
        }
        self.checked.remove(&(logical.clone(), job));
        if !self.ledger.release(&logical, job) {
            return;
        }
        if self
            .transcripts
            .get(&logical)
            .is_some_and(|state| !state.projection.ready())
        {
            self.transcripts.remove(&logical);
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
        let logical = self.resolve(key)?;
        let result = self.details_ready(job, &logical, anchor, request, budget);
        self.settle(&logical, job, &result);
        result
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
        let logical = self.resolve(key)?;
        let result = self.page_ready(job, &logical, anchor, direction, request, budget);
        self.settle(&logical, job, &result);
        result
    }

    fn count_events_after(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: &PageAnchor,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError> {
        let logical = self.resolve(key)?;
        let result = (|| {
            if let Some(progress) = self.ensure_transcript(job, &logical, budget)? {
                return Ok(ReadProgress::Pending(progress));
            }
            let transcript = self
                .transcripts
                .get(&logical)
                .ok_or_else(|| ProviderError::Internal("transcript state vanished".into()))?;
            Ok(ReadProgress::Ready(
                transcript.projection.count_after(anchor),
            ))
        })();
        self.settle(&logical, job, &result);
        result
    }

    fn source_record(
        &mut self,
        job: ReadJobId,
        locator: &SourceLocator,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        let key = self.resolve(&locator.session)?;
        let result = self.source_ready(job, locator, budget);
        self.settle(&key, job, &result);
        result
    }
}

/// Hermes reader that stays registered when no database can be opened yet. Each
/// discovery and request retries opening, so a transient lock or a profile still
/// being created is not permanent.
pub struct RetryingHermes {
    config: Config,
    scanner: HermesScanner,
}

impl RetryingHermes {
    /// Tries the configured primary database once; the error, if any, is returned
    /// so the caller can note it without dropping the reader.
    pub fn open(config: &Config) -> (Self, Option<ProviderError>) {
        let scanner = HermesScanner::new(config).expect("a Hermes scanner is cheap to build");
        let error = config
            .harnesses
            .hermes
            .db
            .as_ref()
            .and_then(|path| schema::open_read_only(path).err());
        (
            Self {
                config: config.clone(),
                scanner,
            },
            error,
        )
    }

    fn scanner(&mut self) -> Result<&mut HermesScanner, ProviderError> {
        let _ = &self.config;
        Ok(&mut self.scanner)
    }
}

impl ProviderReader for RetryingHermes {
    fn harness(&self) -> Harness {
        Harness::Hermes
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
        self.scanner.cancel_read(key, job);
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
