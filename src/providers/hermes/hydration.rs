//! Bounded Hermes hydration core: resumable session/usage reads, chain resolution
//! and a partial-safe cancellation boundary.
//!
//! This is not a `ProviderReader` and is not registered. It is the concrete core a
//! later scanner wraps: each `(session, job)` owns its cursors, a cancelled job
//! drops every partial row it read, and a new job always starts from the database
//! again so a writer's newer value is observed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use super::discovery::{busy_for_deadline, set_busy, with_deadline_bound};
use super::lineage::{self, HermesLogicalSession, HermesMarkers, HermesRowMeta};
use super::schema::{
    self, DEFAULT_BUSY_MS, EFFORT_MAX, HermesShape, MAX_CONFIG_BYTES, epoch_ms, short,
};
use super::usage::{
    HermesModelTotals, HermesMoney, HermesSessionTotals, HermesTokens, HermesUsageReconciler,
};
use crate::domain::{
    ActivityStamp, AggregationClass, Harness, Lifecycle, Lineage, ModelDetail, RefreshBudget,
    SessionKey, SessionMetadata, SessionSummary, SessionTiming, SourceKey, Timestamp,
};
use crate::pricing::PriceSchedule;
use crate::providers::sqlite_guard::{interrupted, map_error};
use crate::providers::{HydrationDelta, ProviderError, ReadJobId, ReadProgress};

const BATCH: usize = 64;
/// Longest ancestor/member chain primed per job.
const MAX_CHAIN: usize = 256;
/// Bounded pure lineage input per job. H3 replaces the whole-database scan with
/// chain-local walking; until then an oversized graph fails explicitly instead of
/// growing without limit.
const MAX_SESSION_ROWS: usize = 100_000;
/// Bounded accumulated usage rows per job.
const MAX_USAGE_ROWS: usize = 100_000;

/// One slice of hydration work.
pub struct HermesHydration {
    database: PathBuf,
    prices: PriceSchedule,
    busy_ms: u64,
    connection: Option<Connection>,
    shape: Option<HermesShape>,
    jobs: HashMap<(SessionKey, ReadJobId), Job>,
}

struct Job {
    phase: Phase,
    sessions_cursor: i64,
    rows: Vec<RowFacts>,
    usage_cursor: i64,
    usage: Vec<HermesModelTotals>,
    ledger_cursor: i64,
    ledger: Vec<crate::domain::CompactionObservation>,
    logical: Option<HermesLogicalSession>,
    members: Vec<SessionKey>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Sessions,
    Usage,
    Compactions,
    Finished,
}

struct RowFacts {
    meta: HermesRowMeta,
    title: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    configured: Option<String>,
    activity: Option<Timestamp>,
    archived: bool,
    tokens: HermesTokens,
    calls: Option<u64>,
    money: HermesMoney,
}

struct Slice {
    deadline: Instant,
    records: usize,
    bytes: usize,
}

impl Slice {
    fn new(budget: RefreshBudget) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_millis(budget.max_millis.max(1)),
            records: budget.max_records.max(1),
            bytes: budget.max_bytes.max(1),
        }
    }

    fn exhausted(&self) -> bool {
        self.records == 0 || self.bytes == 0 || Instant::now() >= self.deadline
    }

    fn charge(&mut self, records: usize, bytes: usize) {
        self.records = self.records.saturating_sub(records);
        self.bytes = self.bytes.saturating_sub(bytes);
    }

    fn limit(&self) -> usize {
        self.records.min(BATCH)
    }
}

impl HermesHydration {
    pub fn new(database: impl Into<PathBuf>, prices: PriceSchedule) -> Self {
        Self::with_busy(database, prices, DEFAULT_BUSY_MS)
    }

    pub fn with_busy(database: impl Into<PathBuf>, prices: PriceSchedule, busy_ms: u64) -> Self {
        Self {
            database: database.into(),
            prices,
            busy_ms: busy_ms.max(1),
            connection: None,
            shape: None,
            jobs: HashMap::new(),
        }
    }

    pub fn tracked_jobs(&self) -> usize {
        self.jobs.len()
    }

    pub fn has_state(&self, key: &SessionKey, job: ReadJobId) -> bool {
        self.jobs.contains_key(&(key.clone(), job))
    }

