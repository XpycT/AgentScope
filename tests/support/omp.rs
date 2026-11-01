//! Synthetic OMP-format fixtures: the fixed 256-byte title slot, the session
//! header, entries and producer-shaped payloads, laid out as
//! `<root>/<bucket>/<name>.jsonl` (children under `<bucket>/<parent>/<child>`).
//! Payload shapes mirror oh-my-pi `6a86df8539fccf3ad7818081266e554022fc21bc`
//! (`session-entries.ts`, `session-title-slot.ts`); nothing here is real data.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::support::NOW;

/// Fixed clock of every fixture record (2026-10-30T00:00:00Z).
pub const STAMP: &str = "2026-10-30T00:00:00.000Z";
/// Physical size of the title slot, newline included.
pub const SLOT_BYTES: usize = 256;

/// Epoch milliseconds of [`STAMP`].
pub const STAMP_MS: i64 = NOW;

/// The physical fixed-width title slot: exactly 256 bytes including the newline,
/// padded with spaces inside the `pad` string, titles measured in bytes.
pub fn title_slot(title: &str) -> Vec<u8> {
    let line = |title: &str, pad: &str| {
        format!(
            "{}\n",
            json!({"type":"title","v":1,"title":title,"updatedAt":STAMP,"pad":pad})
        )
    };
    let mut kept = String::new();
    for c in title.chars() {
        let mut next = kept.clone();
        next.push(c);
        if line(&next, "").len() > SLOT_BYTES {
            break;
        }
        kept = next;
    }
    let base = line(&kept, "");
    let pad = " ".repeat(SLOT_BYTES - base.len());
    let slot = line(&kept, &pad).into_bytes();
    assert_eq!(slot.len(), SLOT_BYTES, "slot must be exactly 256 bytes");
    slot
}

