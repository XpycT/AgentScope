mod support;

use std::path::{Path, PathBuf};

use agentscope::config::Config;
use agentscope::domain::{
    Actor, Event, EventKind, Harness, LocatorKind, PageAnchor, PageDirection, PageRequest,
    RefreshBudget, SessionKey, SourceKey, SourceLocator, ToolStatus, TranscriptPage,
};
use agentscope::providers::ProviderError;
use agentscope::providers::opencode::OpenCodeScanner;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use support::NOW;
use support::opencode::*;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

const ALL: PageRequest = PageRequest {
    max_events: 10_000,
    max_bytes: 10_000_000,
};

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn key(id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::OpenCode,
        native_id: id.to_owned(),
        source: SourceKey::DbRow {
            database: DATABASE.with(|d| d.borrow().clone()),
            table: "session_v2".into(),
            id: id.to_owned(),
        },
    }
}

thread_local! {
    /// Database of the scanner this test thread opened last; keys are built from it.
    static DATABASE: std::cell::RefCell<PathBuf> = const { std::cell::RefCell::new(PathBuf::new()) };
}

fn scanner_for(db: &Path) -> OpenCodeScanner {
    scanner_with(db, &config_for_db(db))
}

fn scanner_with(db: &Path, config: &Config) -> OpenCodeScanner {
    DATABASE.with(|d| *d.borrow_mut() = db.to_path_buf());
    OpenCodeScanner::new(config).expect("scanner")
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let conn = create_v2(&db);
    insert_session(&conn, &Session::new("ses_1", 1_000, 1_000));
    (dir, db, conn)
}

fn msg(conn: &Connection, id: &str, ty: &str, created: i64, updated: i64, data: &str) {
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1, 'ses_1', ?2, 0, ?3, ?4, ?5)",
        params![id, ty, created, updated, data],
    )
    .expect("insert message");
}

fn page(
    scanner: &mut OpenCodeScanner,
    anchor: Option<PageAnchor>,
    direction: PageDirection,
    request: PageRequest,
) -> TranscriptPage {
    support::world::try_transcript(
        scanner,
        &key("ses_1"),
        anchor,
        direction,
        request,
        BIG,
        support::world::STEPS,
    )
    .expect("page")
}

fn all(scanner: &mut OpenCodeScanner) -> TranscriptPage {
    page(scanner, None, PageDirection::Forward, ALL)
}

fn text(e: &Event) -> String {
    e.text
        .as_ref()
        .map(|t| t.preview.clone())
        .unwrap_or_default()
}

#[test]
fn maps_user_assistant_system_compaction_idle_and_synthetic() {
    let (_d, db, conn) = fixture();
    msg(
        &conn,
        "m1",
        "user",
        10,
        10,
        &json!({"text": "hello there", "files": [{"name": "a.rs"}], "agents": ["planner"], "time": {"created": 10}}).to_string(),
    );
    msg(
        &conn,
        "m2",
        "assistant",
        20,
        20,
        &json!({"content": [{"type": "text", "text": "hi back"}], "finish": "stop"}).to_string(),
    );
    msg(&conn, "m3", "system", 30, 30, r#"{"text":"be careful"}"#);
    msg(
        &conn,
        "m4",
        "compaction",
        40,
        40,
        r#"{"summary":"earlier work"}"#,
    );
    msg(&conn, "m5", "idle", 50, 50, r#"{"outcome":"succeeded"}"#);
    msg(
        &conn,
        "m6",
        "synthetic",
        60,
        60,
        r#"{"text":"injected note"}"#,
    );
    let mut s = scanner_for(&db);
    let p = all(&mut s);
    let rows: Vec<_> = p
        .events
        .iter()
        .map(|e| (e.id.as_str(), e.kind, e.actor, e.timestamp))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("m1#0", EventKind::UserPrompt, Actor::User, Some(10)),
            ("m1#1", EventKind::System, Actor::User, Some(10)),
            ("m1#2", EventKind::System, Actor::User, Some(10)),
            (
                "m2#0",
                EventKind::AssistantMessage,
                Actor::Assistant,
                Some(20)
            ),
            ("m3#0", EventKind::System, Actor::System, Some(30)),
            ("m4#0", EventKind::Compaction, Actor::System, Some(40)),
            ("m5#0", EventKind::Lifecycle, Actor::System, Some(50)),
            ("m6#0", EventKind::System, Actor::System, Some(60)),
        ]
    );
    assert_eq!(text(&p.events[0]), "hello there");
    assert!(text(&p.events[1]).contains("a.rs"));
    assert!(text(&p.events[2]).contains("planner"));
    assert_eq!(text(&p.events[3]), "hi back");
    assert_eq!(text(&p.events[4]), "be careful");
    assert!(text(&p.events[5]).contains("earlier work"));
    assert!(text(&p.events[6]).contains("succeeded"));
    assert_eq!(text(&p.events[7]), "injected note");
    assert!(p.events.iter().all(|e| e.session == key("ses_1")));
    assert!(!p.has_more);
    assert!(p.diagnostics.is_empty(), "{:?}", p.diagnostics);
    // The record type is carried in provenance.
    assert_eq!(p.events[4].provenance.record_type, "system");
    assert_eq!(p.events[4].provenance.harness, Harness::OpenCode);
}

