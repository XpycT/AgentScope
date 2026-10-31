mod support;

use std::collections::HashMap;
use std::path::Path;

use agentscope::domain::{
    AggregationClass, Lifecycle, RefreshBudget, SessionSummary, TokenCategories,
};
use agentscope::providers::ProviderReader;
use agentscope::providers::opencode::OpenCodeScanner;
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use support::NOW;
use support::facts::price_schedule;
use support::opencode::*;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

fn scan(scanner: &mut OpenCodeScanner) -> HashMap<String, SessionSummary> {
    let mut out = HashMap::new();
    loop {
        let delta = support::world::rescan(scanner, NOW, BIG);
        for s in delta.sessions {
            out.insert(s.meta.key.native_id.clone(), s);
        }
        if delta.scan.done {
            return out;
        }
    }
}

fn scanner_for(db: &Path) -> OpenCodeScanner {
    OpenCodeScanner::new(&config_for_db(db)).expect("scanner")
}

fn priced_scanner(db: &Path, cards: &str) -> OpenCodeScanner {
    let mut config = config_for_db(db);
    config.pricing = price_schedule(cards);
    OpenCodeScanner::new(&config).expect("scanner")
}

fn msg(conn: &Connection, session: &str, id: &str, ty: &str, at: i64, data: Value) {
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, 0, ?4, ?4, ?5)",
        params![id, session, ty, at, data.to_string()],
    )
    .expect("insert message");
}

fn assistant(finish: Option<&str>, tokens: Value, cost: Option<f64>) -> Value {
    let mut v = json!({"content": [{"type": "text", "text": "ok"}], "tokens": tokens});
    if let Some(f) = finish {
        v["finish"] = json!(f);
    }
    if let Some(c) = cost {
        v["cost"] = json!(c);
    }
    v
}

fn tokens(input: u64, output: u64, reasoning: u64, read: u64, write: u64) -> Value {
    json!({"input": input, "output": output, "reasoning": reasoning,
           "cache": {"read": read, "write": write}})
}

fn fixture() -> (tempfile::TempDir, std::path::PathBuf, Connection) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("opencode.db");
    let conn = create_v2(&db);
    (dir, db, conn)
}

#[test]
fn session_totals_beat_message_sums_and_zero_cost_stays_reported() {
    let (_d, db, conn) = fixture();
    let model = model_json("free-model", "acme", "");
    let mut free = Session::new("free", 10, 20);
    free.tokens = Some([10, 5, 2, 3, 4]);
    free.cost = Some(0.0);
    free.model = Some(&model);
    insert_session(&conn, &free);
    // Message sums lag (and carry their own cost) while the turn is in flight; neither
    // may replace the session's reported figures.
    msg(
        &conn,
        "free",
        "m1",
        "assistant",
        11,
        assistant(Some("stop"), tokens(1, 1, 1, 1, 1), Some(9.9)),
    );
    let mut paid = Session::new("paid", 10, 30);
    paid.tokens = Some([100, 50, 0, 0, 0]);
    paid.cost = Some(1.25);
    insert_session(&conn, &paid);

    let out = scan(&mut scanner_for(&db));
    let usage = out["free"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Reported);
    assert_eq!(
        usage.categories,
        TokenCategories {
            input: Some(10),
            // Separately reported reasoning is folded into canonical output (5 + 2).
            output: Some(7),
            reasoning: Some(2),
            cache_read: Some(3),
            cache_write: Some(4),
            ..TokenCategories::default()
        }
    );
    assert_eq!(
        usage.total,
        Some(24),
        "session columns, not the message sum"
    );
    // No message scan backs the session totals, so no model attribution is claimed.
    assert!(usage.models.is_empty());
    assert!(!usage.coverage.complete);
    assert_eq!(usage.reported_cost_usd, Some(0.0));
    // The reported zero stays reported; "acme/free-model" has no card, so the
    // API-equivalent is unknown rather than a complete $0.
    assert_eq!(usage.cost.usd, None);
    assert_eq!(usage.cost.class, AggregationClass::Unknown);
    assert!(
        usage
            .cost
            .note
            .as_deref()
            .unwrap()
            .contains("reported no charge")
    );

    let usage = out["paid"].usage.as_ref().expect("usage");
    assert_eq!(usage.total, Some(150));
    assert_eq!(usage.cost.usd, Some(1.25));
    assert_eq!(usage.cost.class, AggregationClass::Reported);
    assert!(usage.models.is_empty());
}

#[test]
fn partial_session_columns_are_partial_not_derived_or_zero() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 2));
    conn.execute(
        "UPDATE session_v2 SET tokens_input = 7, tokens_output = 3 WHERE id = 's'",
        [],
    )
    .expect("update");
    msg(
        &conn,
        "s",
        "m1",
        "assistant",
        5,
        assistant(None, tokens(100, 100, 100, 100, 100), None),
    );
    let usage = scan(&mut scanner_for(&db))["s"]
        .usage
        .clone()
        .expect("usage");
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.categories.input, Some(7));
    assert_eq!(usage.categories.reasoning, None);
    assert_eq!(usage.total, None);
    assert_eq!(usage.cost.class, AggregationClass::Unknown);
}

