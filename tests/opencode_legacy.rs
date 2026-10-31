mod support;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use agentscope::domain::{
    Actor, AggregationClass, EventKind, Harness, Lifecycle, LocatorKind, PageDirection,
    PageRequest, RefreshBudget, SessionKey, SessionSummary, SourceKey, ToolStatus, Warning,
    WarningKind,
};
use agentscope::providers::opencode::OpenCodeScanner;
use agentscope::providers::{ProviderError, ProviderReader};
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

fn scan(scanner: &mut OpenCodeScanner) -> (HashMap<String, SessionSummary>, Vec<Warning>) {
    let mut sessions = HashMap::new();
    let mut warnings = Vec::new();
    loop {
        let delta = support::world::rescan(scanner, NOW, BIG);
        for s in delta.sessions {
            sessions.insert(s.meta.key.native_id.clone(), s);
        }
        warnings.extend(delta.warnings);
        if delta.scan.done {
            return (sessions, warnings);
        }
    }
}

thread_local! {
    /// Database of the scanner this test thread opened last; keys are built from it.
    static DATABASE: std::cell::RefCell<PathBuf> = const { std::cell::RefCell::new(PathBuf::new()) };
}

fn scanner_for(db: &Path) -> OpenCodeScanner {
    DATABASE.with(|d| *d.borrow_mut() = db.to_path_buf());
    OpenCodeScanner::new(&config_for_db(db)).expect("scanner")
}

fn legacy_key(id: &str) -> SessionKey {
    SessionKey {
        harness: Harness::OpenCode,
        native_id: id.to_owned(),
        source: SourceKey::DbRow {
            database: DATABASE.with(|d| d.borrow().clone()),
            table: "session".into(),
            id: id.to_owned(),
        },
    }
}

fn message(conn: &Connection, session: &str, id: &str, at: i64, data: Value) {
    conn.execute(
        "INSERT INTO message(id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?3, ?4)",
        params![id, session, at, data.to_string()],
    )
    .expect("insert message");
}

fn part(conn: &Connection, session: &str, message: &str, id: &str, at: i64, data: Value) {
    conn.execute(
        "INSERT INTO part(id, message_id, session_id, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
        params![id, message, session, at, data.to_string()],
    )
    .expect("insert part");
}

fn legacy_conversation(conn: &Connection) {
    insert_legacy_session(conn, &Session::new("old", 10, 100));
    message(conn, "old", "m1", 10, json!({"role": "user"}));
    part(
        conn,
        "old",
        "m1",
        "p1",
        10,
        json!({"type": "text", "text": "fix it"}),
    );
    message(
        conn,
        "old",
        "m2",
        20,
        json!({"role": "assistant", "finish": "stop", "cost": 0.5,
               "tokens": {"input": 10, "output": 4, "reasoning": 1,
                          "cache": {"read": 2, "write": 3}}}),
    );
    part(conn, "old", "m2", "p2", 20, json!({"type": "step-start"}));
    part(
        conn,
        "old",
        "m2",
        "p3",
        21,
        json!({"type": "reasoning", "text": "hmm"}),
    );
    part(
        conn,
        "old",
        "m2",
        "p4",
        22,
        json!({"type": "tool", "callID": "c1", "tool": "bash",
               "state": {"status": "completed", "input": {"cmd": "ls"}, "output": "a.rs",
                         "metadata": {"exit": 0}, "time": {"start": 22, "end": 23}}}),
    );
    part(
        conn,
        "old",
        "m2",
        "p5",
        24,
        json!({"type": "text", "text": "done"}),
    );
}

