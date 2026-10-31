//! Claude Desktop Code and local Cowork sources: allowlisted manifests, mixed
//! origins from record evidence, released sessions hidden by default, and a real
//! reader → worker → App smoke over a synthetic Desktop layout.

mod support;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use agentscope::app::{App, FilterState, Screen};
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{
    Harness, OriginSurface, ReleaseState, SessionKey, SessionSummary, SourceKey, Thresholds,
};
use agentscope::providers::claude::ClaudeScanner;
use agentscope::providers::codex::CodexScanner;
use agentscope::ui::{RosterContext, apply_worker_event, flush_requests, handle_key};
use agentscope::worker::{WorkerHandle, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use serde_json::json;
use support::NOW;
use support::claude as fx;
use support::codex as cx;
use support::desktop;
use support::world::BIG;

fn config(desktop: &desktop::Desktop) -> Config {
    let mut config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(desktop.projects.clone()),
            claude_desktop: Some(desktop.data.clone()),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    config.process_enabled = false;
    config
}

fn key(id: &str, path: &Path) -> SessionKey {
    SessionKey {
        harness: Harness::Claude,
        native_id: id.to_owned(),
        source: SourceKey::File(path.to_path_buf()),
    }
}

fn sessions(scanner: &mut ClaudeScanner) -> Vec<agentscope::domain::SessionMetadata> {
    support::world::discover_pass(scanner, NOW, BIG).sessions
}

fn transcript(dir: &Path, session: &str, records: &[serde_json::Value]) -> PathBuf {
    let path = dir.join("proj").join(format!("{session}.jsonl"));
    fx::write(&path, records);
    path
}

#[test]
fn claude_desktop_manifest_links_an_existing_transcript_once() {
    let dir = tempfile::tempdir().unwrap();
    let d = desktop::layout(dir.path());
    let path = transcript(
        &d.projects,
        "code-1",
        &[
            fx::user("code-1", "u0", "Fix the build"),
            fx::assistant("code-1", "u1", "m1", (10, 2)),
        ],
    );
    desktop::manifest(
        &d.manifest_dir,
        "local_code1.json",
        "code-1",
        json!({"entrypoint": "claude-desktop"}),
    );
    // A manifest whose transcript does not exist creates no session.
    desktop::manifest(
        &d.manifest_dir,
        "local_ghost.json",
        "gone-1",
        json!({"entrypoint": "claude-desktop"}),
    );
    let mut scanner = ClaudeScanner::new(&config(&d)).unwrap();
    let found = sessions(&mut scanner);
    assert_eq!(
        found.len(),
        1,
        "the manifest never becomes a second session"
    );
    let meta = &found[0];
    assert_eq!(meta.key, key("code-1", &path));
    assert_eq!(meta.released, ReleaseState::Unknown);
    let surfaces: Vec<OriginSurface> = meta.origins.iter().map(|o| o.surface).collect();
    assert!(surfaces.contains(&OriginSurface::DesktopCode));
    assert!(
        meta.origins
            .iter()
            .any(|o| o.raw.as_deref() == Some("claude-code-sessions"))
    );
    // The record's own entrypoint stays separate evidence.
    assert!(
        meta.origins
            .iter()
            .any(|o| o.raw.as_deref() == Some("claude-desktop"))
    );
}

#[test]
fn claude_desktop_mixed_entrypoints_are_labelled_from_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let d = desktop::layout(dir.path());
    transcript(
        &d.projects,
        "resumed",
        &[
            fx::entrypoint(fx::user("resumed", "u0", "start in the CLI"), "cli"),
            fx::entrypoint(
                fx::assistant("resumed", "u1", "m1", (10, 2)),
                "claude-desktop",
            ),
            fx::entrypoint(
                fx::assistant("resumed", "u2", "m2", (4, 1)),
                "mystery-surface",
            ),
        ],
    );
    desktop::manifest(
        &d.manifest_dir,
        "local_resumed.json",
        "resumed",
        json!({"entrypoint": "claude-desktop"}),
    );
    let mut scanner = ClaudeScanner::new(&config(&d)).unwrap();
    let found = sessions(&mut scanner);
    let summary = support::world::hydrate_complete(&mut scanner, &found[0].key, NOW);
    let surfaces: Vec<OriginSurface> = summary.meta.origins.iter().map(|o| o.surface).collect();
    assert!(
        surfaces.contains(&OriginSurface::Cli),
        "the CLI origin is kept"
    );
    assert!(surfaces.contains(&OriginSurface::DesktopCode));
    // A surface this reader has not confirmed stays Unknown with its raw text.
    let unknown = summary
        .meta
        .origins
        .iter()
        .find(|o| o.raw.as_deref() == Some("mystery-surface"))
        .expect("the unconfirmed entrypoint is kept as evidence");
    assert_eq!(unknown.surface, OriginSurface::Unknown);
}