#[test]
fn maps_text_reasoning_tool_patch_file_agent_and_subtask_parts() {
    let (_d, db, conn) = fixture();
    let data = json!({"content": [
        {"type": "text", "text": "plan"},
        {"type": "reasoning", "text": "thinking hard"},
        {"type": "tool", "id": "call_1", "name": "bash",
         "state": {"status": "completed", "input": {"command": "ls"}, "content": [{"type": "text", "text": "file.txt"}],
                   "metadata": {"exit": 0}},
         "time": {"created": 111, "ran": 112, "completed": 120}},
        {"type": "patch", "files": ["src/main.rs"]},
        {"type": "file", "name": "notes.md"},
        {"type": "agent", "name": "reviewer"},
        {"type": "subtask", "description": "split work"},
    ]});
    msg(&conn, "m1", "assistant", 10, 10, &data.to_string());
    let mut s = scanner_for(&db);
    let p = all(&mut s);
    let ids: Vec<_> = p.events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(
        ids,
        ["m1#0", "m1#1", "m1#2", "m1#3", "m1#4", "m1#5", "m1#6"]
    );
    let orders: Vec<_> = p.events.iter().map(|e| e.source_order).collect();
    assert_eq!(orders, [0, 1, 2, 3, 4, 5, 6]);
    let kinds: Vec<_> = p.events.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        [
            EventKind::AssistantMessage,
            EventKind::Reasoning,
            EventKind::Tool,
            EventKind::System,
            EventKind::System,
            EventKind::System,
            EventKind::System,
        ]
    );
    assert_eq!(text(&p.events[1]), "thinking hard");
    let tool = p.events[2].tool.as_ref().expect("tool");
    assert_eq!(p.events[2].actor, Actor::Tool);
    assert_eq!(tool.name, "bash");
    assert_eq!(tool.call_id.as_deref(), Some("call_1"));
    assert_eq!(tool.status, ToolStatus::Completed);
    assert_eq!(tool.exit_code, Some(0));
    assert_eq!(tool.started_at, Some(111));
    assert_eq!(tool.completed_at, Some(120));
    assert!(tool.input.as_ref().expect("input").preview.contains("ls"));
    assert!(
        tool.output
            .as_ref()
            .expect("output")
            .preview
            .contains("file.txt")
    );
    assert!(!tool.orphan);
    assert!(text(&p.events[3]).contains("src/main.rs"));
    assert!(text(&p.events[4]).contains("notes.md"));
    assert!(text(&p.events[5]).contains("reviewer"));
    assert!(text(&p.events[6]).contains("split work"));
    // Each part has its own locator carrying the part index.
    for (i, e) in p.events.iter().enumerate() {
        assert_eq!(
            e.provenance.locator.kind,
            LocatorKind::DbRow {
                table: "session_message".into(),
                row_id: "m1".into(),
                part_index: Some(i),
            }
        );
    }
}