#[test]
fn missing_session_totals_derive_from_unique_messages_and_mark_derived() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("full", 1, 100));
    msg(&conn, "full", "u1", "user", 2, json!({"text": "go"}));
    msg(
        &conn,
        "full",
        "a1",
        "assistant",
        3,
        assistant(Some("tool-calls"), tokens(10, 5, 1, 2, 3), Some(0.5)),
    );
    msg(
        &conn,
        "full",
        "a2",
        "assistant",
        4,
        assistant(Some("stop"), tokens(20, 6, 0, 4, 0), Some(0.25)),
    );
    // Same-id rows cannot repeat (primary key); a copied record under a new id is a
    // distinct message and does count, so only exact-id duplicates are guarded.
    insert_session(&conn, &Session::new("lagging", 1, 90));
    msg(
        &conn,
        "lagging",
        "b1",
        "assistant",
        3,
        assistant(Some("stop"), tokens(10, 5, 1, 2, 3), None),
    );
    msg(
        &conn,
        "lagging",
        "b2",
        "assistant",
        4,
        json!({"content": [], "finish": "stop"}),
    );
    insert_session(&conn, &Session::new("empty", 1, 80));
    msg(&conn, "empty", "e1", "user", 2, json!({"text": "hi"}));

    let out = scan(&mut scanner_for(&db));
    let usage = out["full"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Derived);
    assert_eq!(usage.categories.input, Some(30));
    assert_eq!(
        usage.categories.output,
        Some(12),
        "11 output + 1 separate reasoning"
    );
    assert_eq!(usage.categories.reasoning, Some(1));
    assert_eq!(usage.categories.cache_read, Some(6));
    assert_eq!(usage.categories.cache_write, Some(3));
    assert_eq!(usage.total, Some(51), "each message counted exactly once");
    assert_eq!(usage.cost.usd, Some(0.75));
    assert_eq!(usage.cost.class, AggregationClass::Derived);

    let usage = out["lagging"].usage.as_ref().expect("usage");
    assert_eq!(
        usage.class,
        AggregationClass::Partial,
        "a message without token data makes the derivation incomplete"
    );
    assert_eq!(usage.total, None);
    assert_eq!(usage.cost.usd, None);
    assert_eq!(usage.cost.class, AggregationClass::Unknown);

    assert!(out["empty"].usage.is_none(), "no data is not zero");
}

#[test]
fn derived_usage_without_reported_cost_uses_configured_prices_or_unknown() {
    let (_d, db, conn) = fixture();
    let model = model_json("priced", "acme", "");
    let mut s = Session::new("s", 1, 2);
    s.model = Some(&model);
    insert_session(&conn, &s);
    msg(&conn, "s", "a1", "assistant", 3, {
        let mut data = assistant(
            Some("stop"),
            tokens(1_000_000, 1_000_000, 1_000_000, 0, 0),
            None,
        );
        data["modelID"] = json!("priced");
        data["providerID"] = json!("acme");
        data
    });
    let card = |reasoning: Option<f64>| {
        let reasoning = reasoning.map_or(String::new(), |r| format!(r#","reasoning":{r}"#));
        format!(
            r#"[{{"billing_provider":"acme","model":"priced","as_of":"2026-01-01",
                "rates":{{"input":1.0,"output":2.0,"cache_read":0.0,"cache_write":0.0{reasoning}}}}}]"#
        )
    };
    let out = scan(&mut priced_scanner(&db, &card(Some(3.0))));
    let cost = &out["s"].usage.as_ref().expect("usage").cost;
    assert_eq!(cost.class, AggregationClass::Estimated);
    assert_eq!(cost.usd, Some(6.0));
    assert_eq!(cost.as_of.as_deref(), Some("2026-01-01"));

    let out = scan(&mut priced_scanner(&db, &card(None)));
    let cost = &out["s"].usage.as_ref().expect("usage").cost;
    assert_eq!(
        cost.class,
        AggregationClass::Unknown,
        "nonzero reasoning without a price is never a partial number"
    );
    assert_eq!(cost.usd, None);
}

struct Case {
    id: &'static str,
    lifecycle: Lifecycle,
    completion: Option<i64>,
    unfinished: bool,
}

