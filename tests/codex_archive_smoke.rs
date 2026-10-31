//! B3 smoke: a synthetic Codex family and an archived rollout through the real
//! reader → worker → App path. Direct child usage, family identity, the archived
//! row's selected identity across compression, and reopened transcript events are
//! observed; no provider write happens.

mod support;

use std::fs;
use std::time::{Duration, Instant};

use agentscope::app::App;
use agentscope::domain::{RefreshBudget, Thresholds};
use agentscope::providers::ProviderReader;
use agentscope::providers::codex::CodexScanner;
use agentscope::ui::{RosterContext, apply_worker_event, flush_requests};
use agentscope::worker::{WorkerHandle, WorkerRequest};
use support::NOW;
use support::codex as cx;

const BIG: RefreshBudget = RefreshBudget {
    max_records: 100_000,
    max_bytes: 100_000_000,
    max_millis: 60_000,
};

fn write(path: &std::path::Path, body: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
}

fn rollout(lines: &[String]) -> String {
    let mut body = lines.join("\n");
    body.push('\n');
    body
}

fn found_parent(app: &App) -> agentscope::domain::SessionKey {
    app.visible_rows()
        .iter()
        .find(|row| row.session.meta.key.native_id == "parent")
        .map(|row| row.session.meta.key.clone())
        .expect("parent is a row")
}

fn pump(
    app: &mut App,
    ctx: &mut RosterContext,
    worker: &WorkerHandle,
    pred: impl Fn(&App) -> bool,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        flush_requests(app, worker);
        while let Ok(event) = worker.rx.try_recv() {
            apply_worker_event(app, ctx, event);
        }
        if pred(app) {
            return;
        }
        if Instant::now() >= deadline {
            let ids: Vec<String> = app
                .visible_rows()
                .iter()
                .map(|row| row.session.meta.key.native_id.clone())
                .collect();
            let _ = worker.tx.send(WorkerRequest::Shutdown);
            panic!("timed out: {what}; rows={ids:?}");
        }
        if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(20)) {
            apply_worker_event(app, ctx, event);
        }
    }
}