#[test]
fn tool_state_completed_and_error_map_to_tool_status() {
    let (_d, db, conn) = fixture();
    let tool =
        |id: &str, state: Value| json!({"type": "tool", "id": id, "name": "t", "state": state});
    let data = json!({"content": [
        tool("a", json!({"status": "completed", "input": {}, "content": "ok"})),
        tool("b", json!({"status": "error", "input": {}, "content": "boom"})),
        tool("c", json!({"status": "weird"})),
        tool("d", json!({})),
    ]});
    msg(&conn, "m1", "assistant", 10, 10, &data.to_string());
    let mut s = scanner_for(&db);
    let p = all(&mut s);
    let statuses: Vec<_> = p
        .events
        .iter()
        .map(|e| e.tool.as_ref().expect("tool").status)
        .collect();
    assert_eq!(
        statuses,
        [
            ToolStatus::Completed,
            ToolStatus::Error,
            ToolStatus::Unknown,
            ToolStatus::Unknown
        ]
    );
    assert!(
        p.events[1]
            .tool
            .as_ref()
            .unwrap()
            .output
            .as_ref()
            .unwrap()
            .preview
            .contains("boom")
    );
}

fn collect(
    scanner: &mut OpenCodeScanner,
    direction: PageDirection,
    max_events: usize,
) -> Vec<Vec<String>> {
    let request = PageRequest {
        max_events,
        max_bytes: 10_000_000,
    };
    let mut anchor = None;
    let mut pages = Vec::new();
    loop {
        let p = page(scanner, anchor.clone(), direction, request);
        assert!(p.events.len() <= max_events);
        pages.push(p.events.iter().map(|e| e.id.clone()).collect::<Vec<_>>());
        if !p.has_more {
            return pages;
        }
        anchor = p.next;
        assert!(anchor.is_some());
    }
}