#[test]
fn finish_and_idle_map_to_turn_evidence_not_session_failure() {
    let (_d, db, conn) = fixture();
    let t = || tokens(1, 1, 0, 0, 0);
    let mut n = 0;
    let mut add = |conn: &Connection, id: &str, rows: &[(&str, i64, Value)]| {
        n += 1;
        insert_session(conn, &Session::new(id, 1, 1000 + n));
        for (i, (ty, at, data)) in rows.iter().enumerate() {
            msg(conn, id, &format!("{id}-{i}"), ty, *at, data.clone());
        }
    };
    let user = || json!({"text": "go"});

    add(
        &conn,
        "stop",
        &[
            ("user", 10, user()),
            ("assistant", 20, assistant(Some("stop"), t(), None)),
        ],
    );
    add(
        &conn,
        "tool-calls",
        &[
            ("user", 10, user()),
            ("assistant", 20, assistant(Some("tool-calls"), t(), None)),
        ],
    );
    add(
        &conn,
        "idle",
        &[
            ("user", 10, user()),
            ("assistant", 20, assistant(Some("tool-calls"), t(), None)),
            ("idle", 30, json!({"outcome": "succeeded"})),
        ],
    );
    add(
        &conn,
        "reopened",
        &[
            ("user", 10, user()),
            ("assistant", 20, assistant(Some("stop"), t(), None)),
            ("user", 40, user()),
        ],
    );
    add(
        &conn,
        "length",
        &[
            ("user", 5, json!({"text": "earlier"})),
            ("assistant", 20, assistant(Some("length"), t(), None)),
        ],
    );
    // Tool errors and `unknown` finishes are tool/turn details, not session failure.
    let mut failing = assistant(Some("unknown"), t(), None);
    failing["content"] = json!([{"type": "tool", "id": "c", "name": "bash",
        "state": {"status": "error", "metadata": {"exit": 2}}}]);
    add(&conn, "tool-error", &[("assistant", 20, failing)]);
    add(
        &conn,
        "odd-idle",
        &[
            ("assistant", 20, assistant(Some("unknown"), t(), None)),
            ("idle", 30, json!({"outcome": "failed"})),
        ],
    );
    add(&conn, "bare-idle", &[("idle", 30, json!({}))]);
    add(&conn, "silent", &[("system", 5, json!({"text": "x"}))]);

    insert_session(&conn, &{
        let mut s = Session::new("col-idle", 1, 2000);
        s.idle_outcome = Some("succeeded");
        s
    });

    let cases = [
        Case {
            id: "stop",
            lifecycle: Lifecycle::Completed,
            completion: Some(20),
            unfinished: false,
        },
        Case {
            id: "tool-calls",
            lifecycle: Lifecycle::Ongoing,
            completion: None,
            unfinished: true,
        },
        Case {
            id: "idle",
            lifecycle: Lifecycle::Completed,
            completion: Some(30),
            unfinished: false,
        },
        Case {
            id: "reopened",
            lifecycle: Lifecycle::Ongoing,
            completion: Some(20),
            unfinished: true,
        },
        Case {
            id: "length",
            lifecycle: Lifecycle::Ongoing,
            completion: None,
            unfinished: true,
        },
        Case {
            id: "tool-error",
            lifecycle: Lifecycle::Unknown,
            completion: None,
            unfinished: false,
        },
        Case {
            id: "odd-idle",
            lifecycle: Lifecycle::Unknown,
            completion: None,
            unfinished: false,
        },
        Case {
            id: "bare-idle",
            lifecycle: Lifecycle::Completed,
            completion: Some(30),
            unfinished: false,
        },
        Case {
            id: "silent",
            lifecycle: Lifecycle::Unknown,
            completion: None,
            unfinished: false,
        },
        Case {
            id: "col-idle",
            lifecycle: Lifecycle::Completed,
            completion: Some(2000),
            unfinished: false,
        },
    ];
    let out = scan(&mut scanner_for(&db));
    for case in cases {
        let s = &out[case.id];
        assert_eq!(s.lifecycle, case.lifecycle, "{}", case.id);
        assert_eq!(s.last_completion, case.completion, "{}", case.id);
        assert_eq!(s.unfinished_work, case.unfinished, "{}", case.id);
        assert_ne!(s.lifecycle, Lifecycle::Failed, "{}", case.id);
        assert_ne!(s.lifecycle, Lifecycle::Cancelled, "{}", case.id);
    }
}

#[test]
fn in_place_message_update_refreshes_lifecycle_and_usage() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 50));
    msg(&conn, "s", "u", "user", 10, json!({"text": "go"}));
    msg(
        &conn,
        "s",
        "a",
        "assistant",
        20,
        assistant(Some("tool-calls"), tokens(1, 1, 0, 0, 0), None),
    );
    let mut scanner = scanner_for(&db);
    let first = scan(&mut scanner);
    assert_eq!(first["s"].lifecycle, Lifecycle::Ongoing);
    assert_eq!(first["s"].usage.as_ref().and_then(|u| u.total), Some(2));

    conn.execute(
        "UPDATE session_message SET time_updated = 60, data = ?1 WHERE id = 'a'",
        params![assistant(Some("stop"), tokens(4, 4, 0, 0, 0), None).to_string()],
    )
    .expect("update");
    let second = scan(&mut scanner);
    assert_eq!(second["s"].lifecycle, Lifecycle::Completed);
    assert_eq!(second["s"].last_completion, Some(60));
    assert_eq!(second["s"].usage.as_ref().and_then(|u| u.total), Some(8));
}

fn raw_msg(conn: &Connection, session: &str, id: &str, ty: &str, at: i64, data: Option<&str>) {
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1, ?2, ?3, 0, ?4, ?4, ?5)",
        params![id, session, ty, at, data],
    )
    .expect("insert raw message");
}

fn budget(records: usize, bytes: usize, millis: u64) -> RefreshBudget {
    RefreshBudget {
        max_records: records,
        max_bytes: bytes,
        max_millis: millis,
    }
}

