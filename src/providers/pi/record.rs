//! Pure interpretation of native Pi session records, pinned to
//! earendil-works/pi `packages/coding-agent/docs/session-format.md`,
//! `session-manager.ts` (`SessionHeader`/`SessionEntry`) and
//! `packages/ai/src/types.ts` (`Usage`, `AssistantMessage`).
//!
//! Only shapes those sources actually record are interpreted. A record of an
//! unknown `type` keeps its identity/tree fields and stays an [`PiEntryKind::Unknown`]
//! diagnostic; a record that cannot supply an entry ID is [`PiRecordError::Malformed`]
//! and is skipped by the caller. Errors never carry record text.
//!
//! Pi's `Usage` is additive: `input` excludes `cacheRead`/`cacheWrite`, `output`
//! already contains `reasoning`, and `cacheWrite1h` is a subset of `cacheWrite`.
//! `totalTokens` is a recorded cross-check, never the source of category semantics.

use std::fmt;

use chrono::DateTime;
use serde_json::{Map, Value};

use crate::domain::{
    InputAccounting, ModelDetail, MoneyFact, MoneySource, OutputAccounting, Provenance,
    RecordedMoney, RequestMetadata, Timestamp, TokenAccounting, TokenCategories, UsageObservation,
    UsageScope,
};

/// Longest entry/record/request ID accepted.
const ID_MAX: usize = 256;
/// Longest model/provider/effort/custom-type text kept.
const TEXT_MAX: usize = 256;
/// Longest recorded record type kept for diagnostics.
const TYPE_MAX: usize = 64;

fn clean(text: &str, max: usize) -> Option<String> {
    (!text.is_empty() && text.len() <= max && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

fn string_field(object: &Map<String, Value>, key: &str, max: usize) -> Option<String> {
    clean(object.get(key)?.as_str()?, max)
}

fn parse_time(text: &str) -> Option<Timestamp> {
    DateTime::parse_from_rfc3339(text)
        .ok()
        .map(|t| t.timestamp_millis())
}

/// Identity and metadata of the first line of a Pi session file.
#[derive(Clone, PartialEq, Eq)]
pub struct PiHeader {
    pub version: Option<u32>,
    pub id: String,
    pub timestamp: Option<Timestamp>,
    pub cwd: Option<String>,
    /// Session file/id this session was created from (`/fork`, `/clone`).
    pub parent_session: Option<String>,
}

impl fmt::Debug for PiHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PiHeader")
            .field("version", &self.version)
            .field("id_len", &self.id.len())
            .field("timestamp", &self.timestamp)
            .field("cwd", &self.cwd.is_some())
            .field("parent_session", &self.parent_session.is_some())
            .finish()
    }
}

/// A supported Pi header version. v2 is the tree format and v3 only renamed the
/// extension message role; both keep `id`/`parentId`. v1 predates entry IDs.
pub fn supported_version(version: Option<u32>) -> bool {
    matches!(version, Some(2 | 3))
}

/// The header of a session file: only a top-level `type: "session"` object with a
/// usable `id`.
pub fn parse_header(value: &Value) -> Option<PiHeader> {
    let object = value.as_object()?;
    if object.get("type")?.as_str()? != "session" {
        return None;
    }
    Some(PiHeader {
        version: object
            .get("version")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
        id: string_field(object, "id", ID_MAX)?,
        timestamp: object
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_time),
        cwd: string_field(object, "cwd", 4096),
        parent_session: string_field(object, "parentSession", 4096),
    })
}

/// A record that cannot be interpreted. Neither variant contains record text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiRecordError {
    /// The record is missing its identity or a field the shape requires.
    Malformed,
    /// A record that is not a session entry (the header) or an unknown role.
    Unsupported,
}

impl fmt::Display for PiRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Malformed => "malformed Pi record",
            Self::Unsupported => "unsupported Pi record",
        })
    }
}

impl std::error::Error for PiRecordError {}

/// The role of a Pi message. Only roles the producer persists are named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PiRole {
    System,
    User,
    Assistant,
    ToolResult,
    Custom,
    Unknown,
}

impl PiRole {
    fn parse(text: &str) -> Self {
        match text {
            "system" => Self::System,
            "user" => Self::User,
            "assistant" => Self::Assistant,
            "toolResult" => Self::ToolResult,
            "custom" => Self::Custom,
            _ => Self::Unknown,
        }
    }
}

