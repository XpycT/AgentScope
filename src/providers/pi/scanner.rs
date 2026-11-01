//! The Pi [`ProviderReader`]: discovery through [`PiCatalog`], lifetime facts
//! through [`PiHydration`], and a bounded transcript index per opened session.
//!
//! * Discovery and hydration slices return `Pending` progress and resume; a
//!   cancelled job drops only abandoned partial work, never completed state another
//!   purpose still uses.
//! * A native Pi session is one JSONL file. The index keeps one small row per
//!   displayed event (entry timestamp, `ordinal << 16 | block` order, native entry
//!   ID, whether it is the first occurrence of a counted request) and never a record
//!   body; events are read back from the source by offset, through the shared
//!   [`SourceReader`], so a replaced or truncated file makes locators stale instead
//!   of serving old bytes.
//! * A recorded call is attached to its event only when this file owns it
//!   ([`PiHydration::ownership`]) and it is the first occurrence of its identity, so
//!   a copied fork prefix and repeated lines never become a second counted request.
//! * A trailing partial line is `Incomplete` to the shared reader: it is never a
//!   record, and the finished line is read once its terminator arrives.
//! * Source bodies are not cached anywhere: pages are rebuilt from the file.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime};

use serde_json::Value;

use crate::config::Config;
use crate::domain::{
    BoundedText, CompactionObservation, CompactionScope, DiagnosticEntry, Event, EventKind,
    Harness, LocatorKind, PageAnchor, PageDirection, PageRequest, Provenance, RefreshBudget,
    RequestPurpose, SessionDetailPage, SessionKey, SessionLink, SessionMetadata, SessionSummary,
    SourceKey, SourceLocator, Timestamp, ToolInteraction, TranscriptPage,
};
use crate::ingest::source::{SourceItem, SourceReader, SourceRecordItem, SourceRecordProgress};
use crate::ingest::text::{display_safe, preview};
use crate::providers::claude::{event_anchor, event_size, fit_event};
use crate::providers::pi::events::{self, Mapped, Slot};
use crate::providers::pi::record::parse_time;
use crate::providers::pi::tree::PiOwnership;
use crate::providers::pi::{
    PiCatalog, PiEntry, PiEntryKind, PiHydration, PiRecordError, PiRole, assistant_model,
    parse_entry, parse_header, parse_pi_usage,
};
use crate::providers::{
    DiscoveryDelta, HydrationDelta, JobLedger, ProviderError, ProviderReader, ReadJobId,
    ReadProgress, SourceRecord, WorkProgress, share_details_budget,
};

/// Hydrations kept at once (finished ones are dropped least-recently-used first).
const MAX_LIVE: usize = 16;
/// Transcript indexes kept at once; in-progress ones are never displaced.
const MAX_INDEXES: usize = 8;

static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Rows and approximate bytes one transcript index may hold.
const INDEX_CAP: usize = 1_000_000;
const INDEX_BYTES_CAP: usize = 64 * 1024 * 1024;
/// Hidden/unreadable record diagnostics kept per index.
const DIAG_CAP: usize = 10_000;
/// Diagnostics returned with one page.
const PAGE_DIAGS: usize = 200;
/// Compaction/branch-summary markers kept per index, newest last.
const COMPACTION_CAP: usize = 512;
/// Ordinals are 48 bits of an order key; content blocks take the low 16.
const MAX_ORDINAL: u64 = (1 << 48) - 1;
/// Display bounds of one text preview.
const TEXT_PREVIEW: usize = 4096;

type Stamp = (PathBuf, Option<(u64, Option<SystemTime>)>);

fn stamps_of(meta: &SessionMetadata) -> Vec<Stamp> {
    match &meta.key.source {
        SourceKey::File(path) => vec![(
            path.clone(),
            std::fs::metadata(path)
                .ok()
                .map(|m| (m.len(), m.modified().ok())),
        )],
        SourceKey::DbRow { .. } => Vec::new(),
    }
}

/// The parts of discovery metadata a hydration depends on: its identity, the
/// resolved fork edge and the interpreted format version.
fn hydration_identity(meta: &SessionMetadata) -> (SessionKey, Option<SessionLink>, Option<String>) {
    (
        meta.key.clone(),
        meta.lineage.fork.clone(),
        meta.harness_version.clone(),
    )
}