#[test]
fn claude_desktop_cowork_root_is_discovered_once_with_layout_origin() {
    let dir = tempfile::tempdir().unwrap();
    let d = desktop::layout(dir.path());
    let path = d
        .cowork_projects
        .join("cowork-project")
        .join("cowork-1.jsonl");
    fx::write(
        &path,
        &[
            fx::user("cowork-1", "u0", "Summarize the report"),
            fx::assistant("cowork-1", "u1", "m1", (3, 1)),
        ],
    );
    let mut scanner = ClaudeScanner::new(&config(&d)).unwrap();
    let found = sessions(&mut scanner);
    assert_eq!(found.len(), 1, "one Cowork transcript, discovered once");
    assert_eq!(found[0].key, key("cowork-1", &path));
    assert!(
        found[0]
            .origins
            .iter()
            .any(|o| o.surface == OriginSurface::Cowork
                && o.raw.as_deref() == Some("local-agent-mode-sessions"))
    );
}

#[test]
fn claude_desktop_released_sessions_are_hidden_until_included() {
    let dir = tempfile::tempdir().unwrap();
    let d = desktop::layout(dir.path());
    transcript(&d.projects, "kept", &[fx::user("kept", "u0", "keep me")]);
    transcript(&d.projects, "gone", &[fx::user("gone", "u0", "remove me")]);
    desktop::manifest(
        &d.manifest_dir,
        "local_kept.json",
        "kept",
        json!({"entrypoint": "claude-desktop"}),
    );
    desktop::manifest(
        &d.manifest_dir,
        "local_gone.json",
        "gone",
        json!({"entrypoint": "claude-desktop", "released": true}),
    );
    let mut scanner = ClaudeScanner::new(&config(&d)).unwrap();
    let found = sessions(&mut scanner);
    let gone = found
        .iter()
        .find(|m| m.key.native_id == "gone")
        .expect("a released session is still discovered");
    assert_eq!(gone.released, ReleaseState::Released);
    let kept = found.iter().find(|m| m.key.native_id == "kept").unwrap();
    assert_eq!(kept.released, ReleaseState::Unknown);

    // Through the application: hidden by default, inspectable after the explicit
    // filter change. Nothing is deleted or rewritten.
    let config = config(&d);
    let worker = WorkerHandle::spawn(vec![Box::new(ClaudeScanner::new(&config).unwrap())], config);
    let mut app = app();
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    wait_for(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 2);
    assert_eq!(
        visible_ids(&app),
        ["kept"],
        "released stays behind the filter"
    );
    app.set_filter(FilterState {
        include_released: true,
        ..FilterState::default()
    });
    let mut ids = visible_ids(&app);
    ids.sort();
    assert_eq!(ids, ["gone", "kept"]);
    // Both are inspectable once shown; the released row selects like any other.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    let selected = app.selected_key().expect("a selected row");
    assert_eq!(selected.native_id, "gone");
    let _ = worker.tx.send(WorkerRequest::Shutdown);
}

#[test]
fn claude_desktop_unknown_release_markers_are_not_guessed() {
    let dir = tempfile::tempdir().unwrap();
    let d = desktop::layout(dir.path());
    for id in ["maybe", "object", "absent"] {
        transcript(&d.projects, id, &[fx::user(id, "u0", "work")]);
    }
    desktop::manifest(
        &d.manifest_dir,
        "local_maybe.json",
        "maybe",
        json!({"released": "maybe"}),
    );
    desktop::manifest(
        &d.manifest_dir,
        "local_object.json",
        "object",
        json!({"released": {"at": "2026-10-30"}}),
    );
    desktop::manifest(&d.manifest_dir, "local_absent.json", "absent", json!({}));
    let mut scanner = ClaudeScanner::new(&config(&d)).unwrap();
    let found = sessions(&mut scanner);
    assert_eq!(found.len(), 3);
    for meta in &found {
        assert_eq!(
            meta.released,
            ReleaseState::Unknown,
            "an unconfirmed marker is not a release state"
        );
    }
    // An explicit marker is honoured.
    let dir = tempfile::tempdir().unwrap();
    let d = desktop::layout(dir.path());
    transcript(&d.projects, "deleted", &[fx::user("deleted", "u0", "work")]);
    desktop::manifest(
        &d.manifest_dir,
        "local_deleted.json",
        "deleted",
        json!({"deletedAt": "2026-10-30T10:00:00Z"}),
    );
    let mut scanner = ClaudeScanner::new(&config(&d)).unwrap();
    assert_eq!(sessions(&mut scanner)[0].released, ReleaseState::Released);
}

