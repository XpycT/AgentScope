//! Codex rollout adapter: discovery-driven roster scanning, usage, and transcripts
//! (spec §7.2).
//!
//! `event_msg.item_completed` is the single source of tool rows. Raw
//! `function_call`/`custom_tool_call` records and their outputs stay source-only
//! (diagnostics) unless a call never got an output (pending) or an output never had
//! a call (orphan). Pages are read straight from the file by byte offset: the
//! offset is the Codex source order and the basis of every [`PageAnchor`].
//! Duplicate session IDs are never merged: every rollout file is its own row.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::{self, File};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::DateTime;
use serde_json::Value;

use super::probe::{HEAD_RECORDS, probe_file, probe_value};

mod metadata;
use super::{
    Admit, DiscoveryDelta, FORGOTTEN_CAP, Forgotten, HydrationDelta, JobLedger, ProviderError,
    ProviderReader, ReadJobId, ReadProgress, Retention, SourceRecord, WorkProgress,
};
use crate::config::Config;
use crate::discovery::{SourceFile, Walk, is_rollout_name};
use crate::domain::{
    ActivityStamp, Actor, AggregationClass, BoundedText, CompactionObservation, CompactionScope,
    CostEstimate, DiagnosticEntry, Event, EventKind, Harness, InputAccounting, Lifecycle, Lineage,
    LocatorKind, ModelDetail, OutputAccounting, PageAnchor, PageDirection, PageRequest,
    ProcessAssociation, Provenance, RecordedMoney, RefreshBudget, ReleaseState, ScanProgress,
    SessionDetailPage, SessionKey, SessionLink, SessionMetadata, SessionMetrics, SessionSummary,
    SourceKey, SourceLocator, SpeedSummary, Timestamp, TokenAccounting, TokenCategories,
    ToolInteraction, ToolStatus, TranscriptPage, UsageCoverage, UsageSummary, Warning, WarningKey,
    WarningKind,
};
use crate::ingest::jsonl::{ContentHash, JsonlCursor, Record};
use crate::ingest::source::{SourceItem, SourceReader, SourceRecordProgress};
use crate::ingest::text::{display_safe, preview, sanitize};
use crate::metrics::{TurnTracker, compaction_summary, context_usage, current_context};
use crate::usage::{Attribution, aggregate_cost, aggregate_summary, codex_total};
use metadata::{
    CodexOwnership, CodexParentEvidence, OwnedUsage, add_origin, codex_origin, codex_parent,
    parse_usage, probe_archive_head,
};

/// Transcript indexes kept at once: every in-progress one is retained so two large live
/// sessions never restart each other; completed ones are dropped first when full.
const MAX_INDEXES: usize = 8;

/// Compaction markers retained per session; more mark the history incomplete.
const MAX_COMPACTIONS: usize = 256;

/// Unpaired calls and orphan outputs tracked per session (each).
const MAX_TRACKED: usize = 256;
/// Superseded call IDs remembered so their late outputs are not orphans.
const MAX_CLOSED: usize = 1024;
/// Records indexed per page/count request, and the most entries (24 bytes each) one
/// session's transcript index may retain.
const INDEX_STEP: usize = 250_000;
const INDEX_CAP: usize = 2_000_000;
/// Consumed prefixes up to this size are re-hashed exactly; larger ones are sampled.
const PREFIX_VERIFY_MAX: u64 = 32 << 20;
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);
/// Largest preview kept for one text field of a page event. `preview` is quadratic
/// in the kept length, and full content is one source-record load away.
const PREVIEW_CAP: usize = 2048;
/// Diagnostics returned (and buffered) per page.
const DIAG_CAP: usize = 200;
const LABEL_CHARS: usize = 64;

/// Open file handles are bounded: only the most recently modified rollouts keep a
/// live cursor. Older files are re-read from the start if they ever change.
const KEEP_CURSORS: usize = 32;
const TITLE_COLS: usize = 80;
const TITLE_BYTES: usize = 1024;
const FIELD_CHARS: usize = 256;

/// Prompts Codex injects on the user's behalf; never used as a title.
const INJECTED_PREFIXES: &[&str] = &[
    "<environment_context",
    "<user_instructions",
    "<instructions",
    "<user_shell_command",
    "<turn_aborted",
    "<permissions",
    "<collaboration_mode",
    "<subagent_notification",
    "<skills_instructions",
    "<apps_instructions",
    "# agents.md",
];

static NULL: Value = Value::Null;

fn field<'a>(v: &'a Value, key: &str) -> &'a Value {
    v.get(key).unwrap_or(&NULL)
}

fn text_field(v: &Value, key: &str) -> Option<String> {
    let s = field(v, key).as_str()?;
    let clean: String = sanitize(s).chars().take(FIELD_CHARS).collect();
    (!clean.trim().is_empty()).then_some(clean)
}

/// Unbounded, unsanitized path text: paths must stay exact; only display text is bounded.
fn path_field(v: &Value, key: &str) -> Option<PathBuf> {
    let s = field(v, key).as_str()?;
    (!s.trim().is_empty()).then(|| PathBuf::from(s))
}

fn parse_ts(v: &Value) -> Option<Timestamp> {
    let parsed = DateTime::parse_from_rfc3339(v.as_str()?).ok()?;
    Some(parsed.timestamp_millis())
}

fn is_injected(text: &str) -> bool {
    let head = text.trim_start().to_ascii_lowercase();
    INJECTED_PREFIXES.iter().any(|p| head.starts_with(p))
}

fn message_text(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn title_from(text: &str) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let title = preview(&collapsed, TITLE_BYTES, TITLE_COLS).preview;
    (!title.trim().is_empty()).then_some(title)
}

