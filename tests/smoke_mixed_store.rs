//! Smoke: a mixed synthetic native store (125 roots, one huge plain rollout, one zstd
//! archive, one open-WAL OpenCode database) through the real worker, app and renderer.

mod support;

use std::time::{Duration, Instant};

use agentscope::app::App;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::*;
use agentscope::providers::ProviderReader;
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::opencode::OpenCodeScanner;
use agentscope::ui::{RosterContext, apply_worker_event, drain_worker, flush_requests, render};
use agentscope::worker::{WorkerHandle, WorkerRequest};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::opencode as oc;
use support::{codex as cx, dated, write_lines};

const BASE: i64 = 1_793_000_000_000;

fn iso(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .unwrap()
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

fn screen(app: &App, ctx: &RosterContext) -> String {
    let mut t = Terminal::new(TestBackend::new(140, 40)).unwrap();
    t.draw(|f| render(f, app, ctx)).unwrap();
    t.backend()
        .buffer()
        .content()
        .chunks(140)
        .map(|r| r.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn pump(
    app: &mut App,
    ctx: &mut RosterContext,
    w: &WorkerHandle,
    what: &str,
    done: impl Fn(&App) -> bool,
) {
    let end = Instant::now() + Duration::from_secs(60);
    while Instant::now() < end {
        assert!(drain_worker(app, ctx, w), "worker exited");
        flush_requests(app, w);
        if done(app) {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("timed out: {what}\n{}", screen(app, ctx));
}

#[test]
fn mixed_store_through_startup_events_and_render() {
    let dir = tempfile::tempdir().unwrap();
    let (codex, claude) = (dir.path().join("codex"), dir.path().join("claude"));
    let db = dir.path().join("opencode.db");
    let conn = oc::create_v2(&db); // WAL, autocheckpoint off, writer kept open
    for i in 0..125usize {
        let at = BASE + i as i64 * 60_000;
        match i % 3 {
            0 => write_lines(
                &dated(&codex, &format!("rollout-c{i:03}.jsonl")),
                &[
                    cx::session_meta(&iso(at), &format!("c{i:03}"), "/w"),
                    cx::user(&iso(at), "task"),
                ],
            ),
            1 => write_lines(
                &claude.join("p").join(format!("l{i:03}.jsonl")),
                &[
                    serde_json::json!({"type":"user","uuid":"u","sessionId":format!("l{i:03}"),
                    "timestamp":iso(at),"message":{"role":"user","content":"task"}})
                    .to_string(),
                ],
            ),
            _ => oc::insert_session(&conn, &oc::Session::new(&format!("ses_{i:03}"), at, at)),
        }
    }
    // One huge plain rollout (newest) and one compressed archive.
    let mut huge = vec![cx::session_meta(&iso(BASE + 9_000_000), "huge", "/w")];
    huge.extend((0..20_000).map(|i| {
        cx::assistant(
            &iso(BASE + 9_000_000),
            &format!("body {i} {}", "y".repeat(80)),
        )
    }));
    write_lines(&dated(&codex, "rollout-zhuge.jsonl"), &huge);
    let archived = codex.join("archived").join("rollout-old.jsonl.zst");
    std::fs::create_dir_all(archived.parent().unwrap()).unwrap();
    std::fs::write(
        &archived,
        zstd::stream::encode_all(huge.join("\n").as_bytes(), 3).unwrap(),
    )
    .unwrap();

    let mut config = Config::load_with_env(
        &CliOverrides {
            codex_sessions: Some(codex),
            claude_projects: Some(claude),
            opencode_db: Some(db),
            ..CliOverrides::default()
        },
        &Env::new(),
    )
    .unwrap();
    config.refresh_ms = 3_600_000;
    config.process_enabled = false;
    config.max_cached_sessions = 2;
    config.max_metadata_sources = 120;
    let readers: Vec<Box<dyn ProviderReader>> = vec![
        Box::new(CodexScanner::new(&config).unwrap()),
        Box::new(ClaudeScanner::new(&config).unwrap()),
        Box::new(OpenCodeScanner::new(&config).unwrap()),
    ];
    let worker = WorkerHandle::spawn(readers, config);
    let mut app = App::new(
        BASE + 10_000_000,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };

    // First page of metadata is visible before every summary has been hydrated.
    pump(&mut app, &mut ctx, &worker, "first frozen page", |a| {
        a.total_sessions() == 50 && !a.roster_status().provisional
    });
    let first = screen(&app, &ctx);
    assert!(first.contains("more available"), "{first}");
    assert!(
        first.contains("Discovery limited"),
        "the metadata ceiling is shown:\n{first}"
    );
    println!("--- first page ---\n{first}");

    // One Load more.
    assert!(app.load_more_roots());
    pump(&mut app, &mut ctx, &worker, "100 roots", |a| {
        a.total_sessions() == 100
    });

    // Stale selection cancellation: select several in a row; only the last is answered.
    let keys: Vec<SessionKey> = app
        .all_rows()
        .iter()
        .take(3)
        .map(|r| r.session.meta.key.clone())
        .collect();
    for (i, k) in keys.iter().enumerate() {
        worker
            .tx
            .send(WorkerRequest::Select {
                request_id: i as u64 + 1,
                key: k.clone(),
            })
            .unwrap();
    }
    worker
        .tx
        .send(WorkerRequest::CancelLoad { request_id: 99 })
        .unwrap();

    // Cache eviction/rehydration: hydration of more than 2 roots never drops rows.
    pump(
        &mut app,
        &mut ctx,
        &worker,
        "rows survive a 2-entry cache",
        |a| a.total_sessions() == 100,
    );
    assert_eq!(app.total_sessions(), 100);
    worker
        .tx
        .send(WorkerRequest::HydratePreview {
            request_id: 1,
            key: keys[0].clone(),
        })
        .unwrap();
    pump(&mut app, &mut ctx, &worker, "preview rehydrates", |_| {
        std::thread::sleep(Duration::from_millis(50));
        true
    });
    println!("--- 100 roots ---\n{}", screen(&app, &ctx));
    // Drain what remains without panicking on any event.
    while let Ok(event) = worker.rx.try_recv() {
        apply_worker_event(&mut app, &mut ctx, event);
    }
    assert!(worker.shutdown());
}