#[test]
fn claude_desktop_smoke_family_origins_costs_and_release() {
    let dir = tempfile::tempdir().unwrap();
    let d = desktop::layout(dir.path());
    let main = transcript(
        &d.projects,
        "code-1",
        &[
            fx::entrypoint(fx::user("code-1", "u0", "Refactor"), "claude-desktop"),
            fx::entrypoint(
                fx::assistant("code-1", "u1", "m1", (100, 20)),
                "claude-desktop",
            ),
        ],
    );
    let child = d
        .projects
        .join("proj")
        .join("code-1")
        .join("subagents")
        .join("agent-kid.jsonl");
    fx::write(
        &child,
        &[fx::agent(
            fx::assistant("code-1", "k1", "mk", (7, 3)),
            "kid",
        )],
    );
    desktop::manifest(
        &d.manifest_dir,
        "local_code1.json",
        "code-1",
        json!({"entrypoint": "claude-desktop"}),
    );
    transcript(&d.projects, "old", &[fx::user("old", "u0", "old work")]);
    desktop::manifest(
        &d.manifest_dir,
        "local_old.json",
        "old",
        json!({"released": true}),
    );

    let config = config(&d);
    let worker = WorkerHandle::spawn(
        vec![Box::new(ClaudeScanner::new(&config).unwrap())],
        config.clone(),
    );
    let mut app = app();
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    let main_key = key("code-1", &main);
    let kid_key = key("kid", &child);
    wait_for(&mut app, &mut ctx, &worker, |a| {
        a.session(&main_key)
            .is_some_and(|row| row.hydrated && row.session.usage.is_some())
    });
    // The released row is hidden; the Code session is the only visible root.
    assert_eq!(visible_ids(&app), ["code-1"]);
    let summary = app.session(&main_key).unwrap().session.clone();
    let main_summary: SessionSummary = summary;
    assert_eq!(
        main_summary.usage.as_ref().and_then(|u| u.total),
        Some(120),
        "the parent reports its own usage only"
    );
    assert!(
        main_summary
            .meta
            .origins
            .iter()
            .any(|o| o.surface == OriginSurface::DesktopCode)
    );

    // Expand the family through the worker; the child appears with an exact parent.
    assert!(app.load_children(&main_key));
    flush_requests(&mut app, &worker);
    wait_for(&mut app, &mut ctx, &worker, |a| {
        a.children_of(&main_key)
            .is_some_and(|c| c.metadata.iter().any(|m| m.key == kid_key))
    });
    let kid = app
        .children_of(&main_key)
        .unwrap()
        .metadata
        .iter()
        .find(|m| m.key == kid_key)
        .cloned()
        .unwrap();
    assert_eq!(kid.lineage.agent_id.as_deref(), Some("kid"));
    assert_eq!(
        kid.lineage.parent,
        Some(agentscope::domain::SessionLink::Resolved(main_key.clone()))
    );
    assert!(
        kid.origins
            .iter()
            .all(|o| o.surface != OriginSurface::DesktopCode),
        "a child is not itself a Desktop Code manifest target"
    );
    // The child's own facts (direct usage; no inherited parent record).
    let mut reader = ClaudeScanner::new(&config).unwrap();
    let kid_meta = sessions(&mut reader)
        .into_iter()
        .find(|m| m.key == kid_key)
        .expect("the child is discovered");
    let kid_summary = support::world::hydrate_complete(&mut reader, &kid_meta.key, NOW);
    assert_eq!(kid_summary.usage.as_ref().and_then(|u| u.total), Some(10));

    // A released session appears only after the explicit filter change.
    app.set_filter(FilterState {
        include_released: true,
        ..FilterState::default()
    });
    let old_key = key("old", &d.projects.join("proj").join("old.jsonl"));
    wait_for(&mut app, &mut ctx, &worker, |a| {
        a.session(&old_key).is_some_and(|row| row.hydrated)
    });
    let released = app.session(&old_key).unwrap().session.clone();
    assert_eq!(released.meta.released, ReleaseState::Released);

    // Open the inspector on the main session: the real render path stays healthy.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('g'), KeyModifiers::NONE),
    );
    handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    flush_requests(&mut app, &worker);
    assert_eq!(app.screen, Screen::Inspector);
    handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    let _ = worker.tx.send(WorkerRequest::Shutdown);
}

