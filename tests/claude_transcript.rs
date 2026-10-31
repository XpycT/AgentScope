mod support;

use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{EventKind, Harness, RefreshBudget, SessionKey, SourceKey, ToolStatus};
use agentscope::providers::claude::ClaudeScanner;
use serde_json::json;
use support::claude::message;
use support::{NOW, write_lines};

fn key() -> SessionKey {
    SessionKey {
        harness: Harness::Claude,
        native_id: "session".into(),
        source: SourceKey::File("fixture.jsonl".into()),
    }
}
fn text(e: &agentscope::domain::Event) -> &str {
    &e.text.as_ref().unwrap().preview
}
fn map(records: Vec<serde_json::Value>) -> agentscope::providers::claude::ClaudeTranscript {
    let indexed = records
        .into_iter()
        .enumerate()
        .map(|(i, value)| (i as u64 * 100, value))
        .collect::<Vec<_>>();
    ClaudeScanner::map_transcript(&key(), &indexed)
}

#[test]
fn tool_only_assistant_exposes_reported_usage_and_real_source_offset() {
    let mut record = message(
        "assistant",
        "a",
        json!([{"type":"tool_use","id":"one","name":"Read","input":{}}]),
    );
    record["message"]["usage"] = json!({"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":3,"cache_read_input_tokens":2,"cache_creation":{"ephemeral_5m_input_tokens":3},"output_tokens_details":{"thinking_tokens":1},"iterations":[{"output_tokens":99}]});
    let mapped = ClaudeScanner::map_transcript(&key(), &[(42, record)]);
    let event = &mapped.events[0];
    let categories = &event.usage.as_ref().unwrap().categories;
    assert_eq!(
        (
            categories.input,
            categories.output,
            categories.cache_write_5m,
            categories.reasoning
        ),
        (Some(10), Some(4), Some(3), Some(1))
    );
    assert!(matches!(
        event.provenance.locator.kind,
        agentscope::domain::LocatorKind::FileOffset { offset: 42, .. }
    ));
}

#[test]
fn maps_distinct_blocks_even_when_assistant_message_ids_match() {
    let records = vec![
        message("user", "u", json!("hello")),
        message(
            "assistant",
            "a",
            json!([{"type":"thinking","thinking":"consider"},{"type":"text","text":"answer"}]),
        ),
        message("assistant", "b", json!([{"type":"text","text":"more"}])),
    ];
    let mapped = map(records);
    assert_eq!(
        mapped.events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        [
            EventKind::UserPrompt,
            EventKind::Reasoning,
            EventKind::AssistantMessage,
            EventKind::AssistantMessage
        ]
    );
    assert_eq!(
        mapped.events.iter().map(text).collect::<Vec<_>>(),
        ["hello", "consider", "answer", "more"]
    );
    assert_ne!(mapped.events[2].id, mapped.events[3].id);
}

#[test]
fn links_tool_results_only_by_id_and_marks_errors_and_orphans() {
    let records = vec![
        message(
            "assistant",
            "a",
            json!([{"type":"tool_use","id":"one","name":"Read","input":{"path":"x"}},{"type":"tool_use","id":"two","name":"Write","input":{}}]),
        ),
        message(
            "user",
            "b",
            json!([{"type":"tool_result","tool_use_id":"two","content":"failed","is_error":true},{"type":"tool_result","tool_use_id":"missing","content":"orphan"}]),
        ),
    ];
    let mapped = map(records);
    assert_eq!(mapped.events.len(), 3);
    let tools: Vec<_> = mapped
        .events
        .iter()
        .map(|e| e.tool.as_ref().unwrap())
        .collect();
    assert_eq!(tools[0].status, ToolStatus::Pending);
    assert_eq!(tools[1].status, ToolStatus::Error);
    assert_eq!(tools[1].output.as_ref().unwrap().preview, "failed");
    assert!(tools[2].orphan);
}

#[test]
fn images_are_placeholders_and_auxiliary_records_are_hidden() {
    let records = vec![
        message(
            "user",
            "u",
            json!([{"type":"image","source":{"data":"SECRET_BINARY"}},{"type":"text","text":"look"}]),
        ),
        json!({"type":"attachment","attachment":{"text":"hidden"}}),
        json!({"type":"file-history-snapshot"}),
    ];
    let mapped = map(records);
    assert_eq!(
        mapped.events.iter().map(text).collect::<Vec<_>>(),
        ["[image]", "look"]
    );
    assert_eq!(mapped.hidden_records, 2);
}

