//! Hermes `state.db` access: the narrow allowlist, the detected shape, fixed
//! bounded projections, and checked timestamp conversion.
//!
//! The allowlist is trusted-query discipline, not just SQLite's column authorizer:
//! `model_config` is never selected raw; discovery extracts only the fixed
//! `reasoning_config` keys. Credential/system-prompt columns and unrelated tables
//! are denied at the connection boundary. No statement here writes, migrates,
//! repairs, or loads an extension.

use std::path::Path;
use std::time::Instant;

use rusqlite::Connection;

use super::super::ProviderError;
use super::super::sqlite_guard::{Allowlist, open_read_only_with, shape_present};
use crate::ingest::text::preview;

/// Busy timeout of a standalone (non-sliced) Hermes connection.
pub(crate) const DEFAULT_BUSY_MS: u64 = 2000;
/// Longest recorded title retained by discovery.
pub(crate) const TITLE_BYTES: usize = 1024;
pub(crate) const TITLE_COLS: usize = 80;
/// Longest configured-effort string accepted from `reasoning_config`.
pub(crate) const EFFORT_MAX: usize = 32;
/// Largest `model_config` cell parsed as JSON. Larger cells are charged as bytes
/// and treated as missing optional metadata: no JSON function ever runs on them.
pub(crate) const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// Tables and the only columns of each that Hermes SQL may read.
static HERMES_ALLOWLIST: Allowlist = Allowlist {
    tables: &[
        (
            "sessions",
            &[
                "rowid",
                "id",
                "parent_session_id",
                "source",
                "model",
                "model_config",
                "started_at",
                "ended_at",
                "end_reason",
                "last_activity_at",
                "title",
                "archived",
                "message_count",
                "tool_call_count",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "reasoning_tokens",
                "api_call_count",
                "estimated_cost_usd",
                "actual_cost_usd",
                "cost_status",
                "cost_source",
                "billing_provider",
            ],
        ),
        (
            "messages",
            &[
                "id",
                "session_id",
                "role",
                "content",
                "tool_call_id",
                "tool_calls",
                "tool_name",
                "timestamp",
                "finish_reason",
                "token_count",
                "active",
                "compacted",
                "_compressed_summary",
                "display_kind",
                "display_metadata",
                "display_identity",
                "display_order",
                "message_uid",
            ],
        ),
        (
            "session_model_usage",
            &[
                "rowid",
                "session_id",
                "model",
                "billing_provider",
                "task",
                "api_call_count",
                "input_tokens",
                "output_tokens",
                "cache_read_tokens",
                "cache_write_tokens",
                "reasoning_tokens",
                "estimated_cost_usd",
                "actual_cost_usd",
                "cost_status",
                "cost_source",
                "first_seen",
                "last_seen",
            ],
        ),
        // Only the table-existence count used to force the WAL/shm handshake on open.
        ("sqlite_master", &["name"]),
        ("sqlite_schema", &["name"]),
    ],
    // Aggregates plus the fixed JSON/type guards used by projections; no file,
    // extension, regexp, or other text-producing function.
    functions: &[
        "count",
        "coalesce",
        "json_extract",
        "json_type",
        "json_valid",
        "length",
        "max",
        "min",
        "substr",
        "sum",
        "typeof",
    ],
};

/// Opens a Hermes database read-only behind the fixed allowlist. Exposed so the
/// access boundary itself can be exercised; the catalog uses the same opener.
pub fn open_read_only(path: &Path) -> Result<Connection, ProviderError> {
    open_with(path, DEFAULT_BUSY_MS)
}

pub(crate) fn open_with(path: &Path, busy_ms: u64) -> Result<Connection, ProviderError> {
    open_read_only_with(path, &HERMES_ALLOWLIST, busy_ms)
}

/// Opens with a deadline already installed for the opener's first schema work.
pub(crate) fn open_with_deadline(
    path: &Path,
    busy_ms: u64,
    deadline: Instant,
) -> Result<Connection, ProviderError> {
    super::super::sqlite_guard::open_read_only_with_deadline(
        path,
        &HERMES_ALLOWLIST,
        busy_ms,
        Some(deadline),
    )
}