    /// Drops exactly this job's partial rows. Other jobs on the key keep theirs.
    pub fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId) {
        self.jobs.remove(&(key.clone(), job));
    }

    /// Loads only the requested chain and its ancestor rows, then continues with
    /// usage. `false` means the slice was interrupted and the call should be retried.
    pub(crate) fn prime(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        members: &[SessionKey],
        budget: RefreshBudget,
    ) -> Result<bool, ProviderError> {
        if database(key) != Some(self.database.as_path()) {
            return Err(ProviderError::Internal(
                "hydration key belongs to another database".into(),
            ));
        }
        self.jobs.entry((key.clone(), job)).or_insert_with(|| Job {
            phase: Phase::Sessions,
            sessions_cursor: -1,
            rows: Vec::new(),
            usage_cursor: -1,
            usage: Vec::new(),
            ledger_cursor: -1,
            ledger: Vec::new(),
            logical: None,
            members: Vec::new(),
        });
        if self
            .jobs
            .get(&(key.clone(), job))
            .is_some_and(|state| state.logical.is_some())
        {
            return Ok(true);
        }
        let Some(first) = members.first() else {
            return Err(ProviderError::Unavailable("session row is missing".into()));
        };
        let deadline = Instant::now() + Duration::from_millis(budget.max_millis.max(1));
        self.open(deadline)?;
        let shape = *self
            .shape
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes shape missing".into()))?;
        let database = self.database.clone();
        let ids: Vec<String> = members
            .iter()
            .map(|member| member.native_id.clone())
            .collect();
        let mut seen: std::collections::HashSet<String> = ids.iter().cloned().collect();
        let mut rows = match self.load_rows(&shape, &database, &ids, deadline)? {
            Some(rows) => rows,
            None => return Ok(false),
        };
        let mut next = rows
            .iter()
            .find(|facts| facts.meta.key == *first)
            .and_then(|facts| facts.meta.parent.clone());
        while let Some(parent) = next {
            if !seen.insert(parent.clone()) {
                break;
            }
            if seen.len() > MAX_CHAIN {
                return Err(ProviderError::Unsupported(
                    "Hermes lineage is longer than the supported ancestor count".into(),
                ));
            }
            let loaded = match self.load_rows(&shape, &database, &[parent], deadline)? {
                Some(rows) => rows,
                None => return Ok(false),
            };
            let Some(facts) = loaded.into_iter().next() else {
                break;
            };
            next = facts.meta.parent.clone();
            rows.push(facts);
        }
        let metas: Vec<HermesRowMeta> = rows.iter().map(|facts| facts.meta.clone()).collect();
        let lineage = lineage::resolve_lineage(&metas);
        let logical = lineage
            .for_key(key)
            .cloned()
            .ok_or_else(|| ProviderError::Unavailable("session row is missing".into()))?;
        if logical.key != *key {
            return Err(ProviderError::Unavailable(
                "the requested row is part of another compression chain".into(),
            ));
        }
        let state = self
            .jobs
            .get_mut(&(key.clone(), job))
            .ok_or_else(|| ProviderError::Internal("hydration job vanished".into()))?;
        state.rows = rows;
        state.members = logical.members.clone();
        state.logical = Some(logical);
        state.phase = Phase::Usage;
        Ok(true)
    }

    /// Reads the requested session rows by native id (same fixed projection as the
    /// full scan). `None` means the slice deadline interrupted the batch.
    fn load_rows(
        &mut self,
        shape: &HermesShape,
        database: &Path,
        ids: &[String],
        deadline: Instant,
    ) -> Result<Option<Vec<RowFacts>>, ProviderError> {
        if ids.is_empty() {
            return Ok(Some(Vec::new()));
        }
        let marks = (1..=ids.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = session_sql(shape).replace(
            "WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
            &format!("WHERE typeof(s.id) = 'text' AND s.id IN ({marks}) ORDER BY s.id"),
        );
        let values: Vec<rusqlite::types::Value> = ids
            .iter()
            .map(|id| rusqlite::types::Value::Text(id.clone()))
            .collect();
        let busy = busy_for_deadline(deadline, self.busy_ms);
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        set_busy(connection, busy)?;
        let mut rows = Vec::new();
        let outcome = with_deadline_bound(connection, deadline, |conn| {
            let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
            let mut query = statement
                .query(rusqlite::params_from_iter(values))
                .map_err(map_error)?;
            while let Some(row) = query.next().map_err(map_error)? {
                rows.push(row_facts(database, row)?);
            }
            Ok(())
        });
        match outcome {
            Ok(()) => Ok(Some(rows)),
            Err(error) if interrupted(&error) => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// One bounded slice. `Pending` leaves only this job's own partial state; a
    /// cancelled job is never visible again, and a fresh job re-reads the database.
    pub fn step(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        if database(key) != Some(self.database.as_path()) {
            return Err(ProviderError::Internal(
                "hydration key belongs to another database".into(),
            ));
        }
        let mut slice = Slice::new(budget);
        self.jobs.entry((key.clone(), job)).or_insert_with(|| Job {
            phase: Phase::Sessions,
            sessions_cursor: -1,
            rows: Vec::new(),
            usage_cursor: -1,
            usage: Vec::new(),
            ledger_cursor: -1,
            ledger: Vec::new(),
            logical: None,
            members: Vec::new(),
        });
        if let Err(error) = self.open(slice.deadline) {
            if interrupted(&error) {
                return Ok(HydrationDelta {
                    summary: self.pending_summary(key, job),
                    done: false,
                });
            }
            return Err(error);
        }
        loop {
            if slice.exhausted() {
                return Ok(HydrationDelta {
                    summary: self.pending_summary(key, job),
                    done: false,
                });
            }
            let phase = self
                .jobs
                .get(&(key.clone(), job))
                .map(|job| job.phase)
                .ok_or_else(|| ProviderError::Internal("hydration job vanished".into()))?;
            match phase {
                Phase::Sessions => {
                    let done = self.read_sessions(key, job, &mut slice)?;
                    if done && let Some(state) = self.jobs.get_mut(&(key.clone(), job)) {
                        state.phase = Phase::Usage;
                    }
                }
                Phase::Usage => {
                    self.prepare_chain(key, job)?;
                    let done = self.read_usage(key, job, &mut slice)?;
                    if done && let Some(state) = self.jobs.get_mut(&(key.clone(), job)) {
                        state.phase = Phase::Compactions;
                    }
                }
                Phase::Compactions => {
                    let done = self.read_ledger(key, job, &mut slice)?;
                    if done && let Some(state) = self.jobs.get_mut(&(key.clone(), job)) {
                        state.phase = Phase::Finished;
                    }
                }
                Phase::Finished => {
                    let summary = self.finish(key, job)?;
                    self.jobs.remove(&(key.clone(), job));
                    return Ok(HydrationDelta {
                        summary,
                        done: true,
                    });
                }
            }
        }
    }

    fn open(&mut self, deadline: Instant) -> Result<&Connection, ProviderError> {
        if self.connection.is_none() {
            let busy = busy_for_deadline(deadline, self.busy_ms);
            let mut conn = schema::open_with_deadline(&self.database, busy, deadline)?;
            let shape = with_deadline_bound(&mut conn, deadline, schema::detect_shape)?;
            self.shape = Some(shape);
            self.connection = Some(conn);
        }
        let shape = self
            .shape
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes shape missing".into()))?;
        if !shape.supported {
            return Err(ProviderError::Unsupported(
                "database has no supported sessions/messages schema".into(),
            ));
        }
        self.connection
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))
    }

    fn read_sessions(
        &mut self,
        key: &SessionKey,
        job: ReadJobId,
        slice: &mut Slice,
    ) -> Result<bool, ProviderError> {
        let shape = *self
            .shape
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes shape missing".into()))?;
        let sql = session_sql(&shape);
        let (cursor, limit) = {
            let state = self
                .jobs
                .get(&(key.clone(), job))
                .ok_or_else(|| ProviderError::Internal("hydration job vanished".into()))?;
            (state.sessions_cursor, slice.limit())
        };
        if limit == 0 {
            return Ok(false);
        }
        let deadline = slice.deadline;
        let database = self.database.clone();
        let key = key.clone();
        let jobs = &mut self.jobs;
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        // The connection may have been opened for a longer slice; the busy wait
        // must fit this slice so the progress handler can interrupt it.
        set_busy(connection, busy_for_deadline(deadline, self.busy_ms))?;
        let mut fetched = 0usize;
        let mut stopped = false;
        let outcome = with_deadline_bound(connection, deadline, |conn| {
            let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
            let mut query = statement
                .query(rusqlite::params![cursor, limit as i64])
                .map_err(map_error)?;
            while let Some(row) = query.next().map_err(map_error)? {
                let rowid: i64 = row.get(0).map_err(map_error)?;
                let facts = row_facts(&database, row)?;
                let at = facts.meta.id.clone();
                let bytes = 64 + at.len() + facts.title.as_ref().map_or(0, String::len);
                if let Some(state) = jobs.get_mut(&(key.clone(), job)) {
                    state.sessions_cursor = rowid;
                    if !facts.meta.id.is_empty() {
                        if state.rows.len() >= MAX_SESSION_ROWS {
                            return Err(ProviderError::Unsupported(
                                "session graph exceeds the bounded lineage index".into(),
                            ));
                        }
                        state.rows.push(facts);
                    }
                }
                fetched += 1;
                slice.charge(1, bytes);
                if slice.exhausted() {
                    stopped = true;
                    break;
                }
            }
            Ok(())
        });
        match outcome {
            Ok(()) => {}
            Err(error) if interrupted(&error) => stopped = true,
            Err(error) => return Err(error),
        }
        // A deadline/budget stop is not query exhaustion: keep the phase and let
        // the next slice continue from the retained cursor.
        Ok(!stopped && fetched < limit)
    }

    fn prepare_chain(&mut self, key: &SessionKey, job: ReadJobId) -> Result<(), ProviderError> {
        let state = self
            .jobs
            .get_mut(&(key.clone(), job))
            .ok_or_else(|| ProviderError::Internal("hydration job vanished".into()))?;
        if state.logical.is_some() {
            return Ok(());
        }
        let rows: Vec<HermesRowMeta> = state.rows.iter().map(|facts| facts.meta.clone()).collect();
        let lineage = lineage::resolve_lineage(&rows);
        let logical = lineage
            .for_key(key)
            .cloned()
            .ok_or_else(|| ProviderError::Unavailable("session row is missing".into()))?;
        state.members = logical.members.clone();
        state.logical = Some(logical);
        Ok(())
    }

    fn read_usage(
        &mut self,
        key: &SessionKey,
        job: ReadJobId,
        slice: &mut Slice,
    ) -> Result<bool, ProviderError> {
        let shape = *self
            .shape
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes shape missing".into()))?;
        if !shape.usage {
            return Ok(true);
        }
        let (cursor, limit, members) = {
            let state = self
                .jobs
                .get(&(key.clone(), job))
                .ok_or_else(|| ProviderError::Internal("hydration job vanished".into()))?;
            (state.usage_cursor, slice.limit(), state.members.clone())
        };
        if limit == 0 {
            return Ok(false);
        }
        if members.is_empty() {
            return Ok(true);
        }
        let marks = (2..=members.len() + 1)
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let status = if shape.usage_cost_status {
            usage_text("cost_status")
        } else {
            "NULL".to_owned()
        };
        let sql = format!(
            "SELECT rowid, session_id, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {} \
             FROM session_model_usage WHERE rowid > ?1 AND session_id IN ({marks}) \
             ORDER BY rowid LIMIT {}",
            usage_text("model"),
            usage_text("billing_provider"),
            usage_text("task"),
            usage_num("api_call_count"),
            usage_num("input_tokens"),
            usage_num("output_tokens"),
            usage_num("cache_read_tokens"),
            usage_num("cache_write_tokens"),
            usage_num("reasoning_tokens"),
            usage_num("estimated_cost_usd"),
            usage_num("actual_cost_usd"),
            status,
            limit
        );
        let deadline = slice.deadline;
        let database = self.database.clone();
        let key = key.clone();
        let jobs = &mut self.jobs;
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        set_busy(connection, busy_for_deadline(deadline, self.busy_ms))?;
        let mut values = vec![rusqlite::types::Value::Integer(cursor)];
        values.extend(
            members
                .iter()
                .map(|member| rusqlite::types::Value::Text(member.native_id.clone())),
        );
        let mut fetched = 0usize;
        let mut stopped = false;
        let outcome = with_deadline_bound(connection, deadline, |conn| {
            let mut statement = conn.prepare_cached(&sql).map_err(map_error)?;
            let mut query = statement
                .query(rusqlite::params_from_iter(values))
                .map_err(map_error)?;
            while let Some(row) = query.next().map_err(map_error)? {
                let rowid: i64 = row.get(0).map_err(map_error)?;
                let totals = usage_row(&database, rowid, row)?;
                let bytes = 96
                    + totals.model.as_ref().map_or(0, |model| model.id.len())
                    + totals.task.len();
                if let Some(state) = jobs.get_mut(&(key.clone(), job)) {
                    state.usage_cursor = rowid;
                    if state.usage.len() >= MAX_USAGE_ROWS {
                        return Err(ProviderError::Unsupported(
                            "usage rows exceed the bounded hydration index".into(),
                        ));
                    }
                    state.usage.push(totals);
                }
                fetched += 1;
                slice.charge(1, bytes);
                if slice.exhausted() {
                    stopped = true;
                    break;
                }
            }
            Ok(())
        });
        match outcome {
            Ok(()) => {}
            Err(error) if interrupted(&error) => stopped = true,
            Err(error) => return Err(error),
        }
        Ok(!stopped && fetched < limit)
    }

    /// One bounded slice of the aggregate compression ledger. The cursor is
    /// physical (`session_model_usage.rowid`); rows read but not stored are
    /// retried by the next slice.
    fn read_ledger(
        &mut self,
        key: &SessionKey,
        job: ReadJobId,
        slice: &mut Slice,
    ) -> Result<bool, ProviderError> {
        let shape = *self
            .shape
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes shape missing".into()))?;
        if !shape.usage {
            return Ok(true);
        }
        let (cursor, limit, members) = {
            let state = self
                .jobs
                .get(&(key.clone(), job))
                .ok_or_else(|| ProviderError::Internal("hydration job vanished".into()))?;
            (state.ledger_cursor, slice.limit(), state.members.clone())
        };
        if limit == 0 {
            return Ok(false);
        }
        let connection = self
            .connection
            .as_mut()
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        let request = super::messages::LedgerRequest {
            members: &members,
            after: cursor,
            limit,
            max_bytes: slice.bytes,
            deadline: Some(slice.deadline),
            generation: None,
        };
        match super::messages::compression_ledger(
            connection,
            shape,
            &self.database,
            &self.prices,
            request,
        )? {
            ReadProgress::Pending(_) => Ok(false),
            ReadProgress::Ready(page) => {
                let count = page.observations.len();
                if let Some(state) = self.jobs.get_mut(&(key.clone(), job)) {
                    state.ledger.extend(page.observations);
                    if let Some(next) = page.next {
                        state.ledger_cursor = next;
                    }
                }
                slice.charge(count, page.bytes);
                Ok(!page.more)
            }
        }
    }

    fn pending_summary(&self, key: &SessionKey, job: ReadJobId) -> SessionSummary {
        let state = self.jobs.get(&(key.clone(), job));
        let meta = state
            .and_then(|state| {
                state
                    .logical
                    .as_ref()
                    .and_then(|logical| self.logical_metadata(state, logical))
                    .or_else(|| {
                        state
                            .rows
                            .iter()
                            .find(|facts| facts.meta.key == *key)
                            .map(base_metadata)
                    })
            })
            .unwrap_or_else(|| placeholder(key));
        SessionSummary::unhydrated(meta)
    }

    fn finish(
        &mut self,
        key: &SessionKey,
        job: ReadJobId,
    ) -> Result<SessionSummary, ProviderError> {
        let state = self
            .jobs
            .get(&(key.clone(), job))
            .ok_or_else(|| ProviderError::Internal("hydration job vanished".into()))?;
        let logical = state
            .logical
            .clone()
            .ok_or_else(|| ProviderError::Internal("hydration chain missing".into()))?;
        let meta = self
            .logical_metadata(state, &logical)
            .ok_or_else(|| ProviderError::Unavailable("session row is missing".into()))?;
        let mut reconciler = HermesUsageReconciler::new(meta.clone(), &self.prices);
        for member in &logical.members {
            let facts = state
                .rows
                .iter()
                .find(|facts| facts.meta.key == *member)
                .ok_or_else(|| ProviderError::Internal("chain member vanished".into()))?;
            let session = HermesSessionTotals {
                key: facts.meta.key.clone(),
                model: model_of(facts),
                api_call_count: facts.calls,
                tokens: facts.tokens,
                money: facts.money,
            };
            let rows: Vec<HermesModelTotals> = state
                .usage
                .iter()
                .filter(|row| row.key == *member)
                .cloned()
                .collect();
            reconciler.observe_segment(&session, &rows);
        }
        let usage = reconciler.finish();
        let tip = state
            .rows
            .iter()
            .find(|facts| Some(&facts.meta.key) == logical.members.last())
            .or_else(|| state.rows.iter().find(|facts| facts.meta.key == *key));
        let closure = tip
            .map(|tip| {
                let turn = self.last_turn(&tip.meta.id).ok().flatten();
                classify_turn_closure(&tip.meta, turn.as_ref())
            })
            .unwrap_or_default();
        let root = state
            .rows
            .iter()
            .find(|facts| Some(&facts.meta.key) == logical.members.first());
        let started = root.and_then(|facts| facts.meta.started);
        let ended = closure
            .closed
            .then_some(tip.and_then(|facts| facts.meta.ended))
            .flatten();
        let elapsed_ms = match (started, ended) {
            (Some(start), Some(end)) if end >= start => Some((end - start) as u64),
            _ => None,
        };
        let last_event_at = state
            .rows
            .iter()
            .filter(|facts| logical.members.contains(&facts.meta.key))
            .filter_map(|facts| facts.activity)
            .max();
        let compactions = self.compactions(state, &logical)?;
        let mut summary = SessionSummary::unhydrated(meta);
        summary.usage = Some(usage);
        summary.lifecycle = closure.lifecycle;
        summary.last_completion = closure.last_completion;
        summary.last_event_at = last_event_at;
        summary.metrics = crate::domain::SessionMetrics {
            // No disk-backed request duration/TTFT or current occupancy exists in
            // these rows: speed, active time and context stay Unknown.
            timing: SessionTiming {
                started_at: started,
                ended_at: ended,
                elapsed_ms,
                active_ms: None,
                model_ms: None,
                active_complete: false,
            },
            compactions,
            ..crate::domain::SessionMetrics::default()
        };
        Ok(summary)
    }

    /// In-place carriers plus rotation edges (Event scope) and the aggregate
    /// compression ledger (Aggregate scope). The aggregate amount is never split
    /// into events and no event before/after/duration is invented.
    fn compactions(
        &self,
        state: &Job,
        logical: &HermesLogicalSession,
    ) -> Result<crate::domain::CompactionSummary, ProviderError> {
        let shape = *self
            .shape
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes shape missing".into()))?;
        let connection = self
            .connection
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        let rows: Vec<HermesRowMeta> = state
            .rows
            .iter()
            .filter(|facts| logical.members.contains(&facts.meta.key))
            .map(|facts| facts.meta.clone())
            .collect();
        let scan = super::messages::read_compactions(
            connection,
            shape,
            &self.database,
            &logical.members,
            &rows,
            None,
            super::messages::CARRIER_SCAN,
            None,
        )?;
        let mut observations = scan.observations;
        observations.extend(state.ledger.iter().cloned());
        Ok(crate::metrics::compaction_summary(
            &observations,
            scan.history_complete,
            &self.prices,
            crate::pricing::ReferencePolicy::Disabled,
        ))
    }

    fn logical_metadata(
        &self,
        job: &Job,
        logical: &HermesLogicalSession,
    ) -> Option<SessionMetadata> {
        let root = job
            .rows
            .iter()
            .find(|facts| Some(&facts.meta.key) == logical.members.first())?;
        let tip = job
            .rows
            .iter()
            .find(|facts| Some(&facts.meta.key) == logical.members.last())
            .unwrap_or(root);
        let mut meta = base_metadata(root);
        meta.model = model_of(tip).or(meta.model);
        let activity = logical
            .members
            .iter()
            .filter_map(|member| {
                job.rows
                    .iter()
                    .find(|facts| &facts.meta.key == member)
                    .and_then(|facts| facts.activity)
            })
            .max();
        meta.activity = ActivityStamp {
            at: activity,
            class: if activity.is_some() {
                AggregationClass::Reported
            } else {
                AggregationClass::Unknown
            },
        };
        meta.archived = tip.archived;
        logical.apply(&mut meta);
        Some(meta)
    }

    fn last_turn(&self, id: &str) -> Result<Option<HermesLastTurn>, ProviderError> {
        let shape = self
            .shape
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes shape missing".into()))?;
        let finish = if shape.message_finish_reason {
            "CASE WHEN typeof(finish_reason) = 'text' THEN finish_reason END"
        } else {
            "NULL"
        };
        let tool = if shape.message_tool_calls {
            "CASE WHEN tool_calls IS NOT NULL \
               AND length(CAST(tool_calls AS BLOB)) > 0 THEN 1 ELSE 0 END"
        } else {
            "0"
        };
        let sql = format!(
            "SELECT CASE WHEN typeof(role) = 'text' THEN role END, {finish}, {tool} \
             FROM messages WHERE session_id = ?1 \
             ORDER BY timestamp DESC, id DESC LIMIT 1"
        );
        let connection = self
            .connection
            .as_ref()
            .ok_or_else(|| ProviderError::Internal("hermes connection missing".into()))?;
        let result = connection.query_row(&sql, rusqlite::params![id], |row| {
            Ok(HermesLastTurn {
                role: row.get(0)?,
                finish_reason: row.get(1)?,
                has_tool_calls: row.get::<_, i64>(2)? != 0,
            })
        });
        match result {
            Ok(turn) => Ok(Some(turn)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(error) => Err(map_error(error)),
        }
    }
}

fn database(key: &SessionKey) -> Option<&Path> {
    match &key.source {
        SourceKey::DbRow { database, .. } => Some(database.as_path()),
        SourceKey::File(path) => Some(path.as_path()),
    }
}

fn placeholder(key: &SessionKey) -> SessionMetadata {
    SessionMetadata {
        key: key.clone(),
        title: key.native_id.clone(),
        title_derived: true,
        project: None,
        cwd: None,
        model: None,
        origins: Vec::new(),
        created_at: None,
        activity: ActivityStamp::default(),
        lineage: Lineage::default(),
        members: Vec::new(),
        archived: false,
        released: crate::domain::ReleaseState::Unknown,
        source_available: true,
        harness_version: None,
    }
}

fn base_metadata(facts: &RowFacts) -> SessionMetadata {
    let recorded_title = facts.title.as_deref().and_then(short);
    let title_derived = recorded_title.is_none();
    let title = recorded_title.unwrap_or_else(|| facts.meta.id.clone());
    SessionMetadata {
        key: facts.meta.key.clone(),
        title,
        title_derived,
        project: None,
        cwd: None,
        model: model_of(facts),
        origins: Vec::new(),
        created_at: facts.meta.started,
        activity: ActivityStamp {
            at: facts.activity,
            class: if facts.activity.is_some() {
                AggregationClass::Reported
            } else {
                AggregationClass::Unknown
            },
        },
        lineage: Lineage::default(),
        members: Vec::new(),
        archived: facts.archived,
        released: crate::domain::ReleaseState::Unknown,
        source_available: true,
        harness_version: None,
    }
}

fn model_of(facts: &RowFacts) -> Option<ModelDetail> {
    facts
        .model
        .as_deref()
        .and_then(short)
        .map(|id| ModelDetail {
            id,
            provider: facts.provider.as_deref().and_then(short),
            variant: None,
            effort: None,
            configured_effort: facts.configured.clone(),
        })
}

fn configured_effort(enabled: Option<&str>, effort: Option<&str>) -> Option<String> {
    match enabled? {
        "false" => Some("off".to_owned()),
        "true" => effort
            .map(str::trim)
            .filter(|level| !level.is_empty() && level.len() <= EFFORT_MAX)
            .map(str::to_owned),
        _ => None,
    }
}

fn usage_text(column: &str) -> String {
    format!("CASE WHEN typeof({column}) = 'text' THEN SUBSTR({column}, 1, 256) END")
}

fn usage_num(column: &str) -> String {
    format!("CASE WHEN typeof({column}) IN ('integer','real') THEN {column} END")
}

fn session_sql(shape: &HermesShape) -> String {
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
    let sized = format!(
        "typeof(s.model_config) = 'text' AND length(CAST(s.model_config AS BLOB)) <= {MAX_CONFIG_BYTES}"
    );
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
    let archived = if shape.archived {
        "CASE WHEN typeof(s.archived) = 'integer' THEN s.archived \
           WHEN typeof(s.archived) = 'real' THEN CAST(s.archived AS INTEGER) END"
            .to_string()
    } else {
        "NULL".to_string()
    };
    format!(
        "SELECT rowid, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, \
         {}, {}, {}, {}, {}, {}, {}, {}, {} \
         FROM sessions s WHERE rowid > ?1 ORDER BY rowid LIMIT ?2",
        "CASE WHEN typeof(s.id) = 'text' THEN s.id END",
        text(shape.parent, "SUBSTR(s.parent_session_id, 1, 256)"),
        text(shape.source, "SUBSTR(s.source, 1, 64)"),
        text(shape.model, "SUBSTR(s.model, 1, 256)"),
        text(shape.billing_provider, "SUBSTR(s.billing_provider, 1, 256)"),
        number(shape.ended, "s.ended_at"),
        text(shape.end_reason, "SUBSTR(s.end_reason, 1, 64)"),
        number(true, "s.started_at"),
        number(shape.last_activity, "s.last_activity_at"),
        text(shape.title, "SUBSTR(s.title, 1, 1024)"),
        archived,
        marker("_delegate_from"),
        marker("_branched_from"),
        marker("_reset_from"),
        enabled,
        effort,
        number(shape.api_call_count, "s.api_call_count"),
        number(shape.input_tokens, "s.input_tokens"),
        number(shape.output_tokens, "s.output_tokens"),
        number(shape.cache_read, "s.cache_read_tokens"),
        number(shape.cache_write, "s.cache_write_tokens"),
        number(shape.reasoning_tokens, "s.reasoning_tokens"),
        number(shape.estimated_cost, "s.estimated_cost_usd"),
        number(shape.actual_cost, "s.actual_cost_usd"),
        text(shape.cost_status, "s.cost_status")
    )
}

fn row_facts(database: &Path, row: &rusqlite::Row<'_>) -> Result<RowFacts, ProviderError> {
    let value = |index: usize| -> Result<Option<f64>, ProviderError> {
        row.get::<_, Option<f64>>(index).map_err(map_error)
    };
    let text = |index: usize| -> Result<Option<String>, ProviderError> {
        row.get::<_, Option<String>>(index).map_err(map_error)
    };
    let id = text(1)?.filter(|id| !id.is_empty()).unwrap_or_default();
    let mut meta = HermesRowMeta::new(database, &id);
    meta.parent = text(2)?;
    meta.source = text(3)?;
    meta.ended = epoch_ms(value(6)?);
    meta.end_reason = text(7)?;
    meta.started = epoch_ms(value(8)?);
    meta.markers = HermesMarkers {
        delegate_from: text(12)?,
        branched_from: text(13)?,
        reset_from: text(14)?,
    };
    let tokens = HermesTokens::from_row(row, 18).map_err(map_error)?;
    let calls: Option<i64> = row.get(17).map_err(map_error)?;
    let money = HermesMoney::from_row(row, 23, 24, 25).map_err(map_error)?;
    Ok(RowFacts {
        meta,
        title: text(10)?,
        model: text(4)?,
        provider: text(5)?,
        configured: configured_effort(text(15)?.as_deref(), text(16)?.as_deref()),
        activity: epoch_ms(value(9)?).or(epoch_ms(value(8)?)),
        archived: row
            .get::<_, Option<i64>>(11)
            .map_err(map_error)?
            .is_some_and(|v| v != 0),
        tokens,
        calls: calls.and_then(|calls| u64::try_from(calls).ok()),
        money,
    })
}

fn usage_row(
    database: &Path,
    rowid: i64,
    row: &rusqlite::Row<'_>,
) -> Result<HermesModelTotals, ProviderError> {
    let session: Option<String> = row.get(1).map_err(map_error)?;
    let id = session.unwrap_or_default();
    let model: Option<String> = row.get(2).map_err(map_error)?;
    let provider: Option<String> = row.get(3).map_err(map_error)?;
    let task: Option<String> = row.get(4).map_err(map_error)?;
    let calls: Option<i64> = row.get(5).map_err(map_error)?;
    Ok(HermesModelTotals {
        row: super::usage::HermesRowId(u64::try_from(rowid).unwrap_or(0)),
        key: SessionKey {
            harness: Harness::Hermes,
            native_id: id.clone(),
            source: SourceKey::DbRow {
                database: database.to_path_buf(),
                table: "sessions".to_owned(),
                id,
            },
        },
        model: model.and_then(|id| {
            short(&id).map(|id| ModelDetail {
                id,
                provider: provider.as_deref().and_then(short),
                variant: None,
                effort: None,
                configured_effort: None,
            })
        }),
        task: task.unwrap_or_default(),
        api_call_count: calls.and_then(|calls| u64::try_from(calls).ok()),
        tokens: HermesTokens::from_row(row, 6).map_err(map_error)?,
        money: HermesMoney::from_row(row, 11, 12, 13).map_err(map_error)?,
    })
}

/// Closure from `ended_at`/`end_reason` plus the final message's `finish_reason`.
/// Close is not success: an error finish is Failed, `agent_close` without a
/// terminal completion message stays Unknown, and a NULL end is never Ongoing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HermesClosure {
    pub closed: bool,
    pub lifecycle: Lifecycle,
    pub last_completion: Option<Timestamp>,
}