thread_local! {
    static KEYS: std::cell::RefCell<Vec<agentscope::domain::SessionKey>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn scan_with(
    scanner: &mut OpenCodeScanner,
    budget: RefreshBudget,
) -> (usize, HashMap<String, SessionSummary>) {
    let mut out = HashMap::new();
    let mut steps = 0;
    let found = support::world::discover_pass(scanner, NOW, BIG);
    KEYS.with(|k| {
        let mut k = k.borrow_mut();
        for meta in &found.sessions {
            if !k.contains(&meta.key) {
                k.push(meta.key.clone());
            }
        }
    });
    // Discovery reports only new or changed rows; every known session is hydrated.
    for key in KEYS.with(|k| k.borrow().clone()) {
        let (summary, n) = support::world::hydrate_steps(scanner, &key, NOW, budget, 10_000);
        steps += n;
        out.insert(key.native_id.clone(), summary);
    }
    (steps, out)
}

#[test]
fn evidence_collection_is_resumable_under_the_refresh_budget() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 100));
    // Non-assistant rows spend budget too: 12 system rows newest-first.
    for i in 0..12 {
        msg(
            &conn,
            "s",
            &format!("x{i:02}"),
            "system",
            10 + i,
            json!({"text": "n"}),
        );
    }
    msg(
        &conn,
        "s",
        "a1",
        "assistant",
        5,
        assistant(Some("length"), tokens(3, 2, 0, 0, 0), None),
    );
    let mut scanner = scanner_for(&db);

    // The roster row is read but no evidence is published until it completes.
    let key = support::world::discover_pass(&mut scanner, NOW, BIG)
        .sessions
        .remove(0)
        .key;
    KEYS.with(|k| k.borrow_mut().push(key.clone()));
    let first = scanner
        .hydrate(
            support::world::new_job(),
            &key,
            NOW,
            budget(4, 100_000_000, 60_000),
        )
        .unwrap();
    assert!(!first.done, "evidence outstanding keeps hydration open");

    let (steps, out) = scan_with(&mut scanner, budget(4, 100_000_000, 60_000));
    assert!(steps >= 3, "13 rows at 4 per step, got {steps} steps");
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Derived);
    assert_eq!(usage.total, Some(5));

    // Byte budget: at least one row per step still makes progress.
    let mut scanner = scanner_for(&db);
    let (steps, out) = scan_with(&mut scanner, budget(100_000, 1, 60_000));
    assert!(
        steps > 3,
        "1-byte budget reads one row per step, got {steps}"
    );
    assert_eq!(out["s"].usage.as_ref().and_then(|u| u.total), Some(5));
}

#[test]
fn exhausted_budget_reads_nothing_and_keeps_state() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 100));
    msg(
        &conn,
        "s",
        "a1",
        "assistant",
        5,
        assistant(Some("stop"), tokens(1, 1, 0, 0, 0), None),
    );
    let mut scanner = scanner_for(&db);
    for zero in [
        budget(0, 1000, 1000),
        budget(10, 0, 1000),
        budget(10, 1000, 0),
    ] {
        let d = scanner.discover(NOW, zero).unwrap();
        assert!(d.sessions.is_empty() && !d.scan.done);
    }
    let (_, out) = scan_with(&mut scanner, budget(10, 100_000, 60_000));
    assert_eq!(out["s"].lifecycle, Lifecycle::Completed);
    assert_eq!(out["s"].usage.as_ref().and_then(|u| u.total), Some(2));
}

#[test]
fn optional_session_columns_are_kept_independently() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("partial.db");
    {
        let conn = create(
            &db,
            "CREATE TABLE session_v2(
                id TEXT PRIMARY KEY, project_id TEXT, parent_id TEXT, fork_session_id TEXT,
                title TEXT, directory TEXT, model TEXT, version TEXT,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                time_archived INTEGER, tokens_input INTEGER, cost REAL);
             CREATE TABLE session_message(
                id TEXT PRIMARY KEY, session_id TEXT NOT NULL, type TEXT NOT NULL,
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL, data TEXT);
             INSERT INTO session_v2(id, time_created, time_updated, tokens_input, cost)
                VALUES ('s', 1, 2, 7, 0.0);",
        );
        drop(conn);
    }
    let out = scan(&mut scanner_for(&db));
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(usage.categories.input, Some(7));
    assert_eq!(usage.categories.output, None);
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.total, None);
    assert_eq!(
        usage.reported_cost_usd,
        Some(0.0),
        "reported zero cost survives"
    );
    assert_eq!(
        usage.cost.usd, None,
        "partial counters give no API-equivalent"
    );
    assert_eq!(usage.cost.class, AggregationClass::Unknown);
}