fn config(root: &std::path::Path) -> Config {
    Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root.into()),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap()
}
const BIG: RefreshBudget = RefreshBudget {
    max_records: 1000,
    max_bytes: 1_000_000,
    max_millis: 60_000,
};

#[test]
fn title_precedence_and_duplicate_sessions_are_separate_sources() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let a = root.join("project-a/a.jsonl");
    let b = root.join("project-b/b.jsonl");
    let records = [
        message(
            "user",
            "injected",
            json!("<environment_context>ignore</environment_context>"),
        ),
        message(
            "user",
            "result",
            json!([{"type":"tool_result","tool_use_id":"missing","content":"not title"}]),
        ),
        message("user", "u", json!("real prompt")),
    ];
    write_lines(
        &a,
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    let mut with_title = records.to_vec();
    with_title.push(json!({"type":"ai-title","aiTitle":"Generated title"}));
    write_lines(
        &b,
        &with_title
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
    );
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let delta = support::world::rescan(&mut scanner, NOW, BIG);
    assert!(delta.scan.done);
    assert_eq!(delta.sessions.len(), 2);
    let first = delta
        .sessions
        .iter()
        .find(|s| s.meta.key.source == SourceKey::File(a.clone()))
        .unwrap();
    let second = delta
        .sessions
        .iter()
        .find(|s| s.meta.key.source == SourceKey::File(b.clone()))
        .unwrap();
    assert_eq!(first.meta.title, "real prompt");
    assert!(first.meta.title_derived);
    assert_eq!(second.meta.title, "Generated title");
    assert!(!second.meta.title_derived);
    assert!(first.meta.source_available && second.meta.source_available);
    assert_ne!(first.meta.key, second.meta.key);
}

#[test]
fn replacement_rebuilds_summary_without_replaying_old_content() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let file = root.join("p/session.jsonl");
    write_lines(
        &file,
        &[
            message("user", "old", json!("old prompt with padding")).to_string(),
            json!({"type":"attachment"}).to_string(),
        ],
    );
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let first = support::world::rescan(&mut scanner, NOW, BIG);
    assert_eq!(first.sessions[0].hidden_records, 1);
    write_lines(&file, &[message("user", "new", json!("new")).to_string()]);
    let second = support::world::rescan(&mut scanner, NOW, BIG);
    assert_eq!(second.sessions[0].meta.title, "new");
    assert_eq!(second.sessions[0].displayed_event_count, 1);
    assert_eq!(second.sessions[0].hidden_records, 0);
}

#[test]
fn unknown_blocks_including_tool_results_are_counted_hidden() {
    let records = vec![
        message(
            "assistant",
            "a",
            json!([{"type":"redacted_thinking"},{"type":"document"},{"type":"server_tool_use"}]),
        ),
        message(
            "user",
            "b",
            json!([{"type":"tool_result","tool_use_id":"missing","content":[{"type":"text","text":"ok"},{"type":"document"},{"type":"web_search_tool_result"}]}]),
        ),
    ];
    let mapped = map(records);
    assert_eq!(mapped.events.len(), 1);
    assert_eq!(mapped.hidden_records, 5);
}

#[test]
fn duplicate_ids_warn_and_explicit_parent_is_not_inferred() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let a = root.join("a/a.jsonl");
    let b = root.join("b/b.jsonl");
    let mut child = message("user", "u", json!("hello"));
    child["isSidechain"] = json!(true);
    child["parentSessionId"] = json!("parent-session");
    write_lines(&a, &[child.to_string()]);
    let mut other_record = message("user", "v", json!("other"));
    other_record["parentUuid"] = json!("ordinary-message-parent");
    write_lines(&b, &[other_record.to_string()]);
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let delta = support::world::rescan(&mut scanner, NOW, BIG);
    assert_eq!(delta.sessions.len(), 2);
    for s in &delta.sessions {
        assert!(
            s.warnings
                .iter()
                .any(|w| w.message.contains("Duplicate ID"))
        );
    }
    let child = delta
        .sessions
        .iter()
        .find(|s| s.meta.key.source == SourceKey::File(a.clone()))
        .unwrap();
    assert_eq!(
        child.meta.lineage.parent,
        Some(agentscope::domain::SessionLink::Unresolved(
            "parent-session".into()
        ))
    );
    assert!(child.meta.lineage.sidechain);
    let other = delta
        .sessions
        .iter()
        .find(|s| s.meta.key.source == SourceKey::File(b.clone()))
        .unwrap();
    assert_eq!(other.meta.lineage.parent, None);
    std::fs::remove_file(b).unwrap();
    let next = support::world::rescan(&mut scanner, NOW, BIG);
    assert_eq!(next.removed.len(), 1);
    assert_eq!(next.sessions.len(), 1);
    assert!(
        !next.sessions[0]
            .warnings
            .iter()
            .any(|w| w.message.contains("Duplicate ID"))
    );
}

