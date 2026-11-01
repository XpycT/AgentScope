//! The OMP [`ProviderReader`]: discovery through [`OmpCatalog`], lifetime facts
//! through [`OmpHydration`], and a bounded transcript index per opened session.
//!
//! * Discovery and hydration slices return `Pending` progress and resume; a
//!   cancelled job drops only abandoned partial work (an unfinished hydration or
//!   index), never completed state another purpose still uses.
//! * A session is one logical conversation: a continuation chain's physical members
//!   are indexed together, in member order. The index keeps one small row per event
//!   (timestamp, `member/offset/block` order key, entry ID, whether it is a counted
//!   request) and never a record body; events are read back from the source by
//!   offset, through the shared [`SourceReader`], so a replaced or truncated file
//!   makes locators stale instead of serving old bytes.
//! * A request is attached to its event only when this session owns it
//!   ([`OmpHydration::ownership`]) and it is the first occurrence of that call, so
//!   copied fork/continuation history and repeated lines never become a second
//!   counted request.
//! * Source bodies are not cached anywhere: pages are rebuilt from the file.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

use serde_json::Value;

use crate::config::Config;
use crate::domain::{
    BoundedText, CompactionObservation, CompactionScope, DiagnosticEntry, Event, Harness,
    LocatorKind, PageAnchor, PageDirection, PageRequest, Provenance, RefreshBudget,
    SessionDetailPage, SessionKey, SessionMetadata, SessionSummary, SourceKey, SourceLocator,
    Timestamp, ToolInteraction, TranscriptPage,
};
use crate::ingest::source::{SourceItem, SourceReader, SourceRecordItem, SourceRecordProgress};
use crate::ingest::text::{display_safe, preview};
use crate::providers::claude::{event_anchor, event_size, fit_event};
use crate::providers::omp::hydration::{file_path, open_error};
use crate::providers::omp::ownership::OmpOwnership;
use crate::providers::omp::records::{self, Mapped, Slot, Text, carries_call};
use crate::providers::omp::tree::parse_entry_header;
use crate::providers::omp::usage::{call_identity, entry_time, text as clean_text};
use crate::providers::omp::{OmpCatalog, OmpHydration, map_omp_usage};
use crate::providers::{
    DiscoveryDelta, HydrationDelta, JobLedger, ProviderError, ProviderReader, ReadJobId,
    ReadProgress, SourceRecord, WorkProgress, share_details_budget,
};

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Hydrations kept at once (finished ones are dropped least-recently-used first).
const MAX_LIVE: usize = 16;
/// Transcript indexes kept at once; in-progress ones are never displaced.
const MAX_INDEXES: usize = 8;
/// Rows and approximate bytes one transcript index may hold.
const INDEX_CAP: usize = 1_000_000;
const INDEX_BYTES_CAP: usize = 64 * 1024 * 1024;
/// Hidden/unreadable record diagnostics kept per index.
const DIAG_CAP: usize = 10_000;
/// Diagnostics returned with one page.
const PAGE_DIAGS: usize = 200;
/// Compaction markers kept per index.
const COMPACTION_CAP: usize = 256;
/// Physical members of one logical session (the member number is 8 bits of an order key).
const MAX_MEMBERS: usize = 255;
const OFFSET_MASK: u64 = (1 << 40) - 1;
/// Display bounds of one text preview.
const TEXT_PREVIEW: usize = 4096;

type Stamp = (PathBuf, Option<(u64, Option<SystemTime>)>);

fn stamps_of(meta: &SessionMetadata) -> Vec<Stamp> {
    member_keys(meta)
        .iter()
        .filter_map(|key| match &key.source {
            SourceKey::File(path) => Some((
                path.clone(),
                std::fs::metadata(path)
                    .ok()
                    .map(|m| (m.len(), m.modified().ok())),
            )),
            SourceKey::DbRow { .. } => None,
        })
        .collect()
}

/// The physical sources of a session in chain order.
fn member_keys(meta: &SessionMetadata) -> Vec<SessionKey> {
    if meta.members.is_empty() {
        vec![meta.key.clone()]
    } else {
        meta.members.clone()
    }
}

/// Half of a budget (at least one record, byte and millisecond).
fn half(budget: RefreshBudget) -> RefreshBudget {
    RefreshBudget {
        max_records: (budget.max_records / 2).max(1),
        max_bytes: (budget.max_bytes / 2).max(1),
        max_millis: (budget.max_millis / 2).max(1),
    }
}

/// `member << 56 | offset << 16 | block`: unique and ordered by position in the chain.
fn order_of(member: usize, offset: u64, block: usize) -> u64 {
    ((member as u64) << 56) | (offset << 16) | (block as u64 & 0xffff)
}

