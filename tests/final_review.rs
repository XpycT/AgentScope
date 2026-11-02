//! Final whole-branch review regressions driven through real adapters, the real
//! worker, and the app: provider retry, unread arrivals across background
//! reloads, Claude pending-row refresh and lifecycle.

mod support;

use std::io::Write;
use std::path::Path;
use std::time::{Duration, Instant};

use agentscope::app::App;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{StatusLabel, Thresholds, Timestamp, ToolStatus};
use agentscope::providers::ProviderReader;
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::opencode::{OpenCodeScanner, RetryingOpenCode};
use agentscope::ui::{RosterContext, drain_worker, flush_requests, handle_key};
use agentscope::worker::{WorkerHandle, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rusqlite::params;
use serde_json::{Value, json};
use support::opencode::*;

const NOW: Timestamp = 1_800_000_000_000;

fn new_app() -> App {
    App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    )
}

fn ctx() -> RosterContext {
    RosterContext {
        no_color: true,
        ..RosterContext::default()
    }
}

fn press(app: &mut App, code: KeyCode) {
    handle_key(app, KeyEvent::new(code, KeyModifiers::NONE));
}

fn quiet_config(cli: CliOverrides) -> Config {
    let mut config = Config::load_with_env(&cli, &Env::new()).unwrap();
    config.refresh_ms = 3_600_000;
    config.process_ms = 3_600_000;
    config.process_enabled = false;
    config
}

fn pump_until(
    app: &mut App,
    ctx: &mut RosterContext,
    worker: &WorkerHandle,
    done: impl Fn(&App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        assert!(drain_worker(app, ctx, worker), "worker exited");
        flush_requests(app, worker);
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("condition not reached");
}

fn pump_for(app: &mut App, ctx: &mut RosterContext, worker: &WorkerHandle, millis: u64) {
    let end = Instant::now() + Duration::from_millis(millis);
    while Instant::now() < end {
        assert!(drain_worker(app, ctx, worker), "worker exited");
        flush_requests(app, worker);
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn user_message(conn: &rusqlite::Connection, id: &str, at: i64, text: &str) {
    conn.execute(
        "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
         VALUES (?1, 'ses_1', 'user', 0, ?2, ?2, ?3)",
        params![id, at, json!({"text": text}).to_string()],
    )
    .unwrap();
    conn.execute(
        "UPDATE session_v2 SET time_updated = ?1 WHERE id = 'ses_1'",
        params![at],
    )
    .unwrap();
}

#[test]
fn opencode_open_failure_at_startup_is_retried_by_discovery() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("opencode.db");
    let config = config_for_db(&db);
    // The database does not exist yet: the first open fails but the reader stays.
    let (mut reader, error) = RetryingOpenCode::open(&config);
    assert!(error.is_some(), "first open fails");
    let budget = agentscope::domain::RefreshBudget {
        max_records: 1000,
        max_bytes: 1_000_000,
        max_millis: 60_000,
    };
    assert!(reader.discover(NOW, budget).is_err(), "still missing");

    let conn = create_v2(&db);
    insert_session(&conn, &Session::new("ses_1", 1_000, 1_000));
    user_message(&conn, "m1", 1_100, "hello");

    // The same reader instance recovers once the database exists.
    let delta = support::world::rescan(&mut reader, NOW, budget);
    assert_eq!(delta.sessions.len(), 1);
}

#[test]
fn opencode_open_failure_then_late_database_appears_through_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("late.db");
    let config = config_for_db(&db);
    let (reader, error) = RetryingOpenCode::open(&config);
    assert!(error.is_some());
    let worker = WorkerHandle::spawn(
        vec![Box::new(reader)],
        quiet_config(CliOverrides {
            opencode_db: Some(db.clone()),
            ..CliOverrides::default()
        }),
    );
    let mut app = new_app();
    let mut ctx = ctx();
    pump_for(&mut app, &mut ctx, &worker, 200);
    assert_eq!(app.total_sessions(), 0);

    let conn = create_v2(&db);
    insert_session(&conn, &Session::new("ses_1", 1_000, 1_000));
    user_message(&conn, "m1", 1_100, "hello");
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    pump_until(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 1);
    assert!(worker.shutdown());
}

#[test]
fn opencode_arrivals_stay_unread_across_a_background_invalidation_reload() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("opencode.db");
    let conn = create_v2(&db);
    insert_session(&conn, &Session::new("ses_1", 1_000, 1_000));
    for i in 0..4 {
        user_message(&conn, &format!("m{i}"), 1_100 + i, &format!("message {i}"));
    }
    let scanner = OpenCodeScanner::new(&config_for_db(&db)).unwrap();
    let worker = WorkerHandle::spawn(
        vec![Box::new(scanner)],
        quiet_config(CliOverrides {
            opencode_db: Some(db.clone()),
            ..CliOverrides::default()
        }),
    );
    let mut app = new_app();
    let mut ctx = ctx();
    pump_until(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 1);
    press(&mut app, KeyCode::Enter);
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.transcript().is_some_and(|t| t.events().len() == 4)
    });
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Char('k'));
    assert_eq!(app.transcript().unwrap().selected(), 1);

    user_message(&conn, "m4", 2_000, "arrived one");
    user_message(&conn, "m5", 2_001, "arrived two");
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.new_event_count() == 2 && a.transcript().is_some_and(|t| t.loading().is_none())
    });
    // Let any background reload and recount finish; the arrivals stay unread.
    pump_for(&mut app, &mut ctx, &worker, 400);
    assert_eq!(app.new_event_count(), 2, "reload must not consume unread");
    assert_eq!(app.transcript().unwrap().selected(), 1, "no auto-scroll");
    assert!(worker.shutdown());
}