#[test]
fn codex_family_and_archived_rollout_through_the_worker() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex");
    let sessions = home.join("sessions");
    let archived = home.join("archived_sessions");
    let date = sessions.join("2026").join("10").join("29");
    let parent = date.join("rollout-parent.jsonl");
    let child = date.join("rollout-child.jsonl");
    let old = archived.join("rollout-old.jsonl.zst");

    write(
        &parent,
        &rollout(&[
            cx::session_meta("2026-10-29T10:00:00Z", "parent", "/work"),
            cx::user("2026-10-29T10:00:01Z", "parent prompt"),
            cx::usage_record_thread(
                "2026-10-29T10:00:02Z",
                "parent",
                cx::usage(600, 0, 400, 0, 1000),
            ),
        ]),
    );
    write(
        &child,
        &rollout(&[
            cx::child_session_meta(
                "2026-10-29T10:00:03Z",
                "kid",
                "parent",
                "parent",
                Some(2),
                "0.159.2",
            ),
            cx::usage_record_thread(
                "2026-10-29T10:00:02Z",
                "parent",
                cx::usage(600, 0, 400, 0, 1000),
            ),
            cx::turn_context("2026-10-29T10:00:04Z", "gpt-5.5", "/work"),
            cx::usage_record_thread(
                "2026-10-29T10:00:05Z",
                "kid",
                cx::usage(700, 0, 450, 0, 1150),
            ),
        ]),
    );
    // An older rollout already archived and compressed by the native app.
    let old_plain = rollout(&[
        cx::session_meta("2026-10-28T10:00:00Z", "old", "/work"),
        cx::user("2026-10-28T10:00:01Z", "older prompt"),
        cx::assistant("2026-10-28T10:00:02Z", "older answer"),
    ]);
    fs::create_dir_all(&archived).unwrap();
    fs::write(
        &old,
        zstd::stream::encode_all(old_plain.as_bytes(), 3).unwrap(),
    )
    .unwrap();

    let mut config = support::config_for_store(&sessions, &archived);
    config.refresh_ms = 25;
    let worker = WorkerHandle::spawn(
        vec![Box::new(CodexScanner::new(&config).unwrap())],
        config.clone(),
    );
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    pump(
        &mut app,
        &mut ctx,
        &worker,
        |a| a.total_sessions() == 2,
        "two root rows (the child is a family member)",
    );
    let old_key = app
        .visible_rows()
        .iter()
        .find(|row| row.session.meta.key.native_id == "old")
        .map(|row| row.session.meta.key.clone())
        .expect("archived rollout is a row");
    let parent_key = found_parent(&app);

    // Direct facts: the parent keeps its own usage; the child owns only its delta.
    let mut reader = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut reader, NOW, BIG);
    for meta in &found.sessions {
        let summary = support::world::hydrate_complete(&mut reader, &meta.key, NOW);
        match meta.key.native_id.as_str() {
            "parent" => assert_eq!(summary.usage.as_ref().and_then(|u| u.total), Some(1000)),
            "kid" => assert_eq!(summary.usage.as_ref().and_then(|u| u.total), Some(150)),
            "old" => assert_eq!(summary.usage.as_ref().and_then(|u| u.total), None),
            other => panic!("unexpected session {other}"),
        }
    }
    let kid = found
        .sessions
        .iter()
        .find(|m| m.key.native_id == "kid")
        .unwrap();
    assert_eq!(
        kid.lineage.parent,
        Some(agentscope::domain::SessionLink::Resolved(
            parent_key.clone()
        ))
    );

    // Open the archived row's transcript through the worker and real UI state.
    while app.selected_key().as_ref() != Some(&old_key) {
        assert!(app.selected_index().is_some());
        app.move_selection(1);
    }
    assert!(app.open_inspector());
    pump(
        &mut app,
        &mut ctx,
        &worker,
        |a| a.transcript().is_some_and(|t| !t.events().is_empty()),
        "archived transcript events",
    );
    let text: Vec<String> = app
        .transcript()
        .unwrap()
        .events()
        .iter()
        .filter_map(|e| e.text.as_ref().map(|t| t.preview.clone()))
        .collect();
    assert!(
        text.iter().any(|t| t.contains("older prompt")),
        "the archived rollout reopens its events: {text:?}"
    );

    app.close_inspector();
    for _ in 0..app.visible_rows().len() {
        if app.selected_key().as_ref() == Some(&parent_key) {
            break;
        }
        app.move_selection(-1);
    }
    assert_eq!(app.selected_key().as_ref(), Some(&parent_key));

    // A live → archived move, then native compression, keeps the confirmed key.
    let moved = archived.join("rollout-parent.jsonl");
    fs::create_dir_all(&archived).unwrap();
    fs::rename(date.join("rollout-parent.jsonl"), &moved).unwrap();
    let bytes = fs::read(&moved).unwrap();
    fs::write(
        archived.join("rollout-parent.jsonl.zst"),
        zstd::stream::encode_all(&bytes[..], 3).unwrap(),
    )
    .unwrap();
    fs::remove_file(&moved).unwrap();
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    pump(
        &mut app,
        &mut ctx,
        &worker,
        |a| {
            a.total_sessions() == 2
                && a.session(&parent_key)
                    .is_some_and(|row| row.session.meta.archived)
        },
        "the archived+compressed parent keeps its row and selection",
    );
    assert_eq!(app.total_sessions(), 2, "no duplicate live/archive row");
    assert!(
        app.session(&parent_key).unwrap().session.meta.archived,
        "the current physical source is the archived store"
    );
    assert_eq!(
        app.selected_key().as_ref(),
        Some(&parent_key),
        "the selected identity survives the move and compression"
    );
    // The compressed archived source is readable (the same reader path the worker
    // uses); the App transition itself is asserted above by key and archived state.
    let mut fresh = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut fresh, NOW, BIG).sessions;
    let meta = found
        .iter()
        .find(|m| m.key.native_id == "parent")
        .expect("the archived parent is discovered");
    assert!(meta.archived);
    support::world::hydrate_complete(&mut fresh, &meta.key, NOW);
    let page = loop {
        match fresh
            .transcript_page(
                support::world::new_job(),
                &meta.key,
                None,
                agentscope::domain::PageDirection::Backward,
                agentscope::domain::PageRequest {
                    max_events: 50,
                    max_bytes: 1_000_000,
                },
                BIG,
            )
            .unwrap()
        {
            agentscope::providers::ReadProgress::Pending(_) => continue,
            agentscope::providers::ReadProgress::Ready(page) => break page,
        }
    };
    assert!(
        page.events.iter().any(|e| e
            .text
            .as_ref()
            .is_some_and(|t| t.preview.contains("parent prompt"))),
        "the moved session's events reopen from its compressed archived source"
    );

    let _ = worker.tx.send(WorkerRequest::Shutdown);
}