fn split_order(order: u64) -> (usize, u64, usize) {
    (
        (order >> 56) as usize,
        (order >> 16) & OFFSET_MASK,
        (order & 0xffff) as usize,
    )
}

fn stable_id(entry: &str, order: u64) -> String {
    format!("{entry}@{order:x}")
}

/// One event of the transcript.
struct Row {
    ts: Option<Timestamp>,
    order: u64,
    entry: String,
    /// A uniquely owned model call whose usage the event carries.
    counted: bool,
}

struct Diag {
    order: u64,
    label: &'static str,
    hidden: bool,
}

struct Member {
    key: SessionKey,
    path: PathBuf,
    reader: SourceReader,
    /// Generation of `reader` when its records were last indexed: a change means the
    /// source was replaced or truncated, so every earlier row is void.
    generation: u64,
}

enum Stepped {
    Building(WorkProgress),
    Ready,
    Restart,
}

struct Index {
    generation: u64,
    /// Bytes of source records read back since the index was opened.
    read_bytes: usize,
    /// The most recently read record line: `(order >> 16, parsed, events, length)`.
    last: Option<(u64, Value, Vec<Slot>, usize)>,
    members: Vec<Member>,
    rows: Vec<Row>,
    sorted: bool,
    diags: Vec<Diag>,
    diags_sorted: bool,
    /// Order keys of compaction events, oldest first, newest `COMPACTION_CAP` kept.
    compactions: Vec<u64>,
    compaction_total: usize,
    /// First occurrences of counted calls: `(entry ID, response ID)`, then responses.
    seen: HashSet<(String, Option<String>)>,
    responses: HashSet<String>,
    bytes: usize,
    over: bool,
    ready: bool,
    used: u64,
}

fn over() -> ProviderError {
    ProviderError::Unsupported("OMP transcript exceeds the index entry or metadata limit".into())
}

fn changed() -> ProviderError {
    ProviderError::Unavailable("indexed source changed".into())
}

/// A native entry ID, or empty when the record has none usable.
fn entry_label(value: &Value) -> String {
    // The header's `id` names the session, not an entry.
    if value.get("type").and_then(Value::as_str) == Some("session") {
        return "session".to_owned();
    }
    value
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
        .unwrap_or("")
        .to_owned()
}

impl Index {
    fn open(keys: &[SessionKey], max_record_bytes: usize) -> Result<Self, ProviderError> {
        if keys.len() > MAX_MEMBERS {
            return Err(ProviderError::Unsupported(
                "OMP continuation chain is longer than the supported member count".into(),
            ));
        }
        let mut members = Vec::with_capacity(keys.len());
        for key in keys {
            let path = file_path(key)?;
            let reader = SourceReader::open(&path, max_record_bytes).map_err(|e| open_error(&e))?;
            members.push(Member {
                key: key.clone(),
                path,
                generation: reader.generation(),
                reader,
            });
        }
        Ok(Self {
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            read_bytes: 0,
            last: None,
            members,
            rows: Vec::new(),
            sorted: true,
            diags: Vec::new(),
            diags_sorted: true,
            compactions: Vec::new(),
            compaction_total: 0,
            seen: HashSet::new(),
            responses: HashSet::new(),
            bytes: 0,
            over: false,
            ready: false,
            used: 0,
        })
    }

    fn diag(&mut self, order: u64, label: &'static str, hidden: bool) {
        if self.diags.len() >= DIAG_CAP {
            return;
        }
        if self.diags.last().is_some_and(|last| order < last.order) {
            self.diags_sorted = false;
        }
        self.diags.push(Diag {
            order,
            label,
            hidden,
        });
    }

    fn push_row(&mut self, row: Row) -> Result<(), ProviderError> {
        self.bytes = self.bytes.saturating_add(64 + row.entry.len());
        if self.rows.len() >= INDEX_CAP || self.bytes > INDEX_BYTES_CAP {
            self.over = true;
            return Err(over());
        }
        if self
            .rows
            .last()
            .is_some_and(|last| (row.ts, row.order) < (last.ts, last.order))
        {
            self.sorted = false;
        }
        self.rows.push(row);
        Ok(())
    }

    /// Whether this is the first occurrence of an owned model call.
    fn counts_call(&mut self, member: usize, value: &Value, owner: &OmpHydration) -> bool {
        let Some((response, _)) = call_identity(value) else {
            return false;
        };
        let Some(header) = parse_entry_header(value, 0) else {
            // Without a native entry ID ownership cannot be proven.
            return false;
        };
        if !self.seen.insert((header.id.clone(), response.clone())) {
            return false;
        }
        if let Some(response) = &response
            && !self.responses.insert(response.clone())
        {
            return false;
        }
        self.bytes = self.bytes.saturating_add(header.id.len() + 96);
        matches!(
            owner.ownership(&self.members[member].key, &header.id, response.as_deref()),
            Some(OmpOwnership::Owned)
        )
    }

