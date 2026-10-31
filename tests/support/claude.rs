//! Synthetic Claude-format fixtures: transcripts, native children and the
//! `sessionId`/`agentId` relationship real subagent files carry.

use std::path::Path;

use serde_json::{Value, json};

use crate::support::write_lines;

/// A minimal record of `session` with arbitrary `content`: the shape the other
/// Claude suites vary field by field.
pub fn message(kind: &str, uuid: &str, content: Value) -> Value {
    json!({"type":kind,"uuid":uuid,"sessionId":"session", "cwd":"/projects/demo", "version":"2.1", "timestamp":"2026-10-30T10:00:00Z", "message":{"id":"shared", "role":kind, "model":"claude-test", "content":content}})
}

/// A usage block whose Claude total is `input + output` (caches are zero).
fn usage(input: u64, output: u64) -> Value {
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    })
}

/// A user record of the session `session`.
pub fn user(session: &str, uuid: &str, text: &str) -> Value {
    json!({
        "type": "user",
        "uuid": uuid,
        "sessionId": session,
        "cwd": "/projects/demo",
        "version": "2.1",
        "timestamp": "2026-10-30T10:00:00Z",
        "message": {"role": "user", "content": [{"type": "text", "text": text}]},
    })
}

/// An assistant record with usage: `tokens` is (input, output).
pub fn assistant(session: &str, uuid: &str, message: &str, tokens: (u64, u64)) -> Value {
    json!({
        "type": "assistant",
        "uuid": uuid,
        "sessionId": session,
        "cwd": "/projects/demo",
        "version": "2.1",
        "timestamp": "2026-10-30T10:01:00Z",
        "message": {
            "id": message,
            "role": "assistant",
            "model": "claude-test",
            "content": [{"type": "text", "text": "answer"}],
            "usage": usage(tokens.0, tokens.1),
        },
    })
}

/// Names the model that answered a record.
pub fn model_of(mut record: Value, model: &str) -> Value {
    record["message"]["model"] = json!(model);
    record
}

/// A generated-title record (`ai-title`).
pub fn ai_title(session: &str, uuid: &str, text: &str) -> Value {
    json!({
        "type": "ai-title",
        "uuid": uuid,
        "sessionId": session,
        "timestamp": "2026-10-30T10:02:00Z",
        "aiTitle": text,
    })
}

/// Stamps a record with the subagent that wrote it.
pub fn agent(mut record: Value, agent: &str) -> Value {
    record["agentId"] = json!(agent);
    record["isSidechain"] = json!(true);
    record
}

/// Adds a nested spawner (and its spawning tool call) to a record.
pub fn spawned_by(mut record: Value, parent_agent: &str, tool_use: &str) -> Value {
    record["parentAgentId"] = json!(parent_agent);
    record["parentToolUseId"] = json!(tool_use);
    record
}

/// Adds the surface that recorded the record.
pub fn entrypoint(mut record: Value, entry: &str) -> Value {
    record["entrypoint"] = json!(entry);
    record
}

/// A message UUID with no session meaning, for `parentUuid` decoys.
pub fn parent_uuid(mut record: Value, uuid: &str) -> Value {
    record["parentUuid"] = json!(uuid);
    record
}

pub fn write(path: &Path, records: &[Value]) {
    write_lines(
        path,
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
}

/// Writes a child transcript sidecar holding the allowlisted identity keys.
pub fn write_sidecar(child: &Path, keys: Value) {
    let name = child.file_stem().unwrap().to_string_lossy();
    let path = child.with_file_name(format!("{name}.meta.json"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, keys.to_string()).unwrap();
}