impl Default for HermesClosure {
    fn default() -> Self {
        Self {
            closed: false,
            lifecycle: Lifecycle::Unknown,
            last_completion: None,
        }
    }
}

/// The latest physical message of the logical session's tip. A newer user/tool
/// row or an assistant row with pending tool calls is unfinished work, not a
/// terminal completion.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HermesLastTurn {
    pub role: Option<String>,
    pub finish_reason: Option<String>,
    pub has_tool_calls: bool,
}

pub fn classify_closure(row: &HermesRowMeta, last_finish_reason: Option<&str>) -> HermesClosure {
    let turn = HermesLastTurn {
        role: Some("assistant".to_owned()),
        finish_reason: last_finish_reason.map(str::to_owned),
        has_tool_calls: false,
    };
    classify_turn_closure(row, Some(&turn))
}

pub fn classify_turn_closure(
    row: &HermesRowMeta,
    last_turn: Option<&HermesLastTurn>,
) -> HermesClosure {
    let Some(ended) = row.ended else {
        return HermesClosure {
            closed: false,
            lifecycle: Lifecycle::Unknown,
            last_completion: None,
        };
    };
    let finish = last_turn
        .and_then(|turn| turn.finish_reason.as_deref())
        .map(|reason| reason.trim().to_ascii_lowercase());
    let reason = row.end_reason.as_deref().unwrap_or("").to_ascii_lowercase();
    if finish
        .as_deref()
        .is_some_and(|reason| ["error", "agent_error", "content_filter"].contains(&reason))
        || ["error", "failed", "agent_error"].contains(&reason.as_str())
    {
        return HermesClosure {
            closed: true,
            lifecycle: Lifecycle::Failed,
            last_completion: None,
        };
    }
    if let Some(turn) = last_turn {
        let role = turn
            .role
            .as_deref()
            .unwrap_or_default()
            .to_ascii_lowercase();
        // A newer unfinished turn outranks an older terminal finish reason.
        if role == "user" || role == "tool" || (role == "assistant" && turn.has_tool_calls) {
            return HermesClosure {
                closed: true,
                lifecycle: Lifecycle::Unknown,
                last_completion: None,
            };
        }
        if (role == "assistant" || role.is_empty())
            && finish
                .as_deref()
                .is_some_and(|reason| ["stop", "length", "end_turn", "completed"].contains(&reason))
        {
            return HermesClosure {
                closed: true,
                lifecycle: Lifecycle::Completed,
                last_completion: Some(ended),
            };
        }
    }
    // `agent_close` and every other close reason prove closure only.
    HermesClosure {
        closed: true,
        lifecycle: Lifecycle::Unknown,
        last_completion: None,
    }
}