/// Optional columns/tables probed once per connection. Discovery keeps every
/// session in a supported database visible even when a probe is absent; the
/// affected facts stay `Unknown`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HermesShape {
    /// Minimum shape: `sessions(id, started_at)` and
    /// `messages(id, session_id, role, content, timestamp)`.
    pub supported: bool,
    pub parent: bool,
    pub source: bool,
    pub model: bool,
    pub model_config: bool,
    pub ended: bool,
    pub end_reason: bool,
    pub last_activity: bool,
    pub title: bool,
    pub archived: bool,
    pub billing_provider: bool,
    pub message_count: bool,
    pub tool_call_count: bool,
    pub api_call_count: bool,
    pub input_tokens: bool,
    pub output_tokens: bool,
    pub cache_read: bool,
    pub cache_write: bool,
    pub reasoning_tokens: bool,
    pub estimated_cost: bool,
    pub actual_cost: bool,
    pub cost_status: bool,
    pub cost_source: bool,
    pub message_tool_call_id: bool,
    pub message_tool_calls: bool,
    pub message_tool_name: bool,
    pub message_finish_reason: bool,
    pub message_token_count: bool,
    pub message_active: bool,
    pub message_compacted: bool,
    pub message_summary: bool,
    pub message_display_kind: bool,
    pub message_display_metadata: bool,
    pub message_display_identity: bool,
    pub message_display_order: bool,
    pub message_uid: bool,
    pub usage: bool,
    pub usage_cost_status: bool,
    pub usage_cost_source: bool,
    pub usage_seen: bool,
}

impl HermesShape {
    /// Optional-column presence for one probe: `(present, description)`.
    fn fields(&self) -> impl Iterator<Item = (bool, &'static str)> {
        [
            (self.parent, "parent"),
            (self.source, "source"),
            (self.model, "model"),
            (self.model_config, "model_config"),
            (self.ended, "ended_at"),
            (self.end_reason, "end_reason"),
            (self.last_activity, "last_activity_at"),
            (self.title, "title"),
            (self.archived, "archived"),
            (self.billing_provider, "billing_provider"),
            (self.message_count, "message_count"),
            (self.tool_call_count, "tool_call_count"),
            (self.api_call_count, "api_call_count"),
            (self.input_tokens, "input_tokens"),
            (self.output_tokens, "output_tokens"),
            (self.cache_read, "cache_read_tokens"),
            (self.cache_write, "cache_write_tokens"),
            (self.reasoning_tokens, "reasoning_tokens"),
            (self.estimated_cost, "estimated_cost_usd"),
            (self.actual_cost, "actual_cost_usd"),
            (self.cost_status, "cost_status"),
            (self.cost_source, "cost_source"),
            (self.message_tool_call_id, "tool_call_id"),
            (self.message_tool_calls, "tool_calls"),
            (self.message_tool_name, "tool_name"),
            (self.message_finish_reason, "finish_reason"),
            (self.message_token_count, "token_count"),
            (self.message_active, "active"),
            (self.message_compacted, "compacted"),
            (self.message_summary, "_compressed_summary"),
            (self.message_display_kind, "display_kind"),
            (self.message_display_metadata, "display_metadata"),
            (self.message_display_identity, "display_identity"),
            (self.message_display_order, "display_order"),
            (self.message_uid, "message_uid"),
            (self.usage, "session_model_usage"),
            (self.usage_cost_status, "session_model_usage.cost_status"),
            (self.usage_cost_source, "session_model_usage.cost_source"),
            (self.usage_seen, "session_model_usage.seen"),
        ]
        .into_iter()
    }

    /// Number of known optional fields that this database lacks. Content-free, so
    /// it can be shown as a limit warning.
    pub fn missing_optional(&self) -> usize {
        self.fields().filter(|(present, _)| !present).count()
    }
}

fn column(conn: &Connection, table: &str, column: &str) -> Result<bool, ProviderError> {
    shape_present(conn, &format!("SELECT {column} FROM {table} LIMIT 0"))
}

