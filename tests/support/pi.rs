//! Synthetic native Pi fixtures. Shapes follow earendil-works/pi
//! `docs/session-format.md`, `session-manager.ts` and `packages/ai/src/types.ts`;
//! every value is synthetic and every timestamp uses [`STAMP`].

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::support::NOW;

/// Fixed clock of every fixture record.
pub const STAMP: &str = "2026-10-30T00:00:00.000Z";
/// Epoch milliseconds of [`STAMP`].
pub const STAMP_MS: i64 = NOW;
/// The default synthetic model route of an assistant fixture.
pub const PROVIDER: &str = "anthropic";
pub const MODEL: &str = "claude-test";

/// A category-specific ISO timestamp.
pub fn stamp(offset_secs: i64) -> String {
    let ms = STAMP_MS + offset_secs * 1000;
    chrono::DateTime::from_timestamp_millis(ms)
        .expect("fixture clock")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

/// A Pi session header (`SessionHeader`, no `id`/`parentId` tree fields).
pub fn header(id: &str, parent: Option<&str>) -> Value {
    let mut header = json!({
        "type": "session", "version": 3, "id": id, "timestamp": STAMP, "cwd": "/work/demo"
    });
    if let Some(parent) = parent {
        header["parentSession"] = json!(parent);
    }
    header
}

/// A header of an explicit version (`0` omits the field, as v1 does).
pub fn header_version(id: &str, version: u32) -> Value {
    let mut header = header(id, None);
    if version == 0 {
        header.as_object_mut().expect("header").remove("version");
    } else {
        header["version"] = json!(version);
    }
    header
}

/// An entry of `kind`; `payload` object fields are merged at the top level.
pub fn entry(id: &str, parent: Option<&str>, kind: &str, payload: Value) -> Value {
    let mut entry = json!({"type": kind, "id": id, "parentId": parent, "timestamp": STAMP});
    if let (Some(target), Some(extra)) = (entry.as_object_mut(), payload.as_object()) {
        for (key, value) in extra {
            target.insert(key.clone(), value.clone());
        }
    }
    entry
}

/// A message entry. Assistant content is an array of text blocks; user and system
/// content is a plain string. An assistant gets the synthetic route and stop reason.
pub fn message(
    id: &str,
    parent: Option<&str>,
    role: &str,
    content: &str,
    usage: Option<Value>,
) -> Value {
    let content = if matches!(role, "assistant" | "toolResult") {
        json!([{"type": "text", "text": content}])
    } else {
        json!(content)
    };
    let mut payload = json!({"role": role, "content": content, "timestamp": STAMP_MS});
    match role {
        "assistant" => {
            payload["api"] = json!("anthropic-messages");
            payload["provider"] = json!(PROVIDER);
            payload["model"] = json!(MODEL);
            payload["stopReason"] = json!("stop");
        }
        "toolResult" => {
            payload["toolCallId"] = json!("call-1");
            payload["toolName"] = json!("read");
            payload["isError"] = json!(false);
        }
        _ => {}
    }
    if let Some(usage) = usage {
        payload["usage"] = usage;
    }
    entry(id, parent, "message", json!({"message": payload}))
}

/// A producer-shaped `Usage` with every required counter explicit. `reasoning` is
/// absent (optional); the recorded total equals the additive categories.
pub fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Value {
    json!({
        "input": input, "output": output, "cacheRead": cache_read,
        "cacheWrite": cache_write, "totalTokens": input + output + cache_read + cache_write
    })
}

/// Adds the reasoning subset (already inside `output`).
pub fn with_reasoning(mut usage: Value, reasoning: u64) -> Value {
    usage["reasoning"] = json!(reasoning);
    usage
}

/// Adds the producer's saved cost; `provider_reported` selects the billing origin.
pub fn with_cost(mut usage: Value, total: f64, provider_reported: bool) -> Value {
    usage["cost"] = json!({
        "input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0,
        "total": total, "totalFromProvider": provider_reported
    });
    usage
}

/// A model change entry (`provider` + `modelId`).
pub fn model_change(id: &str, parent: &str, provider: &str, model: &str) -> Value {
    entry(
        id,
        Some(parent),
        "model_change",
        json!({"provider": provider, "modelId": model}),
    )
}

/// A thinking-level change entry.
pub fn thinking_change(id: &str, parent: &str, level: &str) -> Value {
    entry(
        id,
        Some(parent),
        "thinking_level_change",
        json!({"thinkingLevel": level}),
    )
}

/// A session-info (display name) entry.
pub fn session_info(id: &str, parent: &str, name: &str) -> Value {
    entry(id, Some(parent), "session_info", json!({"name": name}))
}

/// A model-attributed usage entry outside the conversation tree.
pub fn usage_entry(
    id: &str,
    parent: &str,
    kind: &str,
    provider: &str,
    model: &str,
    usage: Value,
) -> Value {
    entry(
        id,
        Some(parent),
        "usage",
        json!({"kind": kind, "provider": provider, "model": model, "usage": usage}),
    )
}

/// A tool result with a recorded tool duration (never model time).
pub fn tool_result(id: &str, parent: &str, duration_ms: u64) -> Value {
    let mut entry = message(id, Some(parent), "toolResult", "done", None);
    entry["message"]["durationMs"] = json!(duration_ms);
    entry
}

/// A compaction marker with optional attributable summary usage.
pub fn compaction(id: &str, parent: &str, usage: Option<Value>) -> Value {
    let mut entry = entry(
        id,
        Some(parent),
        "compaction",
        json!({"summary": "synthetic summary", "firstKeptEntryId": parent, "tokensBefore": 500}),
    );
    if let Some(usage) = usage {
        entry["usage"] = usage;
    }
    entry
}

/// Sets an entry's ISO timestamp.
pub fn at(mut entry: Value, iso: &str) -> Value {
    entry["timestamp"] = json!(iso);
    entry
}

/// Overrides an assistant message's provider/model.
pub fn with_model(mut entry: Value, provider: &str, model: &str) -> Value {
    entry["message"]["provider"] = json!(provider);
    entry["message"]["model"] = json!(model);
    entry
}

/// Sets an assistant message's confirmed provider response id.
pub fn with_response(mut entry: Value, response: &str) -> Value {
    entry["message"]["responseId"] = json!(response);
    entry
}

/// Sets an assistant message's recorded whole-request duration.
pub fn with_duration(mut entry: Value, duration_ms: u64) -> Value {
    entry["message"]["durationMs"] = json!(duration_ms);
    entry
}

/// Sets the Pi thinking level requested for one assistant response.
pub fn with_thinking(mut entry: Value, level: &str) -> Value {
    entry["message"]["thinkingLevel"] = json!(level);
    entry
}

/// Sets the provider-native effort of one assistant response.
pub fn with_provider_effort(mut entry: Value, effort: &str) -> Value {
    entry["message"]["providerThinkingLevel"] = json!(effort);
    entry
}

fn path_for(root: &Path, bucket: &str, name: &str) -> PathBuf {
    root.join(bucket).join(format!("{name}.jsonl"))
}

fn lines(entries: &[Value]) -> Vec<u8> {
    let mut body = Vec::new();
    for entry in entries {
        body.extend_from_slice(entry.to_string().as_bytes());
        body.push(b'\n');
    }
    body
}

/// Writes `<root>/<bucket>/<name>.jsonl` with `entries` (the first is normally a
/// [`header`]). `name` may hold `/` for a nested fixture.
pub fn write_session(root: &Path, bucket: &str, name: &str, entries: &[Value]) -> PathBuf {
    write_raw(root, bucket, name, &lines(entries))
}

/// Writes exactly `body` for torn or hostile fixtures.
pub fn write_raw(root: &Path, bucket: &str, name: &str, body: &[u8]) -> PathBuf {
    let path = path_for(root, bucket, name);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    fs::write(&path, body).expect("write fixture");
    path
}

/// Appends records to an existing session file.
pub fn append(path: &Path, entries: &[Value]) {
    use std::io::Write;
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(path)
        .expect("open");
    file.write_all(&lines(entries)).expect("append");
}