    fn add(
        &mut self,
        member: usize,
        item: &SourceRecordItem,
        owner: &OmpHydration,
    ) -> Result<(), ProviderError> {
        let LocatorKind::FileOffset { offset, .. } = &item.locator else {
            return Ok(());
        };
        if *offset > OFFSET_MASK {
            self.over = true;
            return Err(over());
        }
        let first = order_of(member, *offset, 0);
        match &item.item {
            SourceItem::Json(value) => match records::map(value, false) {
                Mapped::Skip => {}
                Mapped::Hidden(label) => self.diag(first, label, true),
                Mapped::Events(slots) => {
                    let ts = entry_time(value);
                    let entry = entry_label(value);
                    let counted = carries_call(value) && self.counts_call(member, value, owner);
                    for (block, slot) in slots.iter().enumerate() {
                        let order = order_of(member, *offset, block);
                        if slot.kind == crate::domain::EventKind::Compaction {
                            self.compaction_total += 1;
                            if self.compactions.len() >= COMPACTION_CAP {
                                self.compactions.remove(0);
                            }
                            self.compactions.push(order);
                        }
                        self.push_row(Row {
                            ts,
                            order,
                            entry: entry.clone(),
                            counted: block == 0 && counted,
                        })?;
                    }
                }
            },
            SourceItem::Invalid | SourceItem::TooLarge | SourceItem::Damaged(_) => {
                self.diag(first, "unreadable record", true);
            }
            SourceItem::Incomplete | SourceItem::Skipping => {}
        }
        Ok(())
    }

    /// One budgeted indexing slice over every member: new records are appended, a
    /// replaced or truncated member voids the index.
    fn step(
        &mut self,
        budget: RefreshBudget,
        owner: &OmpHydration,
    ) -> Result<Stepped, ProviderError> {
        if self.over {
            return Err(over());
        }
        let started = Instant::now();
        let mut progress = WorkProgress::default();
        let mut caught_up = true;
        let mut added = false;
        for at in 0..self.members.len() {
            if self.members[at].reader.generation() != self.members[at].generation {
                return Ok(Stepped::Restart);
            }
            let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            let spent = progress.records >= budget.max_records.max(1)
                || progress.expanded_bytes >= budget.max_bytes.max(1)
                || (elapsed >= budget.max_millis && (at > 0 || progress.records > 0));
            if spent {
                caught_up = false;
                break;
            }
            let slice = RefreshBudget {
                max_records: budget.max_records.max(1) - progress.records,
                max_bytes: budget.max_bytes.max(1) - progress.expanded_bytes,
                max_millis: budget.max_millis.saturating_sub(elapsed).max(1),
            };
            let batch = self.members[at]
                .reader
                .step(slice)
                .map_err(|e| open_error(&e))?;
            if batch.invalidated {
                return Ok(Stepped::Restart);
            }
            progress.records += batch.records.len();
            progress.expanded_bytes += batch.expanded_bytes;
            for item in &batch.records {
                let before = self.rows.len() + self.diags.len();
                self.add(at, item, owner)?;
                added |= self.rows.len() + self.diags.len() != before;
            }
            if !batch.done {
                caught_up = false;
                break;
            }
        }
        if added {
            if !self.sorted {
                self.rows.sort_by_key(|row| (row.ts, row.order));
                self.sorted = true;
            }
            if !self.diags_sorted {
                self.diags.sort_by_key(|d| d.order);
                self.diags_sorted = true;
            }
        }
        self.ready = caught_up;
        Ok(if caught_up {
            Stepped::Ready
        } else {
            Stepped::Building(progress)
        })
    }

    fn locator(&self, key: &SessionKey, order: u64) -> SourceLocator {
        let (member, offset, _) = split_order(order);
        SourceLocator {
            session: key.clone(),
            kind: LocatorKind::FileOffset {
                path: self.members[member].path.clone(),
                offset,
            },
            generation: Some(self.generation),
        }
    }

