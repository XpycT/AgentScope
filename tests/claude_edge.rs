mod support;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{
    AggregationClass, EventKind, Harness, RefreshBudget, SessionKey, SourceKey,
};
use agentscope::providers::ProviderReader;
use agentscope::providers::claude::ClaudeScanner;
use serde_json::{Value, json};
use std::io::Write;
use support::{NOW, write_lines};
const BIG: RefreshBudget = RefreshBudget {
    max_records: 1000,
    max_bytes: 1_000_000,
    max_millis: 60_000,
};
fn record(uuid: &str, id: &str, content: Value) -> Value {
    json!({"type":"assistant","uuid":uuid,"sessionId":"s","message":{"id":id,"content":content,"usage":{"input_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":2}}})
}
fn setup(
    records: &[Value],
    entries: usize,
    bytes: usize,
) -> (tempfile::TempDir, std::path::PathBuf, ClaudeScanner) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let path = root.join("p/s.jsonl");
    write_lines(
        &path,
        &records.iter().map(ToString::to_string).collect::<Vec<_>>(),
    );
    let mut c = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(root),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    c.dedup_ledger_entries = entries;
    c.dedup_ledger_bytes = bytes;
    (dir, path, ClaudeScanner::new(&c).unwrap())
}
#[test]
fn blocks_survive_shared_message_id_and_same_block_revision_replaces() {
    let key = SessionKey {
        harness: Harness::Claude,
        native_id: "s".into(),
        source: SourceKey::File("x".into()),
    };
    let records = vec![
        (0, record("a", "m", json!([{"type":"text","text":"old"}]))),
        (
            1,
            record(
                "b",
                "m",
                json!([{"type":"thinking","thinking":"think"},{"type":"tool_use","id":"call","name":"Read","input":{}}]),
            ),
        ),
        (2, record("a", "m", json!([{"type":"text","text":"new"}]))),
    ];
    let events = ClaudeScanner::map_transcript(&key, &records).events;
    assert_eq!(events.len(), 3);
    assert_eq!(events.iter().filter(|e| e.usage.is_some()).count(), 1);
    assert!(
        events
            .iter()
            .any(|e| e.text.as_ref().is_some_and(|t| t.preview == "new"))
    );
    assert!(
        !events
            .iter()
            .any(|e| e.text.as_ref().is_some_and(|t| t.preview == "old"))
    );
    assert!(events.iter().any(|e| e.kind == EventKind::Reasoning));
    assert!(events.iter().any(|e| e.kind == EventKind::Tool));
}
#[test]
fn ledger_overflow_freezes_and_marks_partial() {
    for (entries, bytes) in [(1, 1_000_000), (100, 1)] {
        let (_dir, _path, mut scanner) = setup(
            &[
                record("a", "first", json!("a")),
                record("b", "second", json!("b")),
                record("c", "third", json!("c")),
                record("d", "first", json!("duplicate after freeze")),
            ],
            entries,
            bytes,
        );
        let u = support::world::rescan(&mut scanner, NOW, BIG)
            .sessions
            .remove(0)
            .usage
            .unwrap();
        assert_eq!(u.class, AggregationClass::Partial);
        assert_eq!(u.total, if entries == 1 { Some(3) } else { None });
    }
}
#[test]
fn duplicate_after_refresh_does_not_add_again() {
    let (_dir, path, mut scanner) = setup(&[record("a", "m", json!("a"))], 100, 1000);
    assert_eq!(
        support::world::rescan(&mut scanner, NOW, BIG)
            .sessions
            .remove(0)
            .usage
            .unwrap()
            .total,
        Some(3)
    );
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(file, "{}", record("b", "m", json!("b"))).unwrap();
    assert_eq!(
        support::world::rescan(&mut scanner, NOW, BIG)
            .sessions
            .remove(0)
            .usage
            .unwrap()
            .total,
        Some(3)
    );
}
#[test]
fn sidechain_record_does_not_change_parent_totals() {
    let mut side = record("side", "child", json!("child"));
    side["isSidechain"] = json!(true);
    let (_dir, _path, mut scanner) = setup(
        &[record("parent", "parent", json!("parent")), side],
        100,
        1000,
    );
    let s = support::world::rescan(&mut scanner, NOW, BIG)
        .sessions
        .remove(0);
    assert!(s.meta.lineage.sidechain);
    assert_eq!(s.usage.unwrap().total, Some(3));
}