/// The payload of one supported record. Message bodies, summaries, tool output and
/// extension data are never retained here; only the facts an index/accounting fold
/// needs, which are re-read from the source for display.
#[derive(Clone, PartialEq)]
pub enum PiEntryKind {
    Message {
        role: PiRole,
        /// The message has a `usage` object (an assistant call).
        usage: bool,
        /// Confirmed provider response identity, when recorded.
        response_id: Option<String>,
    },
    ModelChange {
        provider: String,
        model_id: String,
    },
    ThinkingLevelChange {
        level: String,
    },
    Usage {
        kind: String,
        provider: String,
        model: String,
    },
    Compaction {
        first_kept: Option<String>,
        tokens_before: Option<u64>,
        usage: bool,
    },
    BranchSummary {
        from_id: Option<String>,
        usage: bool,
    },
    ContextEdit {
        target_id: Option<String>,
        replacement: bool,
    },
    SessionInfo {
        name: Option<String>,
    },
    Custom {
        custom_type: Option<String>,
    },
    /// A record of a type this adapter does not interpret. Its identity and tree
    /// position are still kept so it can be diagnosed and traversed.
    Unknown {
        record_type: String,
    },
}

impl fmt::Debug for PiEntryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let named = |f: &mut fmt::Formatter<'_>, name: &str| f.write_str(name);
        match self {
            Self::Message {
                role,
                usage,
                response_id,
            } => f
                .debug_struct("Message")
                .field("role", role)
                .field("usage", usage)
                .field("response_id", &response_id.is_some())
                .finish(),
            Self::ModelChange { .. } => named(f, "ModelChange"),
            Self::ThinkingLevelChange { .. } => named(f, "ThinkingLevelChange"),
            Self::Usage { .. } => named(f, "Usage"),
            Self::Compaction {
                first_kept,
                tokens_before,
                usage,
            } => f
                .debug_struct("Compaction")
                .field("first_kept", &first_kept.is_some())
                .field("tokens_before", tokens_before)
                .field("usage", usage)
                .finish(),
            Self::BranchSummary { from_id, usage } => f
                .debug_struct("BranchSummary")
                .field("from_id", &from_id.is_some())
                .field("usage", usage)
                .finish(),
            Self::ContextEdit {
                target_id,
                replacement,
            } => f
                .debug_struct("ContextEdit")
                .field("target_id", &target_id.is_some())
                .field("replacement", replacement)
                .finish(),
            Self::SessionInfo { name } => f
                .debug_struct("SessionInfo")
                .field("name", &name.is_some())
                .finish(),
            Self::Custom { custom_type } => f
                .debug_struct("Custom")
                .field("custom_type", &custom_type.is_some())
                .finish(),
            Self::Unknown { .. } => named(f, "Unknown"),
        }
    }
}

/// Identity, tree position and interpreted payload of one record.
#[derive(Clone, PartialEq)]
pub struct PiEntry {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp: Option<Timestamp>,
    /// Zero-based position in the record stream.
    pub ordinal: u64,
    /// Physical byte offset of the record.
    pub offset: u64,
    pub kind: PiEntryKind,
}

impl fmt::Debug for PiEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PiEntry")
            .field("id_len", &self.id.len())
            .field("parent_id", &self.parent_id.is_some())
            .field("timestamp", &self.timestamp)
            .field("ordinal", &self.ordinal)
            .field("offset", &self.offset)
            .field("kind", &self.kind)
            .finish()
    }
}

fn required_string(
    object: &Map<String, Value>,
    key: &str,
    max: usize,
) -> Result<String, PiRecordError> {
    string_field(object, key, max).ok_or(PiRecordError::Malformed)
}

fn optional_string(object: &Map<String, Value>, key: &str, max: usize) -> Option<String> {
    string_field(object, key, max)
}

fn usage_present(object: &Map<String, Value>, key: &str) -> Result<bool, PiRecordError> {
    match object.get(key) {
        None => Ok(false),
        Some(Value::Object(_)) => Ok(true),
        Some(_) => Err(PiRecordError::Malformed),
    }
}