#[test]
fn unreadable_idle_rows_are_neutral_and_never_close_a_turn() {
    let (_d, db, conn) = fixture();
    let huge = format!(
        r#"{{"outcome":"succeeded","pad":"{}"}}"#,
        "x".repeat(1_100_000)
    );
    let cases: [(&str, Option<&str>); 5] = [
        ("null", None),
        ("malformed", Some("{not json")),
        ("array", Some("[]")),
        ("oversized", Some(huge.as_str())),
        ("bad-outcome", Some(r#"{"outcome":7}"#)),
    ];
    for (i, (name, data)) in cases.iter().enumerate() {
        insert_session(&conn, &Session::new(name, 1, 100 + i as i64));
        raw_msg(
            &conn,
            name,
            &format!("{name}-u"),
            "user",
            10,
            Some(r#"{"text":"go"}"#),
        );
        raw_msg(&conn, name, &format!("{name}-i"), "idle", 20, *data);
    }
    let out = scan(&mut scanner_for(&db));
    for (name, _) in cases {
        assert_eq!(out[name].lifecycle, Lifecycle::Ongoing, "{name}");
        assert!(out[name].unfinished_work, "{name}");
        assert_eq!(out[name].last_completion, None, "{name}");
    }
}

#[test]
fn session_level_idle_succeeded_reaches_status_synthesis() {
    use agentscope::domain::{StatusLabel, Thresholds};
    use agentscope::status::synthesize;
    use std::time::Duration;
    let (_d, db, conn) = fixture();
    let mut s = Session::new("done", 1, 5_000);
    s.idle_outcome = Some("succeeded");
    insert_session(&conn, &s);
    let out = scan(&mut scanner_for(&db));
    let summary = &out["done"];
    assert_eq!(summary.lifecycle, Lifecycle::Completed);
    assert_eq!(summary.last_completion, Some(5_000));
    let t = Thresholds {
        quiet: Duration::from_secs(300),
        stall: Duration::from_secs(900),
    };
    let quiet = synthesize(summary, 5_000 + 301_000, &t);
    assert_eq!(quiet.label, StatusLabel::Complete);
    let recent = synthesize(summary, 5_000 + 1_000, &t);
    assert_eq!(recent.label, StatusLabel::Recent);
}

#[test]
fn skipped_oversized_message_ids_make_derived_usage_partial() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 100));
    msg(
        &conn,
        "s",
        "a1",
        "assistant",
        5,
        assistant(Some("stop"), tokens(1, 1, 0, 0, 0), Some(0.1)),
    );
    let long_id = "m".repeat(5000);
    msg(
        &conn,
        "s",
        &long_id,
        "assistant",
        6,
        assistant(Some("stop"), tokens(1, 1, 0, 0, 0), Some(0.1)),
    );
    let out = scan(&mut scanner_for(&db));
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.total, None, "an incomplete sum is not a total");
    assert_eq!(usage.cost.usd, None);
}

#[test]
fn oversized_ids_never_stall_derivation_at_one_record_per_step() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 100));
    let big = |tag: char| tag.to_string().repeat(5000);
    let one = |input: u64| assistant(Some("stop"), tokens(input, 0, 0, 0, 0), None);
    msg(&conn, "s", "a1", "assistant", 5, one(1));
    msg(&conn, "s", &big('m'), "assistant", 6, one(1000));
    msg(&conn, "s", &big('n'), "assistant", 7, one(1000));
    // Same millisecond as an oversized row, sorting after its id prefix.
    msg(&conn, "s", "z1", "assistant", 7, one(10));
    msg(&conn, "s", &(big('m') + "x"), "assistant", 7, one(1000));
    msg(&conn, "s", "a2", "assistant", 20, one(100));
    let mut scanner = scanner_for(&db);
    let (steps, out) = scan_with(&mut scanner, budget(1, 100_000_000, 60_000));
    assert!(
        steps <= 20,
        "one row per step, never rereading: {steps} steps"
    );
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.categories.input, Some(111), "only keyable rows count");
    assert_eq!(usage.total, None);
}

#[test]
fn lifecycle_completing_on_the_last_allowed_row_defers_derivation() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 100));
    msg(
        &conn,
        "s",
        "a1",
        "assistant",
        5,
        assistant(Some("stop"), tokens(2, 3, 0, 0, 0), None),
    );
    for tight in [budget(1, 100_000_000, 60_000), budget(100, 1, 60_000)] {
        let mut scanner = scanner_for(&db);
        // Lifecycle finishes on the only allowed row; derivation must wait.
        let key = support::world::discover_pass(&mut scanner, NOW, BIG)
            .sessions
            .remove(0)
            .key;
        KEYS.with(|k| k.borrow_mut().push(key.clone()));
        let first = scanner
            .hydrate(support::world::new_job(), &key, NOW, tight)
            .unwrap();
        assert!(!first.done, "evidence outstanding keeps hydration open");
        let (steps, out) = scan_with(&mut scanner, tight);
        assert!(
            steps >= 2,
            "lifecycle and derivation are separate steps: {steps}"
        );
        assert_eq!(out["s"].lifecycle, Lifecycle::Completed);
        let usage = out["s"].usage.as_ref().expect("usage");
        assert_eq!(usage.class, AggregationClass::Derived);
        assert_eq!(usage.total, Some(5));
    }
}

