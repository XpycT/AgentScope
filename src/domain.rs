//! Normalized, provider-independent domain types shared by every layer.

use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

/// Epoch milliseconds, UTC.
pub type Timestamp = i64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RefreshBudget {
    pub max_records: usize,
    pub max_bytes: usize,
    pub max_millis: u64,
}

/// Agent product whose local store produced a session. Distinct from a model's
/// billing/routing provider, which is recorded text on [`ModelDetail`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Harness {
    Codex,
    Claude,
    OpenCode,
    Omp,
    Hermes,
    Pi,
}

impl Harness {
    /// Every harness in stable display order.
    pub const ALL: [Harness; 6] = [
        Harness::Codex,
        Harness::Claude,
        Harness::OpenCode,
        Harness::Omp,
        Harness::Hermes,
        Harness::Pi,
    ];
}

/// Logical source of a session. `File` is the canonical, uncompressed identity;
/// a physical compressed copy is addressed by a locator, never a second key.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SourceKey {
    File(PathBuf),
    DbRow {
        database: PathBuf,
        table: String,
        id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SessionKey {
    pub harness: Harness,
    pub native_id: String,
    pub source: SourceKey,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionLink {
    Resolved(SessionKey),
    Ambiguous,
    Unresolved(String),
}

/// Relationship evidence. `Debug` prints only presence of harness-supplied
/// agent strings.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Lineage {
    pub parent: Option<SessionLink>,
    pub fork: Option<SessionLink>,
    pub sidechain: bool,
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub parent_agent_id: Option<String>,
    pub spawning_tool_id: Option<String>,
    pub spawn_depth: Option<u32>,
}

impl fmt::Debug for Lineage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lineage")
            .field("parent", &self.parent.is_some())
            .field("fork", &self.fork.is_some())
            .field("sidechain", &self.sidechain)
            .field("agent_id", &self.agent_id.is_some())
            .field("agent_type", &self.agent_type.is_some())
            .field("parent_agent_id", &self.parent_agent_id.is_some())
            .field("spawning_tool_id", &self.spawning_tool_id.is_some())
            .field("spawn_depth", &self.spawn_depth)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    UserPrompt,
    AssistantMessage,
    Reasoning,
    Tool,
    System,
    Lifecycle,
    Compaction,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor {
    User,
    Assistant,
    System,
    Tool,
}

/// Display-safe text. `Debug` prints lengths only, never the preview.
#[derive(Clone, PartialEq, Eq)]
pub struct BoundedText {
    pub preview: String,
    pub byte_len: usize,
    pub truncated: bool,
    pub locator: Option<SourceLocator>,
}

impl fmt::Debug for BoundedText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BoundedText")
            .field("preview_len", &self.preview.len())
            .field("byte_len", &self.byte_len)
            .field("truncated", &self.truncated)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLocator {
    pub session: SessionKey,
    pub kind: LocatorKind,
    /// Opaque identity of the scanned source generation.
    pub generation: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LocatorKind {
    FileOffset {
        path: PathBuf,
        offset: u64,
    },
    DbRow {
        table: String,
        row_id: String,
        part_index: Option<usize>,
    },
    /// Record `ordinal` (zero-based, in decompressed record order) inside a
    /// physical zstd file. Decompressed byte offsets cannot seek into the file.
    CompressedRecord {
        path: PathBuf,
        ordinal: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provenance {
    pub harness: Harness,
    pub source: SourceKey,
    pub locator: SourceLocator,
    pub record_type: String,
    pub harness_version: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenCategories {
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub cache_write: Option<u64>,
    pub cache_write_5m: Option<u64>,
    pub cache_write_1h: Option<u64>,
    pub cache_read: Option<u64>,
    pub output: Option<u64>,
    pub reasoning: Option<u64>,
    pub total: Option<u64>,
    /// Additive provider-side orchestration, only when the source reports it explicitly.
    pub orchestration_input: Option<u64>,
    pub orchestration_output: Option<u64>,
    pub orchestration_cache_read: Option<u64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AggregationClass {
    Reported,
    Derived,
    Partial,
    Estimated,
    #[default]
    Unknown,
}

/// Whether `input` already contains the cache read/write counters. Proven by
/// the adapter from the source shape, never inferred from a billing route.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InputAccounting {
    /// Cache reads and writes are subsets of `input`.
    InclusiveCaches,
    /// Cache reads and writes are reported next to `input`.
    AdditiveCaches,
    #[default]
    Unknown,
}

/// Whether `output` already contains the reasoning counter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OutputAccounting {
    IncludesReasoning,
    SeparateReasoning,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TokenAccounting {
    pub input: InputAccounting,
    pub output: OutputAccounting,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum RequestPurpose {
    #[default]
    Conversation,
    Compaction,
    Auxiliary(String),
}

/// Per-request facts that select a price: identity, time, tier and region.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestMetadata {
    /// Provider/request ID, only when the source confirms it.
    pub id: Option<String>,
    pub purpose: RequestPurpose,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    pub duration_ms: Option<f64>,
    pub ttft_ms: Option<f64>,
    pub service_tier: Option<String>,
    pub inference_region: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MoneySource {
    ReportedApi,
    SavedCatalogEstimate,
    UserOverride,
    BundledReference,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MoneyFact {
    pub usd: f64,
    pub source: MoneySource,
    pub as_of: Option<String>,
}

/// Money a source recorded. None of it is trusted to bypass the price engine
/// unless it is a reported API amount above zero or a verified saved estimate.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordedMoney {
    pub reported_api: Option<MoneyFact>,
    pub saved_estimate: Option<MoneyFact>,
    pub unverified_usd: Option<f64>,
}

/// USD per priced leg. `None` is unknown or unused; a priced empty leg is `Some(0.0)`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CostBreakdown {
    pub input: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
    pub cache_write_other: Option<f64>,
    pub output: Option<f64>,
    pub reasoning: Option<f64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CostEstimate {
    /// Complete primary amount; `None` unless every used leg is priced.
    pub usd: Option<f64>,
    pub class: AggregationClass,
    pub as_of: Option<String>,
    pub note: Option<String>,
    /// Sum of the legs that were priced, even when `usd` is unknown.
    pub known_subtotal: f64,
    pub breakdown: CostBreakdown,
    pub source: Option<MoneySource>,
    /// Priced from the native-vendor API reference because no billing route was
    /// recorded. The recorded provider is never invented; UI labels this a
    /// reference estimate.
    pub reference: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageScope {
    Session,
    Turn,
    Message,
}

#[derive(Clone, Debug, PartialEq)]
pub struct UsageObservation {
    pub model: Option<ModelDetail>,
    pub scope: UsageScope,
    pub categories: TokenCategories,
    pub accounting: TokenAccounting,
    pub request: Option<RequestMetadata>,
    pub money: RecordedMoney,
    pub provenance: Provenance,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsageCoverage {
    pub observed_requests: usize,
    pub priced_requests: usize,
    pub complete: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModelUsageSummary {
    pub model: ModelDetail,
    pub categories: TokenCategories,
    pub total: Option<u64>,
    pub cost: CostEstimate,
    pub coverage: UsageCoverage,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UsageSummary {
    pub categories: TokenCategories,
    pub total: Option<u64>,
    pub class: AggregationClass,
    pub cost: CostEstimate,
    pub reported_cost_usd: Option<f64>,
    pub saved_estimate_usd: Option<f64>,
    pub coverage: UsageCoverage,
    pub models: Vec<ModelUsageSummary>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServerToolCounts {
    pub web_search: u64,
    pub web_fetch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WarningKind {
    Io,
    Parse,
    Unsupported,
    Unavailable,
    Process,
    Pricing,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct WarningKey {
    pub harness: Harness,
    pub source: String,
    pub kind: WarningKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Warning {
    pub key: WarningKey,
    pub message: String,
    pub count: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanProgress {
    pub harness: Harness,
    pub scanned: usize,
    pub total: Option<usize>,
    pub done: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticEntry {
    pub locator: SourceLocator,
    pub label: String,
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    pub id: String,
    pub session: SessionKey,
    pub timestamp: Option<Timestamp>,
    pub source_order: u64,
    pub kind: EventKind,
    pub actor: Actor,
    pub text: Option<BoundedText>,
    pub tool: Option<ToolInteraction>,
    pub usage: Option<UsageObservation>,
    pub provenance: Provenance,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ToolInteraction {
    pub name: String,
    pub call_id: Option<String>,
    pub input: Option<BoundedText>,
    pub output: Option<BoundedText>,
    pub status: ToolStatus,
    pub exit_code: Option<i32>,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    pub orphan: bool,
    /// Confirmed native child session this tool call ran, from allowlisted tool
    /// metadata only. Never parsed from output text.
    pub child_session: Option<SessionLink>,
}

/// `Debug` prints presence and sizes only: tool previews and an unresolved child
/// id are untrusted input and never belong in a diagnostic dump.
impl fmt::Debug for ToolInteraction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ToolInteraction")
            .field("name_len", &self.name.len())
            .field("call_id", &self.call_id.is_some())
            .field("input", &self.input.is_some())
            .field("output", &self.output.is_some())
            .field("status", &self.status)
            .field("exit_code", &self.exit_code)
            .field("started_at", &self.started_at)
            .field("completed_at", &self.completed_at)
            .field("orphan", &self.orphan)
            .field("child_session", &self.child_session.is_some())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    Completed,
    Error,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    Unknown,
    Ongoing,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessAssociation {
    Matched {
        pid: u32,
        started_at: Option<Timestamp>,
    },
    Candidates {
        count: usize,
    },
    Unmatched,
    NotChecked,
    Unavailable,
}

/// Recorded model identity. `id` is the exact-match pricing key; `provider` is
/// the recorded billing/routing route (text), never a [`Harness`]. `Debug`
/// prints lengths and presence only.
#[derive(Clone, PartialEq, Eq)]
pub struct ModelDetail {
    pub id: String,
    pub provider: Option<String>,
    pub variant: Option<String>,
    /// Effective reasoning effort reported for the request.
    pub effort: Option<String>,
    /// Effort the user configured, when the harness records it separately.
    pub configured_effort: Option<String>,
}

impl fmt::Debug for ModelDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ModelDetail")
            .field("id_len", &self.id.len())
            .field("provider", &self.provider.is_some())
            .field("variant", &self.variant.is_some())
            .field("effort", &self.effort.is_some())
            .field("configured_effort", &self.configured_effort.is_some())
            .finish()
    }
}

/// A timestamp together with how it was obtained.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ActivityStamp {
    pub at: Option<Timestamp>,
    pub class: AggregationClass,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OriginSurface {
    Cli,
    DesktopCode,
    Ide,
    Cowork,
    Other,
    Unknown,
}

/// Maximum characters retained in [`OriginObservation::raw`].
pub const ORIGIN_RAW_MAX_CHARS: usize = 64;

/// One piece of origin evidence. Resumed sessions can carry several.
/// `Debug` never prints the raw text.
#[derive(Clone, PartialEq, Eq)]
pub struct OriginObservation {
    pub surface: OriginSurface,
    pub raw: Option<String>,
    pub recorded_at: Option<Timestamp>,
}

impl OriginObservation {
    /// Builds an observation, keeping at most [`ORIGIN_RAW_MAX_CHARS`] of `raw`.
    pub fn new(surface: OriginSurface, raw: Option<&str>, recorded_at: Option<Timestamp>) -> Self {
        Self {
            surface,
            raw: raw.map(|r| r.chars().take(ORIGIN_RAW_MAX_CHARS).collect()),
            recorded_at,
        }
    }
}

impl fmt::Debug for OriginObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("OriginObservation")
            .field("surface", &self.surface)
            .field("raw", &self.raw.is_some())
            .field("recorded_at", &self.recorded_at)
            .finish()
    }
}

/// Whether a desktop app marked the session released. `Unknown` means no marker
/// was read, not that the session is present.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReleaseState {
    #[default]
    Unknown,
    Present,
    Released,
}

/// Cheap, discovery-time session facts. `Debug` omits titles, paths, models,
/// origins, and other content-bearing fields.
#[derive(Clone, PartialEq)]
pub struct SessionMetadata {
    pub key: SessionKey,
    pub title: String,
    pub title_derived: bool,
    pub project: Option<String>,
    pub cwd: Option<PathBuf>,
    /// Latest recorded model.
    pub model: Option<ModelDetail>,
    pub origins: Vec<OriginObservation>,
    pub created_at: Option<Timestamp>,
    /// Latest persisted activity and how it was determined.
    pub activity: ActivityStamp,
    pub lineage: Lineage,
    /// Logical members of a chained session; empty means only `key`.
    pub members: Vec<SessionKey>,
    /// Harness-reported archived state; never inferred.
    pub archived: bool,
    pub released: ReleaseState,
    pub source_available: bool,
    pub harness_version: Option<String>,
}

impl fmt::Debug for SessionMetadata {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionMetadata")
            .field("harness", &self.key.harness)
            .field("title_len", &self.title.len())
            .field("title_derived", &self.title_derived)
            .field("model", &self.model)
            .field("origins", &self.origins)
            .field("activity", &self.activity)
            .field("lineage", &self.lineage)
            .field("members", &self.members.len())
            .field("archived", &self.archived)
            .field("released", &self.released)
            .field("source_available", &self.source_available)
            .finish_non_exhaustive()
    }
}

/// Measured session timing. Every field is `None`/`false` until evidence exists.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SessionTiming {
    pub started_at: Option<Timestamp>,
    pub ended_at: Option<Timestamp>,
    pub elapsed_ms: Option<u64>,
    pub active_ms: Option<u64>,
    pub model_ms: Option<f64>,
    pub active_complete: bool,
}

/// One measured whole-request throughput sample. `Debug` omits the request id.
#[derive(Clone, PartialEq)]
pub struct TimedRate {
    pub model: Option<ModelDetail>,
    pub request_id: Option<String>,
    pub started_at: Option<Timestamp>,
    pub completed_at: Option<Timestamp>,
    pub output_tokens: u64,
    pub duration_ms: f64,
    pub ttft_ms: Option<f64>,
    pub tokens_per_second: f64,
}

impl fmt::Debug for TimedRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TimedRate")
            .field("model", &self.model)
            .field("request_id", &self.request_id.is_some())
            .field("completed_at", &self.completed_at)
            .field("output_tokens", &self.output_tokens)
            .field("duration_ms", &self.duration_ms)
            .field("tokens_per_second", &self.tokens_per_second)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct SpeedSummary {
    pub latest: Option<TimedRate>,
    pub weighted_tokens_per_second: Option<f64>,
    pub measured_output: u64,
    pub measured_duration_ms: f64,
    pub measured_requests: usize,
    pub total_requests: usize,
    pub complete: bool,
}

/// Latest confirmed prompt occupancy against the applicable window. `Debug`
/// omits the compaction epoch text.
#[derive(Clone, Default, PartialEq)]
pub struct ContextUsage {
    pub used: Option<u64>,
    pub capacity: Option<u64>,
    pub class: AggregationClass,
    pub recorded_at: Option<Timestamp>,
    pub model: Option<ModelDetail>,
    pub compaction_epoch: Option<String>,
}

impl fmt::Debug for ContextUsage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ContextUsage")
            .field("used", &self.used)
            .field("capacity", &self.capacity)
            .field("class", &self.class)
            .field("recorded_at", &self.recorded_at)
            .field("model", &self.model)
            .field("compaction_epoch", &self.compaction_epoch.is_some())
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CompactionScope {
    Event,
    Aggregate,
    #[default]
    Unknown,
}

/// One observed compaction marker. `Debug` omits ids, method text, and summary.
#[derive(Clone, PartialEq)]
pub struct CompactionObservation {
    pub id: String,
    pub timestamp: Option<Timestamp>,
    pub method: Option<String>,
    pub trigger: Option<String>,
    pub summary: Option<BoundedText>,
    pub before_tokens: Option<u64>,
    pub after_tokens: Option<u64>,
    pub duration_ms: Option<f64>,
    pub usage: Option<UsageObservation>,
    pub scope: CompactionScope,
    pub provenance: Provenance,
}

impl fmt::Debug for CompactionObservation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompactionObservation")
            .field("timestamp", &self.timestamp)
            .field("before_tokens", &self.before_tokens)
            .field("after_tokens", &self.after_tokens)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CompactionSummary {
    pub observed: usize,
    pub history_complete: bool,
    pub known_cost_usd: Option<f64>,
    pub cost_scope: CompactionScope,
    pub unpriced: usize,
    pub latest: Option<CompactionObservation>,
}

/// Measured facts that need record-level evidence. The default is entirely Unknown.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionMetrics {
    pub timing: SessionTiming,
    pub speed: SpeedSummary,
    pub context: ContextUsage,
    pub compactions: CompactionSummary,
}

/// One turn's recorded span. `ended_at` is `None` while the turn is open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TurnInterval {
    pub started_at: Timestamp,
    pub ended_at: Option<Timestamp>,
}

/// One page of record-level session detail, read on demand and never cached in
/// the session summary. Requests, compactions and events share the page budget.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SessionDetailPage {
    pub requests: Vec<UsageObservation>,
    pub compactions: Vec<CompactionObservation>,
    pub recent: Vec<Event>,
    pub next: Option<PageAnchor>,
    pub has_more: bool,
    pub truncated: bool,
}

/// One session: cheap discovery metadata plus hydrated facts. `Debug` is content-free.
#[derive(Clone, PartialEq)]
pub struct SessionSummary {
    pub meta: SessionMetadata,
    pub usage: Option<UsageSummary>,
    pub metrics: SessionMetrics,
    pub lifecycle: Lifecycle,
    pub unfinished_work: bool,
    pub last_event_at: Option<Timestamp>,
    pub last_completion: Option<Timestamp>,
    pub displayed_event_count: usize,
    pub hidden_records: usize,
    pub server_tools: Option<ServerToolCounts>,
    pub process: ProcessAssociation,
    pub warnings: Vec<Warning>,
}

impl SessionSummary {
    /// A session known only by its discovery metadata: every hydrated fact is
    /// Unknown (no usage, default metrics, `Lifecycle::Unknown`). It is not a
    /// reported summary.
    pub fn unhydrated(meta: SessionMetadata) -> Self {
        Self {
            meta,
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
            warnings: Vec::new(),
        }
    }
}

impl fmt::Debug for SessionSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SessionSummary")
            .field("meta", &self.meta)
            .field("last_completion", &self.last_completion)
            .field("unfinished_work", &self.unfinished_work)
            .field("displayed_event_count", &self.displayed_event_count)
            .field("hidden_records", &self.hidden_records)
            .field("lifecycle", &self.lifecycle)
            .field("process", &self.process)
            .field("warnings", &self.warnings.len())
            .finish_non_exhaustive()
    }
}

/// Total order: timestamp (`None` first), then source order, then stable ID.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct PageAnchor {
    pub timestamp: Option<Timestamp>,
    pub source_order: u64,
    pub stable_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageDirection {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PageRequest {
    pub max_events: usize,
    pub max_bytes: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct TranscriptPage {
    pub events: Vec<Event>,
    pub diagnostics: Vec<DiagnosticEntry>,
    pub next: Option<PageAnchor>,
    pub has_more: bool,
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub label: StatusLabel,
    pub evidence: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusLabel {
    Active,
    Recent,
    Quiet,
    PossiblyStalled,
    Complete,
    Failed,
    Cancelled,
    Unavailable,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Thresholds {
    pub quiet: Duration,
    pub stall: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_text_debug_hides_preview() {
        let t = BoundedText {
            preview: "TOPSECRET".into(),
            byte_len: 9,
            truncated: false,
            locator: None,
        };
        let dbg = format!("{t:?}");
        assert!(!dbg.contains("TOPSECRET"));
        assert!(dbg.contains("byte_len"));
    }

    #[test]
    fn anchor_orders_missing_timestamp_first_then_order_then_id() {
        let a = |t, o, id: &str| PageAnchor {
            timestamp: t,
            source_order: o,
            stable_id: id.into(),
        };
        assert!(a(None, 9, "z") < a(Some(0), 0, "a"));
        assert!(a(Some(1), 1, "z") < a(Some(1), 2, "a"));
        assert!(a(Some(1), 1, "a") < a(Some(1), 1, "b"));
    }
}