/// Interprets one JSONL record at `ordinal`/`offset`. The session header is not an
/// entry and is [`PiRecordError::Unsupported`]; an unknown record type is kept as
/// [`PiEntryKind::Unknown`] rather than dropped.
pub fn parse_entry(value: &Value, ordinal: u64, offset: u64) -> Result<PiEntry, PiRecordError> {
    let object = value.as_object().ok_or(PiRecordError::Malformed)?;
    let record_type = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or(PiRecordError::Malformed)?;
    if record_type == "session" {
        return Err(PiRecordError::Unsupported);
    }
    let id = required_string(object, "id", ID_MAX)?;
    let parent_id = match object.get("parentId") {
        None | Some(Value::Null) => None,
        Some(Value::String(parent)) => Some(clean(parent, ID_MAX).ok_or(PiRecordError::Malformed)?),
        Some(_) => return Err(PiRecordError::Malformed),
    };
    let timestamp = object
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(parse_time);
    let kind = match record_type {
        "message" => {
            let message = object
                .get("message")
                .and_then(Value::as_object)
                .ok_or(PiRecordError::Malformed)?;
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .ok_or(PiRecordError::Malformed)?;
            PiEntryKind::Message {
                role: PiRole::parse(role),
                usage: usage_present(message, "usage")?,
                response_id: optional_string(message, "responseId", ID_MAX),
            }
        }
        "model_change" => PiEntryKind::ModelChange {
            provider: required_string(object, "provider", TEXT_MAX)?,
            model_id: required_string(object, "modelId", TEXT_MAX)?,
        },
        "thinking_level_change" => PiEntryKind::ThinkingLevelChange {
            level: required_string(object, "thinkingLevel", TEXT_MAX)?,
        },
        "usage" => {
            if !usage_present(object, "usage")? {
                return Err(PiRecordError::Malformed);
            }
            PiEntryKind::Usage {
                kind: optional_string(object, "kind", TEXT_MAX).unwrap_or_else(|| "usage".into()),
                provider: required_string(object, "provider", TEXT_MAX)?,
                model: required_string(object, "model", TEXT_MAX)?,
            }
        }
        "compaction" => PiEntryKind::Compaction {
            first_kept: optional_string(object, "firstKeptEntryId", ID_MAX),
            tokens_before: object.get("tokensBefore").and_then(Value::as_u64),
            usage: usage_present(object, "usage")?,
        },
        "branch_summary" => PiEntryKind::BranchSummary {
            from_id: optional_string(object, "fromId", ID_MAX),
            usage: usage_present(object, "usage")?,
        },
        "context_edit" => PiEntryKind::ContextEdit {
            target_id: optional_string(object, "targetId", ID_MAX),
            replacement: object
                .get("replacement")
                .is_some_and(|value| !value.is_null()),
        },
        "session_info" => PiEntryKind::SessionInfo {
            name: optional_string(object, "name", TEXT_MAX),
        },
        "custom" | "custom_message" => PiEntryKind::Custom {
            custom_type: optional_string(object, "customType", TEXT_MAX),
        },
        other => PiEntryKind::Unknown {
            record_type: other.chars().take(TYPE_MAX).collect(),
        },
    };
    Ok(PiEntry {
        id,
        parent_id,
        timestamp,
        ordinal,
        offset,
        kind,
    })
}

/// The `usage` object of a record (nested under `usage`) or a bare usage object.
fn usage_map(value: &Value) -> Option<&Map<String, Value>> {
    let object = value.as_object()?;
    if let Some(usage) = object.get("usage") {
        return usage.as_object();
    }
    const KEYS: [&str; 6] = [
        "input",
        "output",
        "cacheRead",
        "cacheWrite",
        "totalTokens",
        "cost",
    ];
    KEYS.iter()
        .any(|key| object.contains_key(*key))
        .then_some(object)
}

fn counter(object: &Map<String, Value>, key: &str) -> Option<u64> {
    object.get(key).and_then(Value::as_u64)
}

/// Pi's additive token total: input + output + read + write. `reasoning` is already
/// inside output and `cacheWrite1h` inside `cacheWrite`, so neither is added.
/// Any missing required counter leaves the total Unknown.
pub fn additive_total(categories: &TokenCategories) -> Option<u64> {
    categories
        .input?
        .checked_add(categories.output?)?
        .checked_add(categories.cache_read?)?
        .checked_add(categories.cache_write?)
}

/// The `totalTokens` a record reports, if any. Used only as a cross-check.
pub fn reported_total(value: &Value) -> Option<u64> {
    usage_map(value)?.get("totalTokens").and_then(Value::as_u64)
}

/// Whether the recorded `totalTokens` disagrees with the additive schema.
pub fn total_conflicts(value: &Value, categories: &TokenCategories) -> bool {
    match (reported_total(value), additive_total(categories)) {
        (Some(reported), Some(additive)) => reported != additive,
        _ => false,
    }
}

/// Whether a record that could not be interpreted might still have carried a model
/// call. Used by the fold to keep accounting coverage partial instead of reporting a
/// complete total after dropping usage.
pub fn record_may_carry_usage(value: &Value) -> bool {
    let Some(object) = value.as_object() else {
        return false;
    };
    if object.contains_key("usage") {
        return true;
    }
    match object.get("type").and_then(Value::as_str) {
        Some("usage" | "compaction" | "branch_summary") => true,
        Some("message") => object
            .get("message")
            .and_then(Value::as_object)
            .is_some_and(|message| {
                message.get("role").and_then(Value::as_str) == Some("assistant")
                    || message.contains_key("usage")
            }),
        _ => false,
    }
}