fn claude_config(root: &Path) -> Config {
    quiet_config(CliOverrides {
        claude_projects: Some(root.to_path_buf()),
        ..CliOverrides::default()
    })
}

fn rec(kind: &str, uuid: &str, ts: &str, stop: Option<&str>, content: Value) -> String {
    json!({"type":kind,"uuid":uuid,"sessionId":"sess","timestamp":ts,
        "message":{"id":uuid,"role":kind,"stop_reason":stop,"content":content}})
    .to_string()
}

fn append_line(path: &Path, lines: &[String]) {
    let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    for l in lines {
        writeln!(f, "{l}").unwrap();
    }
}

#[test]
fn claude_result_append_refreshes_the_visible_row_and_completes_the_status() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let path = root.join("p/sess.jsonl");
    support::write_lines(
        &path,
        &[
            rec("user", "u1", "2026-10-29T10:00:00Z", None, json!("run it")),
            rec(
                "assistant",
                "a1",
                "2026-10-29T10:00:01Z",
                Some("tool_use"),
                json!([{"type":"tool_use","id":"t1","name":"Bash","input":{}}]),
            ),
        ],
    );
    let config = claude_config(&root);
    let worker = WorkerHandle::spawn(vec![Box::new(ClaudeScanner::new(&config).unwrap())], config);
    let mut app = new_app();
    let mut ctx = ctx();
    pump_until(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 1);
    let key = app.selected_key().unwrap();
    assert_ne!(
        app.session(&key).unwrap().status.label,
        StatusLabel::Complete,
        "an unfinished tool_use turn is not complete"
    );
    press(&mut app, KeyCode::Enter);
    let pending = |a: &App| {
        a.transcript().is_some_and(|t| {
            t.events().iter().any(|e| {
                e.tool
                    .as_ref()
                    .is_some_and(|x| x.status == ToolStatus::Pending)
            })
        })
    };
    pump_until(&mut app, &mut ctx, &worker, pending);

    append_line(
        &path,
        &[
            rec(
                "user",
                "u2",
                "2026-10-29T10:00:02Z",
                None,
                json!([{"type":"tool_result","tool_use_id":"t1","content":"ok"}]),
            ),
            rec(
                "assistant",
                "a2",
                "2026-10-29T10:00:03Z",
                Some("end_turn"),
                json!([{"type":"text","text":"finished"}]),
            ),
        ],
    );
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.transcript().is_some_and(|t| {
            t.events().iter().any(|e| {
                e.tool
                    .as_ref()
                    .is_some_and(|x| x.status == ToolStatus::Completed)
            }) && !t.events().iter().any(|e| {
                e.tool
                    .as_ref()
                    .is_some_and(|x| x.status == ToolStatus::Pending)
            })
        })
    });
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.session(&key).unwrap().status.label == StatusLabel::Complete
    });
    assert!(worker.shutdown());
}