#[test]
fn legacy_schema_falls_back_when_v2_is_absent_or_empty() {
    // Absent: legacy tables only.
    let dir = tempfile::tempdir().expect("tempdir");
    let only = dir.path().join("only.db");
    {
        let conn = create_legacy(&only);
        legacy_conversation(&conn);
    }
    let mut scanner = scanner_for(&only);
    let (sessions, warnings) = scan(&mut scanner);
    let old = &sessions["old"];
    assert!(warnings.is_empty(), "no v2 shape means no fallback warning");
    assert!(old.meta.model.is_none(), "legacy model JSON stays unset");
    assert_eq!(old.lifecycle, Lifecycle::Completed);
    assert_eq!(old.last_completion, Some(20));
    let usage = old.usage.as_ref().expect("derived usage");
    assert_eq!(usage.class, AggregationClass::Derived);
    assert_eq!(usage.total, Some(20));
    assert_eq!(usage.cost.usd, Some(0.5));
    assert_eq!(usage.cost.class, AggregationClass::Derived);

    let page = support::world::try_transcript(
        &mut scanner,
        &legacy_key("old"),
        None,
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .expect("legacy page");
    let rows: Vec<_> = page
        .events
        .iter()
        .map(|e| (e.id.as_str(), e.kind, e.actor))
        .collect();
    assert_eq!(
        rows,
        vec![
            ("p1#0", EventKind::UserPrompt, Actor::User),
            ("p3#0", EventKind::Reasoning, Actor::Assistant),
            ("p4#0", EventKind::Tool, Actor::Tool),
            ("p5#0", EventKind::AssistantMessage, Actor::Assistant),
        ]
    );
    assert_eq!(page.diagnostics.len(), 1, "the step marker stays reachable");
    let tool = page.events[2].tool.as_ref().expect("tool");
    assert_eq!(tool.name, "bash");
    assert_eq!(tool.call_id.as_deref(), Some("c1"));
    assert_eq!(tool.status, ToolStatus::Completed);
    assert_eq!(tool.exit_code, Some(0));
    assert_eq!(
        tool.output.as_ref().map(|o| o.preview.as_str()),
        Some("a.rs")
    );
    assert_eq!((tool.started_at, tool.completed_at), (Some(22), Some(23)));

    // Paging by anchor never repeats or skips.
    let mut seen = Vec::new();
    let mut anchor = None;
    loop {
        let p = support::world::try_transcript(
            &mut scanner,
            &legacy_key("old"),
            anchor.clone(),
            PageDirection::Forward,
            PageRequest {
                max_events: 1,
                max_bytes: 10_000,
            },
            BIG,
            support::world::STEPS,
        )
        .expect("page");
        seen.extend(p.events.iter().map(|e| e.id.clone()));
        if !p.has_more {
            break;
        }
        anchor = p.next;
    }
    assert_eq!(seen, ["p1#0", "p3#0", "p4#0", "p5#0"]);

    // The locator addresses the exact part row.
    let locator = page.events[2].provenance.locator.clone();
    assert!(matches!(
        &locator.kind,
        LocatorKind::DbRow { table, row_id, .. } if table == "part" && row_id == "p4"
    ));
    let record = support::world::try_source(&mut scanner, &locator, BIG, support::world::STEPS)
        .expect("record");
    assert!(record.raw.contains("\"callID\""));
    assert!(record.unavailable.is_none());

    // Empty v2 tables with legacy rows: legacy wins, with a warning.
    let both = dir.path().join("both.db");
    {
        let conn = create(&both, &format!("{V2_SCHEMA}{}", legacy_tables_only()));
        legacy_conversation(&conn);
    }
    let (sessions, warnings) = scan(&mut scanner_for(&both));
    assert!(sessions.contains_key("old"));
    assert!(
        warnings
            .iter()
            .any(|w| w.key.kind == WarningKind::Unsupported && w.message.contains("legacy"))
    );
}

/// Legacy tables without the ones `V2_SCHEMA` already defines (`project`, `account`).
fn legacy_tables_only() -> &'static str {
    "
CREATE TABLE session(
    id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, title TEXT, directory TEXT,
    version TEXT, share_url TEXT, time_created INTEGER NOT NULL,
    time_updated INTEGER NOT NULL, time_archived INTEGER);
CREATE TABLE message(
    id TEXT PRIMARY KEY, session_id TEXT NOT NULL,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
CREATE TABLE part(
    id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
    time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
"
}

#[test]
fn populated_v2_is_never_mixed_with_legacy_rows() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("mixed.db");
    {
        let conn = create(&db, &format!("{V2_SCHEMA}{}", legacy_tables_only()));
        legacy_conversation(&conn);
        let mut v2 = Session::new("new", 5, 500);
        v2.tokens = Some([1, 1, 1, 1, 1]);
        insert_session(&conn, &v2);
        insert_message(&conn, "m-new", "new", 6, 6);
    }
    let mut scanner = scanner_for(&db);
    let (sessions, warnings) = scan(&mut scanner);
    assert!(sessions.contains_key("new"));
    assert!(
        !sessions.contains_key("old"),
        "legacy rows stay out of a v2 roster"
    );
    assert!(warnings.is_empty());
    let err = support::world::try_transcript(
        &mut scanner,
        &legacy_key("old"),
        None,
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .expect_err("legacy key is not valid against v2");
    assert!(matches!(err, ProviderError::Unsupported(_)));
}

#[test]
fn legacy_lifecycle_and_usage_follow_message_finish_and_tokens() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("legacy.db");
    {
        let conn = create_legacy(&db);
        insert_legacy_session(&conn, &Session::new("busy", 1, 50));
        message(&conn, "busy", "b1", 10, json!({"role": "user"}));
        message(
            &conn,
            "busy",
            "b2",
            20,
            json!({"role": "assistant", "finish": "tool-calls", "tokens": {"input": 1}}),
        );
        insert_legacy_session(&conn, &Session::new("bare", 1, 40));
        message(&conn, "bare", "c1", 10, json!({"role": "user"}));
    }
    let (sessions, _) = scan(&mut scanner_for(&db));
    let busy = &sessions["busy"];
    assert_eq!(busy.lifecycle, Lifecycle::Ongoing);
    assert!(busy.unfinished_work);
    let usage = busy.usage.as_ref().expect("usage");
    assert_eq!(
        usage.class,
        AggregationClass::Partial,
        "missing token categories are never zero"
    );
    assert_eq!(usage.total, None);
    assert!(sessions["bare"].usage.is_none());
}

#[test]
fn unsupported_schema_lists_nothing_and_refuses_transcripts() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("odd.db");
    {
        let conn = create(
            &db,
            "CREATE TABLE session(id TEXT); CREATE TABLE message(id TEXT);",
        );
        conn.execute("INSERT INTO session VALUES ('x')", [])
            .expect("insert");
    }
    let mut scanner = scanner_for(&db);
    let (sessions, warnings) = scan(&mut scanner);
    assert!(sessions.is_empty());
    assert!(
        warnings
            .iter()
            .any(|w| w.key.kind == WarningKind::Unsupported)
    );
    let err = support::world::try_transcript(
        &mut scanner,
        &legacy_key("x"),
        None,
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .expect_err("unsupported");
    assert!(matches!(err, ProviderError::Unsupported(_)));
}

#[test]
fn legacy_message_rows_of_every_kind_spend_the_evidence_budget() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("legacy.db");
    {
        let conn = create_legacy(&db);
        insert_legacy_session(&conn, &Session::new("s", 1, 100));
        for i in 0..9 {
            message(
                &conn,
                "s",
                &format!("u{i}"),
                10 + i,
                json!({"role": "user"}),
            );
        }
        message(
            &conn,
            "s",
            "a",
            5,
            json!({"role": "assistant", "finish": "stop",
                   "tokens": {"input": 1, "output": 1, "reasoning": 0,
                              "cache": {"read": 0, "write": 0}}}),
        );
    }
    let mut scanner = scanner_for(&db);
    let small = RefreshBudget {
        max_records: 3,
        ..BIG
    };
    let key = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .remove(0)
        .key;
    let first = scanner
        .hydrate(support::world::new_job(), &key, NOW, small)
        .unwrap();
    assert!(!first.done);
    let mut out = HashMap::new();
    let (summary, rest) = support::world::hydrate_steps(&mut scanner, &key, NOW, small, 1000);
    let steps = rest + 1;
    out.insert(key.native_id.clone(), summary);
    assert!(steps >= 3, "10 message rows at 3 per step, got {steps}");
    assert_eq!(out["s"].usage.as_ref().and_then(|u| u.total), Some(2));
    assert_eq!(
        out["s"].lifecycle,
        Lifecycle::Ongoing,
        "newer user rows reopen"
    );
}