#[test]
fn ids_sorting_after_a_naive_prefix_cursor_are_still_skipped_once() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 100));
    let one = |input: u64| assistant(Some("stop"), tokens(input, 0, 0, 0, 0), None);
    let prefix = "m".repeat(4097);
    msg(&conn, "s", "a1", "assistant", 5, one(1));
    msg(&conn, "s", &"m".repeat(5000), "assistant", 6, one(1000));
    // Shares the prefix but sorts above `prefix + U+10FFFF`.
    msg(
        &conn,
        "s",
        &format!("{prefix}\u{10FFFF}x"),
        "assistant",
        6,
        one(1000),
    );
    msg(
        &conn,
        "s",
        &format!("{prefix}\u{10FFFF}\u{10FFFF}z"),
        "assistant",
        6,
        one(1000),
    );
    msg(&conn, "s", "a2", "assistant", 20, one(100));
    let (steps, out) = scan_with(&mut scanner_for(&db), budget(1, 100_000_000, 60_000));
    assert!(steps <= 15, "bounded progress, got {steps} steps");
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(usage.categories.input, Some(101));
}

#[test]
fn a_valid_id_equal_to_a_synthetic_prefix_bound_is_counted_once() {
    let (_d, db, conn) = fixture();
    insert_session(&conn, &Session::new("s", 1, 100));
    let one = |input: u64| assistant(Some("stop"), tokens(input, 0, 0, 0, 0), None);
    // Its prefix bound is exactly "n".
    let oversized = format!("m{}", "\u{10FFFF}".repeat(4096));
    msg(&conn, "s", &oversized, "assistant", 6, one(1000));
    msg(&conn, "s", "n", "assistant", 6, one(10));
    msg(&conn, "s", "a2", "assistant", 20, one(100));
    let (steps, out) = scan_with(&mut scanner_for(&db), budget(1, 100_000_000, 60_000));
    assert!(steps <= 15, "got {steps} steps");
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Partial);
    assert_eq!(
        usage.categories.input,
        Some(110),
        "\"n\" counted exactly once"
    );
}

#[test]
fn a_message_naming_another_model_leaves_the_session_aggregate_unattributed() {
    let (_d, db, conn) = fixture();
    let model = model_json("one", "acme", "");
    let mut s = Session::new("s", 1, 2);
    s.model = Some(&model);
    insert_session(&conn, &s);
    for (id, at, model_id) in [("a1", 3, "one"), ("a2", 4, "two")] {
        let mut data = assistant(Some("stop"), tokens(1_000_000, 1_000_000, 0, 0, 0), None);
        data["modelID"] = json!(model_id);
        data["providerID"] = json!("acme");
        msg(&conn, "s", id, "assistant", at, data);
    }
    let cards = r#"[{"billing_provider":"acme","model":"one","rates":{"input":1.0,"output":2.0,"cache_read":0.0,"cache_write":0.0}},
                    {"billing_provider":"acme","model":"two","rates":{"input":9.0,"output":9.0,"cache_read":0.0,"cache_write":0.0}}]"#;
    let out = scan(&mut priced_scanner(&db, cards));
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(usage.class, AggregationClass::Derived);
    assert_eq!(usage.categories.input, Some(2_000_000), "totals are kept");
    assert!(
        usage.models.is_empty(),
        "not attributed to the current model"
    );
    assert_eq!(usage.cost.usd, None);
    assert!(!usage.coverage.complete);

    // Messages that agree with the session model keep the attribution.
    let (_d, db, conn) = fixture();
    let mut s = Session::new("s", 1, 2);
    s.model = Some(&model);
    insert_session(&conn, &s);
    let mut data = assistant(Some("stop"), tokens(1_000_000, 1_000_000, 0, 0, 0), None);
    data["modelID"] = json!("one");
    data["providerID"] = json!("acme");
    msg(&conn, "s", "a1", "assistant", 3, data);
    let out = scan(&mut priced_scanner(&db, cards));
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(usage.models.len(), 1);
    assert_eq!(usage.cost.usd, Some(3.0));
}