/// How one persisted record is treated. Roster counting and transcript paging both
/// go through [`classify`], so displayed/hidden counts always agree with the pages.
enum Class<'a> {
    /// `response_item` message (payload).
    Message(&'a Value),
    /// `response_item` reasoning (payload).
    Reasoning(&'a Value),
    /// `compacted` (payload).
    Compaction(&'a Value),
    Turn {
        started: bool,
    },
    /// `event_msg.item_completed` tool item: the authoritative, single tool row.
    Tool(&'a Value),
    /// `response_item` function/custom call (payload); source-only unless unpaired.
    Call(&'a Value),
    /// `response_item` function/custom output (payload); source-only unless orphaned.
    Output(&'a Value),
    /// `item_completed` of a kind already rendered from `response_item` records.
    Duplicate(&'a str),
    /// Bookkeeping that never renders.
    Hidden(&'a str),
    Unknown(&'a str),
}

fn classify(value: &Value) -> Class<'_> {
    let payload = field(value, "payload");
    match field(value, "type").as_str() {
        Some("response_item") => match field(payload, "type").as_str() {
            Some("message") => Class::Message(payload),
            Some("reasoning") => Class::Reasoning(payload),
            Some("function_call" | "custom_tool_call") => Class::Call(payload),
            Some("function_call_output" | "custom_tool_call_output") => Class::Output(payload),
            Some(other) => Class::Hidden(other),
            None => Class::Hidden("response_item"),
        },
        Some("event_msg") => match field(payload, "type").as_str() {
            Some("task_started") => Class::Turn { started: true },
            Some("task_complete") => Class::Turn { started: false },
            Some("item_completed") => {
                let item = field(payload, "item");
                match field(item, "type").as_str() {
                    Some(
                        "CommandExecution"
                        | "FileChange"
                        | "McpToolCall"
                        | "Extension"
                        | "CollabAgentToolCall",
                    ) => Class::Tool(item),
                    Some(
                        kind @ ("UserMessage" | "AgentMessage" | "Reasoning" | "ContextCompaction"
                        | "SubAgentActivity"),
                    ) => Class::Duplicate(kind),
                    Some(other) => Class::Unknown(other),
                    None => Class::Unknown("item_completed"),
                }
            }
            Some(other) => Class::Hidden(other),
            None => Class::Hidden("event_msg"),
        },
        Some("compacted") => Class::Compaction(payload),
        Some(
            kind @ ("session_meta"
            | "turn_context"
            | "token_usage_record"
            | "world_state"
            | "additional_tools"
            | "inter_agent_communication_metadata"),
        ) => Class::Hidden(kind),
        Some(other) => Class::Unknown(other),
        None => Class::Unknown(""),
    }
}

struct Call {
    id: String,
    offset: u64,
    open: bool,
}

/// Everything learned from one rollout file so far. Usage snapshots are
/// replaced, never accumulated: both Codex sources are cumulative.
#[derive(Default)]
struct Parsed {
    meta_seen: bool,
    id: Option<String>,
    cwd: Option<PathBuf>,
    version: Option<String>,
    parent: Option<String>,
    nickname: Option<String>,
    created_at: Option<Timestamp>,
    /// Recorded launch-surface evidence of the identity header.
    origins: Vec<crate::domain::OriginObservation>,
    /// Full header relationship evidence (immediate parent, root thread, role).
    evidence: CodexParentEvidence,
    /// Child-owned usage boundary and the inherited baseline it subtracts.
    ownership: CodexOwnership,
    /// Part of the copied prefix carried a counter we could not attribute.
    usage_unproven: bool,
    /// Zero-based ordinal of the record `apply` is processing.
    ordinal: u64,
    /// Records consumed from this source (including malformed/oversized lines).
    records: u64,
    models: Vec<String>,
    prompt_title: Option<String>,
    record_usage: Option<TokenCategories>,
    count_usage: Option<TokenCategories>,
    context_window: Option<u64>,
    /// Latest request's prompt occupancy (`last_token_usage.input_tokens`) with its
    /// record time and the compaction epoch it was recorded in.
    last_prompt: Option<(u64, Option<Timestamp>, u64)>,
    turns: TurnTracker,
    /// `(offset, time)` of the newest [`MAX_COMPACTIONS`] `compacted` records.
    compactions: Vec<(u64, Option<Timestamp>)>,
    compaction_total: u64,
    /// `Some(true)` after `task_started`, `Some(false)` after `task_complete`.
    turn_open: Option<bool>,
    last_completion: Option<Timestamp>,
    last_persisted: Option<Timestamp>,
    last_event: Option<Timestamp>,
    displayed: usize,
    hidden: usize,
    malformed: u32,
    oversized: u32,
    unknown: u32,
    /// Byte offset just past the last complete record consumed. Transcript reads
    /// never go beyond it, so pages and counts agree with the roster.
    end: u64,
    /// Calls no completion has claimed yet, oldest first. `open` calls have no output.
    calls: VecDeque<Call>,
    /// Offsets of unpaired calls and orphan outputs, which render as tool rows.
    /// Every offset here was also counted in `hidden` when it was read.
    shown: BTreeSet<u64>,
    /// `end` as of the last delta that emitted this session. A displayed row at an
    /// earlier offset may be loaded by a consumer.
    emitted_end: u64,
    /// A row that was already emitted as displayed was retired or replaced since the
    /// last check; the loaded page is stale. Consumed by the scan loop.
    shown_changed: bool,
    /// Outputs with no call seen.
    orphans: u32,
    /// More calls were open than tracked, so later unmatched outputs are not orphans.
    /// More calls were open than tracked, so later unmatched outputs are not orphans.
    pairing_overflow: bool,
    /// Calls superseded by an `item_completed`; their late output is not an orphan.
    closed: HashSet<String>,
    /// Unique per parse restart; keys the transcript index cache.
    generation: u64,
    /// `(end, hash)` of every consumed byte, from the cursor's running hash.
    prefix: Option<(u64, u64)>,
    /// File identity captured when the scan first stamped this parse.
    identity: Option<FileId>,
    /// Length and modified time observed at the last stamp.
    stamp_meta: Option<(u64, Option<SystemTime>)>,
    /// Sampled `(end, hash)`, only kept for files too large to re-hash.
    fingerprint: Option<(u64, u64)>,
}

impl Parsed {
    fn display(&mut self, ts: Option<Timestamp>) {
        self.displayed += 1;
        self.last_event = self.last_event.max(ts);
    }

    fn window(&mut self, v: &Value) {
        if let Some(w) = field(v, "model_context_window").as_u64() {
            self.context_window = Some(w);
        }
    }

    /// New genuine work after `task_complete` makes the session unfinished again;
    /// `last_completion` keeps the earlier turn's timestamp.
    fn retire_shown(&mut self, offset: u64) {
        if self.shown.remove(&offset) && offset < self.emitted_end {
            self.shown_changed = true;
        }
    }

    fn reopen_turn(&mut self) {
        if self.turn_open == Some(false) {
            self.turn_open = Some(true);
            // The new turn began before (or without) a `task_started`, so its span is
            // not fully recorded: active coverage can no longer be complete.
            self.turns.mark_incomplete();
        }
    }

    fn tool_completed(&mut self, item: &Value, ts: Option<Timestamp>) {
        self.display(ts);
        if !item_pending(item) {
            self.claim_oldest_call();
        }
    }

    fn apply(&mut self, offset: u64, value: &Value) {
        let ts = parse_ts(field(value, "timestamp"));
        self.last_persisted = self.last_persisted.max(ts);
        let payload = field(value, "payload");
        match classify(value) {
            Class::Message(p) => {
                if field(p, "role").as_str() == Some("user") {
                    let text = message_text(field(p, "content"));
                    if !is_injected(&text) {
                        if self.prompt_title.is_none() {
                            self.prompt_title = title_from(&text);
                        }
                        self.reopen_turn();
                    }
                }
                self.display(ts);
            }
            Class::Reasoning(_) => self.display(ts),
            Class::Compaction(_) => {
                self.display(ts);
                self.compaction_total += 1;
                // Keep the newest markers: `latest` must describe the newest event.
                if self.compactions.len() >= MAX_COMPACTIONS {
                    self.compactions.remove(0);
                }
                self.compactions.push((offset, ts));
            }
            Class::Tool(item) => self.tool_completed(item, ts),
            Class::Turn { started } => {
                self.display(ts);
                if started {
                    self.turn_open = Some(true);
                    self.window(payload);
                    self.turns.start(ts);
                } else {
                    self.turn_open = Some(false);
                    self.last_completion = ts;
                    self.turns.end(ts);
                }
            }
            Class::Call(p) => self.call(offset, p, ts),
            Class::Output(p) => self.output(offset, p, ts),
            Class::Duplicate(_) | Class::Hidden(_) => self.hidden += 1,
            Class::Unknown(_) => {
                self.unknown += 1;
                self.hidden += 1;
            }
        }
        match field(value, "type").as_str() {
            Some("session_meta") => self.session_meta(payload, ts),
            Some("turn_context") => {
                if let Some(model) = text_field(payload, "model") {
                    self.models.retain(|m| *m != model);
                    self.models.push(model);
                }
                if self.cwd.is_none() {
                    self.cwd = path_field(payload, "cwd");
                }
            }
            Some("token_usage_record") => {
                // Tolerate the record body at the top level as well as under `payload`.
                let body = if payload.is_null() { value } else { payload };
                self.usage_record(body);
            }
            Some("event_msg") => match field(payload, "type").as_str() {
                Some("token_count") => {
                    let info = field(payload, "info");
                    let raw_total = field(info, "total_token_usage");
                    if let Some(total) = parse_usage(raw_total) {
                        let owned = if self.is_child() {
                            self.owned_categories(self.ordinal, raw_total)
                        } else {
                            Some(total)
                        };
                        if let Some(total) = owned {
                            self.count_usage = Some(total);
                        }
                    }
                    self.window(info);
                    if let Some(prompt) =
                        field(field(info, "last_token_usage"), "input_tokens").as_u64()
                    {
                        self.last_prompt = Some((prompt, ts, self.compaction_total));
                    }
                }
                Some("token_usage_record") => self.usage_record(payload),
                _ => {}
            },
            _ => {}
        }
    }

    fn session_meta(&mut self, p: &Value, ts: Option<Timestamp>) {
        if self.meta_seen {
            return;
        }
        self.meta_seen = true;
        self.id = text_field(p, "id").or_else(|| text_field(p, "session_id"));
        if let Some(cwd) = path_field(p, "cwd") {
            self.cwd = Some(cwd);
        }
        self.version = text_field(p, "cli_version");
        self.created_at = parse_ts(field(p, "timestamp")).or(ts);
        let evidence = codex_parent(p);
        self.parent = evidence.parent_thread.clone();
        self.nickname = evidence.agent_nickname.clone();
        self.ownership =
            CodexOwnership::new(evidence.history_boundary, evidence.boundary_supported);
        self.evidence = evidence;
        if let Some(origin) = codex_origin(p, ts) {
            add_origin(&mut self.origins, origin);
        }
    }

    /// A spawned child or a guardian review, including a header that proves the
    /// spawn only through `source.subagent.thread_spawn`.
    fn is_child(&self) -> bool {
        self.evidence.is_child
    }

    /// A spawned child thread (not a review), the only shape marked as a sidechain.
    fn is_spawned(&self) -> bool {
        self.evidence.is_spawned
    }

    /// A usage record naming a thread other than this rollout is copied evidence, not
    /// this session's spend. Before the declared boundary it may still supply the
    /// inherited baseline; after it, it is ignored.
    fn names_other_thread(&self, body: &Value) -> bool {
        match (text_field(body, "thread_id"), self.id.as_deref()) {
            (Some(named), Some(own)) => named != own,
            _ => false,
        }
    }

    /// The owned cumulative categories of one usage body, when the header proves how
    /// to separate them from inherited copies.
    fn owned_categories(&mut self, ordinal: u64, body: &Value) -> Option<TokenCategories> {
        if self.names_other_thread(body)
            && self
                .ownership
                .first_owned_ordinal
                .is_some_and(|first| ordinal >= first)
        {
            return None;
        }
        match metadata::apply_owned_usage(&mut self.ownership, ordinal, body) {
            OwnedUsage::Delta(delta) => Some(delta.categories),
            OwnedUsage::Snapshot(categories) => Some(categories),
            OwnedUsage::Inherited => None,
            OwnedUsage::Unknown => {
                self.usage_unproven = true;
                None
            }
        }
    }

    /// A call is source-only once its output arrives; until then it renders as a
    /// pending tool row. Pairing uses the call's own `call_id` and never the
    /// `item_completed` ID, which does not match in observed data.
    fn call(&mut self, offset: u64, p: &Value, ts: Option<Timestamp>) {
        self.hidden += 1;
        self.reopen_turn();
        let Some(id) = text_field(p, "call_id") else {
            return;
        };
        if let Some(at) = self.calls.iter().position(|c| c.id == id)
            && let Some(previous) = self.calls.remove(at)
        {
            self.retire_shown(previous.offset);
        }
        while self.calls.len() >= MAX_TRACKED && self.calls.front().is_some_and(|c| !c.open) {
            self.calls.pop_front();
        }
        if self.calls.len() >= MAX_TRACKED {
            self.pairing_overflow = true;
            return;
        }
        self.calls.push_back(Call {
            id,
            offset,
            open: true,
        });
        self.shown.insert(offset);
        self.last_event = self.last_event.max(ts);
    }

    fn output(&mut self, offset: u64, p: &Value, ts: Option<Timestamp>) {
        self.hidden += 1;
        let Some(id) = text_field(p, "call_id") else {
            return;
        };
        if let Some(call) = self.calls.iter_mut().find(|c| c.id == id) {
            // Resolved by its own output. It keeps its place in the queue until an
            // `item_completed` claims it, so completions stay aligned with calls.
            if call.open {
                call.open = false;
                let offset = call.offset;
                self.retire_shown(offset);
            }
        } else if self.closed.remove(&id) {
            // The authoritative completion already replaced this call; stay source-only.
        } else if !self.pairing_overflow && (self.orphans as usize) < MAX_TRACKED {
            self.orphans += 1;
            self.shown.insert(offset);
            self.last_event = self.last_event.max(ts);
        }
    }

    /// An authoritative `item_completed` tool row claims the oldest call no earlier
    /// completion has claimed. Calls already resolved by their raw output are still
    /// claimed (they just have no pending row to retire), so a completion never
    /// consumes a different call that is genuinely still open. IDs never join
    /// (spec 6.3), so completion order is the only evidence.
    fn claim_oldest_call(&mut self) {
        let Some(call) = self.calls.pop_front() else {
            return;
        };
        if !call.open {
            return;
        }
        self.retire_shown(call.offset);
        if self.closed.len() >= MAX_CLOSED {
            self.pairing_overflow = true;
        } else {
            self.closed.insert(call.id);
        }
    }

    fn set_end(&mut self, end: u64, hash: Option<u64>) {
        if self.generation == 0 {
            self.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
        }
        self.end = end;
        self.prefix = hash.map(|h| (end, h));
    }

    /// An archived source is addressed by record ordinal; its generation is the
    /// decoder's, so replacement is detected by the streaming reader itself.
    fn set_archive_end(&mut self, records: u64, generation: u64) {
        self.end = records;
        self.generation = generation;
    }

    fn usage_record(&mut self, body: &Value) {
        self.window(body);
        let ordinal = self.ordinal;
        let current = if self.is_child() {
            self.owned_categories(ordinal, body)
        } else {
            parse_usage(field(body, "thread_token_usage"))
        };
        if let Some(thread) = current {
            self.record_usage = Some(thread);
        }
    }
}

struct FileState {
    source: SourceFile,
    /// Canonical, uncompressed identity the published session key uses.
    canonical: PathBuf,
    /// The source was discovered under the native archived store (user-visible
    /// location state, independent of how its bytes are addressed).
    archived: bool,
    /// Address the source as a compressed stream: expanded ordinals, not byte offsets.
    compressed: bool,
    cursor: Option<JsonlCursor>,
    /// Streaming decoder of a compressed rollout; its continuation is per session.
    archive: Option<SourceReader>,
    parsed: Parsed,
    rotations_seen: u64,
    rotation_warnings: u32,
    reached_eof: bool,
    available: bool,
    /// Already-parsed content was rewritten or rotated; reported by the next `discover`.
    invalidated: bool,
    /// Session ID this state was requested under. Discovery fixes a source's key from
    /// its bounded header probe; hydration must answer to the same key.
    pinned: Option<String>,
    /// Recency tick for bounded eviction.
    used: u64,
}

impl FileState {
    fn new(source: SourceFile, canonical: PathBuf, archived: bool) -> Self {
        let compressed = CodexScanner::compressed(&source.path);
        Self {
            source,
            canonical,
            archived,
            compressed,
            cursor: None,
            archive: None,
            parsed: Parsed::default(),
            rotations_seen: 0,
            rotation_warnings: 0,
            reached_eof: false,
            available: true,
            invalidated: false,
            pinned: None,
            used: 0,
        }
    }

    fn restart(&mut self, source: SourceFile) {
        self.source = source;
        self.cursor = None;
        self.archive = None;
        self.parsed = Parsed::default();
        self.rotations_seen = 0;
        self.reached_eof = false;
    }

    /// Records what the scan consumed (metadata, identity, and for very large files
    /// sampled bytes) so later reads can tell the scanned file from a replacement.
    fn stamp(&mut self) {
        let Ok(meta) = fs::metadata(&self.source.path) else {
            return;
        };
        let p = &mut self.parsed;
        p.stamp_meta = Some((meta.len(), meta.modified().ok()));
        if p.identity.is_none() {
            p.identity = Some(file_id(&meta));
        }
        if p.end > PREFIX_VERIFY_MAX && p.fingerprint.map(|f| f.0) != Some(p.end) {
            p.fingerprint = fingerprint(&self.source.path, p.end).map(|hash| (p.end, hash));
        }
    }

    /// Whether the file on disk still holds the bytes this parse consumed.
    fn source_intact(&self) -> bool {
        let p = &self.parsed;
        if p.end == 0 {
            return true;
        }
        let (Some((end, hash)), Some(identity)) = (p.prefix, p.identity) else {
            return false;
        };
        let Ok(meta) = fs::metadata(&self.source.path) else {
            return false;
        };
        if file_id(&meta) != identity || meta.len() < end {
            return false;
        }
        if p.stamp_meta == Some((meta.len(), meta.modified().ok())) {
            return true;
        }
        // Appended or rewritten since the scan: the consumed prefix must be unchanged.
        if end <= PREFIX_VERIFY_MAX {
            prefix_hash(&self.source.path, end) == Some(hash)
        } else {
            p.fingerprint
                .is_some_and(|(e, h)| e == end && fingerprint(&self.source.path, end) == Some(h))
        }
    }

    fn stem(&self) -> String {
        self.canonical
            .file_stem()
            .map(|s| sanitize(&s.to_string_lossy()))
            .unwrap_or_default()
    }

    fn id(&self) -> String {
        self.pinned
            .clone()
            .or_else(|| self.parsed.id.clone())
            .unwrap_or_else(|| self.stem())
    }

    fn key(&self) -> SessionKey {
        SessionKey {
            harness: Harness::Codex,
            native_id: self.id(),
            source: SourceKey::File(self.canonical.clone()),
        }
    }

    /// Record time when any record carried one, else the file's modified time.
    fn persisted_at(&self) -> Option<Timestamp> {
        self.parsed.last_persisted.or_else(|| {
            let since = self.source.modified.duration_since(UNIX_EPOCH).ok()?;
            i64::try_from(since.as_millis()).ok()
        })
    }

    /// Latest persisted activity: a record timestamp is Reported; the file's
    /// modified time is only a Derived stand-in.
    fn activity(&self) -> ActivityStamp {
        let class = if self.parsed.last_persisted.is_some() {
            AggregationClass::Reported
        } else {
            AggregationClass::Derived
        };
        match self.persisted_at() {
            Some(at) => ActivityStamp {
                at: Some(at),
                class,
            },
            None => ActivityStamp::default(),
        }
    }
}

enum Advance {
    Done,
    Exhausted,
}

struct Meter {
    started: Instant,
    records: usize,
    bytes: usize,
    budget: RefreshBudget,
}

impl Meter {
    fn spent(&self) -> bool {
        self.records >= self.budget.max_records
            || self.bytes >= self.budget.max_bytes
            || self.started.elapsed().as_millis() >= u128::from(self.budget.max_millis)
    }
}

fn warn(path: &Path, label: &str, kind: WarningKind, message: String, count: u32) -> Warning {
    Warning {
        key: WarningKey {
            harness: Harness::Codex,
            source: format!("{}{label}", path.display()),
            kind,
        },
        message,
        count,
    }
}

impl Meter {
    fn new(budget: RefreshBudget) -> Self {
        Self {
            started: Instant::now(),
            records: 0,
            bytes: 0,
            budget,
        }
    }

    fn progress(&self) -> WorkProgress {
        WorkProgress {
            records: self.records,
            expanded_bytes: self.bytes,
        }
    }
}

/// Lightweight facts about one discovered source. The full parse lives in `states`.
struct Known {
    key: SessionKey,
    len: u64,
    modified: SystemTime,
    /// Discovery pass that last saw this source.
    pass: u64,
    /// Latest activity as probed (a tail record time, else the modified time).
    activity: Option<Timestamp>,
    /// The source could be opened; an unreadable one is probed again every pass.
    available: bool,
    /// Native store this physical source belongs to.
    store: usize,
    /// The source sits in the native archived store.
    archived: bool,
    /// Immediate parent thread the header declares, when any.
    parent: Option<String>,
}

/// File names and ID fallback for sources whose header the probe could not read.
fn stem_of(path: &Path) -> String {
    crate::ingest::source::canonical_rollout_identity(path)
        .file_stem()
        .map(|s| sanitize(&s.to_string_lossy()))
        .unwrap_or_default()
}

fn modified_ms(modified: SystemTime) -> Option<Timestamp> {
    let since = modified.duration_since(UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// The session facts shared by discovery probes and hydrated summaries.
fn build_meta(
    p: &Parsed,
    id: String,
    canonical: &Path,
    activity: ActivityStamp,
    available: bool,
    parent: Option<SessionLink>,
    archived: bool,
) -> SessionMetadata {
    let (title, title_derived) = match (&p.nickname, &p.prompt_title) {
        (Some(nick), _) => (title_from(nick).unwrap_or_else(|| id.clone()), false),
        (None, Some(prompt)) => (prompt.clone(), true),
        (None, None) => (id.clone(), false),
    };
    let child = p.is_spawned();
    let fork = p
        .evidence
        .forked_from
        .clone()
        .filter(|_| !child)
        .map(SessionLink::Unresolved);
    SessionMetadata {
        key: SessionKey {
            harness: Harness::Codex,
            native_id: id,
            source: SourceKey::File(canonical.to_path_buf()),
        },
        title,
        title_derived,
        project: p
            .cwd
            .as_deref()
            .and_then(Path::file_name)
            .map(|n| n.to_string_lossy().into_owned()),
        cwd: p.cwd.clone(),
        model: p.models.last().map(|id| ModelDetail {
            id: id.clone(),
            provider: None,
            variant: None,
            effort: None,
            configured_effort: None,
        }),
        origins: p.origins.clone(),
        created_at: p.created_at,
        activity,
        lineage: Lineage {
            parent,
            fork,
            sidechain: child,
            agent_type: p.evidence.agent_role.clone(),
            spawn_depth: p.evidence.spawn_depth,
            ..Lineage::default()
        },
        members: vec![],
        archived,
        released: ReleaseState::Unknown,
        source_available: available,
        harness_version: p.version.clone(),
    }
}

fn limit_warning(cap: usize) -> Warning {
    Warning {
        key: WarningKey {
            harness: Harness::Codex,
            source: "metadata-limit".into(),
            kind: WarningKind::Unsupported,
        },
        message: format!(
            "Discovery limited: only the newest {cap} Codex sources are tracked; \
             raise max_metadata_sources to see more"
        ),
        count: 1,
    }
}

/// A Codex rollout root: the native sessions store, an explicit extra store, or the
/// native archived store. Roots of one native store share a `store` ID, so identical
/// session IDs across stores are never folded together.
#[derive(Clone, Debug)]
struct RootSpec {
    path: PathBuf,
    archived: bool,
    store: usize,
    /// The user named it, so a missing root is reported.
    explicit: bool,
}

/// Discovery output accumulated across one call.
#[derive(Default)]
struct Found {
    sessions: Vec<SessionMetadata>,
    removed: Vec<SessionKey>,
    warnings: Vec<Warning>,
}

pub struct CodexScanner {
    config: Config,
    /// Hydrated (or hydrating) sessions, bounded by `max_cached_sessions`.
    states: HashMap<PathBuf, FileState>,
    walk: Option<Walk>,
    /// Root currently being walked, carried across discovery slices.
    walk_root: Option<RootSpec>,
    /// Rollout roots known to this scanner, in preference order.
    roots: Vec<RootSpec>,
    /// Roots still to be walked in this pass.
    queue: VecDeque<usize>,
    pass_id: u64,
    pass_sources: usize,
    /// Every source the last passes discovered.
    known: HashMap<PathBuf, Known>,
    /// Logical sessions per native ID inside one store, for duplicate detection.
    by_id: HashMap<String, Vec<SessionKey>>,
    /// Published logical key → the physical source currently read for it.
    current: HashMap<SessionKey, PathBuf>,
    /// Logical keys some physical source represented in the running pass.
    seen_keys: HashSet<SessionKey>,
    /// Latest discovery metadata per logical key, so a representation switch can
    /// republish the row without re-reading the source.
    last_meta: HashMap<SessionKey, SessionMetadata>,
    invalidated: Vec<SessionKey>,
    ledger: JobLedger,
    /// Newest-first retention that bounds `known`/`by_id` to the metadata ceiling.
    retention: Retention<PathBuf>,
    /// Evicted sources that may still be frozen into a roster or read by a job.
    forgotten: Forgotten<PathBuf, SessionKey>,
    /// Decoders for archived records, one per `(session, job)`.
    archives: HashMap<(SessionKey, ReadJobId), ArchiveJob>,
    /// Page index of the one session being inspected.
    indexes: HashMap<PathBuf, Option<Index>>,
    index_limits: IndexLimits,
    tick: u64,
}

impl CodexScanner {
    /// Cheap: no filesystem access until the first `discover`.
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        let mut roots = Vec::new();
        let store = 0usize;
        if let Some(root) = config.harnesses.codex.root.clone() {
            roots.push(RootSpec {
                path: root,
                archived: false,
                store,
                explicit: true,
            });
        }
        // The archived store belongs to the same native store as the primary root.
        if let Some(archived) = config.harnesses.codex.archived.clone() {
            roots.push(RootSpec {
                path: archived,
                archived: true,
                store,
                explicit: config.harnesses.codex.archived_explicit,
            });
        }
        for (store, extra) in config.harnesses.codex.extra_roots.iter().enumerate() {
            roots.push(RootSpec {
                path: extra.clone(),
                archived: false,
                store: store + 1,
                explicit: true,
            });
        }
        Ok(Self {
            config: config.clone(),
            states: HashMap::new(),
            walk: None,
            walk_root: None,
            roots,
            queue: VecDeque::new(),
            pass_id: 0,
            pass_sources: 0,
            known: HashMap::new(),
            by_id: HashMap::new(),
            current: HashMap::new(),
            seen_keys: HashSet::new(),
            last_meta: HashMap::new(),
            invalidated: Vec::new(),
            ledger: JobLedger::default(),
            retention: Retention::new(config.max_metadata_sources, config.max_metadata_bytes),
            forgotten: Forgotten::new(FORGOTTEN_CAP),
            archives: HashMap::new(),
            indexes: HashMap::new(),
            index_limits: IndexLimits {
                step: INDEX_STEP,
                cap: INDEX_CAP,
            },
            tick: 0,
        })
    }

    /// Overrides how many records one page/count request indexes and the most one
    /// session may hold. Beyond the cap, transcript requests fail with a clear error.
    pub fn set_index_limits(&mut self, step_records: usize, max_entries: usize) {
        self.index_limits = IndexLimits {
            step: step_records.max(1),
            cap: max_entries,
        };
    }

    /// Number of rollout files currently held open (bounded by `KEEP_CURSORS`).
    pub fn open_cursors(&self) -> usize {
        self.states.values().filter(|s| s.cursor.is_some()).count()
    }

    /// The index slot of `path`, making room by dropping a completed index first.
    fn index_slot<'a>(
        indexes: &'a mut HashMap<PathBuf, Option<Index>>,
        path: &Path,
    ) -> Option<&'a mut Option<Index>> {
        if !indexes.contains_key(path) && indexes.len() >= MAX_INDEXES {
            // Only completed indexes may be dropped; when every slot is a live
            // in-progress index the caller leaves the new job pending instead.
            let victim = indexes
                .iter()
                .find(|(_, i)| i.as_ref().is_none_or(|i| !i.building))
                .map(|(p, _)| p.clone())?;
            indexes.remove(&victim);
        }
        Some(indexes.entry(path.to_path_buf()).or_insert(None))
    }

    /// Sessions with retained hydration state (bounded by `max_cached_sessions`).
    pub fn cached_sessions(&self) -> usize {
        self.states.len()
    }

    /// `(session, job)` pairs still holding partial work, including archive decoders.
    pub fn job_state(&self) -> usize {
        self.ledger.len().max(self.archives.len())
    }

    /// Records and approximate expanded bytes one compressed page job retains
    /// between bounded calls. `None` once the job completed or was cancelled.
    pub fn page_retained(&self, key: &SessionKey, job: ReadJobId) -> Option<(usize, usize)> {
        match self.archives.get(&(key.clone(), job))? {
            ArchiveJob::Page(page) => Some((page.mapped.len(), page.retained_bytes)),
            ArchiveJob::Record(_) => None,
        }
    }

    // ---- discovery -------------------------------------------------------

    fn index_id(&mut self, key: &SessionKey) {
        let id = key.native_id.clone();
        let keys = self.by_id.entry(id.clone()).or_default();
        if !keys.iter().any(|k| k == key) {
            keys.push(key.clone());
        }
        if keys.len() == 2 {
            // The first source now has a duplicate; probe it again so a cached summary
            // is rehydrated with the warning.
            self.mark_stale(&id);
        }
    }

    fn unindex_id(&mut self, key: &SessionKey) {
        if let Some(keys) = self.by_id.get_mut(&key.native_id) {
            keys.retain(|k| k != key);
            if keys.is_empty() {
                self.by_id.remove(&key.native_id);
            } else if keys.len() == 1 {
                self.mark_stale(&key.native_id);
            }
        }
    }

    /// Forces the next pass to probe every source of `id` again.
    fn mark_stale(&mut self, id: &str) {
        let paths: Vec<PathBuf> = self
            .by_id
            .get(id)
            .into_iter()
            .flatten()
            .filter_map(|key| self.current.get(key))
            .cloned()
            .collect();
        for path in paths {
            if let Some(known) = self.known.get_mut(&path) {
                known.len = u64::MAX;
            }
        }
    }

    /// Whether the physical source is a compressed rollout.
    fn compressed(path: &Path) -> bool {
        crate::ingest::source::canonical_rollout_identity(path) != path
    }

    /// Whether the physical source sits in the native archived store.
    fn in_archived_root(&self, path: &Path) -> bool {
        self.roots
            .iter()
            .any(|root| root.archived && path.starts_with(&root.path))
    }

    /// Lower is preferred when two physical sources represent one logical session:
    /// a live store before its archived store, and a plain file before its archive.
    fn rank(&self, path: &Path) -> (u8, u8) {
        (
            u8::from(self.in_archived_root(path)),
            u8::from(Self::compressed(path)),
        )
    }

    /// Native rollout file name without the compression suffix. Two copies of one
    /// rollout (live/archived, plain/zstd) keep this name.
    fn rollout_name(path: &Path) -> Option<std::ffi::OsString> {
        crate::ingest::source::canonical_rollout_identity(path)
            .file_name()
            .map(|n| n.to_os_string())
    }

    /// The canonical identity of a newly discovered physical source. Compression only
    /// strips the native zstd suffix; a live/archived move is folded onto the already
    /// confirmed key of the same native store when the native rollout name and header
    /// ID prove they are one representation.
    fn canonical_for(&self, id: &str, physical: &Path, store: usize, archived: bool) -> PathBuf {
        let plain = crate::ingest::source::canonical_rollout_identity(physical);
        let name = Self::rollout_name(physical);
        let candidates: Vec<&SessionKey> = self
            .by_id
            .get(id)
            .into_iter()
            .flatten()
            .filter(|key| {
                self.current
                    .get(*key)
                    .and_then(|path| self.known.get(path))
                    .is_some_and(|known| known.store == store)
            })
            .collect();
        // In-place compression: the same path without `.zst` was already published,
        // either as the canonical key or as the key's current physical source.
        if let Some(key) = candidates.iter().find(|key| {
            matches!(&key.source, SourceKey::File(path) if path == &plain)
                || self.current.get(*key).is_some_and(|path| path == &plain)
        }) {
            return match &key.source {
                SourceKey::File(path) => path.clone(),
                SourceKey::DbRow { .. } => plain,
            };
        }
        // An archive move keeps the native rollout file name in the same store.
        if let Some(key) = candidates.iter().find(|key| {
            let Some(other) = self.current.get(*key) else {
                return false;
            };
            let Some(known) = self.known.get(other) else {
                return false;
            };
            known.archived != archived && Self::rollout_name(other) == name
        }) {
            return match &key.source {
                SourceKey::File(path) => path.clone(),
                SourceKey::DbRow { .. } => plain,
            };
        }
        plain
    }

    /// Points a logical key at the best physical source seen so far. Returns whether
    /// the key's readable source changed (so retained pages must be rebuilt).
    fn prefer_source(&mut self, key: &SessionKey, path: &Path) -> bool {
        match self.current.get(key) {
            Some(current) if current != path => {
                let better = self.rank(path) < self.rank(current);
                if better {
                    let old = current.clone();
                    self.current.insert(key.clone(), path.to_path_buf());
                    self.drop_state(&old, key);
                    true
                } else {
                    false
                }
            }
            Some(_) => false,
            None => {
                self.current.insert(key.clone(), path.to_path_buf());
                true
            }
        }
    }

    /// Drops a state whose physical source no longer represents the key, unless a job
    /// is still consuming it; an abandoned index and open cursor always go.
    fn drop_state(&mut self, path: &Path, key: &SessionKey) {
        if !self.ledger.has_consumer(key) {
            self.states.remove(path);
        }
        self.indexes.remove(path);
    }

    /// The physical source currently read for `key`.
    fn current_path(&self, key: &SessionKey) -> Option<PathBuf> {
        self.current.get(key).cloned()
    }

    /// Probes one source (bounded header, and a tail only for a plain file) and
    /// records it under its canonical identity.
    fn visit(&mut self, source: SourceFile, root: &RootSpec, bytes: &mut usize, found: &mut Found) {
        if let Some(known) = self.known.get_mut(&source.path)
            && known.available
            && known.len == source.len
            && known.modified == source.modified
        {
            known.pass = self.pass_id;
            let key = known.key.clone();
            self.seen_keys.insert(key);
            return;
        }
        let archived = root.archived || self.in_archived_root(&source.path);
        let max = self.config.max_record_bytes;
        let mut parsed = Parsed::default();
        let mut probe_bytes = 0usize;
        let (available, id, activity) = if Self::compressed(&source.path) {
            match probe_archive_head(&source.path, max) {
                Ok(probe) => {
                    if probe.damaged {
                        found.warnings.push(warn(
                            &source.path,
                            "#damaged",
                            WarningKind::Parse,
                            "compressed source is damaged after its readable prefix".into(),
                            1,
                        ));
                    }
                    let mut activity = None;
                    for (ordinal, value) in probe.records.iter().take(HEAD_RECORDS) {
                        parsed.ordinal = *ordinal;
                        parsed.records = *ordinal + 1;
                        parsed.apply(*ordinal, value);
                        activity = activity.max(parse_ts(field(value, "timestamp")));
                    }
                    probe_bytes = probe.expanded_bytes;
                    let activity = activity.map_or_else(
                        || ActivityStamp {
                            at: modified_ms(source.modified),
                            class: AggregationClass::Derived,
                        },
                        |at| ActivityStamp {
                            at: Some(at),
                            class: AggregationClass::Reported,
                        },
                    );
                    let id = parsed.id.clone().unwrap_or_else(|| stem_of(&source.path));
                    (true, id, activity)
                }
                Err(_) => {
                    found.warnings.push(warn(
                        &source.path,
                        "#unavailable",
                        WarningKind::Unavailable,
                        "source unavailable".into(),
                        1,
                    ));
                    let id = stem_of(&source.path);
                    let activity = ActivityStamp {
                        at: modified_ms(source.modified),
                        class: AggregationClass::Derived,
                    };
                    (false, id, activity)
                }
            }
        } else {
            match probe_file(&source.path, source.len) {
                Ok(probe) => {
                    *bytes = bytes.saturating_add(probe.bytes);
                    for (offset, line) in probe.head.iter().take(HEAD_RECORDS) {
                        if let Some(value) = probe_value(line, max) {
                            parsed.ordinal = parsed.records;
                            parsed.records += 1;
                            parsed.apply(*offset, &value);
                        }
                    }
                    let mut tail_at = None;
                    let mut tail_model = None;
                    for line in &probe.tail {
                        if let Some(value) = probe_value(line, max) {
                            tail_at = tail_at.max(parse_ts(field(&value, "timestamp")));
                            if field(&value, "type").as_str() == Some("turn_context") {
                                tail_model = text_field(field(&value, "payload"), "model");
                            }
                        }
                    }
                    parsed.last_persisted = parsed.last_persisted.max(tail_at);
                    if let Some(model) = tail_model {
                        parsed.models.retain(|m| *m != model);
                        parsed.models.push(model);
                    }
                    let id = parsed.id.clone().unwrap_or_else(|| stem_of(&source.path));
                    let activity = match parsed.last_persisted {
                        Some(at) => ActivityStamp {
                            at: Some(at),
                            class: AggregationClass::Reported,
                        },
                        None => ActivityStamp {
                            at: modified_ms(source.modified),
                            class: AggregationClass::Derived,
                        },
                    };
                    (true, id, activity)
                }
                Err(_) => {
                    found.warnings.push(warn(
                        &source.path,
                        "#unavailable",
                        WarningKind::Unavailable,
                        "source unavailable".into(),
                        1,
                    ));
                    let activity = ActivityStamp {
                        at: modified_ms(source.modified),
                        class: AggregationClass::Derived,
                    };
                    (false, stem_of(&source.path), activity)
                }
            }
        };
        *bytes = bytes.saturating_add(probe_bytes);
        let canonical = self.canonical_for(&id, &source.path, root.store, archived);
        let key = SessionKey {
            harness: Harness::Codex,
            native_id: id,
            source: SourceKey::File(canonical.clone()),
        };
        let parent = parsed
            .parent
            .as_deref()
            .map(|id| self.resolve_parent(id, &key));
        let meta = build_meta(
            &parsed,
            key.native_id.clone(),
            &canonical,
            activity,
            available,
            parent,
            archived,
        );
        let weight = crate::roster::metadata_bytes(&meta) + source.path.as_os_str().len();
        match self.retention.admit(&source.path, meta.activity.at, weight) {
            Admit::Dropped | Admit::Displaced => {
                // Bookkeeping only: a tracked source that no longer fits is forgotten
                // (and watched for deletion), never reported as removed.
                self.forget(&source.path);
                found
                    .warnings
                    .push(limit_warning(self.config.max_metadata_sources));
                return;
            }
            Admit::Kept { evicted } => {
                // Eviction only forgets this adapter's bookkeeping: the files still
                // exist and stay readable, so no removal is reported.
                self.forgotten.remove(&source.path);
                for path in evicted {
                    self.forget(&path);
                    found
                        .warnings
                        .push(limit_warning(self.config.max_metadata_sources));
                }
            }
        }
        if let Some(old) = self.known.get(&source.path)
            && old.key != key
        {
            let old_key = old.key.clone();
            self.unindex_id(&old_key);
            if self.current.get(&old_key) == Some(&source.path) {
                self.current.remove(&old_key);
                found.removed.push(old_key);
            }
        }
        let preferred = self.prefer_source(&key, &source.path);
        if self
            .known
            .get(&source.path)
            .is_none_or(|known| known.key != key)
        {
            self.index_id(&key);
        }
        self.seen_keys.insert(key.clone());
        self.known.insert(
            source.path.clone(),
            Known {
                key: key.clone(),
                len: source.len,
                modified: source.modified,
                pass: self.pass_id,
                activity: meta.activity.at,
                available,
                store: root.store,
                archived,
                parent: parsed.parent.clone(),
            },
        );
        // A child probed before its spawner was published resolves on the next pass:
        // forcing its probe republishes the exact edge instead of a stale null.
        let id = key.native_id.clone();
        let waiting: Vec<PathBuf> = self
            .known
            .iter()
            .filter(|(path, known)| {
                known.parent.as_deref() == Some(id.as_str()) && path.as_path() != source.path
            })
            .map(|(path, _)| path.clone())
            .collect();
        for path in waiting {
            if let Some(known) = self.known.get_mut(&path) {
                known.len = u64::MAX;
            }
        }
        self.last_meta.insert(key.clone(), meta.clone());
        // A non-preferred alias of an already published logical session is bookkeeping
        // only; the row and its selection stay on the preferred physical source.
        if self.current.get(&key) == Some(&source.path) || preferred {
            found.sessions.push(meta);
        }
    }

    /// A pass finished: sources it did not see are gone.
    fn finish_pass(&mut self, found: &mut Found) {
        let gone: Vec<PathBuf> = self
            .known
            .iter()
            .filter(|(_, k)| k.pass != self.pass_id)
            .map(|(p, _)| p.clone())
            .collect();
        for path in gone {
            self.retention.remove(&path);
            let forgotten = self.forgotten.remove(&path);
            if let Some(known) = self.known.remove(&path) {
                let key = known.key.clone();
                if self.seen_keys.contains(&key) {
                    // Another representation of the same logical session was seen:
                    // this physical source is gone, the session is not.
                    if self.current.get(&key) == Some(&path) {
                        self.switch_alias(&key, &path, found);
                    }
                    self.drop_state(&path, &key);
                    continue;
                }
                self.unindex_id(&key);
                self.current.remove(&key);
                found.removed.push(key);
                found.warnings.push(warn(
                    &path,
                    "#removed",
                    WarningKind::Unavailable,
                    "session file is no longer present".into(),
                    1,
                ));
            } else if let Some(key) = forgotten
                && !self.seen_keys.contains(&key)
            {
                self.current.remove(&key);
                found.removed.push(key);
                found.warnings.push(warn(
                    &path,
                    "#removed",
                    WarningKind::Unavailable,
                    "session file is no longer present".into(),
                    1,
                ));
            }
            self.states.remove(&path);
            self.indexes.remove(&path);
        }
        // Forgotten sources are not walked into `known`; a bounded existence check lets a
        // genuine deletion of a frozen or in-flight row reach the roster.
        for path in self.forgotten.keys() {
            match fs::metadata(&path) {
                Ok(_) => {}
                // Only a confirmed absence is a deletion; any other error keeps the watch.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(key) = self.forgotten.remove(&path)
                        && !self.seen_keys.contains(&key)
                    {
                        self.current.remove(&key);
                        found.removed.push(key);
                        found.warnings.push(warn(
                            &path,
                            "#removed",
                            WarningKind::Unavailable,
                            "session file is no longer present".into(),
                            1,
                        ));
                    }
                }
                Err(_) => found.warnings.push(warn(
                    &path,
                    "#unavailable",
                    WarningKind::Unavailable,
                    "source unavailable".into(),
                    1,
                )),
            }
        }
    }

    /// Repoints a key whose preferred physical source disappeared at the best source
    /// that represented it in this pass. The row keeps its identity, so no removal is
    /// published and a loaded selection stays valid; the alias is re-probed on the
    /// next pass so its own metadata replaces the vanished copy's.
    fn switch_alias(&mut self, key: &SessionKey, gone: &Path, found: &mut Found) {
        let mut best: Option<(u8, u8, PathBuf)> = None;
        for (path, known) in &self.known {
            if &known.key != key || known.pass != self.pass_id || path == gone {
                continue;
            }
            let rank = self.rank(path);
            if best.as_ref().is_none_or(|b| rank < (b.0, b.1)) {
                best = Some((rank.0, rank.1, path.clone()));
            }
        }
        if let Some((_, _, path)) = best {
            self.current.insert(key.clone(), path.clone());
            if let Some(known) = self.known.get_mut(&path) {
                known.len = u64::MAX;
            }
            // The row keeps its key; republish it with the new representation's
            // archived state so the location is honest.
            if let Some(mut meta) = self.last_meta.get(key).cloned() {
                meta.archived = self.known.get(&path).is_some_and(|known| known.archived);
                meta.source_available = self.known.get(&path).is_some_and(|known| known.available);
                found.sessions.push(meta);
            }
        }
    }

    // ---- hydration -------------------------------------------------------

    fn source_file(&self, path: &Path) -> SourceFile {
        match fs::metadata(path) {
            Ok(meta) => SourceFile {
                path: path.to_path_buf(),
                modified: meta.modified().unwrap_or(UNIX_EPOCH),
                len: meta.len(),
            },
            // A vanished source never matches the old stamp, so the state restarts
            // and reports the source unavailable.
            Err(_) => SourceFile {
                path: path.to_path_buf(),
                modified: UNIX_EPOCH,
                len: 0,
            },
        }
    }

    /// One bounded slice of scanning `path` under `key`.
    fn step_state(&mut self, path: &Path, key: &SessionKey, meter: &mut Meter) -> Advance {
        self.tick += 1;
        let canonical = match &key.source {
            SourceKey::File(path) => path.clone(),
            SourceKey::DbRow { .. } => path.to_path_buf(),
        };
        let archived =
            self.known.get(path).is_some_and(|known| known.archived) || self.in_archived_root(path);
        if self.states.get(path).is_some_and(|s| {
            s.id() != key.native_id || s.canonical != canonical || s.archived != archived
        }) {
            self.states.remove(path);
        }
        let source = self.source_file(path);
        self.states.entry(path.to_path_buf()).or_insert_with(|| {
            let mut state = FileState::new(source.clone(), canonical, archived);
            state.pinned = Some(key.native_id.clone());
            state
        });
        let advance = self.advance(source, meter);
        if let Some(state) = self.states.get_mut(path) {
            state.used = self.tick;
            if matches!(advance, Advance::Done) {
                state.parsed.emitted_end = state.parsed.end;
                if std::mem::take(&mut state.invalidated) {
                    self.invalidated.push(state.key());
                }
            }
        }
        if matches!(advance, Advance::Done) {
            self.maintain(path);
        }
        advance
    }

    /// Bounds retained states and open cursors after a scan completes.
    fn maintain(&mut self, current: &Path) {
        let cap = self.config.max_cached_sessions.max(1);
        while self.states.len() > cap {
            let victim = self
                .states
                .iter()
                .filter(|(p, s)| p.as_path() != current && !self.ledger.has_consumer(&s.key()))
                .min_by_key(|(_, s)| s.used)
                .map(|(p, _)| p.clone());
            match victim {
                Some(path) => {
                    self.states.remove(&path);
                }
                None => break,
            }
        }
        let mut open: Vec<(u64, PathBuf)> = self
            .states
            .iter()
            .filter(|(_, s)| s.cursor.is_some() && s.reached_eof)
            .map(|(p, s)| (s.used, p.clone()))
            .collect();
        let total = self.states.values().filter(|s| s.cursor.is_some()).count();
        if total > KEEP_CURSORS {
            open.sort_unstable();
            for (_, path) in open.into_iter().take(total - KEEP_CURSORS) {
                if let Some(state) = self.states.get_mut(&path) {
                    state.cursor = None;
                }
            }
        }
    }

    fn context(&self, state: &FileState) -> (usize, Option<SessionLink>) {
        let dup = self.by_id.get(&state.id()).map_or(1, Vec::len);
        let key = state.key();
        let parent = state
            .parsed
            .parent
            .as_deref()
            .map(|id| self.resolve_parent(id, &key));
        (dup, parent)
    }

    /// The most recently persisted logical session with that ID; a tie at the top is
    /// ambiguous. A root-thread ID is never promoted to an immediate parent elsewhere.
    fn resolve_parent(&self, parent_id: &str, child: &SessionKey) -> SessionLink {
        let candidates: Vec<&Known> = self
            .by_id
            .get(parent_id)
            .into_iter()
            .flatten()
            .filter(|key| *key != child)
            .filter_map(|key| self.current.get(key))
            .filter_map(|path| self.known.get(path))
            .collect();
        let Some(newest) = candidates.iter().map(|k| k.activity).max() else {
            return SessionLink::Unresolved(parent_id.to_owned());
        };
        let mut top = candidates.iter().filter(|k| k.activity == newest);
        match (top.next(), top.next()) {
            (Some(only), None) => SessionLink::Resolved(only.key.clone()),
            _ => SessionLink::Ambiguous,
        }
    }

    /// Drops the bookkeeping of `path` but remembers it, bounded, so a frozen row stays
    /// readable and a later physical deletion is still noticed. The logical current
    /// path is kept: a forgotten key must still resolve to the source it names.
    fn forget(&mut self, path: &Path) {
        if let Some(known) = self.known.remove(path) {
            self.unindex_id(&known.key);
            // State an active job is still consuming is not disposed of: only the
            // bookkeeping is forgotten, so a read in flight never restarts.
            if !self.ledger.has_consumer(&known.key) {
                self.states.remove(path);
            }
            self.forgotten.insert(path.to_path_buf(), known.key);
        }
    }

    /// The session ID the discovery probe would assign to the file as it is now.
    fn probed_id(&self, path: &Path) -> Option<String> {
        if Self::compressed(path) {
            let probe = probe_archive_head(path, self.config.max_record_bytes).ok()?;
            let mut parsed = Parsed::default();
            for (ordinal, value) in probe.records.iter().take(HEAD_RECORDS) {
                parsed.ordinal = *ordinal;
                parsed.records = *ordinal + 1;
                parsed.apply(*ordinal, value);
            }
            return Some(parsed.id.unwrap_or_else(|| stem_of(path)));
        }
        let meta = fs::metadata(path).ok()?;
        let probe = probe_file(path, meta.len()).ok()?;
        let mut parsed = Parsed::default();
        for (offset, line) in probe.head.iter().take(HEAD_RECORDS) {
            if let Some(value) = probe_value(line, self.config.max_record_bytes) {
                parsed.ordinal = parsed.records;
                parsed.records += 1;
                parsed.apply(*offset, &value);
            }
        }
        Some(parsed.id.unwrap_or_else(|| stem_of(path)))
    }

    /// An existing rollout file under one of this scanner's configured roots.
    fn allowlisted(&self, path: &Path) -> bool {
        self.roots.iter().any(|root| path.starts_with(&root.path))
            && path
                .file_name()
                .is_some_and(|n| is_rollout_name(&n.to_string_lossy()))
            && fs::metadata(path).is_ok_and(|m| m.is_file())
    }

    /// Sources this adapter currently tracks (bounded by the metadata ceiling).
    pub fn tracked_sources(&self) -> usize {
        self.retention.len()
    }

    /// The physical source currently representing `key`, if discovery has published it.
    /// The canonical key path is never opened as if it had to exist: an archived or
    /// compressed copy renders under the canonical identity but reads its own file.
    fn discovered(&self, key: &SessionKey) -> Result<PathBuf, ProviderError> {
        if key.harness != Harness::Codex {
            return Err(ProviderError::Unsupported("not a Codex session".into()));
        }
        if !matches!(&key.source, SourceKey::File(_)) {
            return Err(ProviderError::Unsupported(
                "not a Codex file session".into(),
            ));
        }
        let path = self
            .current_path(key)
            .ok_or_else(|| ProviderError::Unavailable("unknown Codex session".into()))?;
        match self.known.get(&path) {
            Some(known) if known.key == *key => Ok(path),
            Some(_) => Err(ProviderError::Unavailable("unknown Codex session".into())),
            // Retention may have forgotten a source that still exists (a frozen row or an
            // in-flight read). Only a key this adapter published is honoured, and only
            // while the file still carries that session's identity.
            None if self.forgotten.get(&path) == Some(key)
                && self.allowlisted(&path)
                && self.probed_id(&path).as_deref() == Some(key.native_id.as_str()) =>
            {
                Ok(path)
            }
            None => Err(ProviderError::Unavailable("unknown Codex session".into())),
        }
    }

    /// Makes the session's scan complete, or reports the work this slice did.
    fn prepare(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        budget: RefreshBudget,
    ) -> Result<Option<WorkProgress>, ProviderError> {
        let path = self.discovered(key)?;
        if let Some(state) = self.states.get_mut(&path)
            && state.id() == key.native_id
            && state.reached_eof
        {
            self.tick += 1;
            state.used = self.tick;
            return Ok(None);
        }
        let mut meter = Meter::new(budget);
        self.ledger.join(key, job);
        match self.step_state(&path, key, &mut meter) {
            Advance::Exhausted => Ok(Some(meter.progress())),
            Advance::Done => Ok(None),
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

    fn compaction_observations(&self, state: &FileState) -> Vec<CompactionObservation> {
        let p = &state.parsed;
        let key = state.key();
        p.compactions
            .iter()
            .map(|(offset, at)| CompactionObservation {
                id: format!("codex:{offset}"),
                timestamp: *at,
                method: None,
                trigger: None,
                summary: None,
                before_tokens: None,
                after_tokens: None,
                duration_ms: None,
                usage: None,
                scope: CompactionScope::Event,
                provenance: Provenance {
                    harness: Harness::Codex,
                    source: key.source.clone(),
                    locator: SourceLocator {
                        session: key.clone(),
                        kind: if state.compressed {
                            LocatorKind::CompressedRecord {
                                path: state.source.path.clone(),
                                ordinal: *offset,
                            }
                        } else {
                            LocatorKind::FileOffset {
                                path: state.source.path.clone(),
                                offset: *offset,
                            }
                        },
                        generation: Some(p.generation),
                    },
                    record_type: "compacted".into(),
                    harness_version: p.version.clone(),
                },
            })
            .collect()
    }

    /// One bounded slice of a compressed rollout. The streaming decoder owns the
    /// continuation, so no decompressed history is retained and a replacement
    /// restarts the parse with a new generation.
    fn advance_archive(&mut self, source: SourceFile, meter: &mut Meter) -> Advance {
        let max = self.config.max_record_bytes;
        let Some(state) = self.states.get_mut(&source.path) else {
            return Advance::Done;
        };
        if state.reached_eof
            && state.available
            && state.source.len == source.len
            && state.source.modified == source.modified
        {
            return Advance::Done;
        }
        state.source = source.clone();
        if state.archive.is_none() {
            match SourceReader::open(&source.path, max) {
                Ok(reader) => {
                    // Scanner-owned identity: a rehydrated/cached-away state gets a
                    // fresh generation, so an old compressed locator can never be
                    // accepted against a new decoder that starts at zero.
                    state.parsed.generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
                    state.archive = Some(reader);
                }
                Err(_) => {
                    state.available = false;
                    return Advance::Done;
                }
            }
        }
        loop {
            if meter.spent() {
                state.stamp();
                return Advance::Exhausted;
            }
            let budget = RefreshBudget {
                max_records: meter
                    .budget
                    .max_records
                    .saturating_sub(meter.records)
                    .max(1),
                max_bytes: meter.budget.max_bytes.saturating_sub(meter.bytes).max(1),
                max_millis: meter
                    .budget
                    .max_millis
                    .saturating_sub(meter.started.elapsed().as_millis() as u64)
                    .max(1),
            };
            let Some(reader) = state.archive.as_mut() else {
                return Advance::Done;
            };
            let batch = match reader.step(budget) {
                Ok(batch) => batch,
                Err(_) => {
                    state.available = false;
                    state.archive = None;
                    state.reached_eof = false;
                    return Advance::Done;
                }
            };
            if batch.invalidated {
                // The archive is no longer the one observed: nothing parsed from it
                // can be trusted, its ordinals start over, and every locator from the
                // previous generation is stale.
                let generation = NEXT_GENERATION.fetch_add(1, Ordering::Relaxed);
                state.parsed = Parsed::default();
                state.parsed.generation = generation;
                state.invalidated = true;
                state.reached_eof = false;
            }
            let count = batch.records.len();
            for record in batch.records {
                state.parsed.ordinal = record.ordinal;
                state.parsed.records = record.ordinal + 1;
                match record.item {
                    SourceItem::Json(value) => state.parsed.apply(record.ordinal, &value),
                    SourceItem::Invalid => state.parsed.malformed += 1,
                    SourceItem::TooLarge => state.parsed.oversized += 1,
                    // Damage is source-scoped, reported once by the boundary counter.
                    SourceItem::Damaged(_) => state.rotation_warnings += 1,
                    SourceItem::Incomplete | SourceItem::Skipping => {}
                }
            }
            state
                .parsed
                .set_archive_end(state.parsed.records, state.parsed.generation);
            meter.records = meter.records.saturating_add(count);
            meter.bytes = meter.bytes.saturating_add(batch.expanded_bytes);
            state.available = true;
            if batch.done {
                state.reached_eof = true;
                state.stamp();
                return Advance::Done;
            }
        }
    }

    fn advance(&mut self, source: SourceFile, meter: &mut Meter) -> Advance {
        let compressed = self
            .states
            .get(&source.path)
            .is_some_and(|state| state.compressed)
            || Self::compressed(&source.path);
        if compressed {
            return self.advance_archive(source, meter);
        }
        let max_record_bytes = self.config.max_record_bytes;
        let state = self
            .states
            .entry(source.path.clone())
            .or_insert_with(|| FileState::new(source.clone(), source.path.clone(), false));
        let unchanged = state.source.len == source.len && state.source.modified == source.modified;
        if state.reached_eof && unchanged && state.available {
            return Advance::Done;
        }
        if state.cursor.is_none() && state.reached_eof {
            // The cursor was dropped to bound open files; rebuild from the start.
            // A pure append keeps the consumed prefix intact; a rewrite does not,
            // and loaded pages built from the old bytes are then stale.
            let rewritten = !state.source_intact();
            state.restart(source);
            state.invalidated |= rewritten;
        } else {
            state.source = source;
        }
        if state.cursor.is_some() && !state.source_intact() {
            // The bytes already consumed changed (replaced or rewritten, possibly
            // then grown): nothing parsed from them can be trusted, so start over.
            state.cursor = None;
            state.parsed = Parsed::default();
            state.rotations_seen = 0;
            state.rotation_warnings += 1;
            state.reached_eof = false;
            state.invalidated = true;
        }
        if state.cursor.is_none() {
            match JsonlCursor::open(state.source.path.clone(), max_record_bytes) {
                Ok(cursor) => state.cursor = Some(cursor),
                Err(_) => {
                    state.available = false;
                    return Advance::Done;
                }
            }
        }
        loop {
            let Some(cursor) = state.cursor.as_mut() else {
                return Advance::Done;
            };
            let before = cursor.position();
            let rotations_before = cursor.rotation_count();
            let next = cursor.next_record();
            let after = cursor.position();
            let rotations = cursor.rotation_count();
            // A rotation restarts at 0, so the whole post-reset position is new data;
            // offsets alone cannot tell rotation from an append.
            let consumed = if rotations > rotations_before {
                after
            } else {
                after.saturating_sub(before)
            };
            meter.bytes = meter
                .bytes
                .saturating_add(usize::try_from(consumed).unwrap_or(usize::MAX));
            if rotations > state.rotations_seen {
                // The file was replaced or truncated: restart its parse state.
                state.parsed = Parsed::default();
                state.rotations_seen = rotations;
                state.rotation_warnings += 1;
                state.invalidated = true;
            }
            let record = match next {
                Ok(Some(record)) => record,
                Ok(None) => {
                    state.reached_eof = true;
                    state.stamp();
                    state.available = true;
                    return Advance::Done;
                }
                Err(_) => {
                    state.available = false;
                    // Reopening starts at byte 0, so discard what was parsed so far.
                    state.cursor = None;
                    state.parsed = Parsed::default();
                    state.rotations_seen = 0;
                    state.reached_eof = false;
                    return Advance::Done;
                }
            };
            state.reached_eof = false;
            state.available = true;
            match record {
                // A partial trailing line is held by the cursor until it completes.
                Record::Incomplete { .. } => continue,
                Record::Skipping { .. } => {
                    if meter.spent() {
                        state.stamp();
                        return Advance::Exhausted;
                    }
                    continue;
                }
                Record::Json { offset, ref value } => {
                    state.parsed.ordinal = state.parsed.records;
                    state.parsed.records += 1;
                    state.parsed.apply(offset, value);
                    if std::mem::take(&mut state.parsed.shown_changed) {
                        state.invalidated = true;
                    }
                }
                Record::Invalid { .. } => {
                    state.parsed.records += 1;
                    state.parsed.malformed += 1;
                }
                Record::TooLarge { .. } => {
                    state.parsed.records += 1;
                    state.parsed.oversized += 1;
                }
            }
            let hash = state.cursor.as_ref().map(JsonlCursor::content_hash);
            state.parsed.set_end(after, hash);
            meter.records += 1;
            if meter.spent() {
                state.stamp();
                return Advance::Exhausted;
            }
        }
    }

    fn summarize(&self, state: &FileState) -> SessionSummary {
        let p = &state.parsed;
        let path = &state.source.path;
        let (dup, parent) = self.context(state);
        let mut warnings = Vec::new();
        let mut push = |label: &str, kind, message: &str, count: u32| {
            if count > 0 {
                warnings.push(warn(path, label, kind, message.to_owned(), count));
            }
        };
        push(
            "#malformed",
            WarningKind::Parse,
            "malformed records skipped",
            p.malformed,
        );
        push(
            "#oversized",
            WarningKind::Unsupported,
            "oversized records skipped",
            p.oversized,
        );
        push(
            "#unknown",
            WarningKind::Unsupported,
            "unknown record types",
            p.unknown,
        );
        push(
            "#rotation",
            WarningKind::Io,
            "source file was replaced or truncated",
            state.rotation_warnings,
        );
        push(
            "#orphan-tool",
            WarningKind::Parse,
            "tool results without a matching call",
            p.orphans,
        );
        push(
            "#missing-meta",
            WarningKind::Parse,
            "no session_meta record; using file name",
            u32::from(!p.meta_seen),
        );
        push(
            "#unavailable",
            WarningKind::Unavailable,
            "source unavailable",
            u32::from(!state.available),
        );
        if dup > 1 {
            push(
                "#duplicate-id",
                WarningKind::Unsupported,
                "Duplicate ID: another source shares this session ID",
                u32::try_from(dup).unwrap_or(u32::MAX),
            );
        }
        if Self::usage_boundary_unproven(p) {
            push(
                "#usage-boundary",
                WarningKind::Unsupported,
                "child-owned usage boundary is not evidenced; its usage stays partial",
                1,
            );
        }
        SessionSummary {
            meta: build_meta(
                p,
                state.id(),
                &state.canonical,
                state.activity(),
                state.available,
                parent,
                state.archived,
            ),
            usage: self.usage_summary(p),
            metrics: self.session_metrics(state),
            lifecycle: match p.turn_open {
                None => Lifecycle::Unknown,
                Some(true) => Lifecycle::Ongoing,
                Some(false) => Lifecycle::Completed,
            },
            unfinished_work: p.turn_open == Some(true),
            last_event_at: p.last_event,
            last_completion: p.last_completion,
            displayed_event_count: p.displayed + p.shown.len(),
            hidden_records: p.hidden.saturating_sub(p.shown.len()),
            server_tools: None,
            process: ProcessAssociation::NotChecked,
            warnings,
        }
    }

    /// Measured timing, prompt occupancy and compaction facts. Codex records no
    /// per-request duration (item durations are tool time), so speed stays Unknown.
    fn session_metrics(&self, state: &FileState) -> SessionMetrics {
        let p = &state.parsed;
        let model = p.models.last().map(|id| ModelDetail {
            id: id.clone(),
            provider: None,
            variant: None,
            effort: None,
            configured_effort: None,
        });
        let epoch = format!("c{}", p.compaction_total);
        let snapshot = context_usage(
            p.last_prompt.map(|(used, _, _)| used),
            p.context_window,
            AggregationClass::Reported,
            p.last_prompt.and_then(|(_, at, _)| at),
            model,
            p.last_prompt.map(|(_, _, e)| format!("c{e}")),
        );
        let observed = self.compaction_observations(state);
        SessionMetrics {
            timing: p.turns.timing(p.created_at),
            speed: SpeedSummary::default(),
            context: current_context(&snapshot, Some(&epoch)),
            compactions: compaction_summary(
                &observed,
                p.compaction_total as usize == observed.len(),
                &self.config.pricing,
                self.config.reference_policy(Harness::Codex),
            ),
        }
    }

    /// Whether a child rollout's own usage cannot be separated from its copied
    /// inherited prefix. A boundary at or past the observed end proves no owned
    /// record, so the copied counters must not be reported as this session's spend.
    fn usage_boundary_unproven(p: &Parsed) -> bool {
        p.is_child()
            && (!p.ownership.complete
                || p.usage_unproven
                || p.ownership
                    .first_owned_ordinal
                    .is_some_and(|boundary| boundary >= p.records.max(1)))
    }

    /// `token_usage_record.thread_token_usage` wins; `token_count` is only a
    /// fallback when no record exists. Snapshots are never added together. A child
    /// without a trustworthy owned boundary reports partial usage instead of the
    /// copied parent counters.
    fn usage_summary(&self, p: &Parsed) -> Option<UsageSummary> {
        if Self::usage_boundary_unproven(p) {
            return Some(UsageSummary {
                categories: TokenCategories::default(),
                total: None,
                class: AggregationClass::Unknown,
                cost: CostEstimate::default(),
                reported_cost_usd: None,
                saved_estimate_usd: None,
                coverage: UsageCoverage {
                    observed_requests: 0,
                    priced_requests: 0,
                    complete: false,
                    reason: Some(
                        "child-owned usage before the history boundary is inherited".into(),
                    ),
                },
                models: Vec::new(),
            });
        }
        let categories = p.record_usage.clone().or_else(|| p.count_usage.clone())?;
        let model = p.models.last().map(|id| ModelDetail {
            id: id.clone(),
            provider: None,
            variant: None,
            effort: None,
            configured_effort: None,
        });
        let total = codex_total(&categories);
        let class = match (categories.total, total) {
            (Some(_), _) => AggregationClass::Reported,
            (None, Some(_)) => AggregationClass::Derived,
            (None, None) => AggregationClass::Unknown,
        };
        // Codex reports `cached_input_tokens` inside `input_tokens` and reasoning
        // inside `output_tokens`. Whether cache writes would sit inside input is
        // unverified, so any nonzero write leaves the input accounting unknown.
        let accounting = TokenAccounting {
            input: if categories.cache_write.is_some_and(|w| w > 0) {
                InputAccounting::Unknown
            } else {
                InputAccounting::InclusiveCaches
            },
            output: OutputAccounting::IncludesReasoning,
        };
        let money = RecordedMoney::default();
        // Only a session that saw one model can be priced at that model's rates.
        let attribution = match (p.models.len(), model.as_ref()) {
            (1, Some(m)) => Attribution::Model(m),
            (0, _) | (_, None) => Attribution::Unrecorded,
            _ => Attribution::Mixed,
        };
        let cost = aggregate_cost(
            Harness::Codex,
            attribution,
            &categories,
            accounting,
            &money,
            &self.config.pricing,
            self.config.reference_policy(Harness::Codex),
        );
        Some(aggregate_summary(
            attribution,
            &categories,
            accounting,
            total,
            class,
            cost,
            &money,
        ))
    }
}

// ---------------------------------------------------------------------------
// Transcript reading
// ---------------------------------------------------------------------------

enum Body {
    Json(Value),
    Invalid,
    TooLarge,
}

struct Line {
    offset: u64,
    body: Body,
}

/// Same classification as `JsonlCursor`, so pages and the roster agree on what is
/// malformed or oversized.
fn parse_line(bytes: &[u8], max: usize) -> Body {
    let bytes = bytes.strip_suffix(b"\r").unwrap_or(bytes);
    if bytes.len() > max {
        return Body::TooLarge;
    }
    crate::perf::record_parsed();
    serde_json::from_slice(bytes).map_or(Body::Invalid, Body::Json)
}

fn io_error(error: &io::Error) -> ProviderError {
    ProviderError::Io(error.kind().to_string())
}

/// Reads complete lines forward from `start` up to (never past) `end`. A line longer
/// than the record cap is consumed without being buffered.
struct ForwardLines<R: Read> {
    reader: BufReader<io::Take<R>>,
    offset: u64,
    max: usize,
}

impl ForwardLines<File> {
    fn open(path: &Path, start: u64, end: u64, max: usize) -> io::Result<Self> {
        let mut file = File::open(path)?;
        file.seek(SeekFrom::Start(start))?;
        Ok(Self::new(file, start, end, max))
    }
}

impl<R: Read> ForwardLines<R> {
    /// `source` must already be positioned at `start`.
    fn new(source: R, start: u64, end: u64, max: usize) -> Self {
        Self {
            reader: BufReader::new(source.take(end.saturating_sub(start))),
            offset: start,
            max,
        }
    }

    /// One line's start offset and bytes (`None` when oversized); `None` at the end.
    fn read(&mut self) -> io::Result<Option<(u64, Option<Vec<u8>>)>> {
        let start = self.offset;
        let limit = self.max.saturating_add(1);
        let mut buf = Vec::new();
        let mut over = false;
        let mut any = false;
        loop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                break;
            }
            any = true;
            let newline = available.iter().position(|&b| b == b'\n');
            let data = newline.unwrap_or(available.len());
            if !over {
                if buf.len() + data > limit {
                    over = true;
                    buf = Vec::new();
                } else {
                    buf.extend_from_slice(&available[..data]);
                }
            }
            let consumed = data + usize::from(newline.is_some());
            self.reader.consume(consumed);
            self.offset += consumed as u64;
            crate::perf::bytes_read(consumed);
            if newline.is_some() {
                break;
            }
        }
        Ok(any.then_some((start, (!over).then_some(buf))))
    }

    fn next_line(&mut self) -> io::Result<Option<Line>> {
        let max = self.max;
        Ok(self.read()?.map(|(offset, bytes)| Line {
            offset,
            body: bytes.map_or(Body::TooLarge, |b| parse_line(&b, max)),
        }))
    }
}

fn label_text(s: &str) -> String {
    sanitize(s).chars().take(LABEL_CHARS).collect()
}

fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => (!s.is_empty()).then(|| s.clone()),
        other => Some(other.to_string()),
    }
}

fn text_parts(parts: &[Value]) -> String {
    parts
        .iter()
        .filter_map(|p| p.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn render_message(content: &Value) -> String {
    match content {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| match p.get("text").and_then(Value::as_str) {
                Some(text) => Some(text.to_owned()),
                None => p
                    .get("type")
                    .and_then(Value::as_str)
                    .filter(|t| t.contains("image"))
                    .map(|_| "[image]".to_owned()),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Only the persisted summary; `content`/`raw_content` stay in the source view.
fn reasoning_text(p: &Value) -> String {
    match field(p, "summary") {
        Value::Array(parts) => text_parts(parts),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

fn join_command(v: &Value) -> Option<String> {
    match v {
        Value::Array(parts) => {
            let joined = parts
                .iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ");
            (!joined.is_empty()).then_some(joined)
        }
        other => value_text(other),
    }
}

fn non_empty(parts: &[Option<String>]) -> Option<String> {
    let joined = parts
        .iter()
        .flatten()
        .filter(|s| !s.is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

fn duration_ms(item: &Value) -> Option<i64> {
    let d = field(item, "duration");
    let secs = d.get("secs")?.as_i64()?;
    let nanos = d.get("nanos").and_then(Value::as_i64).unwrap_or(0);
    secs.checked_mul(1000)?.checked_add(nanos / 1_000_000)
}

fn item_pending(item: &Value) -> bool {
    let status = field(item, "status")
        .as_str()
        .map(|s| s.to_ascii_lowercase().replace(['_', '-'], ""));
    matches!(
        status.as_deref(),
        Some("inprogress" | "running" | "pending" | "started")
    )
}

fn tool_status(item: &Value, exit_code: Option<i32>, result_error: bool) -> ToolStatus {
    let status = field(item, "status")
        .as_str()
        .map(|s| s.to_ascii_lowercase().replace(['_', '-'], ""));
    let base = match status.as_deref() {
        Some("completed" | "complete" | "success" | "succeeded" | "ok") => ToolStatus::Completed,
        Some(
            "failed" | "error" | "declined" | "cancelled" | "canceled" | "aborted" | "rejected",
        ) => ToolStatus::Error,
        Some("inprogress" | "running" | "pending" | "started") => ToolStatus::Pending,
        _ => match exit_code {
            Some(0) => ToolStatus::Completed,
            Some(_) => ToolStatus::Error,
            None => ToolStatus::Unknown,
        },
    };
    // A nonzero exit code is the tool's failure, never the session's.
    if base == ToolStatus::Completed && (result_error || exit_code.is_some_and(|c| c != 0)) {
        ToolStatus::Error
    } else {
        base
    }
}

/// Raw strings of one tool item before they are bounded for display.
struct ToolParts {
    name: String,
    input: Option<String>,
    output: Option<String>,
    exit_code: Option<i32>,
    result_error: bool,
}

fn tool_parts(item: &Value) -> ToolParts {
    let exit_code = field(item, "exit_code")
        .as_i64()
        .and_then(|c| i32::try_from(c).ok());
    let mut parts = ToolParts {
        name: String::new(),
        input: None,
        output: None,
        exit_code,
        result_error: false,
    };
    match field(item, "type").as_str() {
        Some("CommandExecution") => {
            parts.name = "command".into();
            parts.input = join_command(field(item, "command"));
            parts.output = value_text(field(item, "aggregated_output"))
                .or_else(|| value_text(field(item, "formatted_output")))
                .or_else(|| {
                    non_empty(&[
                        value_text(field(item, "stdout")),
                        value_text(field(item, "stderr")),
                    ])
                });
        }
        Some("FileChange") => {
            parts.name = "file change".into();
            parts.input = match field(item, "changes") {
                Value::Object(changes) => {
                    let paths = changes.keys().cloned().collect::<Vec<_>>().join("\n");
                    (!paths.is_empty()).then_some(paths)
                }
                other => value_text(other),
            };
            parts.output = non_empty(&[
                value_text(field(item, "stdout")),
                value_text(field(item, "stderr")),
            ]);
        }
        Some("McpToolCall") => {
            let server = text_field(item, "server");
            let tool = text_field(item, "tool");
            parts.name = match (server, tool) {
                (Some(s), Some(t)) => format!("{s}/{t}"),
                (None, Some(t)) => t,
                (Some(s), None) => s,
                (None, None) => "mcp tool".into(),
            };
            parts.input = value_text(field(item, "arguments"));
            let result = field(item, "result");
            parts.result_error = field(result, "isError").as_bool() == Some(true);
            parts.output = match field(result, "content") {
                Value::Array(content) if !text_parts(content).is_empty() => {
                    Some(text_parts(content))
                }
                _ => value_text(result),
            };
        }
        Some("Extension") => {
            parts.name = text_field(item, "kind").unwrap_or_else(|| "extension".into());
            parts.input =
                value_text(field(item, "query")).or_else(|| value_text(field(item, "action")));
            parts.output = value_text(field(item, "results"));
        }
        _ => {
            parts.name = match text_field(item, "tool") {
                Some(tool) => format!("agent {tool}"),
                None => "agent call".into(),
            };
        }
    }
    parts
}

struct Mapper<'a> {
    key: &'a SessionKey,
    path: &'a Path,
    /// The physical source is a compressed stream addressed by expanded ordinal.
    compressed: bool,
    generation: u64,
    version: Option<&'a str>,
    shown: &'a BTreeSet<u64>,
    cap: usize,
}

enum Mapped {
    Event(Box<Event>),
    Diag(Box<DiagnosticEntry>),
}

impl Mapper<'_> {
    fn locator(&self, offset: u64) -> SourceLocator {
        SourceLocator {
            session: self.key.clone(),
            generation: Some(self.generation),
            kind: if self.compressed {
                LocatorKind::CompressedRecord {
                    path: self.path.to_path_buf(),
                    ordinal: offset,
                }
            } else {
                LocatorKind::FileOffset {
                    path: self.path.to_path_buf(),
                    offset,
                }
            },
        }
    }

    /// Sanitized, redacted, then cut. Redaction sees the whole value (already bounded
    /// by the record cap), so a secret longer than the preview cap is never left
    /// half-masked by a cut that removed its closing quote.
    fn bounded(&self, text: &str, locator: &SourceLocator) -> BoundedText {
        let mut bounded = preview(text, self.cap, self.cap);
        bounded.locator = Some(locator.clone());
        bounded
    }

    fn bounded_opt(&self, text: Option<String>, locator: &SourceLocator) -> Option<BoundedText> {
        text.filter(|t| !t.is_empty())
            .map(|t| self.bounded(&t, locator))
    }

    fn map(&self, line: &Line) -> Mapped {
        let offset = line.offset;
        let locator = self.locator(offset);
        let diag = |label: String, hidden: bool| {
            Mapped::Diag(Box::new(DiagnosticEntry {
                locator: locator.clone(),
                label,
                hidden,
            }))
        };
        let value = match &line.body {
            Body::TooLarge => return diag("oversized record".into(), true),
            Body::Invalid => return diag("malformed record".into(), true),
            Body::Json(value) => value,
        };
        let ts = parse_ts(field(value, "timestamp"));
        let event = |kind: EventKind,
                     actor: Actor,
                     text: Option<BoundedText>,
                     tool: Option<ToolInteraction>| {
            let top = field(value, "type").as_str().unwrap_or("");
            let record_type = match field(field(value, "payload"), "type").as_str() {
                Some(inner) => format!("{}/{}", label_text(top), label_text(inner)),
                None => label_text(top),
            };
            Mapped::Event(Box::new(Event {
                id: format!("codex:{offset}"),
                session: self.key.clone(),
                timestamp: ts,
                source_order: offset,
                kind,
                actor,
                text,
                tool,
                usage: None,
                provenance: Provenance {
                    harness: Harness::Codex,
                    source: self.key.source.clone(),
                    locator: locator.clone(),
                    record_type,
                    harness_version: self.version.map(str::to_owned),
                },
            }))
        };
        match classify(value) {
            Class::Message(p) => {
                let role = field(p, "role").as_str().unwrap_or("");
                let raw = render_message(field(p, "content"));
                let injected = role == "user" && is_injected(&raw);
                let (kind, actor) = match role {
                    "user" if !injected => (EventKind::UserPrompt, Actor::User),
                    "assistant" => (EventKind::AssistantMessage, Actor::Assistant),
                    _ => (EventKind::System, Actor::System),
                };
                event(kind, actor, self.bounded_opt(Some(raw), &locator), None)
            }
            Class::Reasoning(p) => event(
                EventKind::Reasoning,
                Actor::Assistant,
                self.bounded_opt(Some(reasoning_text(p)), &locator),
                None,
            ),
            Class::Compaction(p) => event(
                EventKind::Compaction,
                Actor::System,
                self.bounded_opt(value_text(field(p, "message")), &locator),
                None,
            ),
            Class::Turn { started } => event(
                EventKind::Lifecycle,
                Actor::System,
                Some(self.bounded(
                    if started {
                        "Turn started"
                    } else {
                        "Turn completed"
                    },
                    &locator,
                )),
                None,
            ),
            Class::Tool(item) => {
                let tool = self.tool_item(item, field(value, "payload"), ts, &locator);
                event(EventKind::Tool, Actor::Tool, None, Some(tool))
            }
            Class::Call(p) if self.shown.contains(&offset) => {
                let tool = ToolInteraction {
                    name: text_field(p, "name").unwrap_or_else(|| "tool call".into()),
                    call_id: text_field(p, "call_id"),
                    input: self.bounded_opt(
                        value_text(field(p, "arguments")).or_else(|| value_text(field(p, "input"))),
                        &locator,
                    ),
                    output: None,
                    status: ToolStatus::Pending,
                    exit_code: None,
                    started_at: ts,
                    completed_at: None,
                    orphan: false,
                    child_session: None,
                };
                event(EventKind::Tool, Actor::Tool, None, Some(tool))
            }
            Class::Output(p) if self.shown.contains(&offset) => {
                let tool = ToolInteraction {
                    name: "tool result".into(),
                    call_id: text_field(p, "call_id"),
                    input: None,
                    output: self.bounded_opt(value_text(field(p, "output")), &locator),
                    status: ToolStatus::Unknown,
                    exit_code: None,
                    started_at: None,
                    completed_at: ts,
                    orphan: true,
                    child_session: None,
                };
                event(EventKind::Tool, Actor::Tool, None, Some(tool))
            }
            Class::Call(p) | Class::Output(p) => diag(
                format!(
                    "source-only: {}",
                    label_text(field(p, "type").as_str().unwrap_or("tool record"))
                ),
                false,
            ),
            Class::Duplicate(kind) => diag(format!("duplicate item: {}", label_text(kind)), true),
            Class::Hidden(kind) => diag(format!("hidden: {}", label_text(kind)), true),
            Class::Unknown(kind) => diag(format!("unknown record: {}", label_text(kind)), true),
        }
    }

    fn tool_item(
        &self,
        item: &Value,
        payload: &Value,
        ts: Option<Timestamp>,
        locator: &SourceLocator,
    ) -> ToolInteraction {
        let parts = tool_parts(item);
        let status = tool_status(item, parts.exit_code, parts.result_error);
        let duration = duration_ms(item);
        let mut completed_at = field(payload, "completed_at_ms").as_i64().or(ts);
        let mut started_at = field(payload, "started_at_ms").as_i64();
        if started_at.is_none() {
            started_at = completed_at
                .zip(duration)
                .and_then(|(done, ms)| done.checked_sub(ms));
        }
        if status == ToolStatus::Pending {
            completed_at = None;
        }
        ToolInteraction {
            name: label_text(&parts.name),
            call_id: text_field(item, "id"),
            input: self.bounded_opt(parts.input, locator),
            output: self.bounded_opt(parts.output, locator),
            status,
            exit_code: parts.exit_code,
            started_at,
            completed_at,
            orphan: false,
            child_session: None,
        }
    }
}

fn anchor_of(e: &Event) -> PageAnchor {
    PageAnchor {
        timestamp: e.timestamp,
        source_order: e.source_order,
        stable_id: e.id.clone(),
    }
}

/// Bytes a row contributes to the page budget: its bounded previews.
fn event_cost(e: &Event) -> usize {
    let len = |t: &Option<BoundedText>| t.as_ref().map_or(0, |b| b.preview.len());
    len(&e.text)
        + e.tool
            .as_ref()
            .map_or(0, |t| t.name.len() + len(&t.input) + len(&t.output))
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

#[derive(Clone, Copy)]
struct Limits {
    max_events: usize,
    max_bytes: usize,
}

impl Limits {
    fn full(&self, events: &[Event], bytes: usize, next: &Event) -> bool {
        !events.is_empty()
            && (events.len() >= self.max_events || bytes + event_cost(next) > self.max_bytes)
    }
}

/// Shrinks one row so the whole interaction (name, input, output, text) fits
/// `budget` bytes. Previews are already redacted, so cutting them is safe. Returns
/// whether anything was cut; the tool name has no flag of its own, so the caller
/// marks the page truncated.
fn fit(event: &mut Event, budget: usize) -> bool {
    let mut fields: Vec<(&mut String, Option<&mut bool>)> = Vec::new();
    if let Some(text) = event.text.as_mut() {
        fields.push((&mut text.preview, Some(&mut text.truncated)));
    }
    if let Some(tool) = event.tool.as_mut() {
        fields.push((&mut tool.name, None));
        for part in [tool.input.as_mut(), tool.output.as_mut()]
            .into_iter()
            .flatten()
        {
            fields.push((&mut part.preview, Some(&mut part.truncated)));
        }
    }
    let mut cut_any = false;
    let mut left = budget;
    let mut remaining = fields.len();
    for (text, flag) in fields {
        let share = left / remaining;
        remaining -= 1;
        if text.len() > share {
            let mut cut = share;
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
            cut_any = true;
            if let Some(flag) = flag {
                *flag = true;
            }
        }
        left -= text.len();
    }
    cut_any
}

fn empty_page() -> TranscriptPage {
    TranscriptPage {
        events: Vec::new(),
        diagnostics: Vec::new(),
        next: None,
        has_more: false,
        truncated: false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum IxKind {
    /// Always a timeline row.
    Row,
    /// Raw call/output: a row only while unpaired (see `Parsed::shown`).
    Pairable,
    /// Hidden, unknown, malformed, or oversized: diagnostics only.
    Diag,
}

#[derive(Clone, Copy)]
struct Ix {
    ts: Option<Timestamp>,
    offset: u64,
    kind: IxKind,
}

impl Ix {
    /// The `PageAnchor` total order; the stable ID (`codex:<offset>`) is a function
    /// of the offset, so it never breaks a tie the offset has not.
    fn key(&self) -> (Option<Timestamp>, u64) {
        (self.ts, self.offset)
    }

    fn displayed(&self, shown: &BTreeSet<u64>) -> bool {
        match self.kind {
            IxKind::Row => true,
            IxKind::Pairable => shown.contains(&self.offset),
            IxKind::Diag => false,
        }
    }
}

/// Records of one session in `PageAnchor` order. Built lazily for the session being
/// inspected (one slot), a bounded number of records per request, extended on
/// append, rebuilt on replacement.
struct Index {
    path: PathBuf,
    generation: u64,
    /// Plain: consumed bytes. Archived: consumed record count.
    end: u64,
    entries: Vec<Ix>,
    /// The session has more records than the cap allows; no entries are kept.
    over: bool,
    /// Part of the file is not indexed yet; abandoned partial work may be dropped.
    building: bool,
    /// Archive indexing continuation; `None` once the pass completed.
    feed: Option<ArchiveFeed>,
}

/// Streaming index continuation for a compressed source. Ordinals are the anchor
/// order, so no decompressed offset is ever stored.
struct ArchiveFeed {
    reader: SourceReader,
    indexed_until: u64,
}

/// Work a page job retains between bounded calls.
enum ArchiveJob {
    Record(SourceReader),
    Page(ArchivePage),
}

/// One compressed page being collected: the decoder is at `next`, and only the
/// page's own records are retained.
struct ArchivePage {
    reader: SourceReader,
    /// Expanded ordinals to collect, ascending.
    targets: Vec<u64>,
    at: usize,
    mapped: HashMap<u64, Line>,
    /// Approximate expanded size of `mapped`, for bounded-retention diagnostics.
    retained_bytes: usize,
}

enum PageCollect {
    Pending(WorkProgress),
    Ready(MappedLines),
}

impl ArchivePage {
    fn step(&mut self, budget: RefreshBudget) -> Result<PageCollect, ProviderError> {
        if self.at >= self.targets.len() {
            return Ok(PageCollect::Ready(MappedLines {
                lines: std::mem::take(&mut self.mapped),
            }));
        }
        let batch = self
            .reader
            .step(budget)
            .map_err(|e| ProviderError::Io(format!("{:?}", e.kind())))?;
        if batch.invalidated {
            return Err(ProviderError::Unavailable(
                "source changed since the last refresh".into(),
            ));
        }
        let mut records = 0usize;
        for record in batch.records {
            records += 1;
            while self.at < self.targets.len() && self.targets[self.at] < record.ordinal {
                self.at += 1;
            }
            if self.targets.get(self.at) != Some(&record.ordinal) {
                continue;
            }
            let body = match record.item {
                SourceItem::Json(value) => Body::Json(value),
                SourceItem::Invalid => Body::Invalid,
                SourceItem::TooLarge => Body::TooLarge,
                SourceItem::Skipping | SourceItem::Incomplete => continue,
                SourceItem::Damaged(_) => {
                    return Err(ProviderError::Unavailable(
                        "compressed source is damaged".into(),
                    ));
                }
            };
            self.retained_bytes = self.retained_bytes.saturating_add(match &body {
                Body::Json(value) => serde_json::to_vec(value).map_or(0, |v| v.len()),
                Body::Invalid | Body::TooLarge => 0,
            });
            self.mapped.insert(
                record.ordinal,
                Line {
                    offset: record.ordinal,
                    body,
                },
            );
            self.at += 1;
        }
        let bytes = batch.expanded_bytes;
        if self.at >= self.targets.len() || batch.done {
            return Ok(PageCollect::Ready(MappedLines {
                lines: std::mem::take(&mut self.mapped),
            }));
        }
        Ok(PageCollect::Pending(WorkProgress {
            records,
            expanded_bytes: bytes,
        }))
    }
}

#[derive(Clone, Copy)]
struct IndexLimits {
    /// Records indexed per page/count request.
    step: usize,
    /// Most entries one session may hold.
    cap: usize,
}

/// Result of one bounded indexing slice.
enum Indexed<'a> {
    Ready(&'a Index),
    /// More of the file remains to index; call again.
    Building(WorkProgress),
}

fn ensure_index<'a>(
    slot: &'a mut Option<Index>,
    state: &FileState,
    max: usize,
    limits: IndexLimits,
    budget: RefreshBudget,
) -> Result<Indexed<'a>, ProviderError> {
    let parsed = &state.parsed;
    let stale = slot.as_ref().is_none_or(|i| {
        i.path != state.source.path || i.generation != parsed.generation || i.end > parsed.end
    });
    if stale {
        *slot = Some(Index {
            path: state.source.path.clone(),
            generation: parsed.generation,
            end: 0,
            entries: Vec::new(),
            over: false,
            building: false,
            feed: None,
        });
    }
    let too_large = || {
        ProviderError::Unsupported(format!(
            "session transcript exceeds the {}-record index limit",
            limits.cap
        ))
    };
    let index = slot.as_mut().expect("index slot was just filled");
    if index.over {
        return Err(too_large());
    }
    if state.compressed {
        return ensure_archive_index(index, state, max, limits, budget);
    }
    if index.end < parsed.end {
        let started = Instant::now();
        let step = limits.step.min(budget.max_records.max(1));
        let allowance = budget.max_bytes.max(1) as u64;
        let from = index.end;
        let mut lines = ForwardLines::open(&index.path, index.end, parsed.end, max)
            .map_err(|e| io_error(&e))?;
        let mut fresh: Vec<Ix> = Vec::new();
        let mut reached_end = false;
        while fresh.len() < step
            && lines.offset.saturating_sub(from) < allowance
            && (fresh.is_empty() || started.elapsed().as_millis() < u128::from(budget.max_millis))
        {
            let Some(line) = lines.next_line().map_err(|e| io_error(&e))? else {
                reached_end = true;
                break;
            };
            let (ts, kind) = match &line.body {
                Body::Json(value) => (
                    parse_ts(field(value, "timestamp")),
                    match classify(value) {
                        Class::Message(_)
                        | Class::Reasoning(_)
                        | Class::Compaction(_)
                        | Class::Turn { .. }
                        | Class::Tool(_) => IxKind::Row,
                        Class::Call(_) | Class::Output(_) => IxKind::Pairable,
                        Class::Duplicate(_) | Class::Hidden(_) | Class::Unknown(_) => IxKind::Diag,
                    },
                ),
                Body::Invalid | Body::TooLarge => (None, IxKind::Diag),
            };
            fresh.push(Ix {
                ts,
                offset: line.offset,
                kind,
            });
        }
        index.end = if reached_end {
            parsed.end
        } else {
            lines.offset
        };
        if index.entries.len() + fresh.len() > limits.cap {
            index.entries = Vec::new();
            index.over = true;
            return Err(too_large());
        }
        let ordered = fresh.windows(2).all(|w| w[0].key() <= w[1].key())
            && index
                .entries
                .last()
                .zip(fresh.first())
                .is_none_or(|(a, b)| a.key() <= b.key());
        let fresh_count = fresh.len();
        index.entries.extend(fresh);
        if !ordered {
            index.entries.sort_unstable_by_key(Ix::key);
        }
        if index.end < parsed.end {
            index.building = true;
            return Ok(Indexed::Building(WorkProgress {
                records: fresh_count,
                expanded_bytes: usize::try_from(index.end - from).unwrap_or(usize::MAX),
            }));
        }
    }
    index.building = false;
    Ok(Indexed::Ready(index))
}

/// Builds the anchor index of a compressed source by decoding it in bounded
/// slices. The reader continuation lives on the index, so a page request never
/// restarts the whole archive or retains the expanded history.
fn ensure_archive_index<'a>(
    index: &'a mut Index,
    state: &FileState,
    max_record_bytes: usize,
    limits: IndexLimits,
    budget: RefreshBudget,
) -> Result<Indexed<'a>, ProviderError> {
    let parsed = &state.parsed;
    let too_large = || {
        ProviderError::Unsupported(format!(
            "session transcript exceeds the {}-record index limit",
            limits.cap
        ))
    };
    // A completed index already covers the scanned stream and stays reusable until
    // the generation/end moves; reopening it would append duplicate entries.
    if index.feed.is_none() && index.end >= parsed.end {
        index.building = false;
        index.entries.sort_unstable_by_key(Ix::key);
        return Ok(Indexed::Ready(index));
    }
    if index.feed.is_none() {
        let reader = SourceReader::open(&state.source.path, max_record_bytes)
            .map_err(|e| ProviderError::Io(format!("{:?}", e.kind())))?;
        index.feed = Some(ArchiveFeed {
            reader,
            indexed_until: 0,
        });
    }
    let started = Instant::now();
    let step = limits.step.min(budget.max_records.max(1));
    let mut fresh = 0usize;
    let mut bytes = 0usize;
    let mut done = false;
    while fresh < step && started.elapsed().as_millis() < u128::from(budget.max_millis) {
        let Some(feed) = index.feed.as_mut() else {
            done = true;
            break;
        };
        let slice = RefreshBudget {
            max_records: (step - fresh).max(1),
            max_bytes: budget.max_bytes.max(1),
            max_millis: budget
                .max_millis
                .saturating_sub(started.elapsed().as_millis() as u64)
                .max(1),
        };
        let batch = feed
            .reader
            .step(slice)
            .map_err(|e| ProviderError::Io(format!("{:?}", e.kind())))?;
        if batch.invalidated {
            index.entries.clear();
            index.end = 0;
            feed.indexed_until = 0;
            continue;
        }
        bytes = bytes.saturating_add(batch.expanded_bytes);
        for record in batch.records {
            fresh += 1;
            let (ts, kind) = match &record.item {
                SourceItem::Json(value) => match classify(value) {
                    Class::Message(_)
                    | Class::Reasoning(_)
                    | Class::Compaction(_)
                    | Class::Turn { .. }
                    | Class::Tool(_) => (parse_ts(field(value, "timestamp")), IxKind::Row),
                    Class::Call(_) | Class::Output(_) => {
                        (parse_ts(field(value, "timestamp")), IxKind::Pairable)
                    }
                    Class::Duplicate(_) | Class::Hidden(_) | Class::Unknown(_) => {
                        (None, IxKind::Diag)
                    }
                },
                SourceItem::Invalid | SourceItem::TooLarge => (None, IxKind::Diag),
                SourceItem::Skipping | SourceItem::Incomplete | SourceItem::Damaged(_) => continue,
            };
            index.entries.push(Ix {
                ts,
                offset: record.ordinal,
                kind,
            });
            feed.indexed_until = record.ordinal + 1;
            index.end = feed.indexed_until;
        }
        if index.entries.len() > limits.cap {
            index.entries = Vec::new();
            index.over = true;
            return Err(too_large());
        }
        if batch.done {
            index.end = parsed.end.max(feed.indexed_until);
            index.feed = None;
            index.building = false;
            done = true;
            break;
        }
        if fresh >= step || bytes >= budget.max_bytes {
            break;
        }
    }
    if done || index.feed.is_none() {
        index.end = parsed.end.max(index.end);
        index.building = false;
        // Anchors use `(timestamp, ordinal)`; archives are decoded in ordinal order,
        // which need not match when recorded timestamps decrease.
        index.entries.sort_unstable_by_key(Ix::key);
        return Ok(Indexed::Ready(index));
    }
    index.building = true;
    Ok(Indexed::Building(WorkProgress {
        records: fresh,
        expanded_bytes: bytes,
    }))
}

impl Index {
    /// Position of the entry an anchor names; `None` for an anchor from another
    /// generation of the file (or a record that no longer exists).
    fn position(&self, anchor: &PageAnchor) -> Option<usize> {
        let key = (anchor.timestamp, anchor.source_order);
        let i = self.entries.partition_point(|e| e.key() < key);
        let entry = self.entries.get(i)?;
        (entry.key() == key && anchor.stable_id == format!("codex:{}", entry.offset)).then_some(i)
    }
}

/// Random access to records by offset, never past the scanned end.
struct Reader {
    file: File,
    end: u64,
    max: usize,
}

impl Reader {
    fn line(&mut self, offset: u64) -> io::Result<Line> {
        (&self.file).seek(SeekFrom::Start(offset))?;
        ForwardLines::new(&self.file, offset, self.end, self.max)
            .next_line()?
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
    }
}

/// Pages in `PageAnchor` order (timestamp, offset, stable ID). Forward returns the
/// rows after the anchor; backward the rows before it, oldest first. A diagnostic
/// record belongs to the page of the next row after it in that order, or to the
/// last page, so both directions partition diagnostics identically.
/// Sequential or pre-collected access to the records a page names.
trait Lines {
    /// `None` means a diagnostic this page deliberately did not retain.
    fn line(&mut self, offset: u64) -> io::Result<Option<Line>>;
}

impl Lines for Reader {
    fn line(&mut self, offset: u64) -> io::Result<Option<Line>> {
        Reader::line(self, offset).map(Some)
    }
}

/// Records already decoded from an archive, addressed by expanded ordinal.
struct MappedLines {
    lines: HashMap<u64, Line>,
}

impl Lines for MappedLines {
    fn line(&mut self, offset: u64) -> io::Result<Option<Line>> {
        Ok(self.lines.remove(&offset))
    }
}

/// Expanded ordinals a page needs, chosen in the same order [`page`] consumes
/// entries so a compressed source is decoded once for exactly those records.
fn page_targets(
    index: &Index,
    anchor: Option<&PageAnchor>,
    direction: PageDirection,
    limits: Limits,
    shown: &BTreeSet<u64>,
) -> Vec<u64> {
    let at = match anchor {
        Some(anchor) => match index.position(anchor) {
            Some(at) => Some(at),
            None => return Vec::new(),
        },
        None => None,
    };
    let len = index.entries.len();
    let forward = direction == PageDirection::Forward;
    let order: Box<dyn Iterator<Item = usize>> = if forward {
        Box::new(at.map_or(0, |i| i + 1)..len)
    } else {
        Box::new((0..at.unwrap_or(len)).rev())
    };
    let mut out = Vec::new();
    let mut events = 0usize;
    let mut diags = 0usize;
    for i in order {
        let entry = index.entries[i];
        if entry.kind == IxKind::Diag {
            // Mirror `page`: diagnostics before the first backward row and beyond
            // the diagnostic cap are never read, so they are not retained either.
            let keep_diag = forward || anchor.is_none() || events > 0;
            if !keep_diag || diags >= DIAG_CAP {
                continue;
            }
            diags += 1;
            out.push(entry.offset);
            continue;
        }
        out.push(entry.offset);
        if entry.displayed(shown) {
            // `page` maps one event past its record limit to decide `has_more`;
            // that lookahead record must be collected too.
            events += 1;
            if events > limits.max_events {
                break;
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

fn page(
    mapper: &Mapper<'_>,
    index: &Index,
    reader: &mut impl Lines,
    anchor: Option<&PageAnchor>,
    direction: PageDirection,
    limits: Limits,
) -> io::Result<TranscriptPage> {
    let at = match anchor {
        Some(a) => match index.position(a) {
            Some(i) => Some(i),
            None => return Ok(empty_page()),
        },
        None => None,
    };
    let len = index.entries.len();
    let forward = direction == PageDirection::Forward;
    let order: Box<dyn Iterator<Item = usize>> = if forward {
        Box::new(at.map_or(0, |i| i + 1)..len)
    } else {
        Box::new((0..at.unwrap_or(len)).rev())
    };
    let mut events: Vec<Event> = Vec::new();
    let mut diags = Vec::new();
    // Forward: diagnostics waiting for the next row. Backward: the page's own.
    let mut pending = Vec::new();
    let mut truncated = false;
    let mut bytes = 0usize;
    let mut has_more = false;
    for i in order {
        let entry = index.entries[i];
        let keep_diag = forward || anchor.is_none() || !events.is_empty();
        if entry.kind == IxKind::Diag {
            let sink = if forward { &pending } else { &diags };
            if !keep_diag {
                continue;
            }
            if sink.len() >= DIAG_CAP {
                truncated = true;
                continue;
            }
        }
        let Some(line) = reader.line(entry.offset)? else {
            // Diagnostics beyond the retention cap are not collected; page reports
            // the truncation without treating them as records.
            if entry.kind == IxKind::Diag {
                truncated = true;
                continue;
            }
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "collected record is unavailable",
            ));
        };
        match mapper.map(&line) {
            Mapped::Diag(d) => {
                if !keep_diag {
                    continue;
                }
                push_diag(
                    if forward { &mut pending } else { &mut diags },
                    *d,
                    &mut truncated,
                );
            }
            Mapped::Event(event) => {
                let mut event = *event;
                let cut = fit(&mut event, limits.max_bytes);
                if limits.full(&events, bytes, &event) {
                    has_more = true;
                    break;
                }
                if forward {
                    for d in pending.drain(..) {
                        push_diag(&mut diags, d, &mut truncated);
                    }
                }
                bytes += event_cost(&event);
                truncated |= cut;
                events.push(event);
            }
        }
    }
    if forward && !has_more {
        for d in pending {
            push_diag(&mut diags, d, &mut truncated);
        }
    }
    if !forward {
        events.reverse();
        diags.reverse();
    }
    truncated |= events.iter().any(event_truncated);
    let next = if forward {
        events.last()
    } else {
        events.first()
    }
    .map(anchor_of);
    Ok(TranscriptPage {
        events,
        diagnostics: diags,
        next,
        has_more,
        truncated,
    })
}

#[cfg(unix)]
type FileId = (u64, u64);
#[cfg(windows)]
type FileId = u64;
#[cfg(not(any(unix, windows)))]
type FileId = ();

#[cfg(unix)]
fn file_id(meta: &fs::Metadata) -> FileId {
    use std::os::unix::fs::MetadataExt;
    (meta.dev(), meta.ino())
}

#[cfg(windows)]
fn file_id(meta: &fs::Metadata) -> FileId {
    use std::os::windows::fs::MetadataExt;
    meta.creation_time()
}

#[cfg(not(any(unix, windows)))]
fn file_id(_meta: &fs::Metadata) -> FileId {}

/// Exact hash of the first `end` bytes, in the same FNV stream the cursor keeps.
fn prefix_hash(path: &Path, end: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    let mut hash = ContentHash::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut left = end;
    while left > 0 {
        let want = usize::try_from(left.min(buf.len() as u64)).ok()?;
        file.read_exact(&mut buf[..want]).ok()?;
        crate::perf::bytes_read(want);
        hash.update(&buf[..want]);
        left -= want as u64;
    }
    Some(hash.finish())
}

/// Hash of three samples of the consumed bytes plus their length. A file replaced
/// by different content no longer matches; a replacement that only differs between
/// the samples is a documented false negative.
fn fingerprint(path: &Path, end: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    if file.metadata().ok()?.len() < end {
        return None;
    }
    let mut hasher = DefaultHasher::new();
    end.hash(&mut hasher);
    let len = end.min(512);
    for start in [0, (end / 2).min(end - len), end - len] {
        file.seek(SeekFrom::Start(start)).ok()?;
        let mut buf = vec![0u8; usize::try_from(len).ok()?];
        file.read_exact(&mut buf).ok()?;
        crate::perf::bytes_read(buf.len());
        buf.hash(&mut hasher);
    }
    Some(hasher.finish())
}

fn find<'a>(
    states: &'a HashMap<PathBuf, FileState>,
    current: &HashMap<SessionKey, PathBuf>,
    key: &SessionKey,
) -> Result<&'a FileState, ProviderError> {
    if key.harness != Harness::Codex {
        return Err(ProviderError::Unsupported("not a Codex session".into()));
    }
    if !matches!(&key.source, SourceKey::File(_)) {
        return Err(ProviderError::Unsupported(
            "not a Codex file session".into(),
        ));
    }
    let path = current
        .get(key)
        .ok_or_else(|| ProviderError::Unavailable("unknown Codex session".into()))?;
    let state = states
        .get(path)
        .filter(|s| s.key() == *key)
        .ok_or_else(|| ProviderError::Unavailable("unknown Codex session".into()))?;
    if !state.available {
        return Err(ProviderError::Unavailable("source unavailable".into()));
    }
    Ok(state)
}

/// End of the data the roster consumed, provided the file on disk still holds the
/// bytes that were scanned. A replaced or rewritten file must not be read through
/// the old offsets.
fn verified_end(state: &FileState) -> Result<u64, ProviderError> {
    if state.compressed {
        // A compressed source is addressed by record count; its stamp must still be the one
        // the scan observed, because decompressed offsets are never stored.
        let Ok(meta) = fs::metadata(&state.source.path) else {
            return Err(ProviderError::Unavailable(
                "source changed since the last refresh".into(),
            ));
        };
        return if state.parsed.stamp_meta == Some((meta.len(), meta.modified().ok())) {
            Ok(state.parsed.end)
        } else {
            Err(ProviderError::Unavailable(
                "source changed since the last refresh".into(),
            ))
        };
    }
    if state.source_intact() {
        Ok(state.parsed.end)
    } else {
        Err(ProviderError::Unavailable(
            "source changed since the last refresh".into(),
        ))
    }
}

impl CodexScanner {
    fn page_ready(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        request: PageRequest,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
        if let Some(progress) = self.prepare(job, key, budget)? {
            return Ok(ReadProgress::Pending(progress));
        }
        let (compressed, physical, generation, shown, version) = {
            let state = find(&self.states, &self.current, key)?;
            (
                state.compressed,
                state.source.path.clone(),
                state.parsed.generation,
                state.parsed.shown.clone(),
                state.parsed.version.clone(),
            )
        };
        let max = self.config.max_record_bytes;
        let limits = Limits {
            max_events: request.max_events.max(1),
            max_bytes: request.max_bytes,
        };
        if compressed {
            let Some(slot) = Self::index_slot(&mut self.indexes, &physical) else {
                self.ledger.join(key, job);
                return Ok(ReadProgress::Pending(WorkProgress::default()));
            };
            let state = find(&self.states, &self.current, key)?;
            let index = match ensure_index(slot, state, max, self.index_limits, budget)? {
                Indexed::Building(progress) => {
                    self.ledger.join(key, job);
                    return Ok(ReadProgress::Pending(progress));
                }
                Indexed::Ready(index) => index,
            };
            let targets = page_targets(index, anchor.as_ref(), direction, limits, &shown);
            let slot = (key.clone(), job);
            let current = match self.archives.get(&slot) {
                Some(ArchiveJob::Page(page)) => page.targets == targets,
                _ => false,
            };
            if !current {
                let reader = SourceReader::open(&physical, max)
                    .map_err(|e| ProviderError::Io(format!("{:?}", e.kind())))?;
                self.archives.insert(
                    slot.clone(),
                    ArchiveJob::Page(ArchivePage {
                        reader,
                        targets,
                        at: 0,
                        mapped: HashMap::new(),
                        retained_bytes: 0,
                    }),
                );
            }
            let collected = match self.archives.get_mut(&slot) {
                Some(ArchiveJob::Page(page)) => page.step(budget),
                _ => Err(ProviderError::Internal("page job vanished".into())),
            };
            return match collected {
                Ok(PageCollect::Pending(progress)) => {
                    self.ledger.join(key, job);
                    Ok(ReadProgress::Pending(progress))
                }
                Ok(PageCollect::Ready(mut lines)) => {
                    self.archives.remove(&slot);
                    self.ledger.release(key, job);
                    let index = self
                        .indexes
                        .get(&physical)
                        .and_then(Option::as_ref)
                        .ok_or_else(|| ProviderError::Internal("archive index vanished".into()))?;
                    let mapper = Mapper {
                        key,
                        path: &physical,
                        compressed: true,
                        generation,
                        version: version.as_deref(),
                        shown: &shown,
                        cap: request.max_bytes.clamp(1, PREVIEW_CAP),
                    };
                    page(
                        &mapper,
                        index,
                        &mut lines,
                        anchor.as_ref(),
                        direction,
                        limits,
                    )
                    .map(ReadProgress::Ready)
                    .map_err(|e| io_error(&e))
                }
                Err(error) => {
                    self.archives.remove(&slot);
                    self.ledger.release(key, job);
                    Err(error)
                }
            };
        }
        let state = find(&self.states, &self.current, key)?;
        let end = verified_end(state)?;
        let Some(slot) = Self::index_slot(&mut self.indexes, &state.source.path) else {
            self.ledger.join(key, job);
            return Ok(ReadProgress::Pending(WorkProgress::default()));
        };
        let index = match ensure_index(slot, state, max, self.index_limits, budget)? {
            Indexed::Building(progress) => {
                self.ledger.join(key, job);
                return Ok(ReadProgress::Pending(progress));
            }
            Indexed::Ready(index) => index,
        };
        let mapper = Mapper {
            key,
            path: &state.source.path,
            compressed: false,
            generation: state.parsed.generation,
            version: state.parsed.version.as_deref(),
            shown: &state.parsed.shown,
            cap: request.max_bytes.clamp(1, PREVIEW_CAP),
        };
        let mut reader = Reader {
            file: File::open(&state.source.path).map_err(|e| io_error(&e))?,
            end,
            max,
        };
        page(
            &mapper,
            index,
            &mut reader,
            anchor.as_ref(),
            direction,
            limits,
        )
        .map(ReadProgress::Ready)
        .map_err(|e| io_error(&e))
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
        // Compactions and events share one record/byte allowance.
        let mut compactions = Vec::new();
        let mut request = request;
        let mut cut = false;
        if first {
            if let Some(progress) = self.prepare(job, key, budget)? {
                return Ok(ReadProgress::Pending(progress));
            }
            let state = find(&self.states, &self.current, key)?;
            compactions = self.compaction_observations(state);
            let (keep, events) = super::share_details_budget(request, compactions.len(), false);
            if compactions.len() > keep {
                compactions.drain(..compactions.len() - keep);
                cut = true;
            }
            request = events;
        }
        let page =
            match self.page_ready(job, key, anchor, PageDirection::Backward, request, budget)? {
                ReadProgress::Pending(progress) => return Ok(ReadProgress::Pending(progress)),
                ReadProgress::Ready(page) => page,
            };
        let truncated = page.truncated || cut;
        // Codex records cumulative usage snapshots, never per-request ledgers, so no
        // request rows exist to report.
        Ok(ReadProgress::Ready(SessionDetailPage {
            requests: Vec::new(),
            compactions,
            recent: page.events,
            next: page.next,
            has_more: page.has_more,
            truncated,
        }))
    }

    fn file_record(
        &mut self,
        job: ReadJobId,
        locator: &SourceLocator,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        let LocatorKind::FileOffset { path, offset } = &locator.kind else {
            return Err(ProviderError::Unsupported(
                "not a Codex file locator".into(),
            ));
        };
        if let Some(progress) = self.prepare(job, &locator.session, budget)? {
            return Ok(ReadProgress::Pending(progress));
        }
        let state = find(&self.states, &self.current, &locator.session)?;
        if locator.generation != Some(state.parsed.generation) {
            return Err(ProviderError::Unavailable("stale source locator".into()));
        }
        if *path != state.source.path {
            return Err(ProviderError::Unsupported(
                "locator does not belong to this session source".into(),
            ));
        }
        let end = verified_end(state)?;
        if *offset >= end {
            return Err(ProviderError::Unavailable(
                "record is beyond the scanned data".into(),
            ));
        }
        let max = self.config.max_record_bytes;
        let read = || -> io::Result<Option<Option<Vec<u8>>>> {
            if *offset > 0 {
                let mut file = File::open(path)?;
                file.seek(SeekFrom::Start(*offset - 1))?;
                let mut previous = [0u8; 1];
                file.read_exact(&mut previous)?;
                if previous[0] != b'\n' {
                    return Ok(None);
                }
            }
            Ok(ForwardLines::open(path, *offset, end, max)?
                .read()?
                .map(|(_, bytes)| bytes))
        };
        let Some(bytes) = read().map_err(|e| io_error(&e))? else {
            return Err(ProviderError::Parse(
                "locator is not a record boundary".into(),
            ));
        };
        // Same rule as `parse_line`: the cap applies after the CRLF terminator is gone.
        let bytes = bytes.map(|mut b| {
            if b.last() == Some(&b'\r') {
                b.pop();
            }
            b
        });
        let Some(bytes) = bytes.filter(|b| b.len() <= max) else {
            return Ok(ReadProgress::Ready(SourceRecord {
                raw: String::new(),
                display: String::new(),
                unavailable: Some(format!("record exceeds the {max}-byte limit")),
            }));
        };
        let raw = String::from_utf8_lossy(&bytes).into_owned();
        let display = display_safe(&raw);
        Ok(ReadProgress::Ready(SourceRecord {
            raw,
            display,
            unavailable: None,
        }))
    }

    /// A record inside an archived (`.zst`) copy of a rollout under the configured
    /// root. The decoder is per `(session, job)`, so one job's indexing can never
    /// move another's position, and it is dropped the moment the job ends.
    fn archived_record(
        &mut self,
        job: ReadJobId,
        locator: &SourceLocator,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        let LocatorKind::CompressedRecord { path, .. } = &locator.kind else {
            return Err(ProviderError::Unsupported(
                "not a Codex archive locator".into(),
            ));
        };
        let allowed = locator.session.harness == Harness::Codex
            && self.roots.iter().any(|root| path.starts_with(&root.path))
            && path
                .file_name()
                .is_some_and(|n| is_rollout_name(&n.to_string_lossy()))
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("zst"))
            && !path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir));
        if !allowed {
            return Err(ProviderError::Unsupported(
                "archive locator is outside the configured Codex roots".into(),
            ));
        }
        // A discovered session's locator names the physical source and generation
        // that produced it. A replaced archive (a new generation) is stale even
        // though a fresh decoder would start at generation zero.
        let discovered = self
            .current
            .get(&locator.session)
            .and_then(|p| self.known.get(p))
            .is_some_and(|known| known.key == locator.session);
        if discovered {
            if let Some(progress) = self.prepare(job, &locator.session, budget)? {
                return Ok(ReadProgress::Pending(progress));
            }
            let state = find(&self.states, &self.current, &locator.session)?;
            if locator.generation != Some(state.parsed.generation) || *path != state.source.path {
                return Err(ProviderError::Unavailable("stale source locator".into()));
            }
            match fs::metadata(path) {
                Ok(meta) if state.parsed.stamp_meta != Some((meta.len(), meta.modified().ok())) => {
                    return Err(ProviderError::Unavailable("stale source locator".into()));
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(ProviderError::Unavailable("stale source locator".into()));
                }
            }
        }
        let slot = (locator.session.clone(), job);
        if !self.archives.contains_key(&slot) {
            let reader = SourceReader::open(path, self.config.max_record_bytes)
                .map_err(|e| ProviderError::Io(format!("{:?}", e.kind())))?;
            self.archives
                .insert(slot.clone(), ArchiveJob::Record(reader));
        }
        let outcome = match self.archives.get_mut(&slot) {
            Some(ArchiveJob::Record(reader)) => {
                // Any generation the archive already passed was validated against the
                // discovered state above; this decoder starts by observing the same
                // physical file, so its own generation is the baseline.
                let mut physical = locator.clone();
                // The logical key may be a confirmed live identity whose copy moved to
                // the archived store; this decoder addresses the current physical file.
                physical.session.source =
                    SourceKey::File(crate::ingest::source::canonical_rollout_identity(path));
                physical.generation = Some(reader.generation());
                reader.record(&physical, budget)
            }
            Some(ArchiveJob::Page(_)) => {
                return Err(ProviderError::Internal(
                    "archive job is serving a page".into(),
                ));
            }
            None => return Err(ProviderError::Internal("archive reader vanished".into())),
        };
        match outcome {
            Ok(SourceRecordProgress::Pending) => {
                self.ledger.join(&locator.session, job);
                Ok(ReadProgress::Pending(WorkProgress::default()))
            }
            Ok(SourceRecordProgress::Ready(record)) => {
                self.archives.remove(&slot);
                self.ledger.release(&locator.session, job);
                Ok(ReadProgress::Ready(record))
            }
            Ok(SourceRecordProgress::Stale) => {
                self.archives.remove(&slot);
                self.ledger.release(&locator.session, job);
                Err(ProviderError::Unavailable("stale source locator".into()))
            }
            Err(e) => {
                self.archives.remove(&slot);
                self.ledger.release(&locator.session, job);
                Err(ProviderError::Io(format!("{:?}", e.kind())))
            }
        }
    }
}

impl ProviderReader for CodexScanner {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    fn discover(
        &mut self,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        let deadline = Instant::now() + Duration::from_millis(budget.max_millis);
        if self.walk.is_none() && self.queue.is_empty() {
            self.pass_id += 1;
            self.pass_sources = 0;
            self.seen_keys.clear();
            self.queue = (0..self.roots.len()).collect();
        }
        let mut found = Found::default();
        let (mut records, mut bytes) = (0usize, 0usize);
        let mut finished = false;
        let mut root: Option<RootSpec> = self.walk_root.clone();
        loop {
            if self.walk.is_none() {
                let Some(index) = self.queue.pop_front() else {
                    finished = true;
                    break;
                };
                let spec = self.roots[index].clone();
                self.walk = Some(Walk::new_in(
                    Harness::Codex,
                    Some(&spec.path),
                    spec.explicit,
                ));
                self.walk_root = Some(spec.clone());
                root = Some(spec);
            }
            let Some(walk) = self.walk.as_mut() else {
                break;
            };
            let batch = walk.step(1, deadline);
            found.warnings.extend(walk.take_warnings());
            let done = walk.is_done();
            if done {
                self.walk = None;
                self.walk_root = None;
                finished = self.queue.is_empty();
            }
            for source in batch {
                self.pass_sources += 1;
                records += 1;
                if let Some(root) = root.as_ref() {
                    self.visit(source, root, &mut bytes, &mut found);
                }
            }
            if finished
                || records >= budget.max_records
                || bytes >= budget.max_bytes
                || Instant::now() >= deadline
            {
                break;
            }
        }
        if finished {
            self.finish_pass(&mut found);
        }
        Ok(DiscoveryDelta {
            sessions: found.sessions,
            removed: found.removed,
            invalidated: std::mem::take(&mut self.invalidated),
            warnings: found.warnings,
            scan: ScanProgress {
                harness: Harness::Codex,
                scanned: self.pass_sources,
                total: finished.then_some(self.pass_sources),
                done: finished,
            },
        })
    }

    fn hydrate(
        &mut self,
        job: ReadJobId,
        key: &SessionKey,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        let path = self.discovered(key)?;
        let mut meter = Meter::new(budget);
        self.ledger.join(key, job);
        let advance = self.step_state(&path, key, &mut meter);
        let Some(state) = self.states.get(&path) else {
            self.ledger.release(key, job);
            return Err(ProviderError::Internal("hydration state vanished".into()));
        };
        match advance {
            Advance::Exhausted => {
                let p = &state.parsed;
                let meta = build_meta(
                    p,
                    state.id(),
                    &state.canonical,
                    state.activity(),
                    state.available,
                    p.parent.clone().map(SessionLink::Unresolved),
                    state.archived,
                );
                Ok(HydrationDelta {
                    summary: SessionSummary::unhydrated(meta),
                    done: false,
                })
            }
            Advance::Done => {
                let summary = self.summarize(state);
                self.ledger.release(key, job);
                Ok(HydrationDelta {
                    summary,
                    done: true,
                })
            }
        }
    }

    fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId) {
        self.archives.remove(&(key.clone(), job));
        if !self.ledger.release(key, job) {
            return;
        }
        // The last consumer is gone: abandoned partial work goes, healthy state stays.
        let Some(path) = self.current_path(key) else {
            return;
        };
        if self
            .states
            .get(&path)
            .is_some_and(|s| !s.reached_eof && s.id() == key.native_id)
        {
            self.states.remove(&path);
        }
        if self
            .indexes
            .get(&path)
            .and_then(Option::as_ref)
            .is_some_and(|i| i.building)
        {
            self.indexes.remove(&path);
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
            if let Some(progress) = self.prepare(job, key, budget)? {
                return Ok(ReadProgress::Pending(progress));
            }
            let state = find(&self.states, &self.current, key)?;
            verified_end(state)?;
            let max = self.config.max_record_bytes;
            let Some(slot) = Self::index_slot(&mut self.indexes, &state.source.path) else {
                self.ledger.join(key, job);
                return Ok(ReadProgress::Pending(WorkProgress::default()));
            };
            let index = match ensure_index(slot, state, max, self.index_limits, budget)? {
                Indexed::Building(progress) => {
                    self.ledger.join(key, job);
                    return Ok(ReadProgress::Pending(progress));
                }
                Indexed::Ready(index) => index,
            };
            let at = (anchor.timestamp, anchor.source_order);
            let from = index.entries.partition_point(|e| e.key() <= at);
            Ok(ReadProgress::Ready(
                index.entries[from..]
                    .iter()
                    .filter(|e| e.displayed(&state.parsed.shown))
                    .count(),
            ))
        })();
        self.settle(key, job, &result);
        result
    }

    fn source_record(
        &mut self,
        job: ReadJobId,
        locator: &SourceLocator,
        budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        let result = match &locator.kind {
            LocatorKind::CompressedRecord { .. } => self.archived_record(job, locator, budget),
            _ => self.file_record(job, locator, budget),
        };
        self.settle(&locator.session, job, &result);
        result
    }
}