/// Half of a budget (at least one record, byte and millisecond).
fn half(budget: RefreshBudget) -> RefreshBudget {
    RefreshBudget {
        max_records: (budget.max_records / 2).max(1),
        max_bytes: (budget.max_bytes / 2).max(1),
        max_millis: (budget.max_millis / 2).max(1),
    }
}

fn order_of(ordinal: u64, block: usize) -> Option<u64> {
    (ordinal <= MAX_ORDINAL && block < 0x1_0000).then_some((ordinal << 16) | block as u64)
}

fn split_order(order: u64) -> (u64, usize) {
    (order >> 16, (order & 0xffff) as usize)
}

fn stable_id(entry: &str, order: u64) -> String {
    format!("{entry}@{order:x}")
}

/// One displayable event of the index.
struct Row {
    ts: Option<Timestamp>,
    order: u64,
    offset: u64,
    entry: String,
    /// This event is the first occurrence of a request this file owns.
    counted: bool,
}

struct Diag {
    order: u64,
    offset: u64,
    label: String,
    hidden: bool,
}

#[derive(Clone)]
struct Marker {
    order: u64,
    offset: u64,
    /// The marker's recorded summary usage is this file's spend.
    counted: bool,
}

/// A partially materialized first details page. Marker bodies are read one at a time
/// under one shared call budget; the built prefix is kept so a `Pending` answer
/// resumes instead of repeating the reads.
struct MarkerPage {
    /// Revision of the retained marker selection when this page started. Appends and
    /// cap evictions change it even when the retained count stays the same, so a
    /// shifted selection can never be continued with stale observations.
    revision: u64,
    skip: usize,
    built: Vec<CompactionObservation>,
    read_bytes: usize,
    records: usize,
}

enum Stepped {
    Building(WorkProgress),
    Ready,
    Restart,
}

struct Index {
    key: SessionKey,
    path: PathBuf,
    /// Session identity plus the fork/version evidence the `counted` flags were
    /// computed from. A later discovery report with different evidence voids the
    /// index, because ownership (and therefore counted usage) must be recomputed.
    identity: (SessionKey, Option<SessionLink>, Option<String>),
    reader: SourceReader,
    /// Unique identity of this index instance: every locator it hands out carries it,
    /// so a locator from an earlier index of the same path (after truncation or
    /// replacement) stays stale even though each new reader restarts its own count.
    generation: u64,
    /// The reader's rotation count when its records were indexed: a change means the
    /// source was replaced or truncated, so every earlier row is void.
    observed: u64,
    /// Bytes of source records read back since the index was opened.
    read_bytes: usize,
    /// Source bytes/records framed by `step` since the index was opened; a details
    /// call subtracts this call's growth from its own allowance so indexing and
    /// materialization share one budget.
    step_bytes: usize,
    step_records: usize,
    /// The most recently read record line: `(ordinal, parsed, slots, length)`.
    last: Option<(u64, Value, Vec<Slot>, usize)>,
    header_seen: bool,
    /// The first record named another session: the path no longer holds this one.
    foreign: bool,
    rows: Vec<Row>,
    sorted: bool,
    diags: Vec<Diag>,
    diags_sorted: bool,
    /// Compaction/branch-summary markers, oldest first; newest `COMPACTION_CAP` kept.
    markers: Vec<Marker>,
    marker_total: usize,
    /// Changes whenever the retained marker selection changes (append or cap eviction).
    marker_revision: u64,
    /// First-page marker materialization in progress, if any.
    marker_page: Option<MarkerPage>,
    /// First occurrences of countable call identities.
    seen: HashSet<String>,
    bytes: usize,
    over: bool,
    ready: bool,
    used: u64,
}

fn over() -> ProviderError {
    ProviderError::Unsupported("Pi transcript exceeds the index entry or metadata limit".into())
}

fn changed() -> ProviderError {
    ProviderError::Unavailable("indexed source changed".into())
}

fn open_error(error: &crate::ingest::source::SourceError) -> ProviderError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ProviderError::Unavailable("session source is unavailable".into())
    } else {
        ProviderError::Io(error.to_string())
    }
}