    /// The parsed record and its events at `order`, read from the source once per line
    /// (consecutive blocks of one record share it). The locator carries the generation
    /// saved when the member was indexed, so a replaced source stays stale.
    fn read(
        &mut self,
        order: u64,
        budget: RefreshBudget,
    ) -> Result<&(u64, Value, Vec<Slot>, usize), ProviderError> {
        if self.last.as_ref().is_some_and(|l| l.0 == order >> 16) {
            return self.last.as_ref().ok_or_else(changed);
        }
        let (member, offset, _) = split_order(order);
        let member = self.members.get_mut(member).ok_or_else(changed)?;
        let locator = SourceLocator {
            session: member.key.clone(),
            kind: LocatorKind::FileOffset {
                path: member.path.clone(),
                offset,
            },
            generation: Some(member.generation),
        };
        match member
            .reader
            .record(&locator, budget)
            .map_err(|e| open_error(&e))?
        {
            SourceRecordProgress::Ready(record) if record.unavailable.is_none() => {
                let len = record.raw.len();
                let value: Value = serde_json::from_str(&record.raw).map_err(|_| changed())?;
                let Mapped::Events(slots) = records::map(&value, true) else {
                    return Err(changed());
                };
                self.read_bytes = self.read_bytes.saturating_add(len);
                self.last = Some((order >> 16, value, slots, len));
                self.last.as_ref().ok_or_else(changed)
            }
            _ => Err(changed()),
        }
    }

    fn find(&self, anchor: &PageAnchor) -> Option<usize> {
        let at = self
            .rows
            .binary_search_by(|row| {
                (row.ts, row.order).cmp(&(anchor.timestamp, anchor.source_order))
            })
            .ok()?;
        (stable_id(&self.rows[at].entry, self.rows[at].order) == anchor.stable_id).then_some(at)
    }

    fn provenance(
        &self,
        key: &SessionKey,
        order: u64,
        record_type: &str,
        version: &Option<String>,
    ) -> Provenance {
        let (member, ..) = split_order(order);
        Provenance {
            harness: Harness::Omp,
            source: SourceKey::File(self.members[member].path.clone()),
            locator: self.locator(key, order),
            record_type: record_type.to_owned(),
            harness_version: version.clone(),
        }
    }

    fn event(
        &mut self,
        at: usize,
        key: &SessionKey,
        version: &Option<String>,
        budget: RefreshBudget,
    ) -> Result<Event, ProviderError> {
        let (ts, order, entry, counted) = {
            let row = &self.rows[at];
            (row.ts, row.order, row.entry.clone(), row.counted)
        };
        let (_, _, block) = split_order(order);
        let (value, slot) = {
            let line = self.read(order, budget)?;
            // Only a counted request needs the whole record again.
            let value = (counted && block == 0).then(|| line.1.clone());
            (value, line.2.get(block).cloned().ok_or_else(changed)?)
        };
        let provenance = self.provenance(key, order, slot.record_type, version);
        let usage = if let Some(value) = value {
            let mut observed = provenance.clone();
            observed.record_type =
                if value.get("type").and_then(Value::as_str) == Some("model_usage") {
                    "model_usage".into()
                } else {
                    "message/assistant".into()
                };
            map_omp_usage(&value, observed)
        } else {
            None
        };
        let locator = provenance.locator.clone();
        let bounded = |text: Text| {
            let p = preview(&text.raw, TEXT_PREVIEW, TEXT_PREVIEW);
            BoundedText {
                truncated: p.truncated || text.cut,
                preview: p.preview,
                byte_len: text.len,
                locator: Some(locator.clone()),
            }
        };
        let Slot {
            kind,
            actor,
            text,
            tool,
            ..
        } = slot;
        Ok(Event {
            id: stable_id(&entry, order),
            session: key.clone(),
            timestamp: ts,
            source_order: order,
            kind,
            actor,
            text: text.map(bounded),
            tool: tool.map(|tool| ToolInteraction {
                name: {
                    let name = display_safe(&tool.name);
                    if name.trim().is_empty() {
                        "tool".into()
                    } else {
                        name
                    }
                },
                call_id: tool.call_id.map(|id| display_safe(&id)),
                input: tool.input.map(bounded),
                output: tool.output.map(bounded),
                status: tool.status,
                exit_code: tool.exit_code,
                started_at: None,
                completed_at: None,
                orphan: false,
                child_session: None,
            }),
            usage,
            provenance,
        })
    }