#[test]
fn sidechain_only_file_has_own_usage_and_parent_after_sidechain_excludes_it() {
    let mut side = record("side", "child", json!("child"));
    side["isSidechain"] = json!(true);
    let (_dir, _path, mut scanner) = setup(&[side.clone()], 100, 1000);
    let summary = support::world::rescan(&mut scanner, NOW, BIG)
        .sessions
        .remove(0);
    assert_eq!(summary.usage.unwrap().total, Some(3));
    let (_dir, _path, mut scanner) = setup(
        &[side, record("parent", "parent", json!("parent"))],
        100,
        1000,
    );
    let summary = support::world::rescan(&mut scanner, NOW, BIG)
        .sessions
        .remove(0);
    assert_eq!(summary.usage.unwrap().total, Some(3));
}

#[test]
fn partial_record_revision_keeps_other_blocks() {
    let key = SessionKey {
        harness: Harness::Claude,
        native_id: "s".into(),
        source: SourceKey::File("x".into()),
    };
    let records = vec![
        (
            0,
            record(
                "a",
                "m",
                json!([{"type":"text","text":"old"},{"type":"thinking","thinking":"retained"}]),
            ),
        ),
        (1, record("a", "m", json!([{"type":"text","text":"new"}]))),
    ];
    let events = ClaudeScanner::map_transcript(&key, &records).events;
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .any(|e| e.text.as_ref().is_some_and(|t| t.preview == "retained"))
    );
    assert!(
        events
            .iter()
            .any(|e| e.text.as_ref().is_some_and(|t| t.preview == "new"))
    );
}

#[test]
fn roster_matches_transcript_after_block_revisions() {
    let records = vec![
        record(
            "a",
            "m",
            json!([{"type":"text","text":"old"},{"type":"unknown"},{"type":"thinking","thinking":"keep"}]),
        ),
        record(
            "a",
            "m",
            json!([{"type":"unknown"},{"type":"text","text":"new"}]),
        ),
    ];
    let (_dir, _path, mut scanner) = setup(&records, 100, 10000);
    let summary = support::world::rescan(&mut scanner, NOW, BIG)
        .sessions
        .remove(0);
    let indexed = records
        .into_iter()
        .enumerate()
        .map(|(i, v)| (i as u64, v))
        .collect::<Vec<_>>();
    let mapped = ClaudeScanner::map_transcript(&summary.meta.key, &indexed);
    assert_eq!(summary.displayed_event_count, mapped.events.len());
    assert_eq!(summary.hidden_records, mapped.hidden_records);
}

#[test]
fn sidechain_first_budget_split_never_exposes_child_as_parent_usage() {
    let mut side = record("side", "child", json!("child"));
    side["isSidechain"] = json!(true);
    side["message"]["usage"]["server_tool_use"] = json!({"web_search_requests":7});
    let parent_user =
        json!({"type":"user","uuid":"user","sessionId":"s","message":{"content":"hello"}});
    let (_dir, _path, mut scanner) = setup(
        &[
            side,
            parent_user,
            record("parent", "parent", json!("parent")),
        ],
        100,
        1000,
    );
    let key = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .remove(0)
        .key;
    // One record of work: the sidechain record is all that has been read so far.
    let first = scanner
        .hydrate(
            support::world::new_job(),
            &key,
            NOW,
            RefreshBudget {
                max_records: 1,
                ..BIG
            },
        )
        .unwrap();
    assert!(!first.done);
    assert!(first.summary.usage.is_none());
    assert!(first.summary.server_tools.is_none());
    let final_summary = support::world::hydrate_complete(&mut scanner, &key, NOW);
    assert_eq!(final_summary.usage.unwrap().total, Some(3));
    assert!(final_summary.server_tools.is_none());
}

#[test]
fn parent_without_assistant_does_not_inherit_sidechain_usage() {
    let mut side = record("side", "child", json!("child"));
    side["isSidechain"] = json!(true);
    let parent_user =
        json!({"type":"user","uuid":"user","sessionId":"s","message":{"content":"hello"}});
    let (_dir, _path, mut scanner) = setup(&[side, parent_user], 100, 1000);
    let summary = support::world::rescan(&mut scanner, NOW, BIG)
        .sessions
        .remove(0);
    assert!(summary.usage.is_none());
}