fn file_path(key: &SessionKey) -> Result<PathBuf, ProviderError> {
    match &key.source {
        SourceKey::File(path) => Ok(path.clone()),
        SourceKey::DbRow { .. } => Err(ProviderError::Unsupported(
            "a Pi session is a file source".into(),
        )),
    }
}

/// The identity of a record's countable request, matching the hydration's
/// normalization: a confirmed `responseId` wins, else the native entry ID.
fn request_identity(entry: &PiEntry) -> Option<String> {
    match &entry.kind {
        PiEntryKind::Message {
            role: PiRole::Assistant,
            usage: true,
            response_id,
            ..
        } => Some(response_id.clone().unwrap_or_else(|| entry.id.clone())),
        PiEntryKind::Usage { .. } => Some(entry.id.clone()),
        PiEntryKind::Compaction { usage: true, .. }
        | PiEntryKind::BranchSummary { usage: true, .. } => Some(entry.id.clone()),
        _ => None,
    }
}

impl Index {
    fn open(meta: &SessionMetadata, max_record_bytes: usize) -> Result<Self, ProviderError> {
        let key = &meta.key;
        let path = file_path(key)?;
        let reader = SourceReader::open(&path, max_record_bytes).map_err(|e| open_error(&e))?;
        Ok(Self {
            key: key.clone(),
            path,
            identity: hydration_identity(meta),
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            observed: reader.generation(),
            reader,
            read_bytes: 0,
            step_bytes: 0,
            step_records: 0,
            last: None,
            header_seen: false,
            foreign: false,
            rows: Vec::new(),
            sorted: true,
            diags: Vec::new(),
            diags_sorted: true,
            markers: Vec::new(),
            marker_total: 0,
            marker_revision: 0,
            marker_page: None,
            seen: HashSet::new(),
            bytes: 0,
            over: false,
            ready: false,
            used: 0,
        })
    }