/// Whether a recorded `cost.total` exists but is not a usable non-negative finite
/// amount. A record without a cost object is not an anomaly.
pub fn invalid_money(value: &Value) -> bool {
    let Some(usage) = usage_map(value) else {
        return false;
    };
    let Some(cost) = usage.get("cost") else {
        return false;
    };
    let Some(cost) = cost.as_object() else {
        return true;
    };
    match cost.get("total") {
        None => false,
        Some(total) => total
            .as_f64()
            .filter(|usd| usd.is_finite() && *usd >= 0.0)
            .is_none(),
    }
}

/// The recorded cost. Pi marks a provider-reported billed total; otherwise the
/// number is the producer's own catalog estimate and stays a retained estimate.
fn recorded_money(usage: &Map<String, Value>) -> RecordedMoney {
    let cost = usage.get("cost").and_then(Value::as_object);
    let total = cost
        .and_then(|cost| cost.get("total"))
        .and_then(Value::as_f64)
        .filter(|usd| usd.is_finite() && *usd >= 0.0);
    let reported = cost
        .and_then(|cost| cost.get("totalFromProvider"))
        .and_then(Value::as_bool)
        == Some(true);
    let mut money = RecordedMoney::default();
    match total {
        Some(usd) if reported => {
            money.reported_api = Some(MoneyFact {
                usd,
                source: MoneySource::ReportedApi,
                as_of: None,
            });
        }
        Some(usd) => {
            money.saved_estimate = Some(MoneyFact {
                usd,
                source: MoneySource::SavedCatalogEstimate,
                as_of: None,
            });
        }
        None => {}
    }
    money
}

fn categories(usage: &Map<String, Value>) -> TokenCategories {
    let mut categories = TokenCategories {
        input: counter(usage, "input"),
        cache_read: counter(usage, "cacheRead"),
        cache_write: counter(usage, "cacheWrite"),
        cache_write_1h: None,
        output: counter(usage, "output"),
        reasoning: counter(usage, "reasoning"),
        total: None,
        ..TokenCategories::default()
    };
    // `cacheWrite1h` is a subset of `cacheWrite`; an impossible split is not kept.
    if let (Some(split), Some(write)) = (counter(usage, "cacheWrite1h"), categories.cache_write)
        && split <= write
    {
        categories.cache_write_1h = Some(split);
    }
    // `reasoning` is a subset of `output`; an impossible split is not kept.
    if let (Some(reasoning), Some(output)) = (categories.reasoning, categories.output)
        && reasoning > output
    {
        categories.reasoning = None;
    }
    categories.total = additive_total(&categories);
    categories
}

/// Whether the required additive counters were all recorded as non-negative
/// integers. Reasoning is optional and an already-inside-output subset.
pub fn categories_incomplete(categories: &TokenCategories) -> bool {
    categories.input.is_none()
        || categories.output.is_none()
        || categories.cache_read.is_none()
        || categories.cache_write.is_none()
        || additive_total(categories).is_none()
}

fn millis(value: Option<&Value>) -> Option<f64> {
    value?
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn epoch_ms(value: Option<&Value>) -> Option<Timestamp> {
    value?.as_i64()
}

/// One recorded model call. `value` is the message/entry that owns the usage object
/// (so `responseId`, `timestamp` and a recorded `durationMs` are visible); a bare
/// usage object is accepted too. `model` is the route/identity the caller proved.
///
/// Pi records no TTFT. A recorded assistant `durationMs` is measured whole-request
/// model time; no request completion, session end or active interval is derived.
pub fn parse_pi_usage(
    value: &Value,
    model: Option<ModelDetail>,
    provenance: Provenance,
) -> UsageObservation {
    let categories = usage_map(value).map(categories).unwrap_or_default();
    let money = usage_map(value).map(recorded_money).unwrap_or_default();
    let object = value.as_object();
    let request = RequestMetadata {
        id: object
            .and_then(|object| object.get("responseId"))
            .and_then(Value::as_str)
            .and_then(|id| clean(id, ID_MAX)),
        purpose: Default::default(),
        started_at: object.and_then(|object| epoch_ms(object.get("timestamp"))),
        completed_at: None,
        duration_ms: object.and_then(|object| millis(object.get("durationMs"))),
        ttft_ms: None,
        service_tier: None,
        inference_region: None,
    };
    UsageObservation {
        model,
        scope: UsageScope::Message,
        categories,
        accounting: TokenAccounting {
            input: InputAccounting::AdditiveCaches,
            output: OutputAccounting::IncludesReasoning,
        },
        request: Some(request),
        money,
        provenance,
    }
}