#[test]
fn legacy_oversized_prefix_group_makes_derived_usage_partial() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("legacy.db");
    {
        let conn = create_legacy(&db);
        insert_legacy_session(&conn, &Session::new("s", 1, 100));
        let assistant = |input: u64| {
            json!({"role": "assistant", "finish": "stop",
                   "tokens": {"input": input, "output": 0, "reasoning": 0,
                              "cache": {"read": 0, "write": 0}}})
        };
        message(&conn, "s", "k1", 5, assistant(1));
        // A user row and an assistant row whose ids share an oversized prefix.
        message(&conn, "s", &"m".repeat(5000), 6, json!({"role": "user"}));
        message(
            &conn,
            "s",
            &format!("{}a", "m".repeat(5000)),
            6,
            assistant(1000),
        );
        message(&conn, "s", "k2", 20, assistant(10));
    }
    let mut scanner = scanner_for(&db);
    let mut out = HashMap::new();
    for _ in 0..50 {
        let d = support::world::rescan(
            &mut scanner,
            NOW,
            RefreshBudget {
                max_records: 1,
                ..BIG
            },
        );
        for s in d.sessions {
            out.insert(s.meta.key.native_id.clone(), s);
        }
        if d.scan.done {
            break;
        }
    }
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.total, None, "a skipped group may hide assistant rows");
    assert_eq!(usage.categories.input, Some(11));
}