// ---- harness -------------------------------------------------------------

fn app() -> App {
    App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    )
}

fn visible_ids(app: &App) -> Vec<String> {
    app.visible_rows()
        .iter()
        .map(|row| row.session.meta.key.native_id.clone())
        .collect()
}

fn wait_for(
    app: &mut App,
    ctx: &mut RosterContext,
    worker: &WorkerHandle,
    pred: impl Fn(&App) -> bool,
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
        assert!(Instant::now() < deadline, "the app never reached the state");
        if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(20)) {
            apply_worker_event(app, ctx, event);
        }
    }
}

#[test]
fn claude_desktop_release_and_deletion_markers_are_independent() {
    let dir = tempfile::tempdir().unwrap();
    let d = desktop::layout(dir.path());
    // A false deletion flag must not cancel a confirmed release.
    for id in ["released", "deleted", "released_false_deleted"] {
        transcript(&d.projects, id, &[fx::user(id, "u0", "work")]);
    }
    desktop::manifest(
        &d.manifest_dir,
        "local_released.json",
        "released",
        json!({"released": true, "deleted": false}),
    );
    desktop::manifest(
        &d.manifest_dir,
        "local_deleted.json",
        "deleted",
        json!({"deleted": true}),
    );
    desktop::manifest(
        &d.manifest_dir,
        "local_both.json",
        "released_false_deleted",
        json!({"released": false, "deletedAt": "2026-10-30T10:00:00Z"}),
    );
    let mut scanner = ClaudeScanner::new(&config(&d)).unwrap();
    let found = sessions(&mut scanner);
    assert_eq!(found.len(), 3);
    for meta in &found {
        assert_eq!(
            meta.released,
            ReleaseState::Released,
            "release evidence stands on its own: {:?}",
            meta.key.native_id
        );
    }
}

// ---- Codex Desktop/IDE origins -------------------------------------------

fn codex_session(
    root: &Path,
    id: &str,
    originator: Option<&str>,
    source: serde_json::Value,
) -> PathBuf {
    let mut payload = json!({
        "id": id,
        "cwd": "/work/demo",
        "cli_version": "0.159.2",
        "source": source,
    });
    if let Some(originator) = originator {
        payload["originator"] = json!(originator);
    }
    let path = support::dated(root, &format!("rollout-{id}.jsonl"));
    support::write_lines(
        &path,
        &[
            cx::session_meta_with("2026-10-29T10:00:00Z", payload),
            cx::user("2026-10-29T10:00:01Z", "go"),
        ],
    );
    path
}

#[test]
fn codex_origins_are_labelled_only_where_evidence_distinguishes_them() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("sessions");
    // The bundled terminal records the Desktop originator while still being CLI.
    codex_session(&root, "terminal", Some("Codex Desktop"), json!("cli"));
    // The Desktop app records `vscode` plus its own originator.
    codex_session(&root, "desktop", Some("Codex Desktop"), json!("vscode"));
    // The VS Code extension is distinguished by its own originator.
    codex_session(&root, "ide", Some("codex_vscode"), json!("vscode"));
    // `vscode` with no originator is ambiguous, not a guessed Desktop session.
    codex_session(&root, "ambiguous", None, json!("vscode"));
    // A plain native CLI session.
    codex_session(&root, "cli", Some("codex_cli_rs"), json!("cli"));

    let config = {
        let mut config = support::config_for(&root);
        config.process_enabled = false;
        config
    };
    let mut scanner = CodexScanner::new(&config).unwrap();
    let found = support::world::discover_pass(&mut scanner, NOW, support::world::BIG).sessions;
    assert_eq!(found.len(), 5);
    let surface = |id: &str| {
        let meta = found.iter().find(|m| m.key.native_id == id).unwrap();
        assert_eq!(meta.origins.len(), 1, "{id}: {:?}", meta.origins);
        meta.origins[0].surface
    };
    assert_eq!(
        surface("terminal"),
        OriginSurface::Cli,
        "a bundled terminal is not the Desktop app"
    );
    assert_eq!(surface("cli"), OriginSurface::Cli);
    assert_eq!(surface("desktop"), OriginSurface::DesktopCode);
    assert_eq!(surface("ide"), OriginSurface::Ide);
    assert_eq!(
        surface("ambiguous"),
        OriginSurface::Unknown,
        "an ambiguous bundled terminal originator is never guessed"
    );
    // The recorded raw evidence stays beside the surface, including for Unknown.
    let ambiguous = found
        .iter()
        .find(|m| m.key.native_id == "ambiguous")
        .unwrap();
    assert_eq!(ambiguous.origins[0].raw.as_deref(), Some("vscode"));
    let desktop = found.iter().find(|m| m.key.native_id == "desktop").unwrap();
    assert_eq!(desktop.origins[0].raw.as_deref(), Some("Codex Desktop"));
}