#[test]
fn forward_and_backward_pages_are_equivalent_at_equal_timestamps() {
    let (_d, db, conn) = fixture();
    // Equal timestamps, ids inserted out of order, some multi-part rows.
    for (id, parts) in [("e", 1), ("b", 3), ("d", 1), ("a", 2), ("c", 2), ("f", 1)] {
        let content: Vec<_> = (0..parts)
            .map(|i| json!({"type": "text", "text": format!("{id}{i}")}))
            .collect();
        msg(
            &conn,
            id,
            "assistant",
            500,
            500,
            &json!({"content": content}).to_string(),
        );
    }
    msg(&conn, "z", "user", 400, 400, r#"{"text":"first"}"#);
    msg(&conn, "y", "user", 600, 600, r#"{"text":"last"}"#);
    let mut s = scanner_for(&db);
    let full: Vec<String> = all(&mut s).events.iter().map(|e| e.id.clone()).collect();
    let expected = [
        "z#0", "a#0", "a#1", "b#0", "b#1", "b#2", "c#0", "c#1", "d#0", "e#0", "f#0", "y#0",
    ];
    assert_eq!(full, expected);
    for size in 1..=5 {
        let forward: Vec<String> = collect(&mut s, PageDirection::Forward, size)
            .into_iter()
            .flatten()
            .collect();
        assert_eq!(forward, expected, "forward size {size}");
        // Backward pages arrive newest-first; each page is in display order.
        let backward_pages = collect(&mut s, PageDirection::Backward, size);
        let backward: Vec<String> = backward_pages.into_iter().rev().flatten().collect();
        assert_eq!(backward, expected, "backward size {size}");
        let forward_pages = collect(&mut s, PageDirection::Forward, size);
        let mut backward_pages = collect(&mut s, PageDirection::Backward, size);
        backward_pages.reverse();
        // Page boundaries differ only at the ends; contents are never duplicated.
        let flat: Vec<_> = forward_pages.iter().flatten().collect();
        let unique: std::collections::HashSet<_> = flat.iter().collect();
        assert_eq!(flat.len(), unique.len());
    }
    // Anchor semantics: next is the last (forward) or first (backward) event.
    let p = page(
        &mut s,
        None,
        PageDirection::Forward,
        PageRequest {
            max_events: 4,
            max_bytes: 1_000_000,
        },
    );
    let anchor = p.next.clone().expect("next");
    assert_eq!(
        (
            anchor.timestamp,
            anchor.source_order,
            anchor.stable_id.as_str()
        ),
        (Some(500), 0, "62#0000000000")
    );
    assert!(p.has_more);
    let back = page(
        &mut s,
        Some(anchor.clone()),
        PageDirection::Backward,
        PageRequest {
            max_events: 100,
            max_bytes: 1_000_000,
        },
    );
    let ids: Vec<_> = back.events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["z#0", "a#0", "a#1"]);
    assert!(!back.has_more);
    // count_events_after the same anchor.
    assert_eq!(
        support::world::try_count(&mut s, &key("ses_1"), &anchor, BIG, support::world::STEPS)
            .expect("count"),
        8
    );
}

#[test]
fn byte_budget_bounds_pages_and_tool_rows_stay_whole() {
    let (_d, db, conn) = fixture();
    let big = "x".repeat(900);
    for i in 0..6 {
        let data = json!({"content": [
            {"type": "tool", "id": format!("c{i}"), "name": "bash",
             "state": {"status": "completed", "input": {"command": big}, "content": big}}
        ]});
        msg(
            &conn,
            &format!("m{i}"),
            "assistant",
            10 + i,
            10 + i,
            &data.to_string(),
        );
    }
    let mut s = scanner_for(&db);
    let request = PageRequest {
        max_events: 100,
        max_bytes: 2_000,
    };
    let mut anchor = None;
    let mut seen = Vec::new();
    loop {
        let p = page(&mut s, anchor.clone(), PageDirection::Forward, request);
        let bytes: usize = p
            .events
            .iter()
            .map(|e| {
                let t = e.tool.as_ref().unwrap();
                t.name.len()
                    + t.input.as_ref().map_or(0, |b| b.preview.len())
                    + t.output.as_ref().map_or(0, |b| b.preview.len())
            })
            .sum();
        assert!(bytes <= request.max_bytes, "page bytes {bytes}");
        for e in &p.events {
            let t = e.tool.as_ref().unwrap();
            // Call and result are one row: both sides present on the same event.
            assert!(t.input.is_some() && t.output.is_some());
            seen.push(e.id.clone());
        }
        if !p.has_more {
            break;
        }
        assert!(!p.events.is_empty());
        anchor = p.next;
    }
    assert_eq!(seen.len(), 6);
}

#[test]
fn updated_row_invalidates_page_content_and_advances_activity() {
    let (_d, db, conn) = fixture();
    msg(
        &conn,
        "m1",
        "assistant",
        10,
        10,
        r#"{"content":[{"type":"text","text":"draft"}]}"#,
    );
    msg(&conn, "m2", "user", 20, 20, r#"{"text":"next"}"#);
    let mut s = scanner_for(&db);
    let first = support::world::rescan(&mut s, NOW, BIG);
    let before = first.sessions[0].meta.activity.at;
    let p = all(&mut s);
    assert_eq!(text(&p.events[0]), "draft");
    let old_locator = p.events[0].text.as_ref().unwrap().locator.clone().unwrap();
    let rec = support::world::try_source(&mut s, &old_locator, BIG, support::world::STEPS)
        .expect("source");
    assert!(rec.raw.contains("draft"));

    // The provider updates the row in place (same id, same created, later updated).
    conn.execute(
        "UPDATE session_message SET data = ?1, time_updated = 9000 WHERE id = 'm1'",
        [r#"{"content":[{"type":"text","text":"final answer"},{"type":"text","text":"more"}]}"#],
    )
    .expect("update");
    let p2 = all(&mut s);
    let ids: Vec<_> = p2.events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["m1#0", "m1#1", "m2#0"], "exact ids preserved");
    assert_eq!(text(&p2.events[0]), "final answer");
    assert_eq!(
        p2.events[0].timestamp,
        Some(10),
        "order key stays time_created"
    );
    // The previous locator now names a superseded generation.
    assert!(matches!(
        support::world::try_source(&mut s, &old_locator, BIG, support::world::STEPS),
        Err(ProviderError::Unavailable(_))
    ));
    let new_locator = p2.events[0].text.as_ref().unwrap().locator.clone().unwrap();
    assert_ne!(new_locator.generation, old_locator.generation);
    assert!(
        support::world::try_source(&mut s, &new_locator, BIG, support::world::STEPS)
            .expect("fresh")
            .raw
            .contains("final answer")
    );
    // Roster activity advances.
    let second = support::world::rescan(&mut s, NOW, BIG);
    assert_eq!(second.sessions.len(), 1);
    assert!(second.sessions[0].meta.activity.at > before);
    assert_eq!(second.sessions[0].meta.activity.at, Some(9000));
}

#[test]
fn source_only_and_unknown_records_appear_in_diagnostics() {
    let (_d, db, conn) = fixture();
    msg(&conn, "m1", "user", 10, 10, r#"{"text":"shown"}"#);
    msg(&conn, "m2", "mystery", 20, 20, r#"{"text":"?"}"#);
    msg(&conn, "m3", "assistant", 30, 30, r#"{"content":[]}"#);
    msg(&conn, "m4", "user", 40, 40, "{not json");
    msg(
        &conn,
        "m5",
        "assistant",
        50,
        50,
        r#"{"content":[{"type":"text","text":"ok"},{"type":"hologram","x":1}]}"#,
    );
    msg(
        &conn,
        "m6",
        "user",
        60,
        60,
        &format!(r#"{{"text":"{}"}}"#, "y".repeat(5_000)),
    );
    let mut config = config_for_db(&db);
    config.max_record_bytes = 1_000;
    let mut s = scanner_with(&db, &config);
    let p = all(&mut s);
    let ids: Vec<_> = p.events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["m1#0", "m5#0"]);
    let diags: Vec<_> = p
        .diagnostics
        .iter()
        .map(|d| {
            let LocatorKind::DbRow {
                row_id, part_index, ..
            } = &d.locator.kind
            else {
                panic!("db locator")
            };
            (row_id.as_str(), *part_index, d.label.as_str(), d.hidden)
        })
        .collect();
    assert_eq!(diags.len(), 5, "{diags:?}");
    assert_eq!(diags[0].0, "m2");
    assert!(diags[0].2.contains("unknown record type"));
    assert_eq!(diags[1].0, "m3");
    assert!(diags[1].2.contains("source-only"));
    assert_eq!(diags[2].0, "m4");
    assert!(diags[2].2.contains("malformed"));
    assert_eq!((diags[3].0, diags[3].1), ("m5", Some(1)));
    assert!(diags[3].2.contains("unknown part type"));
    assert_eq!(diags[4].0, "m6");
    assert!(diags[4].2.contains("oversized"));
    assert!(diags.iter().all(|d| d.3));
    // The oversized body never reaches memory as a preview.
    let m6 = p.diagnostics.last().unwrap();
    let rec = support::world::try_source(&mut s, &m6.locator, BIG, support::world::STEPS)
        .expect("source record");
    assert!(rec.unavailable.is_some() && rec.raw.is_empty());
}

#[test]
fn source_locator_round_trips_and_rejects_foreign_or_stale_locators() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("ses_2", 1_000, 1_000));
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES ('other', 'ses_2', 'user', 0, 5, 5, '{\"text\":\"theirs\"}')",
        [],
    )
    .expect("insert");
    msg(
        &conn,
        "m1",
        "assistant",
        10,
        10,
        r#"{"content":[{"type":"text","text":"mine token=abcdef1234567890SECRET"}]}"#,
    );
    let mut s = scanner_for(&db);
    let p = all(&mut s);
    let locator = p.events[0].text.as_ref().unwrap().locator.clone().unwrap();
    assert_eq!(locator, p.events[0].provenance.locator);
    let rec =
        support::world::try_source(&mut s, &locator, BIG, support::world::STEPS).expect("source");
    assert!(rec.raw.contains("mine"));
    assert!(rec.unavailable.is_none());
    assert!(
        !rec.display.contains("abcdef1234567890SECRET"),
        "display is redacted"
    );

    let reject = |s: &mut OpenCodeScanner, l: SourceLocator| {
        support::world::try_source(s, &l, BIG, support::world::STEPS)
    };
    // Another session's row.
    let mut foreign = locator.clone();
    foreign.kind = LocatorKind::DbRow {
        table: "session_message".into(),
        row_id: "other".into(),
        part_index: Some(0),
    };
    assert!(matches!(
        reject(&mut s, foreign),
        Err(ProviderError::Unsupported(_))
    ));
    // Table outside the allowlist.
    for table in ["account", "session_v2", "session_message; DROP TABLE x"] {
        let mut bad = locator.clone();
        bad.kind = LocatorKind::DbRow {
            table: table.into(),
            row_id: "m1".into(),
            part_index: Some(0),
        };
        assert!(
            matches!(reject(&mut s, bad), Err(ProviderError::Unsupported(_))),
            "{table}"
        );
    }
    // File locator, wrong provider key, missing generation, injected id, bad part.
    let mut file = locator.clone();
    file.kind = LocatorKind::FileOffset {
        path: "x".into(),
        offset: 0,
    };
    assert!(matches!(
        reject(&mut s, file),
        Err(ProviderError::Unsupported(_))
    ));
    let mut nogen = locator.clone();
    nogen.generation = None;
    assert!(matches!(
        reject(&mut s, nogen),
        Err(ProviderError::Unavailable(_))
    ));
    let mut inj = locator.clone();
    inj.kind = LocatorKind::DbRow {
        table: "session_message".into(),
        row_id: "m1' OR '1'='1".into(),
        part_index: Some(0),
    };
    assert!(matches!(
        reject(&mut s, inj),
        Err(ProviderError::Unavailable(_))
    ));
    let mut part = locator.clone();
    part.kind = LocatorKind::DbRow {
        table: "session_message".into(),
        row_id: "m1".into(),
        part_index: Some(7),
    };
    assert!(matches!(
        reject(&mut s, part),
        Err(ProviderError::Unavailable(_))
    ));
    let mut wrong = locator.clone();
    wrong.session = SessionKey {
        harness: Harness::Codex,
        ..key("ses_1")
    };
    assert!(matches!(
        reject(&mut s, wrong),
        Err(ProviderError::Unsupported(_))
    ));
    // Forged session key sources are rejected by the page API too.
    let mut forged = key("ses_1");
    forged.source = SourceKey::DbRow {
        database: DATABASE.with(|d| d.borrow().clone()),
        table: "account".into(),
        id: "ses_1".into(),
    };
    assert!(matches!(
        support::world::try_transcript(
            &mut s,
            &forged,
            None,
            PageDirection::Forward,
            ALL,
            BIG,
            support::world::STEPS
        ),
        Err(ProviderError::Unsupported(_))
    ));
    assert!(matches!(
        support::world::try_transcript(
            &mut s,
            &key("nope"),
            None,
            PageDirection::Forward,
            ALL,
            BIG,
            support::world::STEPS
        ),
        Err(ProviderError::Unavailable(_))
    ));
}

#[test]
fn legacy_schema_has_no_transcript_yet() {
    let dir = tmp();
    let db = dir.path().join("opencode.db");
    let conn = create_legacy(&db);
    insert_legacy_session(&conn, &Session::new("ses_1", 1, 1));
    insert_legacy_message(&conn, "m1", "ses_1", 1, 1);
    let mut s = scanner_for(&db);
    let err = support::world::try_transcript(
        &mut s,
        &key("ses_1"),
        None,
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .unwrap_err();
    assert!(matches!(err, ProviderError::Unsupported(_)), "{err:?}");
}

#[test]
fn anchors_are_strictly_monotonic_in_ord_across_equal_timestamps() {
    let (_d, db, conn) = fixture();
    // Ids that prefix each other and sort around the separator, equal timestamps.
    for (id, parts) in [
        ("a", 2),
        ("a!", 1),
        ("ab", 2),
        ("a\u{1}", 1),
        ("b", 11),
        ("a\u{0}x", 1),
        ("a\u{0}", 2),
        ("a\u{0}\u{0}", 1),
    ] {
        let content: Vec<_> = (0..parts)
            .map(|i| json!({"type": "text", "text": format!("{id}{i}")}))
            .collect();
        msg(
            &conn,
            id,
            "assistant",
            500,
            500,
            &json!({"content": content}).to_string(),
        );
    }
    let mut s = scanner_for(&db);
    let full = all(&mut s);
    let ids: Vec<_> = full.events.iter().map(|e| e.id.clone()).collect();
    for size in [1usize, 2, 3] {
        for direction in [PageDirection::Forward, PageDirection::Backward] {
            let request = PageRequest {
                max_events: size,
                max_bytes: 10_000_000,
            };
            let mut anchor = None;
            let mut anchors: Vec<PageAnchor> = Vec::new();
            let mut got: Vec<Vec<String>> = Vec::new();
            loop {
                let p = page(&mut s, anchor.clone(), direction, request);
                got.push(p.events.iter().map(|e| e.id.clone()).collect());
                if let Some(n) = &p.next {
                    anchors.push(n.clone());
                }
                if !p.has_more {
                    break;
                }
                anchor = p.next;
            }
            for w in anchors.windows(2) {
                match direction {
                    PageDirection::Forward => assert!(w[0] < w[1], "{:?}", w),
                    PageDirection::Backward => assert!(w[0] > w[1], "{:?}", w),
                }
            }
            if direction == PageDirection::Backward {
                got.reverse();
            }
            let flat: Vec<String> = got.into_iter().flatten().collect();
            assert_eq!(flat, ids, "{direction:?} size {size}");
        }
    }
    // Every event's own anchor, ordered by Ord, matches display order.
    let mut each: Vec<PageAnchor> = Vec::new();
    let mut anchors = Vec::new();
    let mut anchor = None;
    loop {
        let p = page(
            &mut s,
            anchor.clone(),
            PageDirection::Forward,
            PageRequest {
                max_events: 1,
                max_bytes: 10_000_000,
            },
        );
        anchors.extend(p.next.clone());
        if !p.has_more {
            break;
        }
        anchor = p.next;
    }
    each.extend(anchors.iter().cloned());
    let mut sorted = each.clone();
    sorted.sort();
    assert_eq!(sorted, each);
    assert_eq!(each.len(), ids.len());
}

#[test]
fn record_cap_is_enforced_in_bytes_for_multibyte_and_nul_text() {
    let (_d, db, conn) = fixture();
    // 300 four-byte characters: 300 code points, 1200 bytes.
    let multibyte = "\u{1F600}".repeat(300);
    msg(
        &conn,
        "m1",
        "user",
        10,
        10,
        &format!(r#"{{"text":"{multibyte}"}}"#),
    );
    // A NUL inside the text; `length()` on TEXT would stop here and under-count.
    let nul_body = format!(r#"{{"text":"a{}"}}"#, "b".repeat(2_000));
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES ('m2', 'ses_1', 'user', 0, 20, 20, CAST(?1 AS TEXT) || char(0) || CAST(?1 AS TEXT))",
        [nul_body],
    )
    .expect("insert nul");
    msg(&conn, "m3", "user", 30, 30, r#"{"text":"small"}"#);
    let mut config = config_for_db(&db);
    config.max_record_bytes = 1_000;
    let mut s = scanner_with(&db, &config);
    let p = all(&mut s);
    let ids: Vec<_> = p.events.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, ["m3#0"]);
    assert_eq!(p.diagnostics.len(), 2, "{:?}", p.diagnostics);
    assert!(
        p.diagnostics.iter().all(|d| d.label.contains("oversized")),
        "{:?}",
        p.diagnostics
    );
    for d in &p.diagnostics {
        let rec = support::world::try_source(&mut s, &d.locator, BIG, support::world::STEPS)
            .expect("record");
        assert!(rec.raw.is_empty());
        assert!(rec.unavailable.as_deref().unwrap().contains("1000-byte"));
    }
    // Exactly at the cap is accepted (bytes, not code points).
    config.max_record_bytes = 1_200 + r#"{"text":""}"#.len();
    let mut s = scanner_with(&db, &config);
    let p = all(&mut s);
    assert_eq!(p.events[0].id, "m1#0");
}

#[test]
fn in_place_row_update_is_reported_as_invalidated_in_the_delta() {
    let (_d, db, conn) = fixture();
    msg(
        &conn,
        "m1",
        "assistant",
        10,
        10,
        r#"{"content":[{"type":"text","text":"draft"}]}"#,
    );
    let mut s = scanner_for(&db);
    let first = support::world::rescan(&mut s, NOW, BIG);
    assert!(first.invalidated.is_empty(), "first sight is not a change");
    let again = support::world::rescan(&mut s, NOW, BIG);
    assert!(again.invalidated.is_empty());
    conn.execute(
        "UPDATE session_message SET data = ?1, time_updated = 9000 WHERE id = 'm1'",
        [r#"{"content":[{"type":"text","text":"final"}]}"#],
    )
    .expect("update");
    let after = support::world::rescan(&mut s, NOW, BIG);
    // Only a hydrated session can have its evidence invalidated, and it is reported once.
    assert!(!after.invalidated.is_empty());
    assert!(
        after
            .invalidated
            .iter()
            .all(|k| *k == after.sessions[0].meta.key)
    );
}

#[test]
fn v2_subagent_metadata_links_the_child_only_from_the_supported_shape() {
    use agentscope::domain::SessionLink;
    let (_d, db, conn) = fixture();
    let tool = |id: &str, name: &str, metadata: Value, output: Option<&str>| {
        let mut state = json!({"status": "completed", "input": {}, "metadata": metadata});
        if let Some(text) = output {
            state["output"] = json!(text);
        }
        json!({"type": "tool", "id": id, "name": name, "state": state})
    };
    let parts = [
        tool("c1", "subagent", json!({"sessionID": "ses_child"}), None),
        // Case-sensitive tool name and key: neither may be normalized into a link.
        tool(
            "c2",
            "Subagent",
            json!({"sessionID": "ses_wrong_name"}),
            None,
        ),
        tool(
            "c3",
            "subagent",
            json!({"sessionid": "ses_wrong_key"}),
            None,
        ),
        tool("c4", "task", json!({"sessionId": "ses_legacy_shape"}), None),
        // Output text mentioning a session is never authority.
        tool(
            "c5",
            "subagent",
            json!({}),
            Some("sessionID: ses_from_output"),
        ),
        // Control characters or absurd lengths are not plausible identifiers.
        tool(
            "c6",
            "subagent",
            json!({"sessionID": "ses_\u{1b}[31m"}),
            None,
        ),
        tool("c7", "subagent", json!({"sessionID": 17}), None),
    ];
    msg(
        &conn,
        "m1",
        "assistant",
        10,
        10,
        &json!({"content": [parts[0].clone()], "model": {"id": "m", "providerID": "p"}})
            .to_string(),
    );
    for (i, part) in parts.iter().enumerate().skip(1) {
        msg(
            &conn,
            &format!("m{}", i + 1),
            "assistant",
            10 + i as i64,
            10 + i as i64,
            &json!({"content": [part], "model": {"id": "m", "providerID": "p"}}).to_string(),
        );
    }

    let page = all(&mut scanner_for(&db));
    let tools: Vec<&Event> = page
        .events
        .iter()
        .filter(|e| e.kind == EventKind::Tool)
        .collect();
    assert_eq!(tools.len(), 7);
    assert_eq!(
        tools[0].tool.as_ref().unwrap().child_session,
        Some(SessionLink::Unresolved("ses_child".into()))
    );
    for event in &tools[1..] {
        assert_eq!(
            event.tool.as_ref().unwrap().child_session,
            None,
            "no guessed jump from {:?}",
            event.tool.as_ref().unwrap().name
        );
    }
}