#[test]
fn legacy_task_metadata_links_the_child_and_other_keys_do_not() {
    use agentscope::domain::SessionLink;
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("legacy.db");
    {
        let conn = create_legacy(&db);
        insert_legacy_session(&conn, &Session::new("old", 10, 100));
        message(&conn, "old", "m1", 10, json!({"role": "assistant"}));
        let tool = |id: &str, name: &str, metadata: Value, output: &str| {
            json!({"type": "tool", "callID": id, "tool": name,
                   "state": {"status": "completed", "input": {}, "output": output,
                             "metadata": metadata}})
        };
        part(
            &conn,
            "old",
            "m1",
            "p1",
            11,
            tool("c1", "task", json!({"sessionId": "ses_child"}), ""),
        );
        // Wrong tool, wrong key, and output text are never authority.
        part(
            &conn,
            "old",
            "m1",
            "p2",
            12,
            tool("c2", "subagent", json!({"sessionID": "ses_v2_shape"}), ""),
        );
        part(
            &conn,
            "old",
            "m1",
            "p3",
            13,
            tool("c3", "task", json!({"sessionID": "ses_wrong_key"}), ""),
        );
        part(
            &conn,
            "old",
            "m1",
            "p4",
            14,
            tool("c4", "task", json!({}), "sessionId: ses_from_output"),
        );
    }
    let mut scanner = scanner_for(&db);
    let page = support::world::try_transcript(
        &mut scanner,
        &legacy_key("old"),
        None,
        PageDirection::Forward,
        ALL,
        BIG,
        support::world::STEPS,
    )
    .expect("legacy page");
    let tools: Vec<&agentscope::domain::Event> = page
        .events
        .iter()
        .filter(|e| e.kind == EventKind::Tool)
        .collect();
    assert_eq!(tools.len(), 4);
    assert_eq!(
        tools[0].tool.as_ref().unwrap().child_session,
        Some(SessionLink::Unresolved("ses_child".into()))
    );
    for event in &tools[1..] {
        assert_eq!(
            event.tool.as_ref().unwrap().child_session,
            None,
            "no guessed jump"
        );
    }
}