    fn diag(&mut self, order: u64, offset: u64, label: String, hidden: bool) {
        if self.diags.len() >= DIAG_CAP {
            return;
        }
        if self.diags.last().is_some_and(|last| order < last.order) {
            self.diags_sorted = false;
        }
        self.diags.push(Diag {
            order,
            offset,
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

    /// Whether this record is the first occurrence of a request this file owns.
    fn counts_call(&mut self, entry: &PiEntry, owner: &PiHydration) -> bool {
        let Some(identity) = request_identity(entry) else {
            return false;
        };
        self.bytes = self.bytes.saturating_add(identity.len() + 96);
        if !self.seen.insert(identity) {
            return false;
        }
        matches!(owner.ownership(&entry.id), PiOwnership::Owned)
    }

    fn add(&mut self, item: &SourceRecordItem, owner: &PiHydration) -> Result<(), ProviderError> {
        let LocatorKind::FileOffset { offset, .. } = &item.locator else {
            return Ok(());
        };
        let first = order_of(item.ordinal, 0).ok_or_else(over)?;
        match &item.item {
            SourceItem::Json(value) => {
                if !self.header_seen {
                    self.header_seen = true;
                    if let Some(header) = parse_header(value) {
                        // The path must still hold the session discovery reported.
                        self.foreign = header.id != self.key.native_id;
                        return Ok(());
                    }
                    if value.get("type").and_then(Value::as_str) == Some("session") {
                        self.diag(first, *offset, "unreadable Pi session header".into(), true);
                        return Ok(());
                    }
                    // A file without an interpretable header can still hold entries.
                }
                self.entry(value, item.ordinal, *offset, owner)
            }
            SourceItem::Invalid => {
                self.diag(first, *offset, "unreadable record".into(), true);
                Ok(())
            }
            SourceItem::TooLarge => {
                self.diag(first, *offset, "oversized record".into(), true);
                Ok(())
            }
            // A pending oversized line or a torn trailing line is not a record.
            SourceItem::Skipping | SourceItem::Incomplete => Ok(()),
            SourceItem::Damaged(damage) => {
                self.diag(first, *offset, format!("damaged source: {damage}"), true);
                Ok(())
            }
        }
    }

    /// Interprets one record. An unsupported or malformed record is a diagnostic, so
    /// the valid records around it stay listable.
    fn entry(
        &mut self,
        value: &Value,
        ordinal: u64,
        offset: u64,
        owner: &PiHydration,
    ) -> Result<(), ProviderError> {
        let first = order_of(ordinal, 0).ok_or_else(over)?;
        let entry = match parse_entry(value, ordinal, offset) {
            Ok(entry) => entry,
            Err(PiRecordError::Unsupported) => {
                self.diag(first, offset, "not a session entry".into(), true);
                return Ok(());
            }
            Err(PiRecordError::Malformed) => {
                self.diag(first, offset, "unreadable record".into(), true);
                return Ok(());
            }
        };
        match events::map(value, false) {
            Mapped::Hidden(label) => {
                self.diag(first, offset, label, true);
                Ok(())
            }
            Mapped::Events(slots) => {
                let counted = self.counts_call(&entry, owner);
                for (block, slot) in slots.iter().enumerate() {
                    let order = order_of(ordinal, block).ok_or_else(over)?;
                    if slot.kind == EventKind::Compaction && block == 0 {
                        self.marker_total += 1;
                        self.marker_revision = self.marker_revision.wrapping_add(1);
                        if self.markers.len() >= COMPACTION_CAP {
                            self.markers.remove(0);
                        }
                        self.markers.push(Marker {
                            order,
                            offset,
                            counted,
                        });
                    }
                    self.push_row(Row {
                        ts: entry.timestamp,
                        order,
                        offset,
                        entry: entry.id.clone(),
                        counted: block == 0 && counted,
                    })?;
                }
                Ok(())
            }
        }
    }

    /// One budgeted indexing slice: new records are appended; a replaced or truncated
    /// source voids the index so the next call rebuilds it.
    fn step(
        &mut self,
        budget: RefreshBudget,
        owner: &PiHydration,
    ) -> Result<Stepped, ProviderError> {
        if self.over {
            return Err(over());
        }
        if self.reader.generation() != self.observed {
            return Ok(Stepped::Restart);
        }
        let started = Instant::now();
        let mut progress = WorkProgress::default();
        loop {
            let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            if progress.records >= budget.max_records.max(1)
                || progress.expanded_bytes >= budget.max_bytes.max(1)
                || (elapsed >= budget.max_millis && progress.records > 0)
            {
                return Ok(Stepped::Building(progress));
            }
            let slice = RefreshBudget {
                max_records: budget.max_records.max(1) - progress.records,
                max_bytes: budget.max_bytes.max(1) - progress.expanded_bytes,
                max_millis: budget.max_millis.saturating_sub(elapsed).max(1),
            };
            let batch = self.reader.step(slice).map_err(|e| open_error(&e))?;
            if batch.invalidated {
                return Ok(Stepped::Restart);
            }
            progress.records += batch.records.len();
            progress.expanded_bytes += batch.expanded_bytes;
            self.step_bytes = self.step_bytes.saturating_add(batch.expanded_bytes);
            self.step_records = self.step_records.saturating_add(batch.records.len());
            for item in &batch.records {
                self.add(item, owner)?;
                if self.foreign {
                    return Err(ProviderError::Unavailable(
                        "the session file now holds another session".into(),
                    ));
                }
            }
            if batch.done {
                break;
            }
        }
        if !self.sorted {
            self.rows.sort_by_key(|row| (row.ts, row.order));
            self.sorted = true;
        }
        if !self.diags_sorted {
            self.diags.sort_by_key(|d| d.order);
            self.diags_sorted = true;
        }
        self.ready = true;
        Ok(Stepped::Ready)
    }

    fn locator(&self, session: &SessionKey, offset: u64, generation: u64) -> SourceLocator {
        SourceLocator {
            session: session.clone(),
            kind: LocatorKind::FileOffset {
                path: self.path.clone(),
                offset,
            },
            generation: Some(generation),
        }
    }

    /// The parsed record and its slots at `order`, read from the source once per line
    /// (consecutive blocks of one record share it). The locator carries the generation
    /// saved when the source was indexed, so a replaced source stays stale.
    fn read(
        &mut self,
        order: u64,
        offset: u64,
        budget: RefreshBudget,
    ) -> Result<&(u64, Value, Vec<Slot>, usize), ProviderError> {
        let ordinal = split_order(order).0;
        if self.last.as_ref().is_some_and(|line| line.0 == ordinal) {
            return self.last.as_ref().ok_or_else(changed);
        }
        let locator = SourceLocator {
            session: self.key.clone(),
            kind: LocatorKind::FileOffset {
                path: self.path.clone(),
                offset,
            },
            // The reader addresses its own generation; the locator handed to callers
            // carries this index instance instead.
            generation: Some(self.observed),
        };
        match self
            .reader
            .record(&locator, budget)
            .map_err(|e| open_error(&e))?
        {
            SourceRecordProgress::Ready(record) if record.unavailable.is_none() => {
                let len = record.raw.len();
                let value: Value = serde_json::from_str(&record.raw).map_err(|_| changed())?;
                let Mapped::Events(slots) = events::map(&value, true) else {
                    return Err(changed());
                };
                self.read_bytes = self.read_bytes.saturating_add(len);
                self.last = Some((ordinal, value, slots, len));
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
        session: &SessionKey,
        offset: u64,
        record_type: &str,
        version: &Option<String>,
    ) -> Provenance {
        Provenance {
            harness: Harness::Pi,
            source: SourceKey::File(self.path.clone()),
            locator: self.locator(session, offset, self.generation),
            record_type: record_type.to_owned(),
            harness_version: version.clone(),
        }
    }

    fn event(
        &mut self,
        at: usize,
        session: &SessionKey,
        version: &Option<String>,
        budget: RefreshBudget,
    ) -> Result<Event, ProviderError> {
        let (ts, order, offset, entry, counted) = {
            let row = &self.rows[at];
            (
                row.ts,
                row.order,
                row.offset,
                row.entry.clone(),
                row.counted,
            )
        };
        let block = split_order(order).1;
        let (value, slot) = {
            let line = self.read(order, offset, budget)?;
            // Only a counted request needs the whole record again.
            let value = (counted && block == 0).then(|| line.1.clone());
            (value, line.2.get(block).cloned().ok_or_else(changed)?)
        };
        let provenance = self.provenance(session, offset, slot.record_type, version);
        let usage = value
            .as_ref()
            .and_then(|value| event_usage(value, &provenance, &entry));
        let locator = provenance.locator.clone();
        let bounded = |text: events::Text| {
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
            session: session.clone(),
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
                exit_code: None,
                started_at: None,
                completed_at: None,
                orphan: false,
                child_session: None,
            }),
            usage,
            provenance,
        })
    }

    /// The compaction/branch-summary marker at `order`, with its bounded recorded
    /// summary and (when this file owns it) its recorded summary usage.
    fn marker(
        &mut self,
        marker: &Marker,
        session: &SessionKey,
        version: &Option<String>,
        budget: RefreshBudget,
        cap: usize,
    ) -> Result<CompactionObservation, ProviderError> {
        let value = self.read(marker.order, marker.offset, budget)?.1.clone();
        let record_type = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("compaction");
        let provenance = self.provenance(session, marker.offset, record_type, version);
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
        let id = value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let usage = marker
            .counted
            .then(|| event_usage(&value, &provenance, &id))
            .flatten();
        Ok(CompactionObservation {
            id: format!("pi:{id}"),
            timestamp: value
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(parse_time),
            method: None,
            trigger: None,
            summary,
            before_tokens: value.get("tokensBefore").and_then(Value::as_u64),
            after_tokens: None,
            duration_ms: None,
            usage,
            scope: CompactionScope::Event,
            provenance,
        })
    }
}

/// The recorded observation of one counted request, normalized exactly as the
/// hydration counts it (purpose, ownership identity, no inferred timing).
fn event_usage(
    value: &Value,
    provenance: &Provenance,
    entry_id: &str,
) -> Option<crate::domain::UsageObservation> {
    let record_type = value.get("type").and_then(Value::as_str)?;
    match record_type {
        "message" => {
            let message = value.get("message")?.as_object()?;
            let model = assistant_model(message, None);
            let mut observation = parse_pi_usage(&value["message"], model, provenance.clone());
            normalize(&mut observation, RequestPurpose::Conversation, entry_id);
            Some(observation)
        }
        "usage" => {
            let model = crate::domain::ModelDetail {
                id: value.get("model").and_then(Value::as_str)?.to_owned(),
                provider: value
                    .get("provider")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                variant: None,
                effort: None,
                configured_effort: None,
            };
            let kind = value
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("usage")
                .to_owned();
            let mut observation = parse_pi_usage(value, Some(model), provenance.clone());
            normalize(&mut observation, RequestPurpose::Auxiliary(kind), entry_id);
            Some(observation)
        }
        "compaction" | "branch_summary" => {
            // The entry persists no provider/model route, so route-dependent pricing
            // stays Unknown instead of borrowing the conversation model.
            let mut observation = parse_pi_usage(value, None, provenance.clone());
            normalize(&mut observation, RequestPurpose::Compaction, entry_id);
            Some(observation)
        }
        _ => None,
    }
}

fn normalize(
    observation: &mut crate::domain::UsageObservation,
    purpose: RequestPurpose,
    entry_id: &str,
) {
    observation.scope = crate::domain::UsageScope::Message;
    if let Some(request) = &mut observation.request {
        request.purpose = purpose;
        request.id = request.id.take().or_else(|| Some(entry_id.to_owned()));
    }
}

struct Live {
    hydration: PiHydration,
    identity: (SessionKey, Option<SessionLink>, Option<String>),
    /// Source stamps sampled when the last slice started.
    stamps: Vec<Stamp>,
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

/// Native Pi sessions as a [`ProviderReader`]: one JSONL file per session, no
/// invented extension children, no request timing the producer did not record.
pub struct PiScanner {
    config: Config,
    catalog: PiCatalog,
    ledger: JobLedger,
    live: HashMap<SessionKey, Live>,
    summaries: HashMap<SessionKey, Cached>,
    indexes: HashMap<SessionKey, Index>,
    tick: u64,
}

impl PiScanner {
    /// Cheap: no filesystem access until the first `discover`.
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        let pi = &config.harnesses.pi;
        let roots: Vec<PathBuf> = pi.root.iter().chain(&pi.extra_roots).cloned().collect();
        Ok(Self {
            config: config.clone(),
            catalog: PiCatalog::new(
                roots,
                config.max_metadata_sources,
                config.max_metadata_bytes,
            ),
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

    /// Details pages with a partially materialized marker prefix still retained.
    pub fn pending_marker_pages(&self) -> usize {
        self.indexes
            .values()
            .filter(|index| index.marker_page.is_some())
            .count()
    }

    fn discovered(&self, key: &SessionKey) -> Result<SessionMetadata, ProviderError> {
        if key.harness != Harness::Pi {
            return Err(ProviderError::Unsupported("not a Pi session".into()));
        }
        file_path(key)?;
        self.catalog
            .reported(key)
            .cloned()
            .ok_or_else(|| ProviderError::Unavailable("unknown Pi session".into()))
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

    /// One hydration slice, creating the fold state when none exists and resuming a
    /// finished one after append-only growth.
    fn step_live(
        &mut self,
        key: &SessionKey,
        meta: SessionMetadata,
        budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        self.tick += 1;
        let identity = hydration_identity(&meta);
        if self
            .live
            .get(key)
            .is_some_and(|live| live.identity != identity)
        {
            // Relationships or the interpreted version changed: start over.
            self.live.remove(key);
        }
        // Sampled before the slice so growth during it is seen next time.
        let stamps = stamps_of(&meta);
        if !self.live.contains_key(key) {
            let hydration = PiHydration::new(meta.clone(), &self.config)?;
            self.live.insert(
                key.clone(),
                Live {
                    hydration,
                    identity,
                    stamps: stamps.clone(),
                    done: false,
                    used: self.tick,
                },
            );
        } else if let Some(live) = self.live.get_mut(key) {
            // Discovery re-probed the source (title/session name/activity can change on
            // an append). The fold keeps its accounting state but must report the newer
            // metadata, or a resumed preview moves activity backwards.
            live.hydration.rebase(meta.clone());
            if live.done && live.stamps != stamps {
                // The file only grew: continue from the consumed prefix so the fold
                // counts the appended records once and none twice.
                live.hydration.resume();
                live.done = false;
                live.stamps = stamps.clone();
            } else {
                live.stamps = stamps.clone();
            }
            live.used = self.tick;
        }
        let outcome = {
            let live = self
                .live
                .get_mut(key)
                .ok_or_else(|| ProviderError::Internal("hydration state vanished".into()))?;
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
                    // The stamps sampled before this slice: growth during it is
                    // re-checked by the next call.
                    stamps,
                    used: self.tick,
                },
            );
        }
        self.trim(key);
        Ok(delta)
    }

    /// Makes the session's hydration complete. A call that had to do hydration work
    /// ends there; a call that only confirmed finished facts keeps half its allowance
    /// for the index, so one call never spends more than one budget.
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
        // Counted usage depends on resolved fork evidence and the interpreted version,
        // so an index built under different evidence must be rebuilt even when the
        // session key is unchanged.
        if self
            .indexes
            .get(key)
            .is_some_and(|index| index.identity != hydration_identity(&meta))
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
            let index = Index::open(&meta, self.config.max_record_bytes)?;
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
                    locator: index.locator(key, d.offset, index.generation),
                    label: d.label.clone(),
                    hidden: d.hidden,
                });
            }
            if overflow {
                diagnostics.push(DiagnosticEntry {
                    locator: index.locator(key, index.diags[visible].offset, index.generation),
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
        let first_page = anchor.is_none();
        // Requests, summary markers and events share one record/byte allowance.
        let allowance = request.max_events.max(1);
        let mut compactions = Vec::new();
        let mut request = request;
        let mut cut = false;
        let started = Instant::now();
        let mut remaining = budget;
        // A finished marker prefix, kept if a page call has to resume.
        let mut marker_page: Option<MarkerPage> = None;
        if first_page {
            let step_before = self
                .indexes
                .get(key)
                .map_or((0, 0), |index| (index.step_bytes, index.step_records));
            if let Some(progress) = self.ensure_index(job, key, budget)? {
                return Ok(ReadProgress::Pending(progress));
            }
            // Indexing done by this call shares the same allowance as the markers and
            // events materialized after it.
            let (step_bytes, step_records) = self
                .indexes
                .get(key)
                .map_or((0, 0), |index| (index.step_bytes, index.step_records));
            let page_budget = RefreshBudget {
                max_records: budget
                    .max_records
                    .max(1)
                    .saturating_sub(step_records.saturating_sub(step_before.1))
                    .max(1),
                max_bytes: budget
                    .max_bytes
                    .max(1)
                    .saturating_sub(step_bytes.saturating_sub(step_before.0))
                    .max(1),
                max_millis: budget.max_millis,
            };
            let version = self.discovered(key)?.harness_version;
            let index = self
                .indexes
                .get_mut(key)
                .ok_or_else(|| ProviderError::Internal("index state vanished".into()))?;
            let markers = index.markers.clone();
            // Marker count and bytes are known before reading them back.
            let (keep, events) = share_details_budget(request, markers.len(), true);
            let skip = markers.len().saturating_sub(keep);
            // Summary bytes come out of the third of the byte allowance markers share.
            let cap = request.max_bytes / 3 / keep.max(1);
            // The selection itself, not merely its length: an append at the cap evicts
            // the oldest marker, so length and `skip` can stay identical while every
            // observation in a stored prefix now names a different position.
            let stale = index
                .marker_page
                .as_ref()
                .is_none_or(|page| page.revision != index.marker_revision || page.skip != skip);
            if stale {
                index.marker_page = Some(MarkerPage {
                    revision: index.marker_revision,
                    skip,
                    built: Vec::new(),
                    read_bytes: 0,
                    records: 0,
                });
            }
            let mut page = index
                .marker_page
                .take()
                .ok_or_else(|| ProviderError::Internal("marker state vanished".into()))?;
            // This slice's own allowance, so a stored prefix costs nothing next call.
            let base_bytes = page.read_bytes;
            let base_records = page.records;
            loop {
                let at = page.skip + page.built.len();
                if at >= page.skip + keep {
                    break;
                }
                let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
                let spent = page.records - base_records >= page_budget.max_records.max(1)
                    || page.read_bytes - base_bytes >= page_budget.max_bytes.max(1)
                    || (elapsed >= budget.max_millis && page.records > base_records);
                if spent {
                    let progress = WorkProgress {
                        records: page.records - base_records,
                        expanded_bytes: page.read_bytes - base_bytes,
                    };
                    index.marker_page = Some(page);
                    self.ledger.join(key, job);
                    return Ok(ReadProgress::Pending(progress));
                }
                let before = index.read_bytes;
                let observation = index.marker(&markers[at], key, &version, page_budget, cap)?;
                page.read_bytes = page.read_bytes.saturating_add(index.read_bytes - before);
                page.records += 1;
                page.built.push(observation);
            }
            let used_bytes = page.read_bytes - base_bytes;
            let used_records = page.records - base_records;
            let elapsed = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
            // A spent marker slice ends the call: the next call resumes the built prefix
            // and then materializes the page events under its own fresh allowance.
            if used_bytes >= page_budget.max_bytes.max(1)
                || used_records >= page_budget.max_records.max(1)
                || (elapsed >= budget.max_millis && used_records > 0)
            {
                let progress = WorkProgress {
                    records: used_records,
                    expanded_bytes: used_bytes,
                };
                index.marker_page = Some(page);
                self.ledger.join(key, job);
                return Ok(ReadProgress::Pending(progress));
            }
            marker_page = Some(MarkerPage {
                skip: page.skip,
                built: std::mem::take(&mut page.built),
                ..page
            });
            // Markers older than the retained set (or evicted from the cap) are cut.
            cut = skip > 0 || index.marker_total > markers.len();
            request = events;
            remaining = RefreshBudget {
                max_records: page_budget
                    .max_records
                    .max(1)
                    .saturating_sub(used_records)
                    .max(1),
                max_bytes: page_budget
                    .max_bytes
                    .max(1)
                    .saturating_sub(used_bytes)
                    .max(1),
                max_millis: budget.max_millis.saturating_sub(elapsed).max(1),
            };
        } else {
            request = share_details_budget(request, 0, true).1;
        }
        let page = match self.page_ready(
            job,
            key,
            anchor,
            PageDirection::Backward,
            request,
            remaining,
        )? {
            ReadProgress::Pending(progress) => {
                // Keep the marker prefix so the next call does not re-read it.
                if let Some(pending) = marker_page
                    && let Some(index) = self.indexes.get_mut(key)
                {
                    index.marker_page = Some(pending);
                }
                return Ok(ReadProgress::Pending(progress));
            }
            ReadProgress::Ready(page) => page,
        };
        if let Some(marker_page) = marker_page {
            compactions = marker_page.built;
        }
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

impl ProviderReader for PiScanner {
    fn harness(&self) -> Harness {
        Harness::Pi
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
        if let Some(index) = self.indexes.get_mut(key) {
            index.marker_page = None;
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
                return Err(ProviderError::Unsupported("not a Pi file locator".into()));
            };
            let meta = self.discovered(key)?;
            if !matches!(&meta.key.source, SourceKey::File(p) if p == path) {
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
            // The index identity above authorizes the record; the reader addresses its
            // own generation of the same physical file.
            let addressed = SourceLocator {
                session: source.session.clone(),
                kind: LocatorKind::FileOffset {
                    path: index.path.clone(),
                    offset: match &source.kind {
                        LocatorKind::FileOffset { offset, .. } => *offset,
                        _ => 0,
                    },
                },
                generation: Some(index.observed),
            };
            match index
                .reader
                .record(&addressed, budget)
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
    fn order_keys_round_trip_and_sort_by_ordinal_then_block() {
        let a = order_of(10, 3).expect("order");
        assert_eq!(split_order(a), (10, 3));
        assert!(order_of(10, 1) < order_of(10, 2));
        assert!(order_of(10, 2) < order_of(11, 0));
        assert!(order_of(MAX_ORDINAL + 1, 0).is_none());
        assert!(order_of(0, 0x1_0000).is_none());
    }

    #[test]
    fn stable_ids_distinguish_repeated_entry_ids() {
        assert_ne!(
            stable_id("e1", order_of(1, 0).unwrap()),
            stable_id("e1", order_of(2, 0).unwrap())
        );
    }
}