    /// The compaction marker at `order`, with its bounded recorded summary.
    fn compaction(
        &mut self,
        order: u64,
        key: &SessionKey,
        version: &Option<String>,
        budget: RefreshBudget,
        cap: usize,
    ) -> Result<CompactionObservation, ProviderError> {
        let value = self.read(order, budget)?.1.clone();
        let provenance = self.provenance(key, order, "compaction", version);
        let summary = value
            .get("summary")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| {
                let p = preview(s, cap.min(TEXT_PREVIEW), TEXT_PREVIEW);
                BoundedText {
                    truncated: p.truncated,
                    preview: p.preview,
                    byte_len: s.len(),
                    locator: Some(provenance.locator.clone()),
                }
            });
        Ok(CompactionObservation {
            id: format!("omp:{}", entry_label(&value)),
            timestamp: entry_time(&value),
            method: clean_text(value.get("method")),
            trigger: None,
            summary,
            before_tokens: value.get("tokensBefore").and_then(Value::as_u64),
            after_tokens: value.get("tokensAfter").and_then(Value::as_u64),
            duration_ms: None,
            // OMP persists no usage for the summarization call: its cost is Unknown.
            usage: None,
            scope: CompactionScope::Event,
            provenance,
        })
    }
}

struct Live {
    hydration: OmpHydration,
    done: bool,
    used: u64,
}

struct Cached {
    summary: SessionSummary,
    /// The discovery metadata and source stamps the summary was folded from.
    base: SessionMetadata,
    stamps: Vec<Stamp>,
    used: u64,
}

/// OMP sessions and nested subagents as a [`ProviderReader`].
pub struct OmpScanner {
    config: Config,
    catalog: OmpCatalog,
    ledger: JobLedger,
    live: HashMap<SessionKey, Live>,
    summaries: HashMap<SessionKey, Cached>,
    indexes: HashMap<SessionKey, Index>,
    tick: u64,
}