/// Probes the known shape without migrating anything. A supported minimum needs
/// the sessions identity/start and the message identity/content/time columns;
/// every other field is optional and independently recorded.
pub(crate) fn detect_shape(conn: &Connection) -> Result<HermesShape, ProviderError> {
    let supported = shape_present(conn, "SELECT id, started_at FROM sessions LIMIT 0")?
        && shape_present(
            conn,
            "SELECT id, session_id, role, content, timestamp FROM messages LIMIT 0",
        )?;
    let mut shape = HermesShape {
        supported,
        ..HermesShape::default()
    };
    if !supported {
        return Ok(shape);
    }
    let s = |name| column(conn, "sessions", name);
    let m = |name| column(conn, "messages", name);
    shape.parent = s("parent_session_id")?;
    shape.source = s("source")?;
    shape.model = s("model")?;
    shape.model_config = s("model_config")?;
    shape.ended = s("ended_at")?;
    shape.end_reason = s("end_reason")?;
    shape.last_activity = s("last_activity_at")?;
    shape.title = s("title")?;
    shape.archived = s("archived")?;
    shape.billing_provider = s("billing_provider")?;
    shape.message_count = s("message_count")?;
    shape.tool_call_count = s("tool_call_count")?;
    shape.api_call_count = s("api_call_count")?;
    shape.input_tokens = s("input_tokens")?;
    shape.output_tokens = s("output_tokens")?;
    shape.cache_read = s("cache_read_tokens")?;
    shape.cache_write = s("cache_write_tokens")?;
    shape.reasoning_tokens = s("reasoning_tokens")?;
    shape.estimated_cost = s("estimated_cost_usd")?;
    shape.actual_cost = s("actual_cost_usd")?;
    shape.cost_status = s("cost_status")?;
    shape.cost_source = s("cost_source")?;
    shape.message_tool_call_id = m("tool_call_id")?;
    shape.message_tool_calls = m("tool_calls")?;
    shape.message_tool_name = m("tool_name")?;
    shape.message_finish_reason = m("finish_reason")?;
    shape.message_token_count = m("token_count")?;
    shape.message_active = m("active")?;
    shape.message_compacted = m("compacted")?;
    shape.message_summary = m("_compressed_summary")?;
    shape.message_display_kind = m("display_kind")?;
    shape.message_display_metadata = m("display_metadata")?;
    shape.message_display_identity = m("display_identity")?;
    shape.message_display_order = m("display_order")?;
    shape.message_uid = m("message_uid")?;
    shape.usage = shape_present(
        conn,
        "SELECT session_id, model FROM session_model_usage LIMIT 0",
    )?;
    if shape.usage {
        let u = |name| column(conn, "session_model_usage", name);
        shape.usage_cost_status = u("cost_status")?;
        shape.usage_cost_source = u("cost_source")?;
        shape.usage_seen = shape_present(
            conn,
            "SELECT first_seen, last_seen FROM session_model_usage LIMIT 0",
        )?;
    }
    Ok(shape)
}

/// A stored epoch-seconds cell as checked epoch milliseconds. Non-finite values,
/// cells outside Hermes' own `0..=4_200_000_000` window, and nonsense storage
/// types stay missing rather than becoming zero.
pub(crate) fn epoch_ms(seconds: Option<f64>) -> Option<i64> {
    let seconds = seconds?;
    if !seconds.is_finite() || !(0.0..=4_200_000_000.0).contains(&seconds) {
        return None;
    }
    let millis = (seconds * 1000.0).round();
    // The window keeps `millis` far inside i64; the check documents that.
    (0.0..=4_200_000_000_000.0)
        .contains(&millis)
        .then_some(millis as i64)
}

/// Bounded, display-safe single-line text; `None` when nothing survives.
pub(crate) fn short(text: &str) -> Option<String> {
    let collapsed = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let out = preview(&collapsed, TITLE_BYTES, TITLE_COLS).preview;
    (!out.trim().is_empty()).then_some(out)
}
