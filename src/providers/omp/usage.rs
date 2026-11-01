//! OMP usage records mapped to shared usage observations, pinned to can1357/oh-my-pi
//! `6a86df8539fccf3ad7818081266e554022fc21bc` (`pi-catalog` `Usage`,
//! `pi-ai` `AssistantMessage`, `session-entries.ts` `ModelUsageEntry`).
//!
//! Counters follow the producer's `Usage` semantics: `input` is the non-cached
//! conversation input and the cache counters sit beside it (additive), `output`
//! already contains `reasoningTokens`, and `orchestration` is additive provider-side
//! work billed on top of the conversation legs. `totalTokens` is kept as the recorded
//! total only; it is never an occupancy figure. The saved `usage.cost` is unverified
//! money: the engine recomputes an API equivalent and both stay inspectable.

use chrono::DateTime;
use serde_json::{Map, Value};

use crate::domain::{
    InputAccounting, ModelDetail, OutputAccounting, Provenance, RecordedMoney, RequestMetadata,
    RequestPurpose, Timestamp, TokenAccounting, TokenCategories, UsageObservation, UsageScope,
};
use crate::ingest::text::preview;

/// Longest recorded model, provider, tier or purpose text kept.
const TEXT_MAX: usize = 256;

pub(super) fn text(value: Option<&Value>) -> Option<String> {
    let text = value?.as_str()?;
    if text.trim().is_empty() {
        return None;
    }
    let clean = preview(text, TEXT_MAX, TEXT_MAX).preview;
    (!clean.trim().is_empty()).then_some(clean)
}

/// A session's lifetime tokens: the non-cached input, the cache reads and writes
/// (additive beside it) and the output (which already contains reasoning). The
/// normalized legs already include provider-side orchestration, so the retained
/// orchestration subset labels are not added again. Any missing core
/// counter makes the total unknown rather than a partial sum.
pub fn omp_total(categories: &TokenCategories) -> Option<u64> {
    let cache_write = categories.cache_write.or_else(|| {
        categories
            .cache_write_5m?
            .checked_add(categories.cache_write_1h?)
    })?;
    categories
        .input?
        .checked_add(categories.output?)?
        .checked_add(categories.cache_read?)?
        .checked_add(cache_write)
}

pub(super) fn counter(object: &Map<String, Value>, key: &str) -> Option<u64> {
    object.get(key).and_then(Value::as_u64)
}

/// A finite millisecond figure; fractions are kept, negatives and non-numbers are not.
fn millis(value: Option<&Value>) -> Option<f64> {
    value?.as_f64().filter(|v| v.is_finite() && *v >= 0.0)
}

/// An epoch-millisecond timestamp recorded as a JSON number.
pub(super) fn epoch_ms(value: Option<&Value>) -> Option<Timestamp> {
    let value = value?;
    value.as_i64().or_else(|| {
        value
            .as_f64()
            .filter(|v| v.is_finite() && v.abs() < 9.0e15)
            .map(|v| v as i64)
    })
}

pub(super) fn entry_time(entry: &Value) -> Option<Timestamp> {
    DateTime::parse_from_rfc3339(entry.get("timestamp")?.as_str()?)
        .ok()
        .map(|t| t.timestamp_millis())
}

fn categories(usage: &Map<String, Value>) -> TokenCategories {
    let cache_write = counter(usage, "cacheWrite");
    // The 5m/1h split is only the producer's breakdown of `cacheWrite`: it is used
    // when it adds up, never to invent a split of a different total.
    let (cache_write_5m, cache_write_1h) = match usage.get("cttl").and_then(Value::as_object) {
        Some(split) => {
            let five = counter(split, "ephemeral5m");
            let hour = counter(split, "ephemeral1h");
            let sum = five.unwrap_or(0).checked_add(hour.unwrap_or(0));
            match (cache_write, sum) {
                (Some(total), Some(sum)) if (five.is_some() || hour.is_some()) && sum <= total => {
                    (Some(five.unwrap_or(0)), Some(hour.unwrap_or(0)))
                }
                _ => (None, None),
            }
        }
        None => (None, None),
    };
    let orchestration = usage.get("orchestration").and_then(Value::as_object);
    let orch = |key: &str| orchestration.and_then(|o| counter(o, key));
    TokenCategories {
        input: counter(usage, "input"),
        cached_input: None,
        cache_write,
        cache_write_5m,
        cache_write_1h,
        cache_read: counter(usage, "cacheRead"),
        output: counter(usage, "output"),
        reasoning: counter(usage, "reasoningTokens"),
        total: counter(usage, "totalTokens"),
        orchestration_input: orch("input"),
        orchestration_output: orch("output"),
        orchestration_cache_read: orch("cacheRead"),
    }
}