#[test]
fn roster_revision_of_paired_tool_call_matches_transcript() {
    let records = vec![
        record(
            "a",
            "m",
            json!([{"type":"tool_use","id":"call","name":"Read","input":{}}]),
        ),
        json!({"type":"user","uuid":"result","message":{"content":[{"type":"tool_result","tool_use_id":"call","content":"ok"}]}}),
        record(
            "a",
            "m",
            json!([{"type":"tool_use","id":"call","name":"Read","input":{}}]),
        ),
    ];
    let (_dir, _path, mut scanner) = setup(&records, 100, 1000);
    let summary = support::world::rescan(&mut scanner, NOW, BIG)
        .sessions
        .remove(0);
    let indexed = records
        .into_iter()
        .enumerate()
        .map(|(i, v)| (i as u64, v))
        .collect::<Vec<_>>();
    assert_eq!(
        summary.displayed_event_count,
        ClaudeScanner::map_transcript(&summary.meta.key, &indexed)
            .events
            .len()
    );
}

#[test]
fn revision_metadata_overflow_warns_and_counts_approximately() {
    for (entries, bytes) in [(1, 10000), (100, 1)] {
        let records = [
            record("a", "m", json!([{"type":"text","text":"old"}])),
            record("a", "m", json!([{"type":"text","text":"new"}])),
            record(
                "b",
                "m",
                json!([{"type":"tool_use","id":"call","name":"Read","input":{}}]),
            ),
        ];
        let (_dir, path, mut scanner) = setup(&records, entries, bytes);
        let summary = support::world::rescan(&mut scanner, NOW, BIG)
            .sessions
            .remove(0);
        assert_eq!(summary.displayed_event_count, 3);
        assert!(
            summary
                .warnings
                .iter()
                .any(|w| w.message.contains("approximate"))
        );
        // A one-byte budget cannot hold even one request's retained state.
        assert_eq!(
            summary.usage.unwrap().total,
            if bytes == 1 { None } else { Some(3) }
        );
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        writeln!(file, "{}", record("a", "m", json!("later duplicate"))).unwrap();
        let later = support::world::rescan(&mut scanner, NOW, BIG)
            .sessions
            .remove(0);
        assert_eq!(later.displayed_event_count, 4);
        assert_eq!(
            later
                .warnings
                .iter()
                .filter(|w| w.message.contains("approximate"))
                .count(),
            1
        );
        assert_eq!(
            later.usage.unwrap().total,
            if bytes == 1 { None } else { Some(3) }
        );
    }
}

#[test]
fn parent_user_after_sidechain_only_refresh_clears_side_usage() {
    let mut side = record("side", "child", json!("child"));
    side["isSidechain"] = json!(true);
    let (_dir, path, mut scanner) = setup(&[side], 100, 10000);
    assert_eq!(
        support::world::rescan(&mut scanner, NOW, BIG)
            .sessions
            .remove(0)
            .usage
            .unwrap()
            .total,
        Some(3)
    );
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    writeln!(
        file,
        "{}",
        json!({"type":"user","uuid":"p","message":{"content":"hello"}})
    )
    .unwrap();
    assert!(
        support::world::rescan(&mut scanner, NOW, BIG)
            .sessions
            .remove(0)
            .usage
            .is_none()
    );
}

#[test]
fn same_timestamp_block_revision_is_reported_as_invalidated() {
    // No timestamps anywhere: persisted time and event count cannot move.
    let (_dir, path, mut scanner) = setup(
        &[record("a", "m", json!([{"type":"text","text":"old"}]))],
        100,
        1 << 20,
    );
    let first = support::world::rescan(&mut scanner, NOW, BIG);
    assert!(first.invalidated.is_empty());
    let count = first.sessions[0].displayed_event_count;
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    writeln!(
        file,
        "{}",
        record("a", "m", json!([{"type":"text","text":"new"}]))
    )
    .unwrap();
    let second = support::world::rescan(&mut scanner, NOW, BIG);
    assert_eq!(second.sessions[0].displayed_event_count, count);
    assert_eq!(
        second.invalidated,
        vec![second.sessions[0].meta.key.clone()]
    );
    // A plain append of a new block is not an invalidation.
    writeln!(
        file,
        "{}",
        record("b", "m2", json!([{"type":"text","text":"next"}]))
    )
    .unwrap();
    let third = support::world::rescan(&mut scanner, NOW, BIG);
    assert!(third.invalidated.is_empty());
}