/// A `session` header. `parent` is `parentSession` (a session id or a file
/// path); `continuation` is `continuationOf`.
pub fn header(id: &str, parent: Option<&str>, continuation: Option<&str>) -> Value {
    let mut header = json!({
        "type": "session", "version": 3, "id": id, "timestamp": STAMP, "cwd": "/work/demo"
    });
    if let Some(parent) = parent {
        header["parentSession"] = json!(parent);
    }
    if let Some(continuation) = continuation {
        header["continuationOf"] = json!(continuation);
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

/// `entry` with `activeLeafId` set to `leaf` (`None` writes an explicit `null`).
pub fn with_leaf(mut entry: Value, leaf: Option<&str>) -> Value {
    entry["activeLeafId"] = leaf.map_or(Value::Null, |l| json!(l));
    entry
}

pub fn user_message(id: &str, parent: Option<&str>, text: &str) -> Value {
    entry(
        id,
        parent,
        "message",
        json!({"message":{"role":"user","content":text,"timestamp":NOW}}),
    )
}

/// An assistant message with provider usage and request timing.
pub fn assistant(id: &str, parent: Option<&str>, output: u64, duration_ms: u64) -> Value {
    entry(
        id,
        parent,
        "message",
        json!({"message":{
            "role":"assistant","content":[{"type":"text","text":"ok"}],
            "api":"anthropic-messages","provider":"anthropic","model":"claude-test",
            "stopReason":"stop","timestamp":NOW,"duration":duration_ms,"ttft":300,
            "usage":{"input":1000,"output":output,"cacheRead":0,"cacheWrite":0,
                     "totalTokens":1000+output,
                     "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}}
        }}),
    )
}

/// A `model_usage` entry: a model call that is not part of the transcript.
pub fn model_usage(id: &str, parent: Option<&str>, purpose: &str, output: u64) -> Value {
    entry(
        id,
        parent,
        "model_usage",
        json!({"purpose":purpose,"role":"smol","api":"anthropic-messages","provider":"anthropic",
               "model":"claude-test","stopReason":"stop",
               "usage":{"input":10,"output":output,"cacheRead":0,"cacheWrite":0,"totalTokens":10+output,
                        "cost":{"input":0.0,"output":0.0,"cacheRead":0.0,"cacheWrite":0.0,"total":0.0}}}),
    )
}

/// A `compaction` entry with the producer's before/after/method metadata.
pub fn compaction(id: &str, parent: Option<&str>, first_kept: &str) -> Value {
    entry(
        id,
        parent,
        "compaction",
        json!({"summary":"synthetic summary","firstKeptEntryId":first_kept,
               "tokensBefore":120000,"tokensAfter":30000,"method":"summary"}),
    )
}

/// A `task` tool call whose result names a child session.
pub fn task_call(id: &str, parent: Option<&str>, agent_id: &str) -> Value {
    entry(
        id,
        parent,
        "message",
        json!({"message":{"role":"toolResult","toolName":"task","toolCallId":"call-1",
                          "content":[{"type":"text","text":"done"}],
                          "details":{"results":[{"id":agent_id,"agent":"scout"}]},
                          "timestamp":NOW}}),
    )
}

/// A `session_exit` marker.
pub fn exit(id: &str, parent: Option<&str>) -> Value {
    entry(id, parent, "session_exit", json!({"reason":"done"}))
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

/// Writes `<root>/<bucket>/<name>.jsonl`: a title slot (titled after `name`) and
/// then `entries` (the first is normally a [`header`]). `name` may hold `/` to
/// place a child in its parent's artifacts directory (`"p/c"`).
pub fn write_session(root: &Path, bucket: &str, name: &str, entries: &[Value]) -> PathBuf {
    write_slotted(root, bucket, name, &title_slot(name), entries)
}

/// Like [`write_session`] with an explicit physical `slot`.
pub fn write_slotted(
    root: &Path,
    bucket: &str,
    name: &str,
    slot: &[u8],
    entries: &[Value],
) -> PathBuf {
    let mut body = slot.to_vec();
    body.extend(lines(entries));
    write_raw(root, bucket, name, &body)
}

/// Writes exactly `body` (legacy files without a slot, torn or hostile bytes).
pub fn write_raw(root: &Path, bucket: &str, name: &str, body: &[u8]) -> PathBuf {
    let path = path_for(root, bucket, name);
    fs::create_dir_all(path.parent().expect("parent")).expect("create dirs");
    fs::write(&path, body).expect("write fixture");
    path
}

// ---------------------------------------------------------------------------
// Producer-shaped call records (pi-catalog `Usage`, pi-ai `AssistantMessage`)
// ---------------------------------------------------------------------------

/// A producer-shaped `Usage` with every counter explicit. `saved` is the saved
/// `cost.total` (OMP's own, ambiguous number; never trusted).
pub fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64, saved: f64) -> Value {
    json!({
        "input": input, "output": output, "cacheRead": cache_read, "cacheWrite": cache_write,
        "totalTokens": input + output + cache_read + cache_write,
        "cost": {"input": 0.0, "output": 0.0, "cacheRead": 0.0, "cacheWrite": 0.0, "total": saved}
    })
}

/// Adds the additive provider-side orchestration counters to a `usage` object.
pub fn with_orchestration(mut usage: Value, input: u64, output: u64, cache_read: u64) -> Value {
    usage["orchestration"] = json!({"input": input, "output": output, "cacheRead": cache_read});
    usage["totalTokens"] =
        json!(usage["totalTokens"].as_u64().unwrap_or(0) + input + output + cache_read);
    usage
}

/// A producer-shaped assistant call billed through the `acme` route and model `m`.
pub fn call(id: &str, parent: Option<&str>, response: Option<&str>, usage: Value) -> Value {
    let mut message = json!({
        "role": "assistant", "content": [{"type": "text", "text": "ok"}],
        "api": "anthropic-messages", "provider": "acme", "model": "m",
        "stopReason": "stop", "timestamp": NOW, "usage": usage
    });
    if let Some(response) = response {
        message["responseId"] = json!(response);
    }
    entry(id, parent, "message", json!({"message": message}))
}

/// Sets a message-level field (`duration`, `serviceTier`, `contextSnapshot`, ...).
pub fn with_message(mut call: Value, key: &str, value: Value) -> Value {
    call["message"][key] = value;
    call
}

/// A producer-shaped context snapshot on a call.
pub fn with_snapshot(call: Value, prompt: u64, removed: Option<u64>) -> Value {
    let mut snapshot = json!({"promptTokens": prompt, "nonMessageTokens": 1200});
    if let Some(removed) = removed {
        snapshot["historyRewriteTokensRemoved"] = json!(removed);
    }
    with_message(call, "contextSnapshot", snapshot)
}

/// A `model_usage` entry on the `acme` route and model `m`.
pub fn aux(id: &str, parent: Option<&str>, purpose: &str, usage: Value) -> Value {
    entry(
        id,
        parent,
        "model_usage",
        json!({"purpose": purpose, "role": "smol", "api": "anthropic-messages",
               "provider": "acme", "model": "m", "stopReason": "stop", "usage": usage}),
    )
}

/// A `task` result quoting what a child cost (the producer's own number).
pub fn task_result(id: &str, parent: Option<&str>, agent_id: &str, quoted: f64) -> Value {
    entry(
        id,
        parent,
        "message",
        json!({"message": {"role": "toolResult", "toolName": "task", "toolCallId": "call-1",
                           "content": [{"type": "text", "text": "done"}],
                           "details": {"results": [{"id": agent_id, "agent": "scout",
                                                    "usage": {"cost": {"total": quoted}}}]},
                           "timestamp": NOW}}),
    )
}

pub fn thinking_level(id: &str, parent: Option<&str>, level: &str, configured: &str) -> Value {
    entry(
        id,
        parent,
        "thinking_level_change",
        json!({"thinkingLevel": level, "configured": configured}),
    )
}

pub fn model_change(id: &str, parent: Option<&str>, selector: &str) -> Value {
    entry(
        id,
        parent,
        "model_change",
        json!({"model": selector, "role": "default"}),
    )
}

pub fn service_tier_change(id: &str, parent: Option<&str>, tier: &str) -> Value {
    entry(
        id,
        parent,
        "service_tier_change",
        json!({"serviceTier": {"anthropic": tier}}),
    )
}

/// Overrides an entry's ISO timestamp.
pub fn at(mut entry: Value, iso: &str) -> Value {
    entry["timestamp"] = json!(iso);
    entry
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