/// The saved `usage.cost.total`, kept as unverified money: OMP computes it from its
/// own catalog (or zero for subscriptions), so it is never a billed amount.
fn money(usage: &Map<String, Value>) -> RecordedMoney {
    let saved = usage
        .get("cost")
        .and_then(Value::as_object)
        .and_then(|cost| cost.get("total"))
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.0);
    RecordedMoney {
        unverified_usd: saved,
        ..RecordedMoney::default()
    }
}

/// One model call recorded by an `assistant` message or a `model_usage` entry as a
/// message-scope observation, or `None` for every other record (including an
/// assistant message with no `usage` object).
///
/// The recorded `message.serviceTier` (the tier the turn ran on, which the live
/// session setting may no longer match) becomes `RequestMetadata.service_tier`. The
/// pinned schema persists no inference region, so none is set. `model.effort` is the
/// effort in force at the call, which only the entry's branch knows; the hydration
/// fold binds it.
pub fn map_omp_usage(entry: &Value, provenance: Provenance) -> Option<UsageObservation> {
    let kind = entry.get("type")?.as_str()?;
    let (body, purpose, message) = match kind {
        "message" => {
            let message = entry.get("message")?.as_object()?;
            if message.get("role")?.as_str()? != "assistant" {
                return None;
            }
            (message, RequestPurpose::Conversation, Some(message))
        }
        "model_usage" => {
            let body = entry.as_object()?;
            let purpose = text(body.get("purpose")).unwrap_or_else(|| "unspecified".to_owned());
            (body, RequestPurpose::Auxiliary(purpose), None)
        }
        _ => return None,
    };
    let usage = body.get("usage")?.as_object()?;
    let model = text(body.get("model")).map(|id| ModelDetail {
        id,
        provider: text(body.get("provider")),
        variant: None,
        effort: None,
        configured_effort: None,
    });
    let entry_at = entry_time(entry);
    let request = match message {
        Some(message) => RequestMetadata {
            id: text(message.get("responseId")),
            purpose,
            started_at: epoch_ms(message.get("timestamp")),
            completed_at: epoch_ms(message.get("completedAt")).or(entry_at),
            duration_ms: millis(message.get("duration")),
            ttft_ms: millis(message.get("ttft")),
            service_tier: text(message.get("serviceTier")),
            inference_region: None,
        },
        None => RequestMetadata {
            purpose,
            completed_at: entry_at,
            ..RequestMetadata::default()
        },
    };
    Some(UsageObservation {
        model,
        scope: UsageScope::Message,
        categories: categories(usage),
        accounting: TokenAccounting {
            input: InputAccounting::AdditiveCaches,
            output: OutputAccounting::IncludesReasoning,
        },
        request: Some(request),
        money: money(usage),
        provenance,
    })
}

/// Server-side tool counts of one usage object (`usage.server`).
pub(super) fn server_tools(entry: &Value) -> Option<(u64, u64)> {
    let usage = match entry.get("type")?.as_str()? {
        "message" => entry.get("message")?.get("usage")?,
        "model_usage" => entry.get("usage")?,
        _ => return None,
    };
    let server = usage.get("server")?.as_object()?;
    Some((
        counter(server, "webSearch").unwrap_or(0),
        counter(server, "webFetch").unwrap_or(0),
    ))
}

/// A stable summary of the recorded counters, to tell a repeat of a call from a
/// different call under a reused entry ID.
pub(super) fn fingerprint(c: &TokenCategories) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (
        c.input,
        c.output,
        c.cache_read,
        c.cache_write,
        c.reasoning,
        c.orchestration_input,
        c.orchestration_output,
        c.orchestration_cache_read,
    )
        .hash(&mut hasher);
    hasher.finish()
}

/// The usage object of a call record (`message` assistant or `model_usage`).
fn usage_object(entry: &Value) -> Option<&Map<String, Value>> {
    match entry.get("type")?.as_str()? {
        "message" => {
            let message = entry.get("message")?.as_object()?;
            (message.get("role")?.as_str()? == "assistant")
                .then(|| message.get("usage")?.as_object())?
        }
        "model_usage" => entry.get("usage")?.as_object(),
        _ => None,
    }
}

/// The confirmed response ID and counter fingerprint of a call record, without
/// building a full observation; `None` for every other record.
pub(super) fn call_identity(entry: &Value) -> Option<(Option<String>, u64)> {
    let usage = usage_object(entry)?;
    let response = match entry.get("type")?.as_str()? {
        "message" => text(entry.get("message")?.get("responseId")),
        _ => None,
    };
    Some((response, fingerprint(&categories(usage))))
}

/// Whether `entry` is an assistant message, with or without a `usage` object.
pub(super) fn is_assistant(entry: &Value) -> bool {
    entry.get("type").and_then(Value::as_str) == Some("message")
        && entry
            .get("message")
            .and_then(|m| m.get("role"))
            .and_then(Value::as_str)
            == Some("assistant")
}