#[test]
fn session_totals_without_message_evidence_and_nested_v2_models_are_handled_by_shape() {
    let cards = r#"[{"billing_provider":"acme","model":"one","rates":{"input":1.0,"output":2.0,"cache_read":0.0,"cache_write":0.0,"reasoning":0.0}},
                    {"billing_provider":"acme","model":"two","rates":{"input":9.0,"output":9.0,"cache_read":0.0,"cache_write":0.0,"reasoning":0.0}}]"#;
    let model = model_json("one", "acme", "");

    // Session totals plus a reported cost skip message derivation entirely; the
    // messages may span models, so no complete per-model estimate exists.
    let (_d, db, conn) = fixture();
    let mut s = Session::new("s", 1, 2);
    s.model = Some(&model);
    s.tokens = Some([1_000_000, 1_000_000, 0, 0, 0]);
    s.cost = Some(0.0);
    insert_session(&conn, &s);
    for (id, at, m) in [("a1", 3, "one"), ("a2", 4, "two")] {
        let mut data = assistant(Some("stop"), tokens(1, 1, 0, 0, 0), None);
        data["modelID"] = json!(m);
        data["providerID"] = json!("acme");
        msg(&conn, "s", id, "assistant", at, data);
    }
    let out = scan(&mut priced_scanner(&db, cards));
    let usage = out["s"].usage.as_ref().expect("usage");
    assert_eq!(
        usage.reported_cost_usd,
        Some(0.0),
        "recorded money is preserved"
    );
    assert!(usage.models.is_empty());
    assert_eq!(usage.cost.usd, None);
    assert!(!usage.coverage.complete);

    // The v2 shape nests the model; a nested switch is detected, a nested match attributes.
    for (second, expect_models) in [("two", 0), ("one", 1)] {
        let (_d, db, conn) = fixture();
        let mut s = Session::new("s", 1, 2);
        s.model = Some(&model);
        insert_session(&conn, &s);
        for (id, at, m) in [("a1", 3, "one"), ("a2", 4, second)] {
            let mut data = assistant(Some("stop"), tokens(1_000_000, 1_000_000, 0, 0, 0), None);
            data["model"] = json!({"id": m, "providerID": "acme", "variant": ""});
            msg(&conn, "s", id, "assistant", at, data);
        }
        let out = scan(&mut priced_scanner(&db, cards));
        let usage = out["s"].usage.as_ref().expect("usage");
        assert_eq!(usage.models.len(), expect_models, "{second}");
        assert_eq!(usage.cost.usd.is_some(), expect_models == 1, "{second}");
    }

    // A flat `id` or a nested `modelID` is not a documented shape for its dispatch.
    let (_d, db, conn) = fixture();
    let mut s = Session::new("s", 1, 2);
    s.model = Some(&model);
    insert_session(&conn, &s);
    let mut data = assistant(Some("stop"), tokens(1_000_000, 1_000_000, 0, 0, 0), None);
    data["id"] = json!("one");
    msg(&conn, "s", "a1", "assistant", 3, data);
    let out = scan(&mut priced_scanner(&db, cards));
    assert!(out["s"].usage.as_ref().unwrap().models.is_empty());
}

#[test]
fn only_the_recorded_creation_time_is_a_native_metric() {
    let (_d, db, conn) = fixture();
    let mut s = Session::new("started", 10, 20);
    s.tokens = Some([10, 5, 2, 3, 4]);
    s.cost = Some(0.5);
    insert_session(&conn, &s);
    let out = scan(&mut scanner_for(&db));
    let m = &out["started"].metrics;
    assert_eq!(m.timing.started_at, Some(10));
    assert_eq!(
        m.timing.ended_at, None,
        "a turn completion is not a session end"
    );
    assert_eq!(m.timing.active_ms, None);
    assert!(!m.timing.active_complete);
    assert_eq!(
        m.speed,
        agentscope::domain::SpeedSummary::default(),
        "aggregate session totals carry no per-request duration"
    );
    assert_eq!((m.context.used, m.context.capacity), (None, None));
    assert_eq!(m.compactions.observed, 0);
    assert!(!m.compactions.history_complete);
    assert_eq!(m.compactions.known_cost_usd, None);
}

#[test]
fn parent_child_nested_and_fork_direct_usage_stays_separate() {
    let (_d, db, conn) = fixture();
    let go = model_json("gpt-5", "opencode-go", "high");
    let zen = model_json("gpt-5", "opencode-zen", "high");
    let other = model_json("gpt-5", "acme", "high");

    let mut parent = Session::new("ses_parent", 100, 400);
    parent.model = Some(&go);
    parent.tokens = Some([1_000, 400, 0, 0, 0]);
    parent.cost = Some(0.25);
    insert_session(&conn, &parent);

    let mut child = Session::new("ses_child", 150, 500);
    child.parent_id = Some("ses_parent");
    child.model = Some(&zen);
    child.tokens = Some([200, 80, 0, 0, 0]);
    child.cost = Some(0.05);
    insert_session(&conn, &child);

    let mut nested = Session::new("ses_nested", 200, 600);
    nested.parent_id = Some("ses_child");
    nested.model = Some(&other);
    nested.tokens = Some([40, 10, 0, 0, 0]);
    nested.cost = Some(0.01);
    insert_session(&conn, &nested);

    // A fork names its source but not a parent: it is a root with reset counters.
    let mut fork = Session::new("ses_fork", 300, 700);
    fork.fork_id = Some("ses_parent");
    fork.model = Some(&go);
    fork.tokens = Some([30, 5, 0, 0, 0]);
    fork.cost = Some(0.002);
    insert_session(&conn, &fork);
    // Copied history: message-level sums would be far larger than the reset counters.
    for (s, id, at) in [
        ("ses_fork", "f1", 310),
        ("ses_fork", "f2", 320),
        ("ses_parent", "p1", 110),
    ] {
        msg(
            &conn,
            s,
            id,
            "assistant",
            at,
            assistant(Some("stop"), tokens(5_000, 5_000, 0, 0, 0), Some(9.0)),
        );
    }

    let out = scan(&mut scanner_for(&db));
    let parent = &out["ses_parent"];
    let child = &out["ses_child"];
    let nested = &out["ses_nested"];
    let fork = &out["ses_fork"];

    assert_eq!(parent.usage.as_ref().unwrap().total, Some(1_400));
    assert_eq!(child.usage.as_ref().unwrap().total, Some(280));
    assert_eq!(nested.usage.as_ref().unwrap().total, Some(50));
    assert_eq!(
        fork.usage.as_ref().unwrap().total,
        Some(35),
        "the fork's own reset counters, not copied history"
    );
    assert_eq!(parent.usage.as_ref().unwrap().cost.usd, Some(0.25));
    assert_eq!(child.usage.as_ref().unwrap().cost.usd, Some(0.05));
    assert_eq!(nested.usage.as_ref().unwrap().cost.usd, Some(0.01));
    assert_eq!(fork.usage.as_ref().unwrap().cost.usd, Some(0.002));

    // Direct usage is not rolled up or added across the family.
    let family: f64 = [parent, child, nested, fork]
        .iter()
        .filter_map(|s| s.usage.as_ref().and_then(|u| u.cost.usd))
        .sum();
    assert!((family - 0.312).abs() < 1e-12);

    // Lineage is explicit: parent/child/nested are separate links, a fork is a root.
    assert!(parent.meta.lineage.parent.is_none());
    assert_eq!(
        child.meta.lineage.parent,
        Some(agentscope::domain::SessionLink::Resolved(
            parent.meta.key.clone()
        ))
    );
    assert_eq!(
        nested.meta.lineage.parent,
        Some(agentscope::domain::SessionLink::Resolved(
            child.meta.key.clone()
        ))
    );
    assert!(fork.meta.lineage.parent.is_none());
    assert_eq!(
        fork.meta.lineage.fork,
        Some(agentscope::domain::SessionLink::Resolved(
            parent.meta.key.clone()
        ))
    );
}

