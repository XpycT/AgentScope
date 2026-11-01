//! AC-09 and the credential boundary: sentinel secrets never appear in Debug output,
//! warnings, errors, or the rendered terminal; control sequences are neutralized and
//! credential-shaped transcript text is masked by default.

mod support;

use std::fmt::Debug;
use std::fs;
use std::io::Write;
use std::time::Duration;

use agentscope::app::App;
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::{Event, Harness, PageDirection, Thresholds};
use agentscope::processes::{ProcessInfo, ProcessSnapshot};
use agentscope::providers::codex::CodexScanner;
use agentscope::providers::opencode::OpenCodeScanner;
use agentscope::providers::{ProviderError, ProviderReader};
use agentscope::ui::{RosterContext, apply_worker_event, handle_key, render};
use agentscope::worker::{WorkerEvent, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::codex as cx;
use support::world::*;
use support::{dated, write_lines};

const FORBIDDEN: [&str; 7] = [
    SENTINEL,
    INLINE_SECRET,
    "SECRET-ACCOUNT",
    "SECRET-CONTROL",
    "SECRET-CREDENTIAL",
    "SECRET-SHARE",
    // The control byte itself (not its harmless escaped text) must never survive.
    "\u{1b}",
];

fn assert_clean(label: &str, text: &str) {
    for needle in FORBIDDEN {
        assert!(
            !text.contains(needle),
            "{label} leaked {needle:?}: {}",
            text.chars().take(200).collect::<String>()
        );
    }
}

fn assert_clean_debug(label: &str, value: &impl Debug) {
    assert_clean(label, &format!("{value:?}"));
    assert_clean(label, &format!("{value:#?}"));
}

fn preview(e: &Event) -> String {
    let mut out = String::new();
    if let Some(t) = &e.text {
        out.push_str(&t.preview);
    }
    if let Some(tool) = &e.tool {
        for t in [&tool.input, &tool.output].into_iter().flatten() {
            out.push_str(&t.preview);
        }
    }
    out
}

/// Adds damaged Codex/Claude files whose broken records embed the sentinel.
fn add_damaged_sources(world: &World) {
    let path = dated(&world.codex_root, "rollout-damaged.jsonl");
    write_lines(
        &path,
        &[
            cx::session_meta("2026-10-29T22:00:00Z", "damaged", "/work/damaged"),
            format!("{{not json {SENTINEL}"),
            format!("{{\"type\":\"future_record\",\"secret\":\"{SENTINEL}\"}}"),
            cx::user(
                "2026-10-29T22:00:03Z",
                &format!("password={INLINE_SECRET} {}", "y".repeat(5000)),
            ),
            cx::user("2026-10-29T22:00:04Z", "after the damage"),
        ],
    );
    fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(format!("{{\"type\":\"resp{SENTINEL}").as_bytes())
        .unwrap();
    write_lines(
        &world.claude_root.join("proj").join("damaged.jsonl"),
        &[
            format!("{{broken {SENTINEL}"),
            claude_line(
                "assistant",
                "d1",
                "2026-10-29T22:10:00Z",
                serde_json::json!([{"type":"unknown_nested","secret":SENTINEL}]),
                None,
            ),
        ],
    );
}

#[test]
fn sentinel_secret_never_appears_in_debug_warnings_or_errors() {
    let world = World::new();
    add_damaged_sources(&world);
    let mut config = world.config();
    config.max_record_bytes = 2048;
    let mut readers: Vec<Box<dyn ProviderReader>> = vec![
        Box::new(CodexScanner::new(&config).unwrap()),
        Box::new(agentscope::providers::claude::ClaudeScanner::new(&config).unwrap()),
        Box::new(OpenCodeScanner::new(&config).unwrap()),
    ];
    let surface = exercise_everything(&mut readers);
    assert!(
        surface.sessions.len() >= 4,
        "healthy and damaged sessions coexist"
    );
    assert!(
        !surface.warnings.is_empty() || surface.sessions.iter().any(|s| !s.warnings.is_empty())
    );

    for s in &surface.sessions {
        assert_clean_debug("SessionSummary", s);
        for w in &s.warnings {
            assert_clean("session warning", &w.message);
            assert_clean_debug("session warning", w);
        }
    }
    for w in &surface.warnings {
        assert_clean("warning message", &w.message);
        assert_clean("warning source", &w.key.source);
        assert_clean_debug("warning", w);
    }
    for page in &surface.pages {
        assert_clean_debug("TranscriptPage", page);
        for e in &page.events {
            assert_clean("event preview", &preview(e));
        }
        for d in &page.diagnostics {
            assert_clean("diagnostic label", &d.label);
        }
    }
    // Content-bearing titles and previews are masked by default, not just in Debug.
    for s in &surface.sessions {
        assert_clean("title", &s.meta.title);
    }
    assert_clean_debug("Config", &config);
}

#[test]
fn provider_errors_never_echo_file_content() {
    let world = World::new();
    let bad = world.dir.path().join("opencode").join("junk.db");
    fs::write(&bad, format!("not a database {SENTINEL}")).unwrap();
    let cli = CliOverrides {
        opencode_db: Some(bad),
        ..CliOverrides::default()
    };
    let config = Config::load_with_env(&cli, &Env::new()).unwrap();
    let mut texts: Vec<String> = Vec::new();
    match OpenCodeScanner::new(&config) {
        Err(e) => texts.push(format!("{e} {e:?}")),
        Ok(mut s) => match s.discover(support::NOW, BIG) {
            Err(e) => texts.push(format!("{e} {e:?}")),
            Ok(d) => texts.extend(d.warnings.iter().map(|w| format!("{w:?} {}", w.message))),
        },
    }
    // Every ProviderError variant renders only its (caller-supplied, content-free) message.
    for e in [
        ProviderError::Io("x".into()),
        ProviderError::Parse("x".into()),
        ProviderError::Unsupported("x".into()),
        ProviderError::Unavailable("x".into()),
        ProviderError::Internal("x".into()),
    ] {
        texts.push(e.to_string());
    }
    assert!(!texts.is_empty());
    for t in texts {
        assert_clean("provider error", &t);
    }
}

#[test]
fn transcripts_mask_credential_shapes_and_neutralize_control_sequences() {
    let world = World::new();
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    let codex_events: Vec<_> = surface
        .pages
        .iter()
        .flat_map(|p| &p.events)
        .filter(|e| e.provenance.harness == Harness::Codex)
        .collect();
    assert!(!codex_events.is_empty());
    let joined: String = codex_events.iter().map(|e| preview(e)).collect();
    assert!(
        joined.contains("done"),
        "the surrounding text survives masking"
    );
    assert_clean("codex transcript", &joined);
    // Raw source access is the explicit opt-in and the only path that sees original bytes.
    let assistant = codex_events
        .iter()
        .find(|e| preview(e).starts_with("done"))
        .expect("assistant event");
    let rec = support::world::try_source(
        readers[0].as_mut(),
        &assistant.provenance.locator,
        support::world::BIG,
        support::world::STEPS,
    )
    .expect("source record");
    assert!(
        rec.raw.contains(INLINE_SECRET),
        "raw view is explicit opt-in"
    );
    assert_clean("source display form", &rec.display);
}

#[test]
fn rendered_screen_and_worker_events_never_contain_secrets() {
    let world = World::new();
    let worker = agentscope::worker::WorkerHandle::spawn(world.readers(), world.config());
    let mut app = App::new(
        support::NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    let mut screens = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut requested = false;
    while std::time::Instant::now() < deadline {
        if let Ok(ev) = worker.rx.recv_timeout(Duration::from_millis(50)) {
            assert_clean_debug("worker event", &ev);
            apply_worker_event(&mut app, &mut ctx, ev);
        }
        if app.total_sessions() == 3 && !requested {
            requested = true;
            // Codex rows sort by recency; find the Codex row and open it.
            for _ in 0..3 {
                if app
                    .selected_key()
                    .is_some_and(|k| k.harness == Harness::Codex)
                {
                    break;
                }
                handle_key(
                    &mut app,
                    KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
                );
            }
            handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
            for r in app.take_requests() {
                worker.tx.send(r).unwrap();
            }
        }
        if app.transcript().is_some_and(|t| t.loaded()) {
            break;
        }
    }
    assert!(app.transcript().is_some_and(|t| t.loaded()));
    for _ in 0..6 {
        handle_key(&mut app, KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &app, &ctx)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        screens.push(text);
    }
    for s in &screens {
        assert_clean("rendered screen", s);
    }
    assert!(screens.iter().any(|s| s.contains("Codex prompt")));
    let _ = worker.tx.send(WorkerRequest::Shutdown);
    let _ = WorkerEvent::RefreshFinished { generation: 0 };
}

#[test]
fn process_observations_never_expose_raw_arguments() {
    let info = ProcessInfo::new(
        4242,
        Some(support::NOW),
        "codex".into(),
        None,
        Some(vec![
            "codex".into(),
            "--api-key".into(),
            INLINE_SECRET.into(),
            format!("password={SENTINEL}"),
            "\u{1b}[31mred".into(),
        ]),
    );
    assert_clean_debug("ProcessInfo", &info);
    assert_clean("command hint", info.command_line.as_deref().unwrap_or(""));
    assert_clean_debug(
        "ProcessSnapshot",
        &ProcessSnapshot {
            sampled_at: support::NOW,
            processes: vec![info],
        },
    );
}

#[test]
fn page_direction_is_irrelevant_to_masking() {
    // Backward and forward reads share the same sanitizer.
    let world = World::new();
    let mut readers = world.readers();
    let mut surface = Surface::default();
    support::world::surface_all(&mut readers, &mut surface);
    let key = surface
        .sessions
        .iter()
        .find(|s| s.meta.key.harness == Harness::Codex)
        .unwrap()
        .meta
        .key
        .clone();
    for d in [PageDirection::Forward, PageDirection::Backward] {
        let page = support::world::try_transcript(
            readers[0].as_mut(),
            &key,
            None,
            d,
            PAGE,
            support::world::BIG,
            support::world::STEPS,
        )
        .unwrap();
        for e in &page.events {
            assert_clean("page preview", &preview(e));
        }
    }
}

#[test]
fn source_record_debug_is_content_free() {
    use agentscope::providers::SourceRecord;
    let record = SourceRecord {
        raw: format!("{{\"prompt\":\"{SENTINEL}\"}}"),
        display: format!("tool output {INLINE_SECRET}"),
        unavailable: Some("SECRET-CREDENTIAL".to_string()),
    };
    assert_clean_debug("source record", &record);
    let event = WorkerEvent::Source {
        request_id: 7,
        record,
    };
    assert_clean_debug("worker source event", &event);
    let text = format!("{event:?}");
    assert!(text.contains("raw_len"), "lengths still shown: {text}");
}

fn screen_text(app: &App, ctx: &RosterContext) -> String {
    screen_text_at(app, ctx, 140, 40)
}

fn screen_text_at(app: &App, ctx: &RosterContext, width: u16, height: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|f| render(f, app, ctx)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|c| c.symbol())
        .collect()
}

/// Titles, model identity, origins, effort and agent identifiers can all carry
/// hostile text. Debug prints presence/length only and rendered views are
/// sanitized and credential-masked.
#[test]
fn new_metadata_is_display_safe() {
    use agentscope::domain::{
        AggregationClass, CompactionObservation, CompactionScope, ContextUsage, Lineage,
        ModelDetail, OriginObservation, OriginSurface, ReleaseState, SessionMetrics, SourceKey,
        TimedRate,
    };
    use support::facts::{self, file_key, metadata};

    let hostile = format!("{SENTINEL}\u{1b}[31m\u{1b}]0;{SENTINEL}\u{7}");
    let secret_shaped = format!("password={INLINE_SECRET}\u{1b}[2J");
    let model = ModelDetail {
        id: format!("model-{hostile}"),
        provider: Some(format!("route-{hostile}")),
        variant: Some(format!("variant-{hostile}")),
        effort: Some(format!("effort-{hostile}")),
        configured_effort: Some(format!("configured-{hostile}")),
    };
    let origin = OriginObservation::new(OriginSurface::Other, Some(&hostile), Some(1));

    let path = std::path::Path::new("sess.jsonl");
    let mut meta = metadata(file_key(Harness::Codex, "s-new", path), &hostile);
    meta.project = Some(hostile.clone());
    meta.model = Some(model.clone());
    meta.origins = vec![origin.clone()];
    meta.released = ReleaseState::Present;
    meta.members = vec![file_key(Harness::Codex, &hostile, path)];
    meta.lineage = Lineage {
        agent_id: Some(hostile.clone()),
        agent_type: Some(hostile.clone()),
        parent_agent_id: Some(hostile.clone()),
        spawning_tool_id: Some(hostile.clone()),
        spawn_depth: Some(2),
        ..Lineage::default()
    };
    meta.harness_version = Some(hostile.clone());
    let mut summary = facts::summary(meta);
    summary.metrics = SessionMetrics {
        context: ContextUsage {
            used: Some(1),
            capacity: Some(2),
            class: AggregationClass::Derived,
            recorded_at: Some(1),
            model: Some(model.clone()),
            compaction_epoch: Some(hostile.clone()),
        },
        ..SessionMetrics::default()
    };
    summary.metrics.speed.latest = Some(TimedRate {
        model: Some(model.clone()),
        request_id: Some(hostile.clone()),
        started_at: None,
        completed_at: Some(1),
        output_tokens: 1,
        duration_ms: 1_000.0,
        ttft_ms: None,
        tokens_per_second: 1.0,
    });
    let locator_session = summary.meta.key.clone();
    summary.metrics.compactions.latest = Some(CompactionObservation {
        id: hostile.clone(),
        timestamp: None,
        method: Some(hostile.clone()),
        trigger: Some(hostile.clone()),
        summary: None,
        before_tokens: Some(2),
        after_tokens: Some(1),
        duration_ms: None,
        usage: None,
        scope: CompactionScope::Event,
        provenance: agentscope::domain::Provenance {
            harness: Harness::Codex,
            source: SourceKey::File("sess.jsonl".into()),
            locator: agentscope::domain::SourceLocator {
                session: locator_session,
                kind: agentscope::domain::LocatorKind::CompressedRecord {
                    path: "sess.jsonl.zst".into(),
                    ordinal: 3,
                },
                generation: None,
            },
            record_type: "compaction".into(),
            harness_version: None,
        },
    });

    assert_clean_debug("ModelDetail", &model);
    assert_clean_debug("OriginObservation", &origin);
    assert_clean_debug("SessionMetadata", &summary.meta);
    assert_clean_debug("Lineage", &summary.meta.lineage);
    assert_clean_debug("SessionMetrics", &summary.metrics);
    assert_clean_debug("SessionSummary", &summary);
    // Origin text is bounded at construction.
    let long = OriginObservation::new(OriginSurface::Cli, Some(&"x".repeat(10_000)), None);
    assert_eq!(
        long.raw.as_deref().map(str::len),
        Some(agentscope::domain::ORIGIN_RAW_MAX_CHARS)
    );

    // Rendered roster, inspector and session-info views, with the secret-shaped text
    // in the fields they display.
    let mut shown = summary.clone();
    shown.meta.title = secret_shaped.clone();
    shown.meta.project = Some(secret_shaped.clone());
    // The session-info overlay renders the recorded agent role too.
    shown.meta.lineage.agent_type = Some(secret_shaped.clone());
    let shown_model = ModelDetail {
        id: secret_shaped.clone(),
        provider: Some(secret_shaped.clone()),
        variant: Some(secret_shaped.clone()),
        effort: Some(secret_shaped.clone()),
        configured_effort: Some(secret_shaped.clone()),
    };
    shown.meta.model = Some(shown_model.clone());
    // A3 renders measured facts too: model identity, compaction epoch/method/trigger.
    shown.metrics.context.model = Some(shown_model.clone());
    shown.metrics.context.compaction_epoch = Some(secret_shaped.clone());
    if let Some(latest) = shown.metrics.speed.latest.as_mut() {
        latest.model = Some(shown_model);
    }
    if let Some(latest) = shown.metrics.compactions.latest.as_mut() {
        latest.method = Some(secret_shaped.clone());
        latest.trigger = Some(secret_shaped.clone());
    }
    let mut app = App::new(
        support::NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(vec![shown], vec![]);
    let ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    let roster = screen_text(&app, &ctx);
    assert_clean("roster", &roster);
    assert!(
        roster.contains("password="),
        "the field is rendered, masked"
    );
    assert!(app.open_inspector());
    let inspector = screen_text(&app, &ctx);
    assert_clean("inspector", &inspector);
    assert!(inspector.contains("Model:"), "the model line is rendered");
    assert!(app.open_session_info());
    assert_clean("session info", &screen_text(&app, &ctx));

    // The roster preview (beside the list, and swapped in on a narrow terminal)
    // shows the same hostile fields, sanitized and masked.
    app.close_overlay();
    app.close_inspector();
    let beside = screen_text_at(&app, &ctx, 160, 40);
    assert_clean("roster preview", &beside);
    assert!(
        beside.contains("Preview") && beside.contains("password="),
        "{beside}"
    );
    app.set_viewport_width(100);
    app.focus_next();
    assert!(app.preview_replaces_list());
    let swapped = screen_text_at(&app, &ctx, 100, 40);
    assert_clean("swapped preview", &swapped);
    assert!(swapped.contains("password="), "{swapped}");
}

/// Claude Desktop application data holds credentials, renderer caches, HMAC audit
/// logs and VM bundles next to the allowlisted manifests and Cowork roots. Only the
/// allowlisted files may be opened, and no sentinel may ever surface.
#[test]
fn claude_desktop_never_reads_credential_cache_audit_or_vm_stores() {
    use agentscope::domain::RefreshBudget;
    use agentscope::perf;
    use agentscope::providers::claude::ClaudeScanner;
    use support::claude as fx;
    use support::desktop;

    const DESKTOP: &str = "SENTINEL-DESKTOP-SECRET";
    let dir = tempfile::tempdir().unwrap();
    let layout = desktop::layout(dir.path());
    // Allowlisted sources: one Code transcript with a manifest, one Cowork transcript.
    fx::write(
        &layout.projects.join("proj").join("code-1.jsonl"),
        &[
            fx::user("code-1", "u0", "hello"),
            fx::assistant("code-1", "u1", "m1", (4, 1)),
        ],
    );
    desktop::manifest(
        &layout.manifest_dir,
        "local_code1.json",
        "code-1",
        serde_json::json!({"entrypoint": "claude-desktop"}),
    );
    let cowork = layout
        .cowork_projects
        .join("cowork-project")
        .join("cowork-1.jsonl");
    fx::write(&cowork, &[fx::user("cowork-1", "u0", "summarize")]);

    // Every forbidden store is locked as well as poisoned: any accidental open both
    // fails and would leave a trace.
    let mut locked = Vec::new();
    for path in &layout.forbidden {
        locked.push(support::Unreadable::new(path));
    }

    let mut config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(layout.projects.clone()),
            claude_desktop: Some(layout.data.clone()),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    config.process_enabled = false;
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    let budget = RefreshBudget {
        max_records: 10_000,
        max_bytes: 1 << 24,
        max_millis: 60_000,
    };
    perf::reset();
    let found = support::world::discover_pass(&mut scanner, support::NOW, budget);
    assert_eq!(
        found.sessions.len(),
        2,
        "the allowlisted transcripts are found"
    );
    // Only allowlisted bytes were read: manifests and transcripts, never the stores.
    let allowlisted: u64 = [
        layout.projects.join("proj").join("code-1.jsonl"),
        layout.manifest_dir.join("local_code1.json"),
        cowork.clone(),
    ]
    .iter()
    .map(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
    .sum();
    let used = perf::snapshot();
    assert!(
        used.bytes_read <= allowlisted + 64 * 1024,
        "discovery read {} bytes for {allowlisted} allowlisted bytes",
        used.bytes_read
    );

    let mut summaries = Vec::new();
    for meta in &found.sessions {
        summaries.push(support::world::hydrate_complete(
            &mut scanner,
            &meta.key,
            support::NOW,
        ));
    }
    assert_clean_debug("discovered Claude metadata", &found.sessions);
    assert_clean_debug("hydrated Claude summaries", &summaries);
    // The Desktop sentinel text never crosses into any reported fact.
    for text in [
        format!("{:?}", found.sessions),
        format!("{:?}", summaries),
        format!("{:?}", found.warnings),
    ] {
        assert!(
            !text.contains(DESKTOP),
            "a forbidden Desktop store leaked into reported facts"
        );
    }
    for s in &summaries {
        assert_clean("claude session title", &s.meta.title);
        for w in &s.warnings {
            assert_clean("claude warning", &w.message);
        }
    }
    for w in &found.warnings {
        assert_clean("claude warning", &w.message);
        for needle in [
            "host-creds",
            "audit",
            "vm_bundles",
            "IndexedDB",
            "ccd-session-secrets",
        ] {
            assert!(
                !w.key.source.contains(needle),
                "a forbidden store was opened: {}",
                w.key.source
            );
        }
    }
    for lock in &locked {
        assert!(lock.active, "the forbidden store stayed unreadable");
    }
    drop(locked);
}

#[cfg(any(unix, windows))]
fn symlink_file(target: &std::path::Path, link: &std::path::Path) -> bool {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(target, link).is_ok()
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(target, link).is_ok()
    }
}

/// A sidecar, a Desktop manifest or a Cowork `.claude` directory that is a link to
/// a credential store is refused before it is opened: no sentinel byte is read.
#[test]
fn claude_desktop_refuses_linked_sidecars_manifests_and_cowork_roots() {
    use agentscope::domain::{OriginSurface, RefreshBudget};
    use agentscope::perf;
    use agentscope::providers::claude::ClaudeScanner;
    use support::claude as fx;
    use support::desktop;

    const SENTINEL: &str = "SENTINEL-LINKED-STORE";
    let dir = tempfile::tempdir().unwrap();
    let layout = desktop::layout(dir.path());
    let secrets = dir.path().join("SECRETS");
    fs::create_dir_all(secrets.join("projects")).unwrap();
    // The linked targets are *valid* allowlisted JSON: if either were opened, its
    // facts would visibly surface on the real transcript.
    fs::write(
        secrets.join("manifest.json"),
        serde_json::json!({
            "cliSessionId": "code-1",
            "entrypoint": SENTINEL,
            "released": true,
        })
        .to_string(),
    )
    .unwrap();
    fs::write(
        secrets.join("sidecar.json"),
        serde_json::json!({
            "agentId": "a",
            "agentType": SENTINEL,
            "parentAgentId": "nobody",
        })
        .to_string(),
    )
    .unwrap();
    // A transcript reachable only through the linked directory: walking it would
    // prove the link was followed.
    fx::write(
        &secrets.join("projects").join("evil").join("evil.jsonl"),
        &[fx::user("evil", "u0", "reachable through the link")],
    );

    // Real session files: a child whose sidecar link must be refused, and a Code
    // transcript that a linked manifest must not enrich.
    let child = layout
        .projects
        .join("proj")
        .join("demo")
        .join("subagents")
        .join("agent-a.jsonl");
    fx::write(
        &child,
        &[fx::agent(fx::assistant("demo", "a1", "m", (4, 1)), "a")],
    );
    let code = layout.projects.join("proj").join("code-1.jsonl");
    fx::write(&code, &[fx::user("code-1", "u0", "hello")]);

    let cowork_session = layout
        .cowork_projects
        .ancestors()
        .nth(2)
        .expect("session directory");
    fs::remove_dir_all(cowork_session.join(".claude")).unwrap();
    assert!(
        support::link_dir(&secrets, &cowork_session.join(".claude")),
        "this platform can create a directory link"
    );

    // File links need platform privileges; when available they prove the manifest
    // and sidecar guards, otherwise only the directory guard runs here.
    let manifest_linked = symlink_file(
        &secrets.join("manifest.json"),
        &layout.manifest_dir.join("local_code1.json"),
    );
    if manifest_linked {
        symlink_file(
            &secrets.join("sidecar.json"),
            &child.with_file_name("agent-a.meta.json"),
        );
    } else {
        eprintln!(
            "note: this platform refused a file symlink; manifest/sidecar links untested here"
        );
    }

    let mut config = Config::load_with_env(
        &CliOverrides {
            claude_projects: Some(layout.projects.clone()),
            claude_desktop: Some(layout.data.clone()),
            ..Default::default()
        },
        &Env::new(),
    )
    .unwrap();
    config.process_enabled = false;
    let mut scanner = ClaudeScanner::new(&config).unwrap();
    perf::reset();
    let found = support::world::discover_pass(
        &mut scanner,
        support::NOW,
        RefreshBudget {
            max_records: 10_000,
            max_bytes: 1 << 24,
            max_millis: 60_000,
        },
    );
    let summaries: Vec<_> = found
        .sessions
        .iter()
        .map(|meta| support::world::hydrate_complete(&mut scanner, &meta.key, support::NOW))
        .collect();

    // The real transcripts are still discovered and hydrated; a linked manifest
    // leaves them exactly as the records describe them.
    let code_meta = found
        .sessions
        .iter()
        .find(|m| m.key.native_id == "code-1")
        .expect("the real Code transcript is discovered");
    let code_summary = summaries
        .iter()
        .find(|s| s.meta.key.native_id == "code-1")
        .unwrap();
    let child_summary = summaries
        .iter()
        .find(|s| s.meta.key.native_id == "a")
        .unwrap();
    assert_eq!(
        code_meta.released,
        agentscope::domain::ReleaseState::Unknown,
        "a linked manifest contributed no release state"
    );
    assert!(
        code_summary
            .meta
            .origins
            .iter()
            .all(|o| o.raw.as_deref() != Some("claude-code-sessions"))
            && code_meta
                .origins
                .iter()
                .all(|o| o.surface != OriginSurface::DesktopCode),
        "a linked manifest contributed no Desktop evidence"
    );
    assert_eq!(
        child_summary.meta.lineage.agent_type, None,
        "a linked sidecar contributed no agent type"
    );
    assert!(
        found.sessions.iter().all(|m| m.key.native_id != "evil"),
        "a linked Cowork project root is not walked"
    );
    // Only the real source bytes were read (sidecars and manifests are charged now).
    let allowlisted: u64 = [code.as_path(), child.as_path()]
        .iter()
        .map(|p| fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    let used = perf::snapshot().bytes_read;
    assert!(
        used <= allowlisted + 4096,
        "read {used} bytes for {allowlisted} allowlisted bytes"
    );
    let debug = format!("{:?} {:?}", found.sessions, summaries);
    assert!(
        !debug.contains(SENTINEL),
        "no linked store content surfaced"
    );
}

/// OpenCode allowlists `agent` and the session `model` JSON, but columns and JSON
/// members beside them are not read. A secret in `session_v2.metadata`, an extra
/// model key, message metadata beside `sessionID`, and tool output must never reach
/// Debug output, warnings, or any screen.
#[test]
fn opencode_metadata_beside_allowlisted_fields_never_surfaces() {
    use agentscope::domain::{RefreshBudget, SessionLink};
    use support::opencode as ox;

    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("opencode.db");
    let hostile = format!("{SENTINEL}\u{1b}[31m\u{1b}]0;{SENTINEL}\u{7}");
    let model = serde_json::json!({
        "id": "gpt-5",
        "providerID": "openai",
        "variant": "high",
        "api_key": SENTINEL,
        "note": hostile,
    })
    .to_string();
    {
        let conn = ox::create_v2(&db);
        let mut parent = ox::Session::new("ses_parent", 100, 900);
        parent.model = Some(&model);
        parent.agent = Some("build");
        parent.tokens = Some([10, 5, 0, 0, 0]);
        parent.cost = Some(0.01);
        ox::insert_session(&conn, &parent);
        ox::insert_session(&conn, &ox::Session::new("ses_child", 50, 60));
        // A non-allowlisted column beside the permitted agent/model columns.
        conn.execute(
            "UPDATE session_v2 SET metadata = ?1 WHERE id = 'ses_parent'",
            [serde_json::json!({"api_key": SENTINEL, "note": hostile}).to_string()],
        )
        .unwrap();
        // Message metadata: the permitted `sessionID` links; every neighbouring
        // secret field and the output text stay unread.
        conn.execute(
            "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
             VALUES ('m1', 'ses_parent', 'assistant', 0, 110, 110,
                     json(?1))",
            [serde_json::json!({
                "content": [{
                    "type": "tool",
                    "id": "c1",
                    "name": "subagent",
                    "state": {
                        "status": "completed",
                        "input": {"prompt": "safe"},
                        "output": "sessionID: ses_from_output",
                        "metadata": {
                            "sessionID": "ses_child",
                            "api_key": SENTINEL,
                            "note": hostile
                        }
                    }
                }],
                "model": {"id": "gpt-5", "providerID": "openai", "variant": "high"},
                "tokens": {"input": 1, "output": 1},
                "secret": SENTINEL
            })
            .to_string()],
        )
        .unwrap();
    }

    let mut config = ox::config_for_db(&db);
    config.process_enabled = false;
    let mut scanner = OpenCodeScanner::new(&config).unwrap();
    let budget = RefreshBudget {
        max_records: 10_000,
        max_bytes: 1 << 24,
        max_millis: 60_000,
    };
    let found = support::world::discover_pass(&mut scanner, support::NOW, budget);
    assert_clean_debug("OpenCode discovered metadata", &found.sessions);
    assert_clean(
        "OpenCode discovery warnings",
        &format!("{:?}", found.warnings),
    );

    let parent_key = found
        .sessions
        .iter()
        .find(|m| m.key.native_id == "ses_parent")
        .unwrap()
        .key
        .clone();
    let parent = support::world::hydrate_complete(&mut scanner, &parent_key, support::NOW);
    assert_clean_debug("OpenCode summary", &parent);
    assert_clean("OpenCode warnings", &format!("{:?}", parent.warnings));
    assert_eq!(parent.meta.lineage.agent_type.as_deref(), Some("build"));
    assert_eq!(
        parent.meta.model.as_ref().unwrap().provider.as_deref(),
        Some("openai")
    );
    assert_eq!(
        parent.meta.model.as_ref().unwrap().effort.as_deref(),
        Some("high")
    );

    let page = support::world::try_transcript(
        &mut scanner,
        &parent_key,
        None,
        PageDirection::Forward,
        support::world::PAGE,
        budget,
        support::world::STEPS,
    )
    .unwrap();
    let tool = page
        .events
        .iter()
        .find_map(|e| e.tool.as_ref())
        .expect("tool event");
    assert_eq!(
        tool.child_session,
        Some(SessionLink::Unresolved("ses_child".into()))
    );
    assert_clean_debug("OpenCode transcript page", &page);
    for event in &page.events {
        assert_clean_debug("OpenCode event", event);
    }

    // Roster, inspector, details and session info never render the secret fields.
    let mut app = App::new(
        support::NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(vec![parent], vec![]);
    let ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    let roster = screen_text(&app, &ctx);
    assert_clean("OpenCode roster", &roster);
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let Some(request_id) = requests.iter().find_map(|r| match r {
        WorkerRequest::Select { request_id, .. } => Some(*request_id),
        _ => None,
    }) else {
        panic!("expected a select request, got {requests:?}");
    };
    assert!(app.apply_transcript(request_id, page.clone()));
    let inspector = screen_text(&app, &ctx);
    assert_clean("OpenCode inspector", &inspector);
    assert!(app.open_details());
    let details = screen_text(&app, &ctx);
    assert_clean("OpenCode details", &details);
    assert!(
        details.contains("Child session:") || details.contains("resolved"),
        "the child link is rendered: {details}"
    );
    app.close_overlay();
    assert!(app.open_session_info());
    let session = screen_text(&app, &ctx);
    assert_clean("OpenCode session info", &session);
    for needle in ["Billing provider: openai", "Model: gpt-5", "Effort: high"] {
        assert!(session.contains(needle), "missing {needle:?}:\n{session}");
    }
}

// ---- B7: the integrated native world ---------------------------------------------

fn assert_clean_native(world: &World, label: &str, text: &str) {
    for needle in world
        .grown()
        .forbidden_texts()
        .into_iter()
        .chain(["\u{1b}", "\u{7}"])
    {
        assert!(
            !text.contains(needle),
            "{label} leaked {needle:?}: {}",
            text.chars().take(200).collect::<String>()
        );
    }
}

#[test]
fn native_world_readers_leak_no_secret_field_or_control_through_any_debug_or_text() {
    let world = World::native();
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    assert!(!surface.warnings.is_empty(), "damaged sources are reported");
    for session in &surface.sessions {
        let m = &session.meta;
        assert_clean_native(&world, "meta debug", &format!("{m:?} {m:#?}"));
        assert_clean_native(&world, "title", &m.title);
        assert_clean_native(&world, "summary debug", &format!("{session:?}"));
    }
    for w in &surface.warnings {
        assert_clean_native(&world, "warning", &format!("{w:?} {}", w.message));
    }
    for page in &surface.pages {
        assert_clean_native(&world, "page debug", &format!("{page:?}"));
        for e in &page.events {
            assert_clean_native(&world, "event text", &preview(e));
        }
    }

    // The locked database's failure text names the problem, never the content, and
    // is the same on every retry.
    let mut locked = world.locked_readers();
    let opencode = locked.last_mut().unwrap();
    for _ in 0..2 {
        let error = opencode
            .discover(support::NOW, BIG)
            .expect_err("the locked WAL stays unavailable");
        assert!(matches!(error, ProviderError::Unavailable(_)));
        assert_clean_native(&world, "locked error", &format!("{error:?} {error}"));
    }
    let mut found = Surface::default();
    let readable = locked.len() - 1;
    surface_all(&mut locked[..readable], &mut found);
    for w in &found.warnings {
        assert_clean_native(&world, "locked warning", &format!("{w:?} {}", w.message));
    }
}

#[test]
fn native_world_screens_and_worker_events_leak_no_secret_field_or_control() {
    let world = World::native();
    let g = world.grown();
    let mut config = world.config();
    config.refresh_ms = 3_600_000;
    let worker = agentscope::worker::WorkerHandle::spawn(world.readers(), config);
    let mut app = App::new(
        support::NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    let step = |app: &mut App, ctx: &mut RosterContext| {
        for r in app.take_requests() {
            worker.tx.send(r).unwrap();
        }
        if let Ok(ev) = worker.rx.recv_timeout(Duration::from_millis(20)) {
            assert_clean_native(&world, "worker event", &format!("{ev:?}"));
            apply_worker_event(app, ctx, ev);
        }
    };
    let until = |app: &mut App, ctx: &mut RosterContext, done: &dyn Fn(&App) -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            step(app, ctx);
            if done(app) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "the app stalled");
        }
    };
    let screen = |app: &App, ctx: &RosterContext, w: u16, h: u16| screen_text_at(app, ctx, w, h);
    let key = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while app.total_sessions() != ROOT_COUNT {
        app.load_more_roots();
        step(&mut app, &mut ctx);
        assert!(
            std::time::Instant::now() < deadline,
            "every root never loaded"
        );
    }
    app.set_filter(agentscope::app::FilterState {
        include_released: true,
        ..Default::default()
    });
    let mut screens: Vec<String> = Vec::new();
    let mut source_screens = 0;
    for target in [
        &g.secret_claude,
        &g.secret_codex,
        &g.secret_opencode,
        &g.zstd,
        &g.truncated_zstd,
        &g.partial_codex,
        &g.damaged_claude,
        &g.desktop_code,
        &g.unknown_billing,
        &g.opencode.parent,
        &g.claude.parent,
        &g.omp.secret,
        &g.omp.family.parent,
        &g.omp.fork,
    ] {
        assert!(app.select_row(target));
        for (w, h) in [(60, 20), (120, 40), (160, 40)] {
            screens.push(screen(&app, &ctx, w, h));
        }
        handle_key(&mut app, key(KeyCode::Enter));
        until(&mut app, &mut ctx, &|a| {
            a.transcript().is_some_and(|t| t.loaded())
        });
        for _ in 0..4 {
            handle_key(&mut app, key(KeyCode::Enter));
            screens.push(screen(&app, &ctx, 120, 40));
            handle_key(&mut app, key(KeyCode::Char('k')));
        }
        // Details, then the source record with masking on (its default).
        handle_key(&mut app, key(KeyCode::Char('d')));
        let settle = std::time::Instant::now() + Duration::from_millis(150);
        while std::time::Instant::now() < settle {
            step(&mut app, &mut ctx);
        }
        screens.push(screen(&app, &ctx, 120, 40));
        handle_key(&mut app, key(KeyCode::Esc));
        handle_key(&mut app, key(KeyCode::Char('v')));
        if app.screen == agentscope::app::Screen::Source {
            until(&mut app, &mut ctx, &|a| {
                a.source_view()
                    .is_some_and(|v| !matches!(v.state, agentscope::app::SourceState::Loading))
            });
            source_screens += 1;
            screens.push(screen(&app, &ctx, 120, 40));
            handle_key(&mut app, key(KeyCode::Esc));
        }
        handle_key(&mut app, key(KeyCode::Char('D')));
        screens.push(screen(&app, &ctx, 120, 40));
        handle_key(&mut app, key(KeyCode::Esc));
        while app.screen != agentscope::app::Screen::Roster {
            handle_key(&mut app, key(KeyCode::Esc));
        }
    }
    for s in &screens {
        assert_clean_native(&world, "rendered screen", s);
    }
    assert!(
        screens
            .iter()
            .any(|s| s.contains("claude family tail entry")),
        "the family sessions were actually drawn"
    );
    assert!(
        screens.iter().any(|s| s.contains(OMP_TAIL)),
        "the OMP family was actually drawn"
    );
    assert!(
        screens.iter().any(|s| s.contains("[REDACTED]")),
        "credential-shaped text was drawn masked, not absent"
    );
    assert!(
        source_screens >= 8,
        "source records were opened: {source_screens}"
    );
    assert_clean_native(&world, "app debug", &format!("{:?}", app.warnings()));
    let _ = worker.tx.send(WorkerRequest::Shutdown);
    let _ = WorkerEvent::RefreshFinished { generation: 0 };
}

#[test]
fn omp_titles_prompts_private_fields_and_decoys_never_surface() {
    let world = World::with_omp();
    let plant = world.omp_plant.as_ref().expect("planted OMP family");
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    let forbidden = [
        SENTINEL,
        INLINE_SECRET,
        FIELD_SECRET,
        OMP_PROMPT_SECRET,
        "\u{1b}",
        "\u{7}",
    ];
    let clean = |label: &str, text: &str| {
        for needle in forbidden {
            assert!(
                !text.contains(needle),
                "{label} leaked {needle:?}: {}",
                text.chars().take(200).collect::<String>()
            );
        }
    };
    for session in &surface.sessions {
        let m = &session.meta;
        clean("meta", &format!("{m:?} {m:#?}"));
        clean("title", &m.title);
        clean("project", m.project.as_deref().unwrap_or(""));
        clean("summary", &format!("{session:?}"));
    }
    for w in &surface.warnings {
        clean("warning", &format!("{w:?} {}", w.message));
    }
    for page in &surface.pages {
        clean("page", &format!("{page:?}"));
        for e in &page.events {
            if let Some(t) = &e.text {
                clean("event text", &t.preview);
            }
            if let Some(tool) = &e.tool {
                clean("tool", &tool.name);
            }
        }
    }
    // The secret-bearing session's own record text is masked, not absent.
    let secret = surface
        .sessions
        .iter()
        .find(|s| s.meta.key == plant.secret)
        .expect("secret session");
    assert!(
        secret.meta.title.contains("[REDACTED]"),
        "{}",
        secret.meta.title
    );
    let reader = readers
        .iter_mut()
        .find(|r| r.harness() == Harness::Omp)
        .unwrap();
    let page = transcript_ready(
        reader.as_mut(),
        &plant.secret,
        None,
        PageDirection::Backward,
        PAGE,
        BIG,
        STEPS,
    );
    let prompt = page
        .events
        .iter()
        .filter_map(|e| e.text.as_ref())
        .map(|t| t.preview.as_str())
        .find(|t| t.contains("password="))
        .expect("the credential-shaped prompt is shown");
    assert!(prompt.contains("[REDACTED]"), "{prompt}");
    // The session-init record (system prompt, task, environment) shows a fixed label.
    assert!(
        page.events
            .iter()
            .filter_map(|e| e.text.as_ref())
            .any(|t| t.preview.contains("content not shown"))
    );
    // The explicit raw record view is sanitized and credential-masked; it is the only
    // place a session-init record's own prompt text is shown.
    for event in &page.events {
        let record = try_source(reader.as_mut(), &event.provenance.locator, BIG, STEPS).unwrap();
        for needle in [SENTINEL, INLINE_SECRET, FIELD_SECRET, "\u{1b}", "\u{7}"] {
            assert!(!record.display.contains(needle), "{needle:?}");
        }
    }
}

#[test]
fn hermes_profiles_and_decoy_credentials_never_surface() {
    let world = World::with_hermes();
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    for session in &surface.sessions {
        assert_clean("Hermes summary", &format!("{:?}", session));
        assert_clean("Hermes metadata", &format!("{:?}", session.meta));
    }
    for page in &surface.pages {
        for event in &page.events {
            assert_clean("Hermes event", &format!("{event:?}"));
            if let Some(text) = &event.text {
                assert_clean("Hermes preview", &text.preview);
            }
            if let Some(tool) = &event.tool {
                assert_clean("Hermes tool", &format!("{tool:?}"));
            }
        }
    }
    for warning in &surface.warnings {
        assert_clean("Hermes warning", &warning.message);
        assert_clean("Hermes warning source", &warning.key.source);
    }
    let masked = surface
        .pages
        .iter()
        .flat_map(|page| page.events.iter())
        .filter_map(|event| event.text.as_ref())
        .any(|text| text.preview.contains("api_key="));
    assert!(
        masked,
        "the credential-shaped transcript text is rendered masked"
    );
    let leaked = surface
        .pages
        .iter()
        .flat_map(|page| page.events.iter())
        .any(|event| {
            event
                .text
                .as_ref()
                .is_some_and(|text| text.preview.contains(INLINE_SECRET))
        });
    assert!(!leaked, "the inline credential is never painted");
}