// ---- B7: Desktop stores inside the integrated native world -------------------------

#[test]
fn native_world_desktop_stores_release_state_and_closed_stores() {
    use support::world::{ROOT_COUNT, World, discover_pass, snapshot};

    let world = World::native();
    let g = world.grown();
    let before = snapshot(world.dir.path());
    // Every store outside the allowlist is poisoned and locked: an open would fail loudly.
    let locks: Vec<_> = g
        .desktop
        .forbidden
        .iter()
        .map(|path| support::Unreadable::new(path))
        .collect();

    let config = world.config();
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    let found = discover_pass(&mut scanner, NOW, BIG);
    let meta = |k: &SessionKey| {
        found
            .sessions
            .iter()
            .rev()
            .find(|m| &m.key == k)
            .unwrap_or_else(|| panic!("{} not discovered", k.native_id))
    };
    let surfaces = |k: &SessionKey| -> Vec<OriginSurface> {
        meta(k).origins.iter().map(|o| o.surface).collect()
    };
    // Shared stores: the Code session is one session reached through its manifest, the
    // Cowork root is read in place, and the released manifest only sets the marker.
    assert!(surfaces(&g.desktop_code).contains(&OriginSurface::DesktopCode));
    assert_eq!(meta(&g.desktop_code).released, ReleaseState::Unknown);
    assert_eq!(meta(&g.desktop_released).released, ReleaseState::Released);
    assert!(surfaces(&g.cowork).contains(&OriginSurface::Cowork));
    assert!(
        meta(&g.desktop_child)
            .origins
            .iter()
            .all(|o| o.surface != OriginSurface::DesktopCode),
        "a child is not itself a Desktop Code manifest target"
    );
    let once = |k: &SessionKey| found.sessions.iter().filter(|m| &m.key == k).count();
    for key in [&g.desktop_code, &g.desktop_released, &g.cowork] {
        assert!(once(key) >= 1);
    }
    for key in &g.desktop.forbidden {
        assert!(
            !found
                .warnings
                .iter()
                .any(|w| w.key.source.contains(&key.to_string_lossy().into_owned())),
            "a forbidden store produced a diagnostic: {}",
            key.display()
        );
    }

    // Through the application: released sessions hide behind the filter and come back
    // only after the explicit choice; nothing is deleted or rewritten.
    let mut config = config;
    config.refresh_ms = 3_600_000;
    let worker = WorkerHandle::spawn(world.readers(), config);
    let mut app = app();
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    let deadline = Instant::now() + Duration::from_secs(30);
    while app.total_sessions() != ROOT_COUNT {
        app.load_more_roots();
        flush_requests(&mut app, &worker);
        if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(20)) {
            apply_worker_event(&mut app, &mut ctx, event);
        }
        assert!(Instant::now() < deadline, "all roots never loaded");
    }
    let visible: Vec<SessionKey> = app
        .visible_rows()
        .iter()
        .map(|row| row.session.meta.key.clone())
        .collect();
    assert_eq!(
        visible.len(),
        ROOT_COUNT - 1,
        "only the released root hides"
    );
    assert!(!visible.contains(&g.desktop_released));
    assert!(visible.contains(&g.desktop_code));
    app.set_filter(FilterState {
        include_released: true,
        ..FilterState::default()
    });
    assert!(
        app.visible_rows()
            .iter()
            .any(|row| row.session.meta.key == g.desktop_released)
    );
    let _ = worker.tx.send(WorkerRequest::Shutdown);

    for lock in locks {
        assert!(lock.active, "the forbidden store stayed unreadable");
        lock.release();
    }
    assert_eq!(
        snapshot(world.dir.path()).files,
        before.files,
        "Desktop manifests, transcripts and stores are unchanged"
    );
}