#[test]
fn sidechain_parent_uuid_is_not_a_session_edge() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let mut record = message("assistant", "child", json!("response"));
    record["isSidechain"] = json!(true);
    record["parentUuid"] = json!("parent-record");
    write_lines(&root.join("p/child.jsonl"), &[record.to_string()]);
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let delta = support::world::rescan(&mut scanner, NOW, BIG);
    // `parentUuid` names a message, so it never becomes a session parent.
    assert_eq!(delta.sessions[0].meta.lineage.parent, None);
    assert!(delta.sessions[0].meta.lineage.sidechain);
}

/// A source's key is fixed by discovery's bounded head probe; hydration answers to it.
#[test]
fn a_session_id_beyond_the_probe_window_keeps_the_discovered_stem_key() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let early = root.join("p/early.jsonl");
    let late = root.join("p/late.jsonl");
    write_lines(
        &early,
        &[
            json!({"type":"attachment"}).to_string(),
            message("user", "u", json!("hello")).to_string(),
        ],
    );
    // The only ID sits between 10 KiB of ID-free records and a 54 KiB tail of them: past
    // the 4 KiB prefix and before the 32 KiB tail.
    let filler = json!({"type": "attachment", "pad": "x".repeat(900)}).to_string();
    let mut lines = vec![filler.clone(); 12];
    lines.push(message("user", "u", json!("hello")).to_string());
    lines.extend(vec![filler; 60]);
    write_lines(&late, &lines);
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, BIG);
    let key_of = |path: &std::path::Path| {
        found
            .sessions
            .iter()
            .find(|m| m.key.source == SourceKey::File(path.to_path_buf()))
            .unwrap()
            .key
            .clone()
    };
    // The ID inside the head window identifies the first source...
    assert_eq!(key_of(&early).native_id, "session");
    // ...the other stays under its file stem, and hydration does not rename it.
    let late_key = key_of(&late);
    assert_eq!(late_key.native_id, "late");
    let summary = support::world::hydrate_complete(&mut scanner, &late_key, NOW);
    assert_eq!(summary.meta.key, late_key);
    assert_eq!(summary.displayed_event_count, 1);
}

#[test]
fn malformed_and_oversized_lines_are_warnings_not_auxiliary() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let file = root.join("p/session.jsonl");
    write_lines(
        &file,
        &[
            "not json".into(),
            format!("\"{}\"", "x".repeat(300)),
            message("user", "u", json!("ok")).to_string(),
        ],
    );
    let mut cfg = config(&root);
    cfg.max_record_bytes = 250;
    let mut scanner = ClaudeScanner::new(&cfg).unwrap();
    let delta = support::world::rescan(&mut scanner, NOW, BIG);
    let s = &delta.sessions[0];
    assert_eq!(s.hidden_records, 0);
    assert!(
        s.warnings
            .iter()
            .any(|w| w.key.kind == agentscope::domain::WarningKind::Parse)
    );
    assert!(
        s.warnings
            .iter()
            .any(|w| w.key.kind == agentscope::domain::WarningKind::Unsupported)
    );
}

#[test]
fn roster_pairs_more_than_256_outstanding_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let file = root.join("p/session.jsonl");
    let calls: Vec<_> = (0..300)
        .map(|i| json!({"type":"tool_use","id":format!("t{i}"),"name":"Read","input":{}}))
        .collect();
    let results: Vec<_> = (0..300)
        .map(|i| json!({"type":"tool_result","tool_use_id":format!("t{i}"),"content":"ok"}))
        .collect();
    let records = [
        message("assistant", "a", json!(calls)),
        message("user", "b", json!(results)),
    ];
    write_lines(
        &file,
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    let mut scanner = ClaudeScanner::new(&config(&root)).unwrap();
    let delta = support::world::rescan(&mut scanner, NOW, BIG);
    assert_eq!(delta.sessions[0].displayed_event_count, 300);
}