impl OmpScanner {
    /// Cheap: no filesystem access until the first `discover`.
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        let omp = &config.harnesses.omp;
        let roots: Vec<PathBuf> = omp.root.iter().chain(&omp.extra_roots).cloned().collect();
        Ok(Self {
            config: config.clone(),
            catalog: OmpCatalog::new(
                roots,
                config.max_metadata_sources,
                config.max_metadata_bytes,
            )
            .with_evidence_limit(config.dedup_ledger_entries.saturating_mul(2)),
            ledger: JobLedger::default(),
            live: HashMap::new(),
            summaries: HashMap::new(),
            indexes: HashMap::new(),
            tick: 0,
        })
    }

    /// Summaries retained for rehydration (bounded by `max_cached_sessions`).
    pub fn cached_sessions(&self) -> usize {
        self.summaries.len()
    }

    /// `(session, job)` pairs still holding partial work.
    pub fn job_state(&self) -> usize {
        self.ledger.len()
    }

    /// Unfinished hydrations plus transcript indexes still being built.
    pub fn active_work(&self) -> usize {
        self.live.values().filter(|l| !l.done).count()
            + self.indexes.values().filter(|i| !i.ready).count()
    }

    fn discovered(&self, key: &SessionKey) -> Result<SessionMetadata, ProviderError> {
        if key.harness != Harness::Omp {
            return Err(ProviderError::Unsupported("not an OMP session".into()));
        }
        file_path(key)?;
        self.catalog
            .reported(key)
            .cloned()
            .ok_or_else(|| ProviderError::Unavailable("unknown OMP session".into()))
    }

    fn forget(&mut self, key: &SessionKey) {
        self.live.remove(key);
        self.summaries.remove(key);
        self.indexes.remove(key);
    }

    fn trim(&mut self, keep: &SessionKey) {
        while self.live.len() > MAX_LIVE {
            let victim = self
                .live
                .iter()
                .filter(|(k, l)| l.done && *k != keep && !self.ledger.has_consumer(k))
                .min_by_key(|(_, l)| l.used)
                .map(|(k, _)| k.clone());
            match victim {
                Some(key) => {
                    self.live.remove(&key);
                }
                None => break,
            }
        }
        while self.summaries.len() > self.config.max_cached_sessions.max(1) {
            let victim = self
                .summaries
                .iter()
                .filter(|(k, _)| *k != keep)
                .min_by_key(|(_, c)| c.used)
                .map(|(k, _)| k.clone());
            match victim {
                Some(key) => {
                    self.summaries.remove(&key);
                }
                None => break,
            }
        }
    }

    /// One hydration slice, creating the fold state when none exists.
    fn step_live(
        &mut self,
        key: &SessionKey,
        meta: SessionMetadata,
        budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        self.tick += 1;
        if self
            .live
            .get(key)
            .is_some_and(|live| live.hydration.metadata() != &meta)
        {
            // Relationships changed since this fold started: start over.
            self.live.remove(key);
        }
        if !self.live.contains_key(key) {
            let hydration = OmpHydration::new(meta.clone(), &self.config)?;
            self.live.insert(
                key.clone(),
                Live {
                    hydration,
                    done: false,
                    used: self.tick,
                },
            );
        }
        let stamps = stamps_of(&meta);
        let outcome = {
            let live = self
                .live
                .get_mut(key)
                .ok_or_else(|| ProviderError::Internal("hydration state vanished".into()))?;
            live.used = self.tick;
            let outcome = live.hydration.step(budget);
            if let Ok(delta) = &outcome {
                live.done = delta.done;
            }
            outcome
        };
        let delta = match outcome {
            Ok(delta) => delta,
            Err(error) => {
                self.live.remove(key);
                return Err(error);
            }
        };
        if delta.done {
            self.summaries.insert(
                key.clone(),
                Cached {
                    summary: delta.summary.clone(),
                    base: meta,
                    stamps,
                    used: self.tick,
                },
            );
        }
        self.trim(key);
        Ok(delta)
    }

    /// Makes the session's hydration complete. A call that had to do hydration work
    /// ends there (the index starts on the next call); a call that only confirmed
    /// finished facts keeps half its allowance for the index, so one call never
    /// spends more than one budget. `Ok` carries the allowance left for the index.
    fn prepare(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        budget: RefreshBudget,
    ) -> Result<Result<RefreshBudget, WorkProgress>, ProviderError> {
        let meta = self.discovered(key)?;
        self.ledger.join(key, job);
        let was_done = self.live.get(key).is_some_and(|live| live.done);
        let share = if was_done { half(budget) } else { budget };
        let delta = self.step_live(key, meta, share)?;
        Ok(if was_done && delta.done {
            Ok(half(budget))
        } else {
            Err(WorkProgress::default())
        })
    }

    /// Makes the transcript index current, or reports the work this slice did.
    fn ensure_index(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        budget: RefreshBudget,
    ) -> Result<Option<WorkProgress>, ProviderError> {
        let budget = match self.prepare(job, key, budget)? {
            Ok(left) => left,
            Err(progress) => return Ok(Some(progress)),
        };
        let meta = self.discovered(key)?;
        let keys = member_keys(&meta);
        if self
            .indexes
            .get(key)
            .is_some_and(|index| index.members.iter().map(|m| &m.key).ne(keys.iter()))
        {
            self.indexes.remove(key);
        }
        if !self.indexes.contains_key(key) {
            if self.indexes.len() >= MAX_INDEXES {
                let victim = self
                    .indexes
                    .iter()
                    .filter(|(k, i)| i.ready && !self.ledger.has_consumer(k))
                    .min_by_key(|(_, i)| i.used)
                    .map(|(k, _)| k.clone());
                match victim {
                    Some(victim) => {
                        self.indexes.remove(&victim);
                    }
                    // Every slot is a live in-progress index: leave this job pending.
                    None => {
                        self.ledger.join(key, job);
                        return Ok(Some(WorkProgress::default()));
                    }
                }
            }
            let index = Index::open(&keys, self.config.max_record_bytes)?;
            self.indexes.insert(key.clone(), index);
        }
        self.tick += 1;
        let live = self
            .live
            .get(key)
            .ok_or_else(|| ProviderError::Internal("hydration state vanished".into()))?;
        let Some(index) = self.indexes.get_mut(key) else {
            return Err(ProviderError::Internal("index state vanished".into()));
        };
        index.used = self.tick;
        match index.step(budget, &live.hydration)? {
            Stepped::Ready => Ok(None),
            Stepped::Building(progress) => {
                self.ledger.join(key, job);
                Ok(Some(progress))
            }
            Stepped::Restart => {
                self.indexes.remove(key);
                self.ledger.join(key, job);
                Ok(Some(WorkProgress::default()))
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
        }
    }

    fn page_ready(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
        if let Some(progress) = self.ensure_index(job, key, budget)? {
            return Ok(ReadProgress::Pending(progress));
        }
        let version = self.discovered(key)?.harness_version;
        let index = self
            .indexes
            .get_mut(key)
            .ok_or_else(|| ProviderError::Internal("index state vanished".into()))?;
        let empty = TranscriptPage {
            events: vec![],
            diagnostics: vec![],
            next: None,
            has_more: false,
            truncated: false,
        };
        let position = match anchor.as_ref() {
            Some(anchor) => match index.find(anchor) {
                Some(position) => Some(position),
                None => return Ok(ReadProgress::Ready(empty)),
            },
            None => None,
        };
        let len = index.rows.len();
        let forward = direction == PageDirection::Forward;
        let candidates: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(position.map_or(0, |p| p + 1)..len)
        } else {
            Box::new((0..position.unwrap_or(len)).rev())
        };
        let mut events = Vec::new();
        let mut selected = Vec::new();
        let mut size = 0usize;
        let mut truncated = false;
        let mut has_more = false;
        let started = Instant::now();
        let read_before = index.read_bytes;
        for at in candidates {
            // Materializing shares one allowance per call: a spent call ends the page
            // early (the next anchor continues it), after at least one event.
            if !events.is_empty()
                && (events.len() >= budget.max_records.max(1)
                    || index.read_bytes - read_before >= budget.max_bytes.max(1)
                    || started.elapsed().as_millis() >= u128::from(budget.max_millis))
            {
                has_more = true;
                break;
            }
            let mut event = index.event(at, key, &version, budget)?;
            let cut = fit_event(&mut event, request.max_bytes);
            let cost = event_size(&event);
            if !events.is_empty()
                && (events.len() >= request.max_events.max(1)
                    || size.saturating_add(cost) > request.max_bytes)
            {
                has_more = true;
                break;
            }
            size += cost;
            truncated |= cut
                || event.text.as_ref().is_some_and(|t| t.truncated)
                || event.tool.as_ref().is_some_and(|t| {
                    t.input.as_ref().is_some_and(|v| v.truncated)
                        || t.output.as_ref().is_some_and(|v| v.truncated)
                });
            events.push(event);
            selected.push(at);
        }
        if !forward {
            events.reverse();
            selected.reverse();
        }
        let next = if forward {
            events.last()
        } else {
            events.first()
        }
        .map(event_anchor);

        // Diagnostics belong to the page whose rows bracket their position.
        let mut diagnostics = Vec::new();
        if !selected.is_empty() || (len == 0 && anchor.is_none()) {
            let first = selected.first().copied().unwrap_or(0);
            let last = selected.last().copied().unwrap_or(0);
            let start = if first == 0 {
                0
            } else {
                let before = index.rows[first - 1].order;
                index.diags.partition_point(|d| d.order <= before)
            };
            let stop = if len == 0 || last + 1 == len {
                index.diags.len()
            } else {
                let until = index.rows[last].order;
                index.diags.partition_point(|d| d.order <= until)
            };
            let start = start.min(stop);
            let overflow = stop - start > PAGE_DIAGS;
            let visible = if overflow {
                start + PAGE_DIAGS - 1
            } else {
                stop
            };
            for d in &index.diags[start..visible] {
                diagnostics.push(DiagnosticEntry {
                    locator: index.locator(key, d.order),
                    label: d.label.into(),
                    hidden: d.hidden,
                });
            }
            if overflow {
                diagnostics.push(DiagnosticEntry {
                    locator: index.locator(key, index.diags[visible].order),
                    label: "diagnostics truncated (200-entry limit); first omitted source".into(),
                    hidden: true,
                });
                truncated = true;
            }
        }
        Ok(ReadProgress::Ready(TranscriptPage {
            events,
            diagnostics,
            next,
            has_more,
            truncated,
        }))
    }

    fn details_ready(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, ProviderError> {
        let first = anchor.is_none();
        // Requests, compactions and events share one record/byte allowance.
        let allowance = request.max_events.max(1);
        let mut compactions = Vec::new();
        let mut request = request;
        let mut cut = false;
        if first {
            if let Some(progress) = self.ensure_index(job, key, budget)? {
                return Ok(ReadProgress::Pending(progress));
            }
            let version = self.discovered(key)?.harness_version;
            let index = self
                .indexes
                .get_mut(key)
                .ok_or_else(|| ProviderError::Internal("index state vanished".into()))?;
            let orders = index.compactions.clone();
            // Marker count and bytes are known before reading them back.
            let (keep, events) = share_details_budget(request, orders.len(), true);
            let skip = orders.len().saturating_sub(keep);
            // Summary bytes come out of the third of the byte allowance markers share.
            let cap = request.max_bytes / 3 / keep.max(1);
            for order in &orders[skip..] {
                compactions.push(index.compaction(*order, key, &version, budget, cap)?);
            }
            // Markers older than the retained set (or evicted from the cap) are cut.
            cut = skip > 0 || index.compaction_total > orders.len();
            request = events;
        } else {
            request = share_details_budget(request, 0, true).1;
        }
        let page =
            match self.page_ready(job, key, anchor, PageDirection::Backward, request, budget)? {
                ReadProgress::Pending(progress) => return Ok(ReadProgress::Pending(progress)),
                ReadProgress::Ready(page) => page,
            };
        // Each returned record consumes the shared allowance exactly once.
        let room = allowance.saturating_sub(compactions.len() + page.events.len());
        let mut requests: Vec<_> = page.events.iter().filter_map(|e| e.usage.clone()).collect();
        let cut = cut || requests.len() > room;
        requests.truncate(room);
        Ok(ReadProgress::Ready(SessionDetailPage {
            requests,
            compactions,
            recent: page.events,
            next: page.next,
            has_more: page.has_more,
            truncated: page.truncated || cut,
        }))
    }
}

