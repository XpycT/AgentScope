//! Claude project discovery, roster scanning and persisted content mapping.
//! Usage aggregation is independent from transcript paging.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use chrono::DateTime;
use serde_json::Value;

use crate::config::Config;
use crate::discovery::{SourceFile, Walk, classify_claude, desktop_stores};
use crate::domain::*;
use crate::ingest::jsonl::{ContentHash, JsonlCursor, Record};
use crate::ingest::text::{display_safe, preview};
use crate::metrics::{TurnTracker, compaction_summary, context_usage, current_context};
use crate::providers::probe::HEAD_RECORDS;
use crate::providers::{
    Admit, DiscoveryDelta, FORGOTTEN_CAP, Forgotten, HydrationDelta, JobLedger, ProviderError,
    ProviderReader, ReadJobId, ReadProgress, Retention, SourceRecord, WorkProgress,
};
use crate::usage::ClaudeLedger;

mod children;
mod metadata;
use children::{ChildState, ClaudeChildId, main_path};
use metadata::{
    COWORK_ORIGIN, DesktopManifest, Overlay, add_origin, bare_state, probe_claude_metadata,
    probe_metadata, read_manifest, record_origin,
};

/// Records a discovery probe parses per source: the leading records and as many
/// trailing ones as fit (the newest matter most).
const PROBE_RECORDS: usize = HEAD_RECORDS * 2;
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