#[test]
fn equal_model_ids_through_different_providers_keep_their_own_route_and_price() {
    let (_d, db, conn) = fixture();
    let cards = r#"[
        {"billing_provider":"opencode-go","model":"shared","as_of":"2026-01-01",
         "rates":{"input":1.0,"output":2.0,"cache_read":0.0,"cache_write":0.0,"reasoning":0.0}},
        {"billing_provider":"opencode-zen","model":"shared","as_of":"2026-01-01",
         "rates":{"input":10.0,"output":20.0,"cache_read":0.0,"cache_write":0.0,"reasoning":0.0}},
        {"billing_provider":"acme","model":"shared","as_of":"2026-01-01",
         "rates":{"input":100.0,"output":200.0,"cache_read":0.0,"cache_write":0.0,"reasoning":0.0}}
    ]"#;
    for (id, provider, at) in [
        ("ses_go", "opencode-go", 10),
        ("ses_zen", "opencode-zen", 20),
        ("ses_acme", "acme", 30),
    ] {
        let model = model_json("shared", provider, "");
        let mut s = Session::new(id, at, at + 1);
        s.model = Some(&model);
        insert_session(&conn, &s);
        let mut data = assistant(Some("stop"), tokens(1_000_000, 1_000_000, 0, 0, 0), None);
        data["model"] = json!({"id": "shared", "providerID": provider, "variant": ""});
        msg(&conn, id, &format!("{id}-a"), "assistant", at + 2, data);
    }

    let out = scan(&mut priced_scanner(&db, cards));
    let cost = |id: &str| out[id].usage.as_ref().unwrap().cost.clone();
    assert_eq!(cost("ses_go").usd, Some(3.0));
    assert_eq!(cost("ses_zen").usd, Some(30.0));
    assert_eq!(cost("ses_acme").usd, Some(300.0));
    for id in ["ses_go", "ses_zen", "ses_acme"] {
        let usage = out[id].usage.as_ref().unwrap();
        assert_eq!(usage.models.len(), 1, "{id}");
        assert_eq!(usage.cost.class, AggregationClass::Estimated, "{id}");
    }
    assert_eq!(
        out["ses_go"].usage.as_ref().unwrap().models[0]
            .model
            .provider
            .as_deref(),
        Some("opencode-go")
    );
    assert_eq!(
        out["ses_zen"].usage.as_ref().unwrap().models[0]
            .model
            .provider
            .as_deref(),
        Some("opencode-zen")
    );

    // No recorded route: Unknown, never filled in with a vendor default.
    let (_d2, db2, conn2) = fixture();
    let model = model_json("shared", "", "");
    let mut unrecorded = Session::new("ses_none", 1, 2);
    unrecorded.model = Some(&model);
    insert_session(&conn2, &unrecorded);
    let mut data = assistant(Some("stop"), tokens(1_000_000, 1_000_000, 0, 0, 0), None);
    data["model"] = json!({"id": "shared", "variant": ""});
    msg(&conn2, "ses_none", "a1", "assistant", 3, data);
    let out = scan(&mut priced_scanner(&db2, cards));
    let usage = out["ses_none"].usage.as_ref().unwrap();
    assert_eq!(usage.cost.usd, None);
    assert_eq!(usage.cost.class, AggregationClass::Unknown);
    assert!(
        usage
            .cost
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("billing route"),
        "{:?}",
        usage.cost
    );
}