impl ProviderReader for OmpScanner {
    fn harness(&self) -> Harness {
        Harness::Omp
    }

    fn discover(
        &mut self,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        let delta = self.catalog.step(budget)?;
        for key in delta.removed.iter().chain(&delta.invalidated) {
            self.forget(key);
        }
        Ok(delta)
    }

    fn hydrate(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        let meta = self.discovered(key)?;
        self.ledger.join(key, job);
        self.tick += 1;
        // A finished summary of unchanged sources needs no work at all.
        if !self.live.contains_key(key)
            && let Some(cached) = self.summaries.get_mut(key)
            && cached.base == meta
            && cached.stamps == stamps_of(&meta)
        {
            cached.used = self.tick;
            let summary = cached.summary.clone();
            self.ledger.release(key, job);
            return Ok(HydrationDelta {
                summary,
                done: true,
            });
        }
        let result = self.step_live(key, meta, budget);
        if !matches!(&result, Ok(delta) if !delta.done) {
            self.ledger.release(key, job);
        }
        result
    }

    fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId) {
        if !self.ledger.release(key, job) {
            return;
        }
        // The last consumer is gone: abandoned partial work goes, healthy state stays.
        if self.live.get(key).is_some_and(|live| !live.done) {
            self.live.remove(key);
        }
        if self.indexes.get(key).is_some_and(|index| !index.ready) {
            self.indexes.remove(key);
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
        let result = self.details_ready(job, key, anchor, request, budget);
        self.settle(key, job, &result);
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
        let result = self.page_ready(job, key, anchor, direction, request, budget);
        self.settle(key, job, &result);
        result
    }

    fn count_events_after(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: &PageAnchor,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError> {
        let result = (|| {
            if let Some(progress) = self.ensure_index(job, key, budget)? {
                return Ok(ReadProgress::Pending(progress));
            }
            let index = self
                .indexes
                .get(key)
                .ok_or_else(|| ProviderError::Internal("index state vanished".into()))?;
            let at = index.rows.partition_point(|row| {
                (row.ts, row.order) <= (anchor.timestamp, anchor.source_order)
            });
            Ok(ReadProgress::Ready(index.rows.len() - at))
        })();
        self.settle(key, job, &result);
        result
    }

    fn source_record(
        &mut self,
        job: ReadJobId,
        source: &SourceLocator,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        let key = &source.session;
        let result = (|| {
            let LocatorKind::FileOffset { path, .. } = &source.kind else {
                return Err(ProviderError::Unsupported("not an OMP file locator".into()));
            };
            let meta = self.discovered(key)?;
            if !member_keys(&meta)
                .iter()
                .any(|m| matches!(&m.source, SourceKey::File(p) if p == path))
            {
                return Err(ProviderError::Unsupported(
                    "locator does not belong to session".into(),
                ));
            }
            // Terminal: the worker retries `Unavailable`, and a replaced source never
            // becomes the one this locator was listed from.
            let stale = || {
                ProviderError::Unsupported(
                    "source changed since this record was listed; reopen the session".into(),
                )
            };
            let index = self.indexes.get_mut(key).ok_or_else(stale)?;
            if source.generation != Some(index.generation) {
                return Err(stale());
            }
            let member = index
                .members
                .iter_mut()
                .find(|m| &m.path == path)
                .ok_or_else(stale)?;
            let locator = SourceLocator {
                session: member.key.clone(),
                kind: source.kind.clone(),
                generation: Some(member.generation),
            };
            match member
                .reader
                .record(&locator, budget)
                .map_err(|e| open_error(&e))?
            {
                SourceRecordProgress::Pending => {
                    self.ledger.join(key, job);
                    Ok(ReadProgress::Pending(WorkProgress::default()))
                }
                SourceRecordProgress::Ready(record) => Ok(ReadProgress::Ready(record)),
                SourceRecordProgress::Stale => Err(stale()),
            }
        })();
        self.settle(key, job, &result);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_keys_round_trip_and_sort_by_member_then_offset() {
        let a = order_of(0, 10, 3);
        let b = order_of(1, 0, 0);
        assert_eq!(split_order(a), (0, 10, 3));
        assert!(a < b);
        assert!(order_of(0, 10, 1) < order_of(0, 10, 2));
        assert!(order_of(0, 10, 2) < order_of(0, 11, 0));
    }

    #[test]
    fn stable_ids_differ_for_repeated_entries() {
        assert_ne!(
            stable_id("e1", order_of(0, 0, 0)),
            stable_id("e1", order_of(1, 0, 0))
        );
    }
}