#[test]
fn real_codex_adapter_lineage_reaches_the_session_info_screen() {
    use agentscope::providers::codex::CodexScanner;
    use support::codex::*;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    support::write_lines(
        &support::dated(&root, "rollout-parent.jsonl"),
        &[session_meta("2026-10-29T10:00:00Z", "parent-id", "/p")],
    );
    support::write_lines(
        &support::dated(&root, "rollout-child.jsonl"),
        &[
            session_meta_with(
                "2026-10-29T10:01:00Z",
                json!({"id":"child-id","cwd":"/p","source":{"subagent":{"thread_spawn":{"parent_thread_id":"parent-id"}}}}),
            ),
            user("2026-10-29T10:01:01Z", "hello"),
        ],
    );
    support::write_lines(
        &support::dated(&root, "rollout-orphan.jsonl"),
        &[session_meta_with(
            "2026-10-29T10:02:00Z",
            json!({"id":"orphan-id","cwd":"/p","source":{"subagent":{"thread_spawn":{"parent_thread_id":"missing-parent"}}}}),
        )],
    );
    let config = support::config_for(&root);
    let mut scanner = CodexScanner::new(&config).unwrap();
    let budget = agentscope::domain::RefreshBudget {
        max_records: 1000,
        max_bytes: 1_000_000,
        max_millis: 60_000,
    };
    let mut sessions = Vec::new();
    loop {
        let d = support::world::rescan(&mut scanner, NOW, budget);
        sessions.extend(d.sessions);
        if d.scan.done {
            break;
        }
    }
    let mut app = new_app();
    app.apply_roster(sessions, vec![]);
    let ctx = ctx();
    let draw_session = |app: &App, ctx: &RosterContext| {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 50)).unwrap();
        terminal
            .draw(|f| agentscope::ui::render(f, app, ctx))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .chunks(120)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    for (child, expected) in [
        ("child-id", "parent      resolved — Codex parent-id"),
        ("orphan-id", "parent      unresolved — id missing-parent"),
    ] {
        let target = app
            .visible_rows()
            .iter()
            .find(|r| r.session.meta.key.native_id == child)
            .map(|r| r.session.meta.key.clone())
            .expect("child in roster");
        app.move_selection(isize::MIN);
        for _ in 0..4 {
            if app.selected_key().as_ref() == Some(&target) {
                break;
            }
            app.move_selection(1);
        }
        assert_eq!(app.selected_key(), Some(target));
        assert!(app.open_inspector());
        let requests = app.take_requests();
        let Some(WorkerRequest::Select { request_id, .. }) = requests
            .iter()
            .find(|r| matches!(r, WorkerRequest::Select { .. }))
        else {
            panic!("expected Select");
        };
        let page = agentscope::domain::TranscriptPage {
            events: vec![],
            diagnostics: vec![],
            next: None,
            has_more: false,
            truncated: false,
        };
        assert!(app.apply_transcript(*request_id, page));
        assert!(app.open_session_info());
        let screen = draw_session(&app, &ctx);
        assert!(screen.contains(expected), "{screen}");
        app.close_overlay();
        app.close_inspector();
    }
}

#[test]
fn real_claude_usage_with_server_tools_shows_categories_pricing_date_and_fee_note() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("projects");
    let record = json!({"type":"assistant","uuid":"a","sessionId":"s","timestamp":"2026-10-29T10:00:00Z",
        "message":{"id":"m","model":"claude-test","stop_reason":"end_turn","content":"hi",
        "usage":{"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":3,
        "cache_read_input_tokens":2,"server_tool_use":{"web_search_requests":2,"web_fetch_requests":0}}}});
    support::write_lines(&root.join("p/s.jsonl"), &[record.to_string()]);
    let mut config = claude_config(&root);
    config.pricing = support::facts::price_schedule(
        r#"[{"billing_provider":"anthropic","model":"claude-test","as_of":"2026-09-15",
            "rates":{"input":3.0,"output":5.0,"cache_read":1.0,"cache_write":4.0}}]"#,
    );
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    let budget = agentscope::domain::RefreshBudget {
        max_records: 1000,
        max_bytes: 1_000_000,
        max_millis: 60_000,
    };
    let sessions = support::world::rescan(&mut scanner, NOW, budget).sessions;
    let mut app = new_app();
    app.apply_roster(sessions, vec![]);
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("expected Select");
    };
    assert!(app.apply_transcript(
        *request_id,
        agentscope::domain::TranscriptPage {
            events: vec![],
            diagnostics: vec![],
            next: None,
            has_more: false,
            truncated: false,
        }
    ));
    assert!(app.open_session_info());
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|f| agentscope::ui::render(f, &app, &ctx()))
        .unwrap();
    let screen = terminal
        .backend()
        .buffer()
        .content()
        .chunks(120)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n");
    for needle in [
        "input              10",
        "cache read          2",
        "output              4",
        "prices as of 2026-09-15",
        "note: server-tool fees excluded",
        "web_search 2",
    ] {
        assert!(screen.contains(needle), "missing {needle:?}:\n{screen}");
    }
}
