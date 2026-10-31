//! Codex rollout JSONL fixture builder (synthetic records only).
#![allow(dead_code)]

use serde_json::{Value, json};

pub fn record(ts: &str, kind: &str, payload: Value) -> String {
    json!({"timestamp": ts, "type": kind, "payload": payload}).to_string()
}

pub fn session_meta(ts: &str, id: &str, cwd: &str) -> String {
    record(
        ts,
        "session_meta",
        json!({"id": id, "cwd": cwd, "originator": "codex_cli_rs", "cli_version": "0.159.2", "source": "cli"}),
    )
}

pub fn session_meta_with(ts: &str, extra: Value) -> String {
    record(ts, "session_meta", extra)
}

/// A spawned child's identity header: the child's own id, the root thread it
/// belongs to, and the immediate parent's thread id.
pub fn child_session_meta(
    ts: &str,
    id: &str,
    root: &str,
    parent: &str,
    boundary: Option<u64>,
    version: &str,
) -> String {
    let mut payload = json!({
        "id": id,
        "session_id": root,
        "parent_thread_id": parent,
        "cwd": "/p",
        "originator": "codex_cli_rs",
        "cli_version": version,
        "thread_source": "subagent",
        "history_mode": "paginated",
        "source": {"subagent": {"thread_spawn": {
            "parent_thread_id": parent,
            "depth": 1,
            "agent_nickname": id,
            "agent_role": "explorer"
        }}}
    });
    if let Some(boundary) = boundary {
        payload["subagent_history_start_ordinal"] = json!(boundary);
    }
    record(ts, "session_meta", payload)
}

/// A guardian review thread: a recorded parent, a review role, no spawn.
pub fn guardian_session_meta(ts: &str, id: &str, root: &str, parent: &str) -> String {
    record(
        ts,
        "session_meta",
        json!({
            "id": id,
            "session_id": root,
            "parent_thread_id": parent,
            "cwd": "/p",
            "originator": "codex_cli_rs",
            "cli_version": "0.159.2",
            "thread_source": "guardian_review",
            "history_mode": "paginated",
            "source": {"subagent": {"other": "guardian"}}
        }),
    )
}

/// A usage record naming the thread that made the request.
pub fn usage_record_thread(ts: &str, thread_id: &str, thread: Value) -> String {
    record(
        ts,
        "token_usage_record",
        json!({"thread_id": thread_id, "thread_token_usage": thread, "usage": thread}),
    )
}

pub fn turn_context(ts: &str, model: &str, cwd: &str) -> String {
    record(ts, "turn_context", json!({"model": model, "cwd": cwd}))
}

fn message(ts: &str, role: &str, kind: &str, text: &str) -> String {
    record(
        ts,
        "response_item",
        json!({"type": "message", "role": role, "content": [{"type": kind, "text": text}]}),
    )
}

pub fn user(ts: &str, text: &str) -> String {
    message(ts, "user", "input_text", text)
}

pub fn developer(ts: &str, text: &str) -> String {
    message(ts, "developer", "input_text", text)
}

pub fn assistant(ts: &str, text: &str) -> String {
    message(ts, "assistant", "output_text", text)
}

pub fn task_started(ts: &str, window: Option<u64>) -> String {
    record(
        ts,
        "event_msg",
        json!({"type": "task_started", "model_context_window": window}),
    )
}

pub fn task_complete(ts: &str) -> String {
    record(ts, "event_msg", json!({"type": "task_complete"}))
}

pub fn usage(input: u64, cached: u64, output: u64, reasoning: u64, total: u64) -> Value {
    json!({
        "input_tokens": input,
        "cached_input_tokens": cached,
        "cache_write_input_tokens": 0,
        "output_tokens": output,
        "reasoning_output_tokens": reasoning,
        "total_tokens": total,
    })
}

pub fn usage_record(ts: &str, thread: Value) -> String {
    record(
        ts,
        "token_usage_record",
        json!({"thread_token_usage": thread, "turn_token_usage": thread, "usage": thread}),
    )
}

pub fn token_count(ts: &str, total: Value, window: Option<u64>) -> String {
    record(
        ts,
        "event_msg",
        json!({"type": "token_count", "info": {
            "total_token_usage": total,
            "last_token_usage": total,
            "model_context_window": window,
        }}),
    )
}

/// `token_usage_record` body at the top level of the record (no `payload`).
pub fn usage_record_top_level(ts: &str, thread: Value) -> String {
    json!({"timestamp": ts, "type": "token_usage_record", "thread_token_usage": thread}).to_string()
}

/// `token_usage_record` carried as an `event_msg` subtype.
pub fn usage_record_event(ts: &str, thread: Value) -> String {
    record(
        ts,
        "event_msg",
        json!({"type": "token_usage_record", "thread_token_usage": thread}),
    )
}

pub fn reasoning(ts: &str, summary: &str, raw: &str) -> String {
    record(
        ts,
        "response_item",
        json!({
            "type": "reasoning",
            "summary": [{"type": "summary_text", "text": summary}],
            "content": [{"type": "reasoning_text", "text": raw}],
        }),
    )
}

pub fn compacted(ts: &str) -> String {
    record(ts, "compacted", json!({"message": "history compacted"}))
}

/// `event_msg.item_completed` wrapping one thread item.
pub fn item_completed(ts: &str, item: Value) -> String {
    record(
        ts,
        "event_msg",
        json!({"type": "item_completed", "thread_id": "t", "turn_id": "u", "item": item}),
    )
}

pub fn item_completed_timed(ts: &str, item: Value, started_ms: i64, completed_ms: i64) -> String {
    record(
        ts,
        "event_msg",
        json!({
            "type": "item_completed", "thread_id": "t", "turn_id": "u", "item": item,
            "started_at_ms": started_ms, "completed_at_ms": completed_ms,
        }),
    )
}

pub fn command_item(id: &str, command: &str, output: &str, exit_code: i64) -> Value {
    json!({
        "type": "CommandExecution", "id": id, "command": ["bash", "-lc", command],
        "cwd": "/p", "status": "completed", "aggregated_output": output,
        "exit_code": exit_code, "duration": {"secs": 2, "nanos": 500_000_000},
    })
}

pub fn custom_call(ts: &str, call_id: &str, name: &str, input: &str) -> String {
    record(
        ts,
        "response_item",
        json!({"type": "custom_tool_call", "call_id": call_id, "name": name, "input": input}),
    )
}

pub fn custom_output(ts: &str, call_id: &str, output: &str) -> String {
    record(
        ts,
        "response_item",
        json!({"type": "custom_tool_call_output", "call_id": call_id, "output": output}),
    )
}

pub fn function_call(ts: &str, call_id: &str, name: &str, arguments: &str) -> String {
    record(
        ts,
        "response_item",
        json!({"type": "function_call", "call_id": call_id, "name": name, "arguments": arguments}),
    )
}

pub fn function_output(ts: &str, call_id: &str, output: &str) -> String {
    record(
        ts,
        "response_item",
        json!({"type": "function_call_output", "call_id": call_id, "output": output}),
    )
}