fn safe(s: &str) -> BoundedText {
    preview(s, 2048, 2048)
}
fn title(s: &str) -> Option<String> {
    let clean = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let result = preview(&clean, 1024, 80).preview;
    (!result.trim().is_empty()).then_some(result)
}
fn injected(s: &str) -> bool {
    let s = s.trim_start().to_ascii_lowercase();
    [
        "<environment_context",
        "<user_instructions",
        "<instructions",
        "<system-reminder",
        "<command-name",
        "<local-command",
        "<user_shell_command",
        "# agents.md",
    ]
    .iter()
    .any(|p| s.starts_with(p))
}
fn timestamp(v: &Value) -> Option<Timestamp> {
    DateTime::parse_from_rfc3339(v.as_str()?)
        .ok()
        .map(|v| v.timestamp_millis())
}
fn string(v: &Value, key: &str) -> Option<String> {
    v.get(key)?
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .map(str::to_owned)
}
fn usage(
    message: &Value,
    provenance: &Provenance,
    at: Option<Timestamp>,
) -> Option<UsageObservation> {
    let u = message.get("usage")?.as_object()?;
    let get = |name: &str| u.get(name).and_then(Value::as_u64);
    let split = u.get("cache_creation");
    let model = string(message, "model").map(|v| safe(&v).preview);
    Some(UsageObservation {
        model: model.map(|id| ModelDetail {
            id,
            provider: None,
            variant: None,
            effort: None,
            configured_effort: None,
        }),
        scope: UsageScope::Message,
        categories: TokenCategories {
            input: get("input_tokens"),
            cache_write: get("cache_creation_input_tokens"),
            cache_read: get("cache_read_input_tokens"),
            output: get("output_tokens"),
            cache_write_5m: split
                .and_then(|v| v.get("ephemeral_5m_input_tokens"))
                .and_then(Value::as_u64),
            cache_write_1h: split
                .and_then(|v| v.get("ephemeral_1h_input_tokens"))
                .and_then(Value::as_u64),
            reasoning: u
                .get("output_tokens_details")
                .and_then(|v| v.get("thinking_tokens"))
                .and_then(Value::as_u64),
            ..Default::default()
        },
        // Claude Messages usage reports cache counters next to `input_tokens`,
        // and thinking inside `output_tokens`.
        accounting: TokenAccounting {
            input: InputAccounting::AdditiveCaches,
            output: OutputAccounting::IncludesReasoning,
        },
        request: Some(RequestMetadata {
            id: string(message, "id"),
            completed_at: at,
            service_tier: u
                .get("service_tier")
                .and_then(Value::as_str)
                .map(|t| safe(t).preview),
            ..RequestMetadata::default()
        }),
        money: RecordedMoney::default(),
        provenance: provenance.clone(),
    })
}
fn locator(key: &SessionKey, offset: u64) -> SourceLocator {
    let path = match &key.source {
        SourceKey::File(path) => path.clone(),
        _ => PathBuf::new(),
    };
    SourceLocator {
        session: key.clone(),
        generation: None,
        kind: LocatorKind::FileOffset { path, offset },
    }
}
fn content_text(v: &Value) -> (String, usize) {
    match v {
        Value::String(s) => (s.clone(), 0),
        Value::Array(a) => {
            let mut hidden = 0;
            let text = a
                .iter()
                .filter_map(|p| match p.get("type").and_then(Value::as_str) {
                    Some("text") => p.get("text").and_then(Value::as_str),
                    Some("image") => Some("[image]"),
                    _ => {
                        hidden += 1;
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
            (text, hidden)
        }
        _ => (String::new(), 0),
    }
}

/// Mapping result, with auxiliary records counted rather than displayed.
pub struct ClaudeTranscript {
    pub events: Vec<Event>,
    pub hidden_records: usize,
}

/// Map records with their original JSONL byte offsets.
pub fn map_transcript(key: &SessionKey, records: &[(u64, Value)]) -> ClaudeTranscript {
    let indexed: Vec<_> = records
        .iter()
        .map(|(offset, value)| (*offset, value))
        .collect();
    map_indexed(key, &indexed)
}

fn map_indexed(key: &SessionKey, records: &[(u64, &Value)]) -> ClaudeTranscript {
    let mut events: Vec<Event> = Vec::new();
    let mut calls: HashMap<String, usize> = HashMap::new();
    let mut hidden = 0;
    let mut latest = HashMap::new();
    for (offset, record) in records {
        if let Some(uuid) = record.get("uuid").and_then(Value::as_str) {
            let count = match &record["message"]["content"] {
                Value::Array(parts) => parts.len(),
                Value::String(_) => 1,
                _ => 0,
            };
            for index in 0..count {
                latest.insert((uuid, index), *offset);
            }
        }
    }
    let mut observed_usage = HashSet::new();
    for (offset, record) in records {
        let uuid = record.get("uuid").and_then(Value::as_str);
        let first_event = events.len();
        let kind = record.get("type").and_then(Value::as_str).unwrap_or("");
        let loc = locator(key, *offset);
        let prov = Provenance {
            harness: Harness::Claude,
            source: key.source.clone(),
            locator: loc.clone(),
            record_type: kind.into(),
            harness_version: string(record, "version").map(|v| safe(&v).preview),
        };
        let ts = record.get("timestamp").and_then(timestamp);
        let message = &record["message"];
        let assistant_record = kind == "assistant";
        let emit = |events: &mut Vec<Event>,
                    index: usize,
                    kind: EventKind,
                    actor: Actor,
                    body: Option<BoundedText>,
                    tool: Option<ToolInteraction>| {
            events.push(Event {
                id: format!(
                    "{}:{}:{}",
                    record
                        .get("uuid")
                        .and_then(Value::as_str)
                        .unwrap_or("record"),
                    offset,
                    index
                ),
                session: key.clone(),
                timestamp: ts,
                source_order: *offset,
                kind,
                actor,
                text: body,
                tool,
                usage: None,
                provenance: prov.clone(),
            });
        };
        if kind == "system" {
            let body = record
                .get("content")
                .and_then(Value::as_str)
                .or_else(|| record.get("subtype").and_then(Value::as_str))
                .unwrap_or("system");
            emit(
                &mut events,
                0,
                EventKind::System,
                Actor::System,
                Some(safe(body)),
                None,
            );
            continue;
        }
        if kind != "user" && kind != "assistant" {
            hidden += 1;
            continue;
        }
        let content = &message["content"];
        let text_part;
        let parts: &[Value] = match content {
            Value::Array(parts) => parts,
            Value::String(s) => {
                text_part = serde_json::json!({"type":"text","text":s});
                std::slice::from_ref(&text_part)
            }
            _ => &[],
        };
        for (index, part) in parts.iter().enumerate() {
            if uuid.is_some_and(|uuid| latest.get(&(uuid, index)) != Some(offset)) {
                continue;
            }
            let typ = part.get("type").and_then(Value::as_str).unwrap_or("");
            match typ {
                "text" => {
                    let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                    emit(
                        &mut events,
                        index,
                        if kind == "user" {
                            EventKind::UserPrompt
                        } else {
                            EventKind::AssistantMessage
                        },
                        if kind == "user" {
                            Actor::User
                        } else {
                            Actor::Assistant
                        },
                        Some(safe(text)),
                        None,
                    );
                }
                "thinking" if kind == "assistant" => emit(
                    &mut events,
                    index,
                    EventKind::Reasoning,
                    Actor::Assistant,
                    Some(safe(
                        part.get("thinking").and_then(Value::as_str).unwrap_or(""),
                    )),
                    None,
                ),
                "image" => emit(
                    &mut events,
                    index,
                    if kind == "user" {
                        EventKind::UserPrompt
                    } else {
                        EventKind::AssistantMessage
                    },
                    if kind == "user" {
                        Actor::User
                    } else {
                        Actor::Assistant
                    },
                    Some(safe("[image]")),
                    None,
                ),
                "tool_use" if kind == "assistant" => {
                    let id = string(part, "id");
                    let tool = ToolInteraction {
                        name: safe(part.get("name").and_then(Value::as_str).unwrap_or("tool"))
                            .preview,
                        call_id: id.clone(),
                        input: part.get("input").map(|v| safe(&v.to_string())),
                        output: None,
                        status: ToolStatus::Pending,
                        exit_code: None,
                        started_at: ts,
                        completed_at: None,
                        orphan: false,
                        child_session: None,
                    };
                    if let Some(id) = id {
                        calls.insert(id, events.len());
                    }
                    emit(
                        &mut events,
                        index,
                        EventKind::Tool,
                        Actor::Tool,
                        None,
                        Some(tool),
                    );
                }
                "tool_result" if kind == "user" => {
                    let id = string(part, "tool_use_id");
                    let (content, unknown) = content_text(&part["content"]);
                    hidden += unknown;
                    let output = safe(&content);
                    let status = if part.get("is_error").and_then(Value::as_bool) == Some(true) {
                        ToolStatus::Error
                    } else {
                        ToolStatus::Completed
                    };
                    if let Some(call) = id.as_ref().and_then(|id| calls.remove(id)) {
                        let tool = events[call].tool.as_mut().expect("tool call");
                        tool.output = Some(output);
                        tool.status = status;
                        tool.completed_at = ts;
                    } else {
                        let tool = ToolInteraction {
                            name: "Orphan tool result".into(),
                            call_id: id,
                            input: None,
                            output: Some(output),
                            status,
                            exit_code: None,
                            started_at: None,
                            completed_at: ts,
                            orphan: true,
                            child_session: None,
                        };
                        emit(
                            &mut events,
                            index,
                            EventKind::Tool,
                            Actor::Tool,
                            None,
                            Some(tool),
                        );
                    }
                }
                _ => hidden += 1,
            }
        }
        if assistant_record && events.len() > first_event {
            let id = message
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty());
            if id.is_none_or(|id| !observed_usage.contains(id))
                && let Some(observation) =
                    usage(message, &prov, record.get("timestamp").and_then(timestamp))
            {
                events[first_event].usage = Some(observation);
                if let Some(id) = id {
                    observed_usage.insert(id.to_owned());
                }
            }
        }
    }
    ClaudeTranscript {
        events,
        hidden_records: hidden,
    }
}

struct ToolRow {
    offset: u64,
    index: usize,
    call: bool,
    id: Option<String>,
}

impl ToolRow {
    fn apply(&self, calls: &mut HashSet<String>) -> usize {
        if self.call {
            if let Some(id) = &self.id {
                calls.insert(id.clone());
            }
            1
        } else if self.id.as_ref().is_some_and(|id| calls.remove(id)) {
            0
        } else {
            1
        }
    }
}

type BlockKey = (Option<String>, u64, usize);

fn block_key(uuid: Option<&str>, offset: u64, index: usize) -> BlockKey {
    (
        uuid.map(str::to_owned),
        if uuid.is_some() { 0 } else { offset },
        index,
    )
}

/// Compaction markers retained per session; more mark the history incomplete.
const MAX_COMPACTIONS: usize = 256;

/// Record-level facts behind `SessionSummary::metrics`, from the parent transcript only.
struct Facts {
    turns: TurnTracker,
    /// Prompt occupancy of the latest parent assistant record, its time and epoch.
    last_prompt: Option<(u64, Option<Timestamp>, u64)>,
    prompt_model: Option<ModelDetail>,
    compactions: Vec<CompactionObservation>,
    compaction_total: u64,
}

impl Facts {
    fn new() -> Self {
        let mut turns = TurnTracker::default();
        // Turn spans are recorded only by some versions and not for every turn.
        turns.mark_incomplete();
        Self {
            turns,
            last_prompt: None,
            prompt_model: None,
            compactions: Vec::new(),
            compaction_total: 0,
        }
    }
}

struct State {
    cursor: Option<JsonlCursor>,
    generation: u64,
    fingerprint: Option<(u64, u64)>,
    summary: SessionSummary,
    prompt: Option<String>,
    generated: Option<String>,
    calls: HashSet<String>,
    tool_rows: HashMap<BlockKey, ToolRow>,
    tool_count: usize,
    /// A summary of this state was handed to a consumer, so later in-place changes
    /// to already-readable content invalidate loaded pages.
    published: bool,
    /// Session ID this state was requested under. Discovery fixes a source's key from
    /// its bounded probe; hydration answers to the same key.
    pinned: Option<String>,
    /// A record named a different session ID than the pinned (approximate) key.
    id_conflict: bool,
    reopen: bool,
    /// The scan reached the end of the file; the next hydration starts a new visit.
    done: bool,
    /// Recency tick for bounded eviction.
    used: u64,
    /// Content already readable changed; consumed (and cleared) by the next delta.
    invalidated: bool,
    ledger: ClaudeLedger,
    side_ledger: Option<ClaudeLedger>,
    facts: Facts,
    parent_seen: bool,
    block_counts: HashMap<(String, usize), (usize, usize)>,
    metadata_entries: usize,
    metadata_bytes: usize,
    metadata_max_entries: usize,
    metadata_max_bytes: usize,
    counts_approximate: bool,
    /// Scan pass whose start-of-visit prefix check already ran for this source.
    checked_pass: u64,
    /// Length and modified time at the last prefix check.
    stamp: Option<(u64, Option<SystemTime>)>,
    /// Exact consumed-prefix verification in progress (files up to
    /// [`PREFIX_VERIFY_MAX`]); advanced under the refresh byte budget.
    verify: Option<PrefixVerify>,
    /// Child identity and lineage, for a `subagents/**/agent-*.jsonl` source.
    child: Option<ChildState>,
    /// Path and Desktop facts that no record states.
    overlay: Overlay,
}

struct PrefixVerify {
    file: File,
    hash: ContentHash,
    left: u64,
    meta: (u64, Option<SystemTime>),
}

enum Verdict {
    Intact,
    Changed,
    Pending,
}
impl State {
    fn new(source: SourceFile, entries: usize, bytes: usize) -> Self {
        let id = source
            .path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = SessionKey {
            harness: Harness::Claude,
            native_id: id.clone(),
            source: SourceKey::File(source.path.clone()),
        };
        let summary = SessionSummary {
            meta: SessionMetadata {
                key,
                title: id,
                title_derived: false,
                project: source
                    .path
                    .parent()
                    .and_then(Path::file_name)
                    .map(|s| safe(&s.to_string_lossy()).preview),
                cwd: None,
                model: None,
                origins: vec![],
                created_at: None,
                activity: ActivityStamp::default(),
                lineage: Lineage::default(),
                members: vec![],
                archived: false,
                released: ReleaseState::Unknown,
                source_available: false,
                harness_version: None,
            },
            usage: None,
            metrics: SessionMetrics::default(),
            lifecycle: Lifecycle::Unknown,
            unfinished_work: false,
            last_event_at: None,
            last_completion: None,
            displayed_event_count: 0,
            hidden_records: 0,
            server_tools: None,
            process: ProcessAssociation::NotChecked,
            warnings: vec![],
        };
        Self {
            cursor: None,
            generation: NEXT_GENERATION.fetch_add(1, Ordering::Relaxed),
            fingerprint: None,
            summary,
            prompt: None,
            generated: None,
            calls: HashSet::new(),
            tool_rows: HashMap::new(),
            tool_count: 0,
            published: false,
            pinned: None,
            id_conflict: false,
            reopen: false,
            done: false,
            used: 0,
            invalidated: false,
            ledger: ClaudeLedger::new(entries, bytes),
            side_ledger: Some(ClaudeLedger::new(entries, bytes)),
            parent_seen: false,
            block_counts: HashMap::new(),
            metadata_entries: 0,
            metadata_bytes: 0,
            metadata_max_entries: entries,
            metadata_max_bytes: bytes,
            counts_approximate: false,
            checked_pass: 0,
            stamp: None,
            facts: Facts::new(),
            verify: None,
            child: None,
            overlay: Overlay::default(),
        }
    }
    /// Starts checking that the file still holds the bytes this cursor consumed. A
    /// same-file rewrite that also grows the file looks like an append to the
    /// cursor, so the consumed prefix is verified whenever the file changed.
    /// Small prefixes are hashed exactly (resumable, see [`Self::verify_step`]);
    /// larger ones use the sampled fingerprint.
    fn begin_verify(&mut self, path: &Path, meta: (u64, Option<SystemTime>)) -> Verdict {
        let Some(cursor) = &self.cursor else {
            return Verdict::Intact;
        };
        let end = cursor.position();
        if end == 0 {
            self.stamp = Some(meta);
            return Verdict::Intact;
        }
        if meta.0 < end {
            return Verdict::Changed;
        }
        if end > PREFIX_VERIFY_MAX {
            let ok = self
                .fingerprint
                .is_some_and(|(e, h)| e == end && fingerprint(path, end) == Some(h));
            if ok {
                self.stamp = Some(meta);
                return Verdict::Intact;
            }
            return Verdict::Changed;
        }
        let Ok(file) = File::open(path) else {
            return Verdict::Changed;
        };
        self.verify = Some(PrefixVerify {
            file,
            hash: ContentHash::new(),
            left: end,
            meta,
        });
        Verdict::Pending
    }
    /// Reads at most `allowance` bytes of the pending verification; returns the
    /// bytes read and the verdict.
    fn verify_step(&mut self, allowance: usize) -> (usize, Verdict) {
        let (Some(v), Some(cursor)) = (self.verify.as_mut(), self.cursor.as_ref()) else {
            return (0, Verdict::Intact);
        };
        let mut read = 0usize;
        let mut buf = [0u8; 8192];
        while v.left > 0 && read < allowance {
            let size = v.left.min(buf.len() as u64).min((allowance - read) as u64) as usize;
            if v.file.read_exact(&mut buf[..size]).is_err() {
                self.verify = None;
                return (read, Verdict::Changed);
            }
            crate::perf::bytes_read(size);
            v.hash.update(&buf[..size]);
            v.left -= size as u64;
            read += size;
        }
        if v.left > 0 {
            return (read, Verdict::Pending);
        }
        let intact = v.hash.finish() == cursor.content_hash();
        let meta = v.meta;
        self.verify = None;
        if intact {
            self.stamp = Some(meta);
            (read, Verdict::Intact)
        } else {
            (read, Verdict::Changed)
        }
    }
    fn reset(&mut self, source: SourceFile, entries: usize, bytes: usize) {
        let published = self.published;
        let pinned = self.pinned.take();
        let cursor = self.cursor.take();
        let child = self.child.take();
        let overlay = std::mem::take(&mut self.overlay);
        *self = Self::new(source, entries, bytes);
        self.published = published;
        if let Some(id) = &pinned {
            self.summary.meta.key.native_id = id.clone();
        }
        self.pinned = pinned;
        self.cursor = cursor;
        if let Some(child) = child {
            self.restore_child(child);
        }
        self.set_overlay(overlay);
    }
    fn warning(&mut self, label: &str, kind: WarningKind, message: &str) {
        let source = match &self.summary.meta.key.source {
            SourceKey::File(p) => format!("{}#{label}", p.display()),
            _ => label.into(),
        };
        if let Some(w) = self
            .summary
            .warnings
            .iter_mut()
            .find(|w| w.key.source == source)
        {
            w.count = w.count.saturating_add(1);
        } else {
            self.summary.warnings.push(Warning {
                key: WarningKey {
                    harness: Harness::Claude,
                    source,
                    kind,
                },
                message: message.into(),
                count: 1,
            });
        }
    }
    /// Turn lifecycle from persisted evidence: an `end_turn` stop completes the turn,
    /// a `tool_use` stop or later genuine user/tool work leaves it open. Sidechains
    /// belong to subagents and never move the parent's lifecycle.
    fn track_turn(&mut self, kind: &str, v: &Value, sidechain: bool) {
        if sidechain {
            return;
        }
        let msg = &v["message"];
        match kind {
            "assistant" => match msg.get("stop_reason").and_then(Value::as_str) {
                Some("end_turn") => {
                    self.summary.last_completion = v
                        .get("timestamp")
                        .and_then(timestamp)
                        .or(self.summary.meta.activity.at)
                        .or(self.summary.last_completion);
                    self.summary.unfinished_work = false;
                    self.summary.lifecycle = Lifecycle::Completed;
                }
                Some("tool_use") => self.open_turn(),
                // Pending tool_use content is unfinished work even when the
                // record carries no (or a null) stop reason, once a lifecycle is known.
                _ => {
                    let pending = msg["content"].as_array().is_some_and(|parts| {
                        parts
                            .iter()
                            .any(|p| p.get("type").and_then(Value::as_str) == Some("tool_use"))
                    });
                    if pending && self.summary.lifecycle != Lifecycle::Unknown {
                        self.open_turn();
                    }
                }
            },
            "user" if v.get("isMeta").and_then(Value::as_bool) != Some(true) => {
                let genuine = match &msg["content"] {
                    Value::String(s) => !injected(s),
                    Value::Array(parts) => {
                        parts
                            .iter()
                            .any(|p| match p.get("type").and_then(Value::as_str) {
                                Some("tool_result") => true,
                                Some("text") => p
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .is_some_and(|t| !injected(t)),
                                _ => false,
                            })
                    }
                    _ => false,
                };
                // Without earlier turn evidence a prompt alone proves nothing.
                if genuine && self.summary.lifecycle != Lifecycle::Unknown {
                    self.open_turn();
                }
            }
            _ => {}
        }
    }
    fn open_turn(&mut self) {
        self.summary.unfinished_work = true;
        self.summary.lifecycle = Lifecycle::Ongoing;
    }
    /// Parent-transcript record facts: turn spans from `turn_duration` records,
    /// `compact_boundary` markers, and the latest assistant record's prompt occupancy
    /// (`input + cache_creation + cache_read`, never output or lifetime totals).
    fn track_metrics(
        &mut self,
        offset: u64,
        kind: &str,
        v: &Value,
        prices: &crate::pricing::PriceSchedule,
        policy: crate::pricing::ReferencePolicy,
    ) {
        let at = v.get("timestamp").and_then(timestamp);
        match kind {
            "system" => match v.get("subtype").and_then(Value::as_str) {
                Some("turn_duration") => {
                    let span = at.zip(v.get("durationMs").and_then(Value::as_u64));
                    if let Some((end, ms)) = span
                        && let Some(start) = i64::try_from(ms).ok().and_then(|m| end.checked_sub(m))
                    {
                        self.facts.turns.start(Some(start));
                        self.facts.turns.end(Some(end));
                    }
                }
                Some("compact_boundary") => {
                    self.facts.compaction_total += 1;
                    // Keep the newest markers: `latest` must describe the newest event.
                    if self.facts.compactions.len() >= MAX_COMPACTIONS {
                        self.facts.compactions.remove(0);
                    }
                    let meta = v.get("compactMetadata");
                    let observation = CompactionObservation {
                        id: format!("claude:{offset}"),
                        timestamp: at,
                        method: None,
                        trigger: meta
                            .and_then(|m| string(m, "trigger"))
                            .map(|t| safe(&t).preview),
                        summary: None,
                        before_tokens: meta
                            .and_then(|m| m.get("preTokens"))
                            .and_then(Value::as_u64),
                        after_tokens: None,
                        duration_ms: None,
                        usage: None,
                        scope: CompactionScope::Event,
                        provenance: Provenance {
                            harness: Harness::Claude,
                            source: self.summary.meta.key.source.clone(),
                            locator: SourceLocator {
                                generation: Some(self.generation),
                                ..locator(&self.summary.meta.key, offset)
                            },
                            record_type: "system/compact_boundary".into(),
                            harness_version: self.summary.meta.harness_version.clone(),
                        },
                    };
                    self.facts.compactions.push(observation);
                }
                _ => return,
            },
            "assistant" => {
                let usage = &v["message"]["usage"];
                let get = |name: &str| usage.get(name).and_then(Value::as_u64);
                let used = get("input_tokens")
                    .zip(get("cache_creation_input_tokens"))
                    .zip(get("cache_read_input_tokens"))
                    .and_then(|((i, w), r)| i.checked_add(w)?.checked_add(r))
                    .filter(|used| *used > 0);
                match used {
                    Some(used) => {
                        self.facts.last_prompt = Some((used, at, self.facts.compaction_total));
                        self.facts.prompt_model =
                            string(&v["message"], "model").map(|id| ModelDetail {
                                id: safe(&id).preview,
                                provider: None,
                                variant: None,
                                effort: None,
                                configured_effort: None,
                            });
                    }
                    None => return,
                }
            }
            _ => return,
        }
        let facts = &self.facts;
        let epoch = format!("c{}", facts.compaction_total);
        let snapshot = context_usage(
            facts.last_prompt.map(|(used, _, _)| used),
            None,
            AggregationClass::Reported,
            facts.last_prompt.and_then(|(_, at, _)| at),
            facts.prompt_model.clone(),
            facts.last_prompt.map(|(_, _, e)| format!("c{e}")),
        );
        self.summary.metrics = SessionMetrics {
            timing: facts.turns.timing(self.summary.meta.created_at),
            speed: SpeedSummary::default(),
            context: current_context(&snapshot, Some(&epoch)),
            compactions: compaction_summary(
                &facts.compactions,
                facts.compaction_total as usize == facts.compactions.len(),
                prices,
                policy,
            ),
        };
    }
    fn ingest(
        &mut self,
        offset: u64,
        v: &Value,
        prices: &crate::pricing::PriceSchedule,
        policy: crate::pricing::ReferencePolicy,
    ) {
        let sidechain = match &self.child {
            Some(child) => !child.id.owns(v),
            None => v.get("isSidechain").and_then(Value::as_bool) == Some(true),
        };
        if let SourceKey::File(path) = &self.summary.meta.key.source
            && let Some(child) = self.child.as_mut()
            && child.absorb(path, v)
        {
            self.summary.meta.lineage = child.lineage.clone();
        }
        if self.child.is_none()
            && let Some(id) = string(v, "sessionId")
        {
            if self.pinned.is_none() {
                self.summary.meta.key.native_id = id;
            } else if self.pinned.as_deref() != Some(id.as_str()) {
                self.id_conflict = true;
            }
        }
        if let Some(cwd) = string(v, "cwd") {
            let p = PathBuf::from(cwd);
            // A child's records repeat the parent's working directory; its project
            // stays the session directory it sits under.
            if self.child.is_none() {
                self.summary.meta.project =
                    p.file_name().map(|n| safe(&n.to_string_lossy()).preview);
            }
            self.summary.meta.cwd = Some(p);
        }
        if let Some(version) = string(v, "version") {
            self.summary.meta.harness_version = Some(safe(&version).preview);
        }
        if self.child.is_none() {
            self.summary.meta.lineage.sidechain |= sidechain;
            // `parentUuid` is a message UUID, never a session; only an explicit
            // session ID names a parent.
            if let Some(parent) =
                string(v, "parentSessionId").or_else(|| string(v, "parent_session_id"))
            {
                self.summary.meta.lineage.parent = Some(SessionLink::Unresolved(parent));
            }
        }
        if let Some(t) = v.get("timestamp").and_then(timestamp) {
            self.summary.meta.created_at =
                Some(self.summary.meta.created_at.map_or(t, |old| old.min(t)));
            self.summary.meta.activity = ActivityStamp {
                at: Some(self.summary.meta.activity.at.map_or(t, |old| old.max(t))),
                class: AggregationClass::Reported,
            };
        }
        // Origin evidence follows the same ownership rule as usage and lifecycle:
        // an inherited or inline sidechain record does not label this session.
        if !sidechain && let Some(origin) = record_origin(v, v.get("timestamp").and_then(timestamp))
        {
            add_origin(&mut self.summary.meta.origins, origin);
        }
        let kind = v.get("type").and_then(Value::as_str).unwrap_or("");
        self.track_turn(kind, v, sidechain);
        if !sidechain {
            self.track_metrics(offset, kind, v, prices, policy);
        }
        if kind == "ai-title" {
            self.generated = v
                .get("aiTitle")
                .and_then(Value::as_str)
                .and_then(title)
                .or(self.generated.take());
        }
        let msg = &v["message"];
        if !sidechain && (kind == "user" || kind == "assistant") {
            if !self.parent_seen {
                self.summary.usage = None;
                self.summary.server_tools = None;
            }
            self.parent_seen = true;
            self.side_ledger = None;
        }
        if kind == "assistant" {
            let prov = Provenance {
                harness: Harness::Claude,
                source: self.summary.meta.key.source.clone(),
                locator: SourceLocator {
                    generation: Some(self.generation),
                    ..locator(&self.summary.meta.key, offset)
                },
                record_type: kind.into(),
                harness_version: self.summary.meta.harness_version.clone(),
            };
            let observation = usage(msg, &prov, v.get("timestamp").and_then(timestamp));
            let tools = msg
                .get("usage")
                .and_then(|u| u.get("server_tool_use"))
                .and_then(Value::as_object)
                .map(|v| ServerToolCounts {
                    web_search: v
                        .get("web_search_requests")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                    web_fetch: v
                        .get("web_fetch_requests")
                        .and_then(Value::as_u64)
                        .unwrap_or(0),
                });
            let ledger = if sidechain {
                self.side_ledger.as_mut()
            } else {
                Some(&mut self.ledger)
            };
            if let Some(ledger) = ledger {
                ledger.observe(
                    msg.get("id").and_then(Value::as_str),
                    observation.as_ref(),
                    tools,
                    prices,
                    policy,
                );
            }
        }
        if self.parent_seen && kind == "assistant" && !sidechain {
            self.summary.usage = self.ledger.summary();
            self.summary.server_tools = self.ledger.server_tools();
        }
        // Parent records own the parent model; sidechain evidence only fills a
        // source that has no parent record yet.
        if kind == "assistant"
            && (!sidechain || !self.parent_seen)
            && let Some(model) = string(msg, "model").map(|s| safe(&s).preview)
            && self
                .summary
                .meta
                .model
                .as_ref()
                .is_none_or(|m| m.id != model)
        {
            self.summary.meta.model = Some(ModelDetail {
                id: model,
                provider: None,
                variant: None,
                effort: None,
                configured_effort: None,
            });
        }
        if kind == "user"
            && self.prompt.is_none()
            && v.get("isMeta").and_then(Value::as_bool) != Some(true)
        {
            let content = &msg["content"];
            let texts: Vec<&str> = match content {
                Value::String(s) => vec![s],
                Value::Array(a) => a
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect(),
                _ => vec![],
            };
            self.prompt = texts.into_iter().filter(|s| !injected(s)).find_map(title);
        }
        let uuid = v.get("uuid").and_then(Value::as_str);
        let parts = match &msg["content"] {
            Value::Array(parts) if kind == "user" || kind == "assistant" => Some(parts.as_slice()),
            _ => None,
        };
        let block_count = if parts.is_some() {
            parts.map_or(0, <[Value]>::len)
        } else if (kind == "user" || kind == "assistant") && msg["content"].is_string() {
            1
        } else {
            0
        };
        if !self.counts_approximate {
            // Charge every observation, including revisions, conservatively.
            // This bounds retained identity metadata and revision rebuild work.
            let uuid_bytes = uuid.map_or(0, str::len).saturating_mul(3);
            let required = (0..block_count).fold(0usize, |total, index| {
                let id_bytes = parts.and_then(|p| p.get(index)).map_or(0, |part| {
                    ["id", "tool_use_id"]
                        .into_iter()
                        .filter_map(|name| part.get(name).and_then(Value::as_str))
                        .map(str::len)
                        .max()
                        .unwrap_or(0)
                });
                total.saturating_add(
                    256usize
                        .saturating_add(uuid_bytes)
                        .saturating_add(id_bytes.saturating_mul(2)),
                )
            });
            if block_count
                > self
                    .metadata_max_entries
                    .saturating_sub(self.metadata_entries)
                || required > self.metadata_max_bytes.saturating_sub(self.metadata_bytes)
            {
                self.counts_approximate = true;
                self.block_counts = HashMap::new();
                self.tool_rows = HashMap::new();
                self.calls = HashSet::new();
                self.warning(
                    "approximate-count",
                    WarningKind::Unsupported,
                    "event and hidden counts approximate: revision metadata limit exceeded",
                );
            } else {
                self.metadata_entries += block_count;
                self.metadata_bytes += required;
            }
        }
        if !self.counts_approximate {
            let mut rebuild_tools = false;
            let mut new_tools = Vec::new();
            for index in 0..block_count {
                let key = block_key(uuid, offset, index);
                let old_tool = self.tool_rows.remove(&key);
                let revised =
                    uuid.is_some_and(|u| self.block_counts.contains_key(&(u.to_owned(), index)));
                let part = parts.and_then(|p| p.get(index));
                let row = part.and_then(|part| {
                    let typ = part.get("type").and_then(Value::as_str);
                    let (call, id) = match (kind, typ) {
                        ("assistant", Some("tool_use")) => (true, string(part, "id")),
                        ("user", Some("tool_result")) => (false, string(part, "tool_use_id")),
                        _ => return None,
                    };
                    Some(ToolRow {
                        offset,
                        index,
                        call,
                        id,
                    })
                });
                if old_tool.is_some() || (revised && row.is_some()) {
                    rebuild_tools = true;
                }
                if let Some(row) = row {
                    new_tools.push(key.clone());
                    self.tool_rows.insert(key, row);
                }
            }
            let old_tool_count = self.tool_count;
            if rebuild_tools {
                self.invalidated = true;
                self.calls.clear();
                let mut rows: Vec<_> = self.tool_rows.values().collect();
                rows.sort_unstable_by_key(|row| (row.offset, row.index));
                self.tool_count = rows.into_iter().map(|row| row.apply(&mut self.calls)).sum();
            } else {
                for key in &new_tools {
                    let row = &self.tool_rows[key];
                    // A result that retires an already-displayed pending call changes
                    // that row in place, so a loaded page is stale.
                    if !row.call
                        && self.published
                        && row.id.as_ref().is_some_and(|id| self.calls.contains(id))
                    {
                        self.invalidated = true;
                    }
                    self.tool_count += row.apply(&mut self.calls);
                }
            }
            self.summary.displayed_event_count =
                self.summary.displayed_event_count - old_tool_count + self.tool_count;
            if let Some(uuid) = uuid {
                for index in 0..block_count {
                    let hidden = parts.and_then(|p| p.get(index)).map_or(0, |part| {
                        match part.get("type").and_then(Value::as_str).unwrap_or("") {
                            "text" | "image" => 0,
                            "thinking" | "tool_use" if kind == "assistant" => 0,
                            "tool_result" if kind == "user" => content_text(&part["content"]).1,
                            _ => 1,
                        }
                    });
                    if let Some((displayed, old_hidden)) = self
                        .block_counts
                        .insert((uuid.to_owned(), index), (0, hidden))
                    {
                        self.summary.displayed_event_count =
                            self.summary.displayed_event_count.saturating_sub(displayed);
                        self.summary.hidden_records =
                            self.summary.hidden_records.saturating_sub(old_hidden);
                        self.invalidated = true;
                    }
                }
            }
        }
        let mapped = map_indexed(&self.summary.meta.key, &[(offset, v)]);
        self.summary.hidden_records += mapped.hidden_records;
        for e in mapped.events {
            if let Some(t) = e.timestamp {
                self.summary.last_event_at =
                    Some(self.summary.last_event_at.map_or(t, |old| old.max(t)));
            }
            if e.tool.is_some() && !self.counts_approximate {
                continue;
            }
            if let Some(uuid) = uuid
                && let Some(index) =
                    e.id.rsplit(':')
                        .next()
                        .and_then(|s| s.parse::<usize>().ok())
                && let Some(counts) = self.block_counts.get_mut(&(uuid.to_owned(), index))
            {
                counts.0 += 1;
            }
            self.summary.displayed_event_count += 1;
        }
        let (name, derived) = if let Some(t) = &self.generated {
            (t.clone(), false)
        } else if let Some(t) = &self.prompt {
            (t.clone(), true)
        } else {
            (safe(&self.summary.meta.key.native_id).preview, false)
        };
        self.summary.meta.title = name;
        self.summary.meta.title_derived = derived;
    }
}
/// Lightweight facts about one discovered source. The full parse lives in `states`.
struct Known {
    key: SessionKey,
    len: u64,
    modified: SystemTime,
    /// Discovery pass that last saw this source.
    pass: u64,
    /// The source could be opened; an unreadable one is probed again every pass.
    available: bool,
}

struct Meter {
    started: Instant,
    records: usize,
    bytes: usize,
    budget: RefreshBudget,
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
    fn spent(&self) -> bool {
        self.records >= self.budget.max_records
            || self.bytes >= self.budget.max_bytes
            || self.started.elapsed().as_millis() >= u128::from(self.budget.max_millis)
    }
    fn progress(&self) -> WorkProgress {
        WorkProgress {
            records: self.records,
            expanded_bytes: self.bytes,
        }
    }
}

enum Scanned {
    Done,
    Exhausted,
}

/// Discovery output accumulated across one call.
#[derive(Default)]
struct Found {
    sessions: Vec<SessionMetadata>,
    removed: Vec<SessionKey>,
    warnings: Vec<Warning>,
}

fn file_path(key: &SessionKey) -> Result<PathBuf, ProviderError> {
    if key.harness != Harness::Claude {
        return Err(ProviderError::Unsupported("not a Claude session".into()));
    }
    match &key.source {
        SourceKey::File(path) => Ok(path.clone()),
        SourceKey::DbRow { .. } => Err(ProviderError::Unsupported(
            "not a Claude file session".into(),
        )),
    }
}

fn modified_ms(modified: SystemTime) -> Option<Timestamp> {
    let since = modified.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    i64::try_from(since.as_millis()).ok()
}

/// A transcript root: the Claude projects store, an explicit extra root, or a
/// local Cowork project root found under a Desktop data directory.
#[derive(Clone, Debug)]
struct RootSpec {
    path: PathBuf,
    cowork: bool,
    /// The user named it, so a missing root is reported.
    explicit: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    Idle,
    /// Reading Desktop Code session manifests, which enrich transcripts.
    Manifests,
    Walking,
}

/// Incremental scanner; transcript bodies are not retained.
pub struct ClaudeScanner {
    config: Config,
    /// Hydrated (or hydrating) sessions, bounded by `max_cached_sessions`.
    states: HashMap<PathBuf, State>,
    walk: Option<Walk>,
    /// Every transcript root known to this reader (Cowork roots are rediscovered
    /// at the start of each pass).
    roots: Vec<RootSpec>,
    /// Roots still to be walked in this pass.
    queue: VecDeque<usize>,
    stage: Stage,
    manifest_queue: Vec<PathBuf>,
    building: HashMap<String, DesktopManifest>,
    /// Desktop manifests of the last completed manifest stage, by `cliSessionId`.
    manifests: HashMap<String, DesktopManifest>,
    pass_id: u64,
    pass_sources: usize,
    known: HashMap<PathBuf, Known>,
    /// Discovered sources per duplicate-ID scope, for duplicate-ID warnings.
    by_id: HashMap<String, Vec<PathBuf>>,
    /// Child sources per (owning session source, agent), so a nested spawner
    /// resolves inside its own family only — never by a session-directory name that
    /// another project may reuse.
    children_by_agent: HashMap<(PathBuf, String), PathBuf>,
    /// Children whose scoped parent is not tracked yet, waiting per the same key.
    unresolved_children: HashMap<(PathBuf, String), Vec<PathBuf>>,
    /// Newest-first retention that bounds `known`/`by_id` to the metadata ceiling.
    retention: Retention<PathBuf>,
    /// Evicted sources that may still be frozen into a roster or read by a job.
    forgotten: Forgotten<PathBuf, SessionKey>,
    invalidated: Vec<SessionKey>,
    ledger: JobLedger,
    indexes: HashMap<PathBuf, Option<TranscriptIndex>>,
    tick: u64,
}
impl ClaudeScanner {
    /// Map persisted records without paging or loading source files.
    pub fn map_transcript(key: &SessionKey, records: &[(u64, Value)]) -> ClaudeTranscript {
        map_transcript(key, records)
    }
    /// Cheap: no filesystem access until the first `discover`.
    pub fn new(config: &Config) -> Result<Self, ProviderError> {
        Ok(Self {
            config: config.clone(),
            states: HashMap::new(),
            walk: None,
            roots: config
                .harnesses
                .claude
                .root
                .iter()
                .chain(&config.harnesses.claude.extra_roots)
                .map(|path| RootSpec {
                    path: path.clone(),
                    cowork: false,
                    explicit: true,
                })
                .collect(),
            queue: VecDeque::new(),
            stage: Stage::Idle,
            manifest_queue: Vec::new(),
            building: HashMap::new(),
            manifests: HashMap::new(),
            pass_id: 0,
            pass_sources: 0,
            known: HashMap::new(),
            by_id: HashMap::new(),
            children_by_agent: HashMap::new(),
            unresolved_children: HashMap::new(),
            retention: Retention::new(config.max_metadata_sources, config.max_metadata_bytes),
            forgotten: Forgotten::new(FORGOTTEN_CAP),
            invalidated: Vec::new(),
            ledger: JobLedger::default(),
            indexes: HashMap::new(),
            tick: 0,
        })
    }

    /// The index slot of `path`, making room by dropping a completed index first.
    fn index_slot<'a>(
        indexes: &'a mut HashMap<PathBuf, Option<TranscriptIndex>>,
        path: &Path,
    ) -> Option<&'a mut Option<TranscriptIndex>> {
        if !indexes.contains_key(path) && indexes.len() >= MAX_INDEXES {
            // Only completed indexes may be dropped; when every slot is a live
            // in-progress index the caller leaves the new job pending instead.
            let victim = indexes
                .iter()
                .find(|(_, i)| i.as_ref().is_none_or(|i| i.ready))
                .map(|(p, _)| p.clone())?;
            indexes.remove(&victim);
        }
        Some(indexes.entry(path.to_path_buf()).or_insert(None))
    }

    /// Sessions with retained hydration state (bounded by `max_cached_sessions`).
    pub fn cached_sessions(&self) -> usize {
        self.states.len()
    }

    /// `(session, job)` pairs still holding partial work.
    pub fn job_state(&self) -> usize {
        self.ledger.len()
    }

    fn limit_warning(&self) -> Warning {
        Warning {
            key: WarningKey {
                harness: Harness::Claude,
                source: "metadata-limit".into(),
                kind: WarningKind::Unsupported,
            },
            message: format!(
                "Discovery limited: only the newest {} Claude sources are tracked; \
                 raise max_metadata_sources to see more",
                self.config.max_metadata_sources
            ),
            count: 1,
        }
    }

    fn count_id(&mut self, id: &str, path: &Path) {
        let paths = self.by_id.entry(id.to_owned()).or_default();
        if !paths.iter().any(|p| p == path) {
            paths.push(path.to_path_buf());
        }
        if paths.len() == 2 {
            // The first source now has a duplicate; probe it again so a cached summary
            // is rehydrated with the warning.
            self.mark_stale(id);
        }
    }

    fn uncount_id(&mut self, id: &str, path: &Path) {
        if let Some(paths) = self.by_id.get_mut(id) {
            paths.retain(|p| p != path);
            if paths.is_empty() {
                self.by_id.remove(id);
            } else if paths.len() == 1 {
                self.mark_stale(id);
            }
        }
    }

    /// Forces the next pass to probe every source of `id` again.
    fn mark_stale(&mut self, id: &str) {
        for path in self.by_id.get(id).into_iter().flatten() {
            if let Some(known) = self.known.get_mut(path) {
                known.len = u64::MAX;
            }
        }
    }

    /// Probes one source (bounded head and tail only) and records it.
    fn visit(
        &mut self,
        source: SourceFile,
        limits: RefreshBudget,
        bytes: &mut usize,
        found: &mut Found,
    ) {
        if let Some(known) = self.known.get_mut(&source.path)
            && known.available
            && known.len == source.len
            && known.modified == source.modified
        {
            known.pass = self.pass_id;
            return;
        }
        let Some((root, cowork)) = self
            .root_of(&source.path)
            .map(|r| (r.path.clone(), r.cowork))
        else {
            return;
        };
        let main = self.main_key(&source.path);
        let (mut meta, available) = match probe_metadata(
            &source.path,
            &root,
            limits,
            self.config.max_record_bytes,
            main.clone(),
        ) {
            Ok((meta, read)) => {
                *bytes = bytes.saturating_add(read);
                (meta, true)
            }
            Err(_) => {
                found.warnings.push(Warning {
                    key: WarningKey {
                        harness: Harness::Claude,
                        source: format!("{}#unavailable", source.path.display()),
                        kind: WarningKind::Unavailable,
                    },
                    message: "source unavailable".into(),
                    count: 1,
                });
                (
                    bare_state(&root, source.clone(), main, 16, 1 << 16)
                        .summary
                        .meta,
                    false,
                )
            }
        };
        self.overlay_for(&source.path, &meta.key.native_id, cowork)
            .apply(&mut meta);
        // A nested spawner whose transcript is discovered later resolves exactly
        // inside this family; outside it the link stays scoped and orphaned.
        self.remember_child(&source.path);
        self.resolve_scoped_parent(&source.path, &mut meta);
        if meta
            .lineage
            .parent
            .as_ref()
            .is_some_and(|link| children::scoped_parent(link).is_some())
        {
            self.note_unresolved_child(&source.path, &meta);
        }
        let dup = self.dup_key(&source.path, &meta.key.native_id);
        meta.source_available = available;
        if meta.activity.at.is_none() {
            meta.activity = ActivityStamp {
                at: modified_ms(source.modified),
                class: AggregationClass::Derived,
            };
        }
        let weight = crate::roster::metadata_bytes(&meta) + source.path.as_os_str().len();
        match self.retention.admit(&source.path, meta.activity.at, weight) {
            Admit::Dropped | Admit::Displaced => {
                // Bookkeeping only: a tracked source that no longer fits is forgotten
                // (and watched for deletion), never reported as removed.
                self.forget(&source.path);
                found.warnings.push(self.limit_warning());
                return;
            }
            Admit::Kept { evicted } => {
                // Eviction only forgets this adapter's bookkeeping: the files still
                // exist and stay readable, so no removal is reported.
                self.forgotten.remove(&source.path);
                for path in evicted {
                    self.forget(&path);
                    found.warnings.push(self.limit_warning());
                }
            }
        }
        if let Some(old) = self.known.get(&source.path)
            && old.key != meta.key
        {
            let old_key = old.key.clone();
            let old_dup = self.dup_key(&source.path, &old_key.native_id);
            self.uncount_id(&old_dup, &source.path);
            found.removed.push(old_key);
        }
        self.count_id(&dup, &source.path);
        self.known.insert(
            source.path.clone(),
            Known {
                key: meta.key.clone(),
                len: source.len,
                modified: source.modified,
                pass: self.pass_id,
                available,
            },
        );
        found.sessions.push(meta);
    }

    /// Tracks a child's agent ID within its family, for exact parent resolution.
    fn remember_child(&mut self, path: &Path) {
        let Some(child) = self
            .root_of(path)
            .and_then(|root| ClaudeChildId::of(&root.path, path))
        else {
            return;
        };
        let Some(owner) = main_path(path) else {
            return;
        };
        let scope = (owner.clone(), child.agent_id);
        if self
            .children_by_agent
            .insert(scope.clone(), path.to_path_buf())
            .is_none()
        {
            // A spawner just became known: children visited before it were left
            // orphaned, so they are probed again and republish the resolved edge.
            for waiting in self.unresolved_children.remove(&scope).unwrap_or_default() {
                self.mark_stale_path(&waiting);
            }
        }
    }

    /// Forces the next pass to probe `path` again even though its source is unchanged.
    fn mark_stale_path(&mut self, path: &Path) {
        if let Some(known) = self.known.get_mut(path) {
            known.len = u64::MAX;
        }
    }

    /// Replaces a family-scoped parent-agent link with the exact key when that
    /// spawner's transcript is a tracked source of the same family. `path` is the
    /// child's own source, which decides the family (two projects may use the same
    /// session-directory name).
    fn resolve_scoped_parent(&self, path: &Path, meta: &mut SessionMetadata) {
        let Some(link) = meta.lineage.parent.as_ref() else {
            return;
        };
        let Some((_, agent)) = children::scoped_parent(link) else {
            return;
        };
        let Some(owner) = main_path(path) else {
            return;
        };
        let Some(source) = self.children_by_agent.get(&(owner, agent.to_owned())) else {
            return;
        };
        meta.lineage.parent = Some(SessionLink::Resolved(SessionKey {
            harness: Harness::Claude,
            native_id: agent.to_owned(),
            source: SourceKey::File(source.clone()),
        }));
    }

    /// Remembers a child whose scoped parent is not known yet, so it is re-probed
    /// when that spawner is discovered.
    fn note_unresolved_child(&mut self, path: &Path, meta: &SessionMetadata) {
        let Some(link) = meta.lineage.parent.as_ref() else {
            return;
        };
        let Some((_, agent)) = children::scoped_parent(link) else {
            return;
        };
        let Some(owner) = main_path(path) else {
            return;
        };
        let waiting = self
            .unresolved_children
            .entry((owner, agent.to_owned()))
            .or_default();
        if !waiting.iter().any(|p| p == path) {
            waiting.push(path.to_path_buf());
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
            self.forgotten.remove(&path);
            if let Some(known) = self.known.remove(&path) {
                let dup = self.dup_key(&path, &known.key.native_id);
                self.uncount_id(&dup, &path);
                found.removed.push(known.key);
            }
            self.forget_child(path.clone());
            self.states.remove(&path);
        }
        // Forgotten sources are not walked into `known`; a bounded existence check lets a
        // genuine deletion of a frozen or in-flight row reach the roster.
        for path in self.forgotten.keys() {
            match std::fs::metadata(&path) {
                Ok(_) => {}
                // Only a confirmed absence is a deletion; any other error keeps the watch.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    if let Some(key) = self.forgotten.remove(&path) {
                        found.removed.push(key);
                    }
                }
                Err(_) => found.warnings.push(Warning {
                    key: WarningKey {
                        harness: Harness::Claude,
                        source: format!("{}#unavailable", path.display()),
                        kind: WarningKind::Unavailable,
                    },
                    message: "source unavailable".into(),
                    count: 1,
                }),
            }
        }
    }

    fn source_file(&self, path: &Path) -> SourceFile {
        match std::fs::metadata(path) {
            Ok(meta) => SourceFile {
                path: path.to_path_buf(),
                modified: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                len: meta.len(),
            },
            Err(_) => SourceFile {
                path: path.to_path_buf(),
                modified: SystemTime::UNIX_EPOCH,
                len: 0,
            },
        }
    }

    /// One bounded slice of scanning `path` under `key`.
    fn scan(&mut self, path: &Path, key: &SessionKey, meter: &mut Meter) -> Scanned {
        self.tick += 1;
        if self
            .states
            .get(path)
            .is_some_and(|s| s.summary.meta.key.native_id != key.native_id)
        {
            self.states.remove(path);
        }
        let source = self.source_file(path);
        let (entries, ledger_bytes) = (
            self.config.dedup_ledger_entries,
            self.config.dedup_ledger_bytes,
        );
        if !self.states.contains_key(path) {
            let state = self.fresh_state(path, key, source.clone(), entries, ledger_bytes);
            self.states.insert(path.to_path_buf(), state);
        }
        let Some(state) = self.states.get_mut(path) else {
            return Scanned::Done;
        };
        state.used = self.tick;
        if state.done {
            state.done = false;
            state.checked_pass = 0;
        }
        let outcome = loop {
            if meter.spent() {
                break Scanned::Exhausted;
            }
            if state.cursor.is_none() {
                state.cursor =
                    JsonlCursor::open(path.to_path_buf(), self.config.max_record_bytes).ok();
                if state.cursor.is_some() && state.reopen {
                    state.reset(source.clone(), entries, ledger_bytes);
                }
            }
            let mut verdict = Verdict::Intact;
            if state.checked_pass != 1 {
                state.checked_pass = 1;
                state.verify = None;
                let meta = std::fs::metadata(path)
                    .ok()
                    .map(|m| (m.len(), m.modified().ok()));
                if let Some(meta) = meta
                    && state.cursor.is_some()
                    && Some(meta) != state.stamp
                {
                    verdict = state.begin_verify(path, meta);
                }
            }
            if state.verify.is_some() {
                let left = meter.budget.max_bytes.saturating_sub(meter.bytes).max(1);
                let (read, v) = state.verify_step(left);
                meter.bytes = meter.bytes.saturating_add(read);
                verdict = v;
            }
            match verdict {
                Verdict::Pending => continue,
                Verdict::Changed => {
                    // Consumed bytes changed in place: nothing parsed from them holds.
                    state.reset(source.clone(), entries, ledger_bytes);
                    state.cursor =
                        JsonlCursor::open(path.to_path_buf(), self.config.max_record_bytes).ok();
                    state.invalidated = true;
                    state.checked_pass = 1;
                    state.stamp = std::fs::metadata(path)
                        .ok()
                        .map(|m| (m.len(), m.modified().ok()));
                }
                Verdict::Intact => {}
            }
            let Some(cursor) = state.cursor.as_mut() else {
                state.summary.meta.source_available = false;
                state.done = true;
                break Scanned::Done;
            };
            let before = cursor.position();
            let rotations = cursor.rotation_count();
            let result = cursor.next_record();
            let after = cursor.position();
            let rotated = cursor.rotation_count() != rotations;
            if rotated {
                state.reset(source.clone(), entries, ledger_bytes);
                state.invalidated = true;
            }
            meter.bytes = meter.bytes.saturating_add(
                usize::try_from(if rotated {
                    after
                } else {
                    after.saturating_sub(before)
                })
                .unwrap_or(usize::MAX),
            );
            match result {
                Ok(Some(Record::Json { offset, value })) => {
                    state.summary.meta.source_available = true;
                    state.ingest(
                        offset,
                        &value,
                        &self.config.pricing,
                        self.config.reference_policy(Harness::Claude),
                    );
                    meter.records += 1;
                }
                Ok(None) => {
                    state.summary.meta.source_available = true;
                    if !state.parent_seen
                        && let Some(ledger) = &state.side_ledger
                    {
                        state.summary.usage = ledger.summary();
                        state.summary.server_tools = ledger.server_tools();
                    }
                    state.done = true;
                    break Scanned::Done;
                }
                Ok(Some(Record::Incomplete { .. })) => {
                    state.summary.meta.source_available = true;
                    state.done = true;
                    break Scanned::Done;
                }
                Ok(Some(Record::Skipping { .. })) => {
                    // Oversized line still draining; bytes were charged above.
                    state.summary.meta.source_available = true;
                }
                Ok(Some(Record::Invalid { .. })) => {
                    state.summary.meta.source_available = true;
                    state.warning("malformed", WarningKind::Parse, "malformed records skipped");
                    meter.records += 1;
                }
                Ok(Some(Record::TooLarge { .. })) => {
                    state.summary.meta.source_available = true;
                    state.warning(
                        "oversized",
                        WarningKind::Unsupported,
                        "oversized records skipped",
                    );
                    meter.records += 1;
                }
                Err(_) => {
                    state.summary.meta.source_available = false;
                    state.cursor = None;
                    state.reopen = true;
                    state.done = true;
                    break Scanned::Done;
                }
            }
        };
        if matches!(outcome, Scanned::Done)
            && let (Some(cursor), SourceKey::File(path)) =
                (&state.cursor, &state.summary.meta.key.source)
        {
            let end = cursor.position();
            if end > PREFIX_VERIFY_MAX
                && state
                    .fingerprint
                    .is_none_or(|(previous, _)| previous != end)
            {
                state.fingerprint = fingerprint(path, end).map(|hash| (end, hash));
            }
        }
        if matches!(outcome, Scanned::Done) {
            self.maintain(path);
        }
        outcome
    }

    /// Bounds retained states after a scan completes.
    fn maintain(&mut self, current: &Path) {
        let cap = self.config.max_cached_sessions.max(1);
        while self.states.len() > cap {
            let victim = self
                .states
                .iter()
                .filter(|(p, s)| {
                    p.as_path() != current && !self.ledger.has_consumer(&s.summary.meta.key)
                })
                .min_by_key(|(_, s)| s.used)
                .map(|(p, _)| p.clone());
            match victim {
                Some(path) => {
                    self.states.remove(&path);
                }
                None => break,
            }
        }
    }

    /// Drops the bookkeeping of `path` but remembers it, bounded, so a frozen row stays
    /// readable and a later physical deletion is still noticed.
    fn forget(&mut self, path: &Path) {
        if let Some(known) = self.known.remove(path) {
            let dup = self.dup_key(path, &known.key.native_id);
            self.uncount_id(&dup, path);
            // State an active job is still consuming is not disposed of: only the
            // bookkeeping is forgotten, so a read in flight never restarts.
            if !self.ledger.has_consumer(&known.key) {
                self.states.remove(path);
            }
            self.forgotten.insert(path.to_path_buf(), known.key);
        }
        self.forget_child(path.to_path_buf());
    }

    /// Drops `path` from the family-scoped child indexes (it is no longer tracked).
    fn forget_child(&mut self, path: PathBuf) {
        self.children_by_agent.retain(|_, p| *p != path);
        for waiting in self.unresolved_children.values_mut() {
            waiting.retain(|p| *p != path);
        }
        self.unresolved_children
            .retain(|_, waiting| !waiting.is_empty());
    }

    /// The session ID the discovery probe would assign to the file as it is now.
    fn probed_id(&self, path: &Path) -> Option<String> {
        let root = self.root_of(path)?;
        let limits = RefreshBudget {
            max_records: PROBE_RECORDS,
            max_bytes: usize::MAX,
            max_millis: u64::MAX,
        };
        probe_claude_metadata(path, &root.path, limits)
            .ok()
            .map(|meta| meta.key.native_id)
    }

    /// An existing transcript at an allowlisted place under a known root.
    fn allowlisted(&self, path: &Path) -> bool {
        self.root_of(path).is_some() && std::fs::metadata(path).is_ok_and(|m| m.is_file())
    }

    /// The transcript root `path` is an allowlisted source of.
    fn root_of(&self, path: &Path) -> Option<&RootSpec> {
        self.roots
            .iter()
            .filter(|r| classify_claude(&r.path, path).is_some())
            .max_by_key(|r| r.path.as_os_str().len())
    }

    /// The key of the main session a child transcript sits under, once discovered.
    fn main_key(&self, path: &Path) -> Option<SessionKey> {
        self.known.get(&main_path(path)?).map(|k| k.key.clone())
    }

    /// Duplicate-ID scope: a child's agent ID is only unique within its session.
    fn dup_key(&self, path: &Path, native_id: &str) -> String {
        // Derived from the path, never from the root registry: a Cowork root that
        // disappears between passes must not change the key its sources were
        // counted with.
        match crate::discovery::claude_family(path) {
            Some(session) => format!("{session}\u{1f}{native_id}"),
            None => native_id.to_owned(),
        }
    }

    /// Facts about `path` that no record states: its Cowork root, and the Desktop
    /// manifest naming it by `cliSessionId` (a child has none).
    fn overlay_for(&self, path: &Path, native_id: &str, cowork: bool) -> Overlay {
        let mut overlay = Overlay::default();
        if cowork {
            add_origin(
                &mut overlay.origins,
                OriginObservation::new(OriginSurface::Cowork, Some(COWORK_ORIGIN), None),
            );
        }
        let child = self
            .root_of(path)
            .is_some_and(|r| ClaudeChildId::of(&r.path, path).is_some());
        if !child && let Some(manifest) = self.manifests.get(native_id) {
            manifest.apply(&mut overlay);
        }
        overlay
    }

    /// A state for hydrating `path` under `key`, bound to its layout and overlay.
    fn fresh_state(
        &self,
        path: &Path,
        key: &SessionKey,
        source: SourceFile,
        entries: usize,
        ledger_bytes: usize,
    ) -> State {
        let root = self.root_of(path).map(|r| (r.path.clone(), r.cowork));
        let mut state = match &root {
            Some((root, _)) => bare_state(root, source, self.main_key(path), entries, ledger_bytes),
            None => State::new(source, entries, ledger_bytes),
        };
        state.pinned = Some(key.native_id.clone());
        state.summary.meta.key.native_id = key.native_id.clone();
        let cowork = root.is_some_and(|(_, cowork)| cowork);
        state.set_overlay(self.overlay_for(path, &key.native_id, cowork));
        state
    }

    /// Starts a pass: locates the allowlisted Desktop stores (bounded listings) and
    /// queues the manifests and every root.
    fn begin_pass(&mut self, found: &mut Found) {
        self.pass_id += 1;
        self.pass_sources = 0;
        self.roots.retain(|r| !r.cowork);
        self.manifest_queue.clear();
        self.building.clear();
        if self.config.harnesses.claude.root.is_none() {
            let mut missing = Walk::new(Harness::Claude, None);
            found.warnings.extend(missing.take_warnings());
        }
        let desktop = self.config.claude_desktop.clone();
        for dir in &desktop.data_dirs {
            let stores = desktop_stores(dir, desktop.explicit);
            found.warnings.extend(stores.warnings);
            self.manifest_queue.extend(stores.manifests);
            for root in stores.cowork_roots {
                if !self.roots.iter().any(|r| r.path == root) {
                    self.roots.push(RootSpec {
                        path: root,
                        cowork: true,
                        explicit: false,
                    });
                }
            }
        }
        self.queue = (0..self.roots.len()).collect();
        self.stage = Stage::Manifests;
    }

    /// All manifests are read: transcripts whose manifest facts changed are
    /// probed again, and cached states take the new facts.
    fn finish_manifests(&mut self) {
        let building = std::mem::take(&mut self.building);
        let changed: HashSet<String> = building
            .iter()
            .filter(|(id, m)| self.manifests.get(*id) != Some(m))
            .map(|(id, _)| id.clone())
            .chain(
                self.manifests
                    .keys()
                    .filter(|id| !building.contains_key(*id))
                    .cloned(),
            )
            .collect();
        self.manifests = building;
        for id in &changed {
            self.mark_stale(id);
        }
        let cached: Vec<(PathBuf, String)> = self
            .states
            .iter()
            .filter(|(_, s)| s.child.is_none())
            .map(|(p, s)| (p.clone(), s.summary.meta.key.native_id.clone()))
            .filter(|(_, id)| changed.contains(id))
            .collect();
        for (path, id) in cached {
            let cowork = self.root_of(&path).is_some_and(|r| r.cowork);
            let overlay = self.overlay_for(&path, &id, cowork);
            if let Some(state) = self.states.get_mut(&path) {
                state.set_overlay(overlay);
            }
        }
        self.stage = Stage::Walking;
    }

    /// Sources this adapter currently tracks (bounded by the metadata ceiling).
    pub fn tracked_sources(&self) -> usize {
        self.retention.len()
    }

    /// Duplicate-ID scopes this adapter currently tracks. Every tracked source
    /// contributes exactly one, so eviction never leaves stale paths behind.
    pub fn duplicate_scopes(&self) -> usize {
        self.by_id.len()
    }

    /// The path of `key` if discovery has published exactly this session.
    fn discovered(&self, key: &SessionKey) -> Result<PathBuf, ProviderError> {
        let path = file_path(key)?;
        match self.known.get(&path) {
            Some(known) if known.key == *key => Ok(path),
            Some(_) => Err(ProviderError::Unavailable("unknown Claude session".into())),
            // Retention may have forgotten a source that still exists (a frozen row or an
            // in-flight read). Only a key this adapter published is honoured, and only
            // while the file still carries that session's identity.
            None if self.forgotten.get(&path) == Some(key)
                && self.allowlisted(&path)
                && self.probed_id(&path).as_deref() == Some(key.native_id.as_str()) =>
            {
                Ok(path)
            }
            None => Err(ProviderError::Unavailable("unknown Claude session".into())),
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
            && state.summary.meta.key.native_id == key.native_id
            && state.done
        {
            self.tick += 1;
            state.used = self.tick;
            return Ok(None);
        }
        let mut meter = Meter::new(budget);
        self.ledger.join(key, job);
        match self.scan(&path, key, &mut meter) {
            Scanned::Exhausted => Ok(Some(meter.progress())),
            Scanned::Done => Ok(None),
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

    /// The duplicate-ID warning for a state about to be published.
    fn publish(&mut self, path: &Path) -> Option<SessionSummary> {
        let native_id = self.states.get(path)?.summary.meta.key.native_id.clone();
        let dup = self.dup_key(path, &native_id);
        let count = self.by_id.get(&dup).map_or(1, Vec::len);
        // A spawner discovered before this hydration now resolves exactly.
        let mut meta = self.states.get(path)?.summary.meta.clone();
        self.resolve_scoped_parent(path, &mut meta);
        let state = self.states.get_mut(path)?;
        state.summary.meta.lineage = meta.lineage;
        let source = match &state.summary.meta.key.source {
            SourceKey::File(p) => format!("{}#duplicate-id", p.display()),
            _ => "#duplicate-id".into(),
        };
        state.summary.warnings.retain(|w| w.key.source != source);
        if count > 1 {
            state.summary.warnings.push(Warning {
                key: WarningKey {
                    harness: Harness::Claude,
                    source,
                    kind: WarningKind::Unsupported,
                },
                message: "Duplicate ID: another source shares this session ID".into(),
                count: u32::try_from(count).unwrap_or(u32::MAX),
            });
        }
        state
            .summary
            .warnings
            .retain(|w| !w.key.source.ends_with("#approximate-id"));
        if state.id_conflict {
            state.summary.warnings.push(Warning {
                key: WarningKey {
                    harness: Harness::Claude,
                    source: format!("{}#approximate-id", path.display()),
                    kind: WarningKind::Unsupported,
                },
                message: "Approximate session ID: the ID in the transcript lies beyond the \
                          discovery probe, so the file name identifies this session"
                    .into(),
                count: 1,
            });
        }
        state.published = true;
        if std::mem::take(&mut state.invalidated) {
            self.invalidated.push(state.summary.meta.key.clone());
        }
        Some(state.summary.clone())
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
        if let Some(progress) = self.prepare(job, key, budget)? {
            return Ok(ReadProgress::Pending(progress));
        }
        let (path, end) = checked_source(&self.states, key)?;
        let generation = self.states[path].generation;
        let Some(slot) = Self::index_slot(&mut self.indexes, path) else {
            self.ledger.join(key, job);
            return Ok(ReadProgress::Pending(WorkProgress::default()));
        };
        let index = match ensure_index(
            slot,
            path,
            generation,
            end,
            self.config.max_record_bytes,
            budget,
        )? {
            Indexed::Building(progress) => {
                self.ledger.join(key, job);
                return Ok(ReadProgress::Pending(progress));
            }
            Indexed::Ready(index) => index,
        };
        let position = match anchor.as_ref() {
            Some(anchor) => match index
                .rows
                .binary_search_by(|row| index.blocks[*row].anchor.cmp(anchor))
            {
                Ok(pos) => Some(pos),
                Err(_) => {
                    return Ok(ReadProgress::Ready(TranscriptPage {
                        events: vec![],
                        diagnostics: vec![],
                        next: None,
                        has_more: false,
                        truncated: false,
                    }));
                }
            },
            None => None,
        };
        let forward = direction == PageDirection::Forward;
        let indices: Box<dyn Iterator<Item = usize>> = if forward {
            Box::new(position.map_or(0, |p| p + 1)..index.rows.len())
        } else {
            Box::new((0..position.unwrap_or(index.rows.len())).rev())
        };
        let mut page = Vec::new();
        let mut size = 0usize;
        let mut truncated = false;
        let mut has_more = false;
        let mut selected = Vec::new();
        for row in indices {
            let mut event = indexed_event(index, row, key, end, self.config.max_record_bytes)?;
            let cut = fit_event(&mut event, request.max_bytes);
            let cost = event_size(&event);
            if !page.is_empty()
                && (page.len() >= request.max_events.max(1)
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
            page.push(event);
            selected.push(row);
        }
        if !forward {
            page.reverse();
            selected.reverse();
        }
        let next = if forward { page.last() } else { page.first() }.map(event_anchor);
        // Assign diagnostics to the next event in total order, or the last event.
        let mut page_diags = Vec::new();
        if !selected.is_empty() || (index.rows.is_empty() && anchor.is_none()) {
            let first = selected.first().copied().unwrap_or(0);
            let last = selected.last().copied().unwrap_or(0);
            let start = if first == 0 {
                0
            } else {
                let previous = &index.blocks[index.rows[first - 1]].anchor;
                index.diag_order.partition_point(|d| {
                    let d = &index.diagnostics[*d];
                    (d.timestamp, d.offset) <= (previous.timestamp, previous.source_order)
                })
            };
            let stop = if index.rows.is_empty() || last + 1 == index.rows.len() {
                index.diag_order.len()
            } else {
                let last_anchor = &index.blocks[index.rows[last]].anchor;
                index.diag_order.partition_point(|d| {
                    let d = &index.diagnostics[*d];
                    (d.timestamp, d.offset) <= (last_anchor.timestamp, last_anchor.source_order)
                })
            };
            let overflow = stop.saturating_sub(start) > 200;
            let visible_stop = if overflow { start + 199 } else { stop };
            for entry in &index.diag_order[start..visible_stop] {
                let d = &index.diagnostics[*entry];
                let mut loc = locator(key, d.offset);
                loc.generation = Some(generation);
                page_diags.push(DiagnosticEntry {
                    locator: loc,
                    label: d.label.into(),
                    hidden: d.hidden,
                });
            }
            if overflow {
                let omitted = &index.diagnostics[index.diag_order[visible_stop]];
                let mut loc = locator(key, omitted.offset);
                loc.generation = Some(generation);
                page_diags.push(DiagnosticEntry {
                    locator: loc,
                    label: "diagnostics truncated (200-entry limit); first omitted source".into(),
                    hidden: true,
                });
                truncated = true;
            }
        }
        Ok(ReadProgress::Ready(TranscriptPage {
            events: page,
            diagnostics: page_diags,
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
            if let Some(progress) = self.prepare(job, key, budget)? {
                return Ok(ReadProgress::Pending(progress));
            }
            let path = file_path(key)?;
            if let Some(state) = self.states.get(&path) {
                compactions = state.facts.compactions.clone();
            }
            let (keep, events) = super::share_details_budget(request, compactions.len(), true);
            if compactions.len() > keep {
                compactions.drain(..compactions.len() - keep);
                cut = true;
            }
            request = events;
        } else {
            request = super::share_details_budget(request, 0, true).1;
        }
        let page =
            match self.page_ready(job, key, anchor, PageDirection::Backward, request, budget)? {
                ReadProgress::Pending(progress) => return Ok(ReadProgress::Pending(progress)),
                ReadProgress::Ready(page) => page,
            };
        // Each returned record consumes the shared allowance exactly once: the request
        // observations take only what the compactions and events left over.
        let room = allowance.saturating_sub(compactions.len() + page.events.len());
        let mut requests: Vec<_> = page.events.iter().filter_map(|e| e.usage.clone()).collect();
        let cut = cut || requests.len() > room;
        requests.truncate(room);
        let truncated = page.truncated || cut;
        // Request observations are the usage the paged events themselves recorded
        // (one per deduplicated assistant message), so they page with the same anchor
        // and budget. The dedup ledger keeps totals only.
        Ok(ReadProgress::Ready(SessionDetailPage {
            requests,
            compactions,
            recent: page.events,
            next: page.next,
            has_more: page.has_more,
            truncated,
        }))
    }
}

/// Transcript indexes kept at once: every in-progress one is retained so two large live
/// sessions never restart each other; completed ones are dropped first when full.
const MAX_INDEXES: usize = 8;

const PREFIX_VERIFY_MAX: u64 = 4 * 1024 * 1024;

// As with the Codex adapter, large consumed prefixes use bounded sampling.
// Rewrites outside the samples can evade this check.
fn fingerprint(path: &Path, end: u64) -> Option<u64> {
    let mut file = File::open(path).ok()?;
    if file.metadata().ok()?.len() < end {
        return None;
    }
    let mut hash = ContentHash::new();
    hash.update(&end.to_le_bytes());
    let len = end.min(512) as usize;
    let mut bytes = [0u8; 512];
    for start in [0, (end / 2).min(end - len as u64), end - len as u64] {
        file.seek(SeekFrom::Start(start)).ok()?;
        file.read_exact(&mut bytes[..len]).ok()?;
        crate::perf::bytes_read(len);
        hash.update(&bytes[..len]);
    }
    Some(hash.finish())
}

// Read only the generation consumed by the roster. Never retain transcript bodies
// between requests, and reject offsets if the scanned prefix has changed.
fn checked_source<'a>(
    states: &'a HashMap<PathBuf, State>,
    key: &SessionKey,
) -> Result<(&'a Path, u64), ProviderError> {
    let SourceKey::File(path) = &key.source else {
        return Err(ProviderError::Unsupported(
            "not a Claude file session".into(),
        ));
    };
    let state = states
        .get(path)
        .filter(|s| {
            key.harness == Harness::Claude
                && s.summary.meta.key == *key
                && s.summary.meta.source_available
        })
        .ok_or_else(|| {
            ProviderError::Unavailable("unknown or unavailable Claude session".into())
        })?;
    let cursor = state
        .cursor
        .as_ref()
        .ok_or_else(|| ProviderError::Unavailable("source unavailable".into()))?;
    if !cursor.same_file().unwrap_or(false) {
        return Err(ProviderError::Unavailable(
            "source identity changed since refresh".into(),
        ));
    }
    let end = cursor.position();
    let intact = if end > PREFIX_VERIFY_MAX {
        state.fingerprint == fingerprint(path, end).map(|hash| (end, hash))
            && state.fingerprint.is_some()
    } else {
        let mut file = File::open(path)
            .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
        let mut hash = ContentHash::new();
        let mut left = end;
        let mut buf = [0u8; 8192];
        while left > 0 {
            let size = left.min(buf.len() as u64) as usize;
            file.read_exact(&mut buf[..size])
                .map_err(|_| ProviderError::Unavailable("source changed since refresh".into()))?;
            crate::perf::bytes_read(size);
            hash.update(&buf[..size]);
            left -= size as u64;
        }
        hash.finish() == cursor.content_hash()
    };
    if !intact {
        return Err(ProviderError::Unavailable(
            "source changed since refresh".into(),
        ));
    }
    let SourceKey::File(path) = &state.summary.meta.key.source else {
        unreachable!()
    };
    Ok((path, end))
}

fn event_anchor(event: &Event) -> PageAnchor {
    PageAnchor {
        timestamp: event.timestamp,
        source_order: event.source_order,
        stable_id: event.id.clone(),
    }
}

fn event_size(event: &Event) -> usize {
    let len = |part: &Option<BoundedText>| part.as_ref().map_or(0, |v| v.preview.len());
    len(&event.text)
        + event
            .tool
            .as_ref()
            .map_or(0, |t| t.name.len() + len(&t.input) + len(&t.output))
}

fn fit_event(event: &mut Event, budget: usize) -> bool {
    let mut fields: Vec<(&mut String, Option<&mut bool>)> = Vec::new();
    if let Some(text) = &mut event.text {
        fields.push((&mut text.preview, Some(&mut text.truncated)));
    }
    if let Some(tool) = &mut event.tool {
        fields.push((&mut tool.name, None));
        for text in [tool.input.as_mut(), tool.output.as_mut()]
            .into_iter()
            .flatten()
        {
            fields.push((&mut text.preview, Some(&mut text.truncated)));
        }
    }
    let mut left = budget;
    let mut remaining = fields.len();
    let mut cut = false;
    for (text, flag) in fields {
        let share = left / remaining;
        remaining -= 1;
        if text.len() > share {
            let mut at = share;
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            text.truncate(at);
            if let Some(flag) = flag {
                *flag = true;
            }
            cut = true;
        }
        left -= text.len();
    }
    cut
}

const INDEX_STEP: usize = 512;
const INDEX_BYTES_PER_STEP: u64 = 4 * 1024 * 1024;
const INDEX_CAP: usize = 2_000_000;
const INDEX_METADATA_CAP: usize = 64 * 1024 * 1024;

#[derive(Clone)]
enum BlockKind {
    Row,
    Call(String),
    Result(String),
    Hidden,
}

struct IndexedBlock {
    anchor: PageAnchor,
    uuid: Option<String>,
    block: usize,
    kind: BlockKind,
    live: bool,
    usage: bool,
    result: Option<u64>,
}

struct IndexedDiagnostic {
    timestamp: Option<Timestamp>,
    offset: u64,
    label: &'static str,
    hidden: bool,
}

struct IndexCursor {
    reader: BufReader<File>,
    position: u64,
    line_start: u64,
    buffer: Vec<u8>,
    oversized: bool,
}

impl IndexCursor {
    fn open(path: &Path) -> std::io::Result<Self> {
        Ok(Self {
            reader: BufReader::new(File::open(path)?),
            position: 0,
            line_start: 0,
            buffer: Vec::new(),
            oversized: false,
        })
    }

    // Consume no more than the scanned boundary and the caller's byte allowance.
    fn next(&mut self, end: u64, max: usize, allowance: u64) -> std::io::Result<Option<Record>> {
        let stop = end.min(self.position.saturating_add(allowance));
        while self.position < stop {
            let available = self.reader.fill_buf()?;
            if available.is_empty() {
                return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
            }
            let len = available.len().min((stop - self.position) as usize);
            let bytes = &available[..len];
            let newline = bytes.iter().position(|&b| b == b'\n');
            let data_len = newline.unwrap_or(len);
            let consumed = data_len + usize::from(newline.is_some());
            if !self.oversized {
                if data_len > max.saturating_add(1).saturating_sub(self.buffer.len()) {
                    self.buffer.clear();
                    self.oversized = true;
                } else {
                    self.buffer.extend_from_slice(&bytes[..data_len]);
                }
            }
            self.reader.consume(consumed);
            self.position += consumed as u64;
            crate::perf::bytes_read(consumed);
            if newline.is_some() {
                let offset = self.line_start;
                self.line_start = self.position;
                let record = if self.oversized {
                    Record::TooLarge { offset }
                } else {
                    let line = self.buffer.strip_suffix(b"\r").unwrap_or(&self.buffer);
                    if line.len() > max {
                        Record::TooLarge { offset }
                    } else {
                        crate::perf::record_parsed();
                        match serde_json::from_slice(line) {
                            Ok(value) => Record::Json { offset, value },
                            Err(_) => Record::Invalid { offset },
                        }
                    }
                };
                self.buffer.clear();
                self.oversized = false;
                return Ok(Some(record));
            }
        }
        Ok(None)
    }
}
#[derive(Default)]
struct Finalization {
    stage: u8,
    at: usize,
    revisions: BTreeMap<(String, usize), usize>,
    calls: BTreeMap<String, usize>,
    seen_ids: BTreeSet<String>,
    last_record: Option<u64>,
    rows: BTreeMap<PageAnchor, usize>,
    diagnostics: BTreeMap<(Option<Timestamp>, u64, usize), usize>,
}

struct TranscriptIndex {
    path: PathBuf,
    generation: u64,
    cursor: IndexCursor,
    end: u64,
    blocks: Vec<IndexedBlock>,
    diag_order: Vec<usize>,
    base_diags: usize,
    finalization: Option<Finalization>,
    ready: bool,
    diagnostics: Vec<IndexedDiagnostic>,
    rows: Vec<usize>,
    usage_ids: HashMap<u64, Option<String>>,
    metadata_bytes: usize,
    over: bool,
}

fn record_fits(index: &TranscriptIndex, value: &Value) -> bool {
    let uuid_len = value
        .get("uuid")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    let message_id = value
        .pointer("/message/id")
        .and_then(Value::as_str)
        .map_or(0, str::len);
    let mut bytes = index
        .metadata_bytes
        .saturating_add(message_id)
        .saturating_add(128);
    let mut entries = index.blocks.len().saturating_add(index.diagnostics.len());
    let content = value.pointer("/message/content");
    let count = match content {
        Some(Value::Array(parts)) => parts.len(),
        Some(Value::String(_)) => 1,
        _ => usize::from(value.get("type").and_then(Value::as_str) == Some("system")),
    };
    for n in 0..count {
        let part = content
            .and_then(Value::as_array)
            .and_then(|parts| parts.get(n));
        let id_len = part
            .and_then(|part| part.get("id").or_else(|| part.get("tool_use_id")))
            .and_then(Value::as_str)
            .map_or(0, str::len);
        bytes = bytes
            .saturating_add(uuid_len.saturating_mul(2))
            .saturating_add(id_len)
            .saturating_add(256);
        entries = entries.saturating_add(2);
        if bytes > INDEX_METADATA_CAP || entries > INDEX_CAP {
            return false;
        }
    }
    bytes <= INDEX_METADATA_CAP && entries < INDEX_CAP
}

impl TranscriptIndex {
    fn add_record(&mut self, offset: u64, value: &Value) {
        let timestamp = value.get("timestamp").and_then(timestamp);
        let kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let uuid = value.get("uuid").and_then(Value::as_str);
        if kind == "system" {
            self.push_block(timestamp, offset, uuid, 0, BlockKind::Row);
            self.blocks.last_mut().expect("system block added").uuid = None;
            return;
        }
        if kind != "user" && kind != "assistant" {
            self.diagnostics.push(IndexedDiagnostic {
                timestamp,
                offset,
                label: "hidden or unknown record",
                hidden: true,
            });
            return;
        }
        if kind == "assistant"
            && value
                .pointer("/message/usage")
                .is_some_and(Value::is_object)
        {
            let id = value
                .pointer("/message/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(str::to_owned);
            self.metadata_bytes = self
                .metadata_bytes
                .saturating_add(id.as_ref().map_or(0, String::len) + 48);
            self.usage_ids.insert(offset, id);
        }
        let content = &value["message"]["content"];
        let string_part;
        let parts = match content {
            Value::Array(parts) => parts.as_slice(),
            Value::String(s) => {
                string_part = serde_json::json!({"type":"text","text":s});
                std::slice::from_ref(&string_part)
            }
            _ => &[],
        };
        for (block, part) in parts.iter().enumerate() {
            let typ = part.get("type").and_then(Value::as_str).unwrap_or("");
            let item = match (kind, typ) {
                (_, "text" | "image") | ("assistant", "thinking") => BlockKind::Row,
                ("assistant", "tool_use") => BlockKind::Call(
                    part.get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_owned(),
                ),
                ("user", "tool_result") => {
                    if content_text(&part["content"]).1 != 0 {
                        self.diagnostics.push(IndexedDiagnostic {
                            timestamp,
                            offset,
                            label: "hidden nested tool content",
                            hidden: true,
                        });
                    }
                    BlockKind::Result(
                        part.get("tool_use_id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_owned(),
                    )
                }
                _ => {
                    self.diagnostics.push(IndexedDiagnostic {
                        timestamp,
                        offset,
                        label: "hidden content block",
                        hidden: true,
                    });
                    BlockKind::Hidden
                }
            };
            self.push_block(timestamp, offset, uuid, block, item);
        }
    }

    fn push_block(
        &mut self,
        timestamp: Option<Timestamp>,
        offset: u64,
        uuid: Option<&str>,
        block: usize,
        kind: BlockKind,
    ) {
        let stable_id = format!("{}:{offset}:{block}", uuid.unwrap_or("record"));
        let extra = stable_id.len()
            + uuid.map_or(0, str::len)
            + match &kind {
                BlockKind::Call(id) | BlockKind::Result(id) => id.len(),
                _ => 0,
            };
        self.metadata_bytes = self
            .metadata_bytes
            .saturating_add(extra + std::mem::size_of::<IndexedBlock>());
        self.blocks.push(IndexedBlock {
            anchor: PageAnchor {
                timestamp,
                source_order: offset,
                stable_id,
            },
            uuid: uuid.map(str::to_owned),
            block,
            kind,
            live: true,
            usage: false,
            result: None,
        });
    }

    fn finish_step(&mut self) -> bool {
        let mut work = self.finalization.take().unwrap_or_default();
        let mut remaining = INDEX_STEP;
        while remaining > 0 {
            match work.stage {
                0 if work.at < self.blocks.len() => {
                    let block = &self.blocks[work.at];
                    if let Some(uuid) = &block.uuid {
                        work.revisions.insert((uuid.clone(), block.block), work.at);
                    }
                }
                1 if work.at < self.blocks.len() => {
                    let block = &mut self.blocks[work.at];
                    block.live = block.uuid.as_ref().is_none_or(|uuid| {
                        work.revisions.get(&(uuid.clone(), block.block)) == Some(&work.at)
                    });
                    block.result = None;
                    block.usage = false;
                }
                2 if work.at < self.blocks.len() => {
                    let block = &self.blocks[work.at];
                    if block.live {
                        match &block.kind {
                            BlockKind::Call(id) if !id.is_empty() => {
                                work.calls.insert(id.clone(), work.at);
                            }
                            BlockKind::Result(id) if !id.is_empty() => {
                                if let Some(call) = work.calls.remove(id) {
                                    let offset = block.anchor.source_order;
                                    let timestamp = block.anchor.timestamp;
                                    self.blocks[call].result = Some(offset);
                                    self.blocks[work.at].live = false;
                                    self.diagnostics.push(IndexedDiagnostic {
                                        timestamp,
                                        offset,
                                        label: "source-only tool result",
                                        hidden: false,
                                    });
                                }
                            }
                            _ => {}
                        }
                    }
                }
                3 if work.at < self.blocks.len() => {
                    let block = &mut self.blocks[work.at];
                    if block.live && !matches!(block.kind, BlockKind::Hidden) {
                        work.rows.insert(block.anchor.clone(), work.at);
                        if work.last_record != Some(block.anchor.source_order) {
                            work.last_record = Some(block.anchor.source_order);
                            if let Some(id) = self.usage_ids.get(&block.anchor.source_order)
                                && id
                                    .as_ref()
                                    .is_none_or(|id| work.seen_ids.insert(id.clone()))
                            {
                                block.usage = true;
                            }
                        }
                    }
                }
                4 if work.at < self.diagnostics.len() => {
                    let diag = &self.diagnostics[work.at];
                    work.diagnostics
                        .insert((diag.timestamp, diag.offset, work.at), work.at);
                }
                5 => {
                    if let Some((_, row)) = work.rows.pop_first() {
                        self.rows.push(row);
                    } else if let Some((_, diag)) = work.diagnostics.pop_first() {
                        self.diag_order.push(diag);
                    } else {
                        work.stage = 6;
                        continue;
                    }
                    remaining -= 1;
                    continue;
                }
                6 => {
                    if work.revisions.pop_first().is_none()
                        && work.calls.pop_first().is_none()
                        && work.seen_ids.pop_first().is_none()
                    {
                        self.ready = true;
                        return true;
                    }
                    remaining -= 1;
                    continue;
                }
                _ => {
                    work.stage += 1;
                    work.at = 0;
                    continue;
                }
            }
            work.at += 1;
            remaining -= 1;
        }
        self.finalization = Some(work);
        false
    }
}

/// Result of one bounded indexing slice.
enum Indexed<'a> {
    Ready(&'a TranscriptIndex),
    /// More of the file remains to index or finalize; call again.
    Building(WorkProgress),
}

fn ensure_index<'a>(
    slot: &'a mut Option<TranscriptIndex>,
    path: &Path,
    generation: u64,
    end: u64,
    max: usize,
    budget: RefreshBudget,
) -> Result<Indexed<'a>, ProviderError> {
    if slot
        .as_ref()
        .is_none_or(|index| index.path != path || index.generation != generation || index.end > end)
    {
        *slot = Some(TranscriptIndex {
            path: path.to_owned(),
            generation,
            cursor: IndexCursor::open(path)
                .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?,
            end: 0,
            blocks: Vec::new(),
            diag_order: Vec::new(),
            base_diags: 0,
            finalization: None,
            ready: false,
            diagnostics: Vec::new(),
            rows: Vec::new(),
            usage_ids: HashMap::new(),
            metadata_bytes: 0,
            over: false,
        });
    }
    let index = slot.as_mut().expect("index initialized");
    let over = || {
        ProviderError::Unsupported("Claude transcript exceeds index entry or metadata limit".into())
    };
    if index.over {
        return Err(over());
    }
    if index.end < end && index.finalization.is_some() && !index.finish_step() {
        return Ok(Indexed::Building(WorkProgress::default()));
    }
    if index.end < end {
        if index.ready || index.finalization.is_some() {
            index.diagnostics.truncate(index.base_diags);
            index.rows.clear();
            index.diag_order.clear();
            index.finalization = None;
            index.ready = false;
        }
        let started = Instant::now();
        let step = INDEX_STEP.min(budget.max_records.max(1));
        let allowance_total = INDEX_BYTES_PER_STEP.min(budget.max_bytes.max(1) as u64);
        let start = index.cursor.position;
        let mut count = 0;
        while index.cursor.position < end
            && count < step
            && index.cursor.position.saturating_sub(start) < allowance_total
            && (count == 0 || started.elapsed().as_millis() < u128::from(budget.max_millis))
        {
            let allowance = allowance_total - index.cursor.position.saturating_sub(start);
            let record = index
                .cursor
                .next(end, max, allowance)
                .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
            match record {
                Some(Record::Json { offset, value }) => {
                    if !record_fits(index, &value) {
                        index.over = true;
                        return Err(over());
                    }
                    index.add_record(offset, &value);
                }
                Some(Record::Invalid { offset }) | Some(Record::TooLarge { offset }) => {
                    index.diagnostics.push(IndexedDiagnostic {
                        timestamp: None,
                        offset,
                        label: "unavailable record",
                        hidden: true,
                    })
                }
                Some(Record::Skipping { .. }) => continue,
                Some(Record::Incomplete { .. }) | None => break,
            }
            index.base_diags = index.diagnostics.len();
            count += 1;
            if index.blocks.len().saturating_add(index.diagnostics.len()) > INDEX_CAP
                || index.metadata_bytes > INDEX_METADATA_CAP
            {
                index.over = true;
                return Err(over());
            }
        }
        index.end = index.cursor.position;
        if index.end < end {
            return Ok(Indexed::Building(WorkProgress {
                records: count,
                expanded_bytes: usize::try_from(index.end - start).unwrap_or(usize::MAX),
            }));
        }
    }
    if !index.ready && !index.finish_step() {
        return Ok(Indexed::Building(WorkProgress::default()));
    }
    Ok(Indexed::Ready(index))
}

fn read_indexed_value(
    path: &Path,
    offset: u64,
    end: u64,
    max: usize,
) -> Result<Value, ProviderError> {
    let mut file =
        File::open(path).map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
    let mut bytes = Vec::new();
    BufReader::new(file)
        .take((end - offset).min(max as u64 + 2))
        .read_until(b'\n', &mut bytes)
        .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
    crate::perf::bytes_read(bytes.len());
    crate::perf::record_parsed();
    serde_json::from_slice(&bytes)
        .map_err(|_| ProviderError::Unavailable("indexed source changed".into()))
}

fn indexed_event(
    index: &TranscriptIndex,
    row: usize,
    key: &SessionKey,
    end: u64,
    max: usize,
) -> Result<Event, ProviderError> {
    let block = &index.blocks[index.rows[row]];
    let offset = block.anchor.source_order;
    let value = read_indexed_value(&index.path, offset, end, max)?;
    let result = block
        .result
        .map(|at| read_indexed_value(&index.path, at, end, max).map(|value| (at, value)))
        .transpose()?;
    let mut records = vec![(offset, value)];
    if let Some(result) = result {
        records.push(result);
    }
    let mut event = map_transcript(key, &records)
        .events
        .into_iter()
        .find(|event| event.id == block.anchor.stable_id)
        .ok_or_else(|| ProviderError::Unavailable("indexed event changed".into()))?;
    event.provenance.locator.generation = Some(index.generation);
    event.usage = if block.usage {
        usage(
            &records[0].1["message"],
            &event.provenance,
            records[0].1.get("timestamp").and_then(timestamp),
        )
    } else {
        None
    };
    Ok(event)
}

impl ProviderReader for ClaudeScanner {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn discover(
        &mut self,
        _now: Timestamp,
        budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        let deadline = Instant::now() + Duration::from_millis(budget.max_millis);
        let mut found = Found::default();
        let (mut records, mut bytes) = (0usize, 0usize);
        if self.stage == Stage::Idle {
            self.begin_pass(&mut found);
        }
        let spent = |records: usize, bytes: usize| {
            records >= budget.max_records || bytes >= budget.max_bytes || Instant::now() >= deadline
        };
        if self.stage == Stage::Manifests {
            while let Some(path) = self.manifest_queue.pop() {
                let (manifest, read) = read_manifest(&path);
                records += 1;
                bytes = bytes.saturating_add(read);
                if let Some((id, manifest)) = manifest {
                    match self.building.get_mut(&id) {
                        Some(existing) => existing.merge(&manifest),
                        None => {
                            self.building.insert(id, manifest);
                        }
                    }
                }
                if spent(records, bytes) {
                    break;
                }
            }
            if self.manifest_queue.is_empty() {
                self.finish_manifests();
            }
        }
        let mut finished = false;
        if self.stage == Stage::Walking {
            loop {
                if self.walk.is_none() {
                    let Some(index) = self.queue.pop_front() else {
                        finished = true;
                        break;
                    };
                    let root = &self.roots[index];
                    self.walk = Some(Walk::new_in(
                        Harness::Claude,
                        Some(&root.path),
                        root.explicit,
                    ));
                }
                let Some(walk) = self.walk.as_mut() else {
                    break;
                };
                let batch = walk.step(1, deadline);
                found.warnings.extend(walk.take_warnings());
                if walk.is_done() {
                    self.walk = None;
                    finished = self.queue.is_empty();
                }
                for source in batch {
                    self.pass_sources += 1;
                    records += 1;
                    let limits = RefreshBudget {
                        max_records: PROBE_RECORDS,
                        max_bytes: budget.max_bytes.saturating_sub(bytes).max(1),
                        max_millis: u64::try_from(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .as_millis(),
                        )
                        .unwrap_or(u64::MAX)
                        .max(1),
                    };
                    self.visit(source, limits, &mut bytes, &mut found);
                }
                if finished || spent(records, bytes) {
                    break;
                }
            }
        }
        if finished {
            self.stage = Stage::Idle;
            self.finish_pass(&mut found);
        }
        Ok(DiscoveryDelta {
            sessions: found.sessions,
            removed: found.removed,
            invalidated: std::mem::take(&mut self.invalidated),
            warnings: found.warnings,
            scan: ScanProgress {
                harness: Harness::Claude,
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
        match self.scan(&path, key, &mut meter) {
            Scanned::Exhausted => {
                let Some(state) = self.states.get(&path) else {
                    self.ledger.release(key, job);
                    return Err(ProviderError::Internal("hydration state vanished".into()));
                };
                Ok(HydrationDelta {
                    summary: SessionSummary::unhydrated(state.summary.meta.clone()),
                    done: false,
                })
            }
            Scanned::Done => {
                self.ledger.release(key, job);
                let summary = self
                    .publish(&path)
                    .ok_or_else(|| ProviderError::Internal("hydration state vanished".into()))?;
                Ok(HydrationDelta {
                    summary,
                    done: true,
                })
            }
        }
    }

    fn cancel_read(&mut self, key: &SessionKey, job: ReadJobId) {
        if !self.ledger.release(key, job) {
            return;
        }
        // The last consumer is gone: abandoned partial work goes, healthy state stays.
        let Ok(path) = file_path(key) else {
            return;
        };
        if self
            .states
            .get(&path)
            .is_some_and(|s| !s.done && s.summary.meta.key.native_id == key.native_id)
        {
            self.states.remove(&path);
        }
        if self
            .indexes
            .get(&path)
            .and_then(Option::as_ref)
            .is_some_and(|i| !i.ready)
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
            let (path, end) = checked_source(&self.states, key)?;
            let generation = self.states[path].generation;
            let Some(slot) = Self::index_slot(&mut self.indexes, path) else {
                self.ledger.join(key, job);
                return Ok(ReadProgress::Pending(WorkProgress::default()));
            };
            let index = match ensure_index(
                slot,
                path,
                generation,
                end,
                self.config.max_record_bytes,
                budget,
            )? {
                Indexed::Building(progress) => {
                    self.ledger.join(key, job);
                    return Ok(ReadProgress::Pending(progress));
                }
                Indexed::Ready(index) => index,
            };
            Ok(ReadProgress::Ready(
                index.rows.len()
                    - index
                        .rows
                        .partition_point(|row| index.blocks[*row].anchor <= *anchor),
            ))
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
        let result = (|| {
            let LocatorKind::FileOffset { path, offset } = &source.kind else {
                return Err(ProviderError::Unsupported(
                    "not a Claude file locator".into(),
                ));
            };
            if let Some(progress) = self.prepare(job, &source.session, budget)? {
                return Ok(ReadProgress::Pending(progress));
            }
            let (expected, end) = checked_source(&self.states, &source.session)?;
            if self.states[expected].generation != source.generation.unwrap_or(0) {
                return Err(ProviderError::Unavailable("stale source locator".into()));
            }
            if path != expected {
                return Err(ProviderError::Unsupported(
                    "locator does not belong to session".into(),
                ));
            }
            if *offset >= end {
                return Err(ProviderError::Unavailable(
                    "record beyond scanned data".into(),
                ));
            }
            let mut file = File::open(path)
                .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
            if *offset > 0 {
                file.seek(SeekFrom::Start(offset - 1))
                    .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
                let mut prior = [0u8; 1];
                file.read_exact(&mut prior)
                    .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
                if prior[0] != b'\n' {
                    return Err(ProviderError::Parse(
                        "locator is not a record boundary".into(),
                    ));
                }
            }
            file.seek(SeekFrom::Start(*offset))
                .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
            let mut bytes = Vec::new();
            BufReader::new(file)
                .take((end - offset).min(self.config.max_record_bytes as u64 + 2))
                .read_until(b'\n', &mut bytes)
                .map_err(|_| ProviderError::Unavailable("source unavailable".into()))?;
            if bytes.last() != Some(&b'\n') {
                return Ok(ReadProgress::Ready(SourceRecord {
                    raw: String::new(),
                    display: String::new(),
                    unavailable: Some("record incomplete or exceeds limit".into()),
                }));
            }
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
            if bytes.len() > self.config.max_record_bytes {
                return Ok(ReadProgress::Ready(SourceRecord {
                    raw: String::new(),
                    display: String::new(),
                    unavailable: Some("record exceeds limit".into()),
                }));
            }
            let raw = String::from_utf8_lossy(&bytes).into_owned();
            let display = display_safe(&raw);
            Ok(ReadProgress::Ready(SourceRecord {
                raw,
                display,
                unavailable: None,
            }))
        })();
        self.settle(&source.session, job, &result);
        result
    }
}
