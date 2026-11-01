//! Spec §17 acceptance scenarios AC-01..AC-10, exercised on synthetic fixtures through
//! the real provider adapters, the real worker thread, and the real keyboard path.
//! Real-data acceptance (installed providers, real terminals) is a manual step; see
//! README "Verification status".

mod support;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::time::{Duration, Instant};

use agentscope::app::{App, Screen};
use agentscope::domain::*;
use agentscope::processes::{
    ProcessError, ProcessInfo, ProcessSnapshot, associate, association_from_observation,
};
use agentscope::providers::ProviderReader;
use agentscope::providers::codex::CodexScanner;
use agentscope::status::synthesize;
use agentscope::ui::{
    KeyAction, RosterContext, apply_worker_event, flush_requests, handle_key, render,
};
use agentscope::worker::{WorkerEvent, WorkerHandle, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::codex as cx;
use support::world::*;
use support::{NOW, dated, write_lines};

const THRESHOLDS: Thresholds = Thresholds {
    quiet: Duration::from_secs(300),
    stall: Duration::from_secs(900),
};

struct Driver {
    app: App,
    ctx: RosterContext,
    worker: WorkerHandle,
}

impl Driver {
    fn start(world: &World) -> Self {
        let worker = WorkerHandle::spawn(world.readers(), world.config());
        Self {
            app: App::new(NOW, THRESHOLDS),
            ctx: RosterContext {
                no_color: true,
                ..RosterContext::default()
            },
            worker,
        }
    }

    fn pump_until(&mut self, what: &str, done: impl Fn(&App) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            flush_requests(&mut self.app, &self.worker);
            if done(&self.app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            if let Ok(ev) = self.worker.rx.recv_timeout(Duration::from_millis(25)) {
                apply_worker_event(&mut self.app, &mut self.ctx, ev);
            }
        }
    }

    fn key(&mut self, code: KeyCode) -> KeyAction {
        handle_key(&mut self.app, KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn ch(&mut self, c: char) {
        let mods = if c.is_uppercase() {
            KeyModifiers::SHIFT
        } else {
            KeyModifiers::NONE
        };
        handle_key(&mut self.app, KeyEvent::new(KeyCode::Char(c), mods));
    }

    fn type_text(&mut self, text: &str) {
        for c in text.chars() {
            self.ch(c);
        }
    }

    fn screen(&self) -> String {
        let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
        terminal.draw(|f| render(f, &self.app, &self.ctx)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }
}

fn append(path: &std::path::Path, text: &str) {
    OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
}

fn scan(reader: &mut dyn ProviderReader) -> ProviderDeltaLite {
    let mut out = ProviderDeltaLite::default();
    loop {
        let d = support::world::rescan(reader, NOW, BIG);
        out.sessions.extend(d.sessions);
        out.invalidated.extend(d.invalidated);
        out.warnings.extend(d.warnings);
        if d.scan.done {
            return out;
        }
    }
}

#[derive(Default)]
struct ProviderDeltaLite {
    sessions: Vec<SessionSummary>,
    invalidated: Vec<SessionKey>,
    warnings: Vec<Warning>,
}

fn page(reader: &mut dyn ProviderReader, key: &SessionKey) -> TranscriptPage {
    support::world::try_transcript(
        reader,
        key,
        None,
        PageDirection::Forward,
        PAGE,
        support::world::BIG,
        support::world::STEPS,
    )
    .expect("page")
}

fn texts(page: &TranscriptPage) -> Vec<String> {
    page.events
        .iter()
        .filter_map(|e| e.text.as_ref().map(|t| t.preview.clone()))
        .collect()
}

// ---- AC-01 ---------------------------------------------------------------------

#[test]
fn ac01_no_disruption_start_issues_no_writes_and_tolerates_provider_appends() {
    let world = World::new();
    let before = snapshot(world.dir.path());
    let mut h = Driver::start(&world);
    h.pump_until("roster", |a| a.total_sessions() == 3);

    // A live provider legitimately appends while AgentScope runs; only that append differs.
    let codex_count = |a: &App| {
        a.all_rows()
            .iter()
            .find(|r| r.session.meta.key.harness == Harness::Codex)
            .map(|r| r.session.displayed_event_count)
            .expect("codex row")
    };
    let original = codex_count(&h.app);
    let appended = format!("{}\n", cx::assistant("2026-10-29T23:55:00Z", "live reply"));
    append(&world.codex_file(), &appended);
    h.worker.tx.send(WorkerRequest::Refresh).unwrap();
    h.pump_until("append visible", |a| codex_count(a) > original);
    assert!(h.worker.shutdown());

    let after = snapshot(world.dir.path());
    let codex_rel = world
        .codex_file()
        .strip_prefix(world.dir.path())
        .unwrap()
        .to_path_buf();
    for (path, bytes) in &after.files {
        if *path == codex_rel {
            let mut expected = before.files[path].clone();
            expected.extend_from_slice(appended.as_bytes());
            assert_eq!(*bytes, expected, "only the provider's own append differs");
        } else {
            assert_eq!(bytes, &before.files[path], "{path:?} untouched");
        }
    }
    assert_eq!(after.files.len(), before.files.len());
}

// ---- AC-02 / AC-03 -------------------------------------------------------------

#[test]
fn ac02_drill_down_reads_ordered_transcript_and_expands_a_tool_result() {
    let world = World::new();
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    for provider in [Harness::Codex, Harness::Claude] {
        let page = surface
            .pages
            .iter()
            .find(|p| {
                p.events
                    .first()
                    .is_some_and(|e| e.provenance.harness == provider)
            })
            .expect("page");
        assert!(
            page.events.len() >= 3,
            "{provider:?} transcript has several events"
        );
        let orders: Vec<_> = page
            .events
            .iter()
            .map(|e| (e.timestamp, e.source_order))
            .collect();
        let mut sorted = orders.clone();
        sorted.sort();
        assert_eq!(orders, sorted, "{provider:?} events are ordered");
        let tool = page
            .events
            .iter()
            .find_map(|e| e.tool.as_ref())
            .expect("tool interaction");
        assert!(
            tool.output.is_some(),
            "{provider:?} tool result is linked to its call"
        );
        assert!(!tool.orphan);
    }
}

#[test]
fn ac03_multi_provider_roster_contains_codex_claude_and_opencode() {
    let world = World::new();
    let mut readers = world.readers();
    let mut surface = Surface::default();
    support::world::surface_all(&mut readers, &mut surface);
    let mut providers: Vec<_> = surface
        .sessions
        .iter()
        .map(|s| s.meta.key.harness)
        .collect();
    providers.sort_by_key(|p| format!("{p:?}"));
    assert_eq!(
        providers,
        [Harness::Claude, Harness::Codex, Harness::OpenCode]
    );
    for s in &surface.sessions {
        assert!(
            !s.meta.title.is_empty()
                || s.meta.title_derived
                || s.meta.key.harness == Harness::Claude
        );
        assert!(s.meta.source_available);
    }
}

// ---- AC-04 ---------------------------------------------------------------------

#[test]
fn ac04_live_append_appears_without_restart_for_every_provider() {
    let world = World::new();
    let mut readers = world.readers();
    let mut first = Surface::default();
    support::world::surface_all(&mut readers, &mut first);
    let count_of = |s: &Surface, p: Harness| {
        s.sessions
            .iter()
            .rev()
            .find(|x| x.meta.key.harness == p)
            .unwrap()
            .displayed_event_count
    };
    let codex_before = count_of(&first, Harness::Codex);
    let claude_before = count_of(&first, Harness::Claude);
    let open_before = count_of(&first, Harness::OpenCode);

    append(
        &world.codex_file(),
        &format!(
            "{}\n",
            cx::assistant("2026-10-29T23:56:00Z", "codex appended")
        ),
    );
    append(
        &world.claude_file(),
        &format!(
            "{}\n",
            claude_line(
                "assistant",
                "a3",
                "2026-10-29T23:56:01Z",
                serde_json::json!("claude appended"),
                None
            )
        ),
    );
    world
        .writer
        .execute(
            "INSERT INTO session_message(id, session_id, type, seq, time_created, time_updated, data)
             VALUES ('m2', ?1, 'user', 1, ?2, ?2, '{\"text\":\"opencode appended\"}')",
            rusqlite::params![OPENCODE_ID, NOW - 1000],
        )
        .unwrap();

    let mut second = Surface::default();
    support::world::surface_all(&mut readers, &mut second);
    assert!(count_of(&second, Harness::Codex) > codex_before);
    assert!(count_of(&second, Harness::Claude) > claude_before);
    assert!(count_of(&second, Harness::OpenCode) > open_before);

    let mut pages = Vec::new();
    for s in &second.sessions {
        let r = readers
            .iter_mut()
            .find(|r| r.harness() == s.meta.key.harness)
            .unwrap();
        pages.push((s.meta.key.harness, texts(&page(r.as_mut(), &s.meta.key))));
    }
    for (provider, needle) in [
        (Harness::Codex, "codex appended"),
        (Harness::Claude, "claude appended"),
        (Harness::OpenCode, "opencode appended"),
    ] {
        assert!(
            pages
                .iter()
                .any(|(p, t)| *p == provider && t.iter().any(|x| x.contains(needle))),
            "{provider:?} append visible in transcript"
        );
    }
}

// ---- AC-05 ---------------------------------------------------------------------

#[test]
fn ac05_partial_malformed_oversized_and_ambiguous_sources_do_not_hide_other_sessions() {
    let world = World::new();
    let damaged = dated(&world.codex_root, "rollout-damaged.jsonl");
    write_lines(
        &damaged,
        &[
            cx::session_meta("2026-10-29T22:00:00Z", "damaged", "/work/damaged"),
            "not json".into(),
            "also {bad".into(),
            cx::user("2026-10-29T22:00:03Z", &"y".repeat(5000)),
            cx::user("2026-10-29T22:00:04Z", "after the damage"),
        ],
    );
    // Ambiguous identity: two files claim the same session id.
    for name in ["rollout-dup-a.jsonl", "rollout-dup-b.jsonl"] {
        write_lines(
            &dated(&world.codex_root, name),
            &[
                cx::session_meta("2026-10-29T21:00:00Z", "dup", "/work/dup"),
                cx::user("2026-10-29T21:00:01Z", "dup prompt"),
            ],
        );
    }
    let tail = cx::assistant("2026-10-29T22:00:05Z", "late reply");
    let (head, rest) = tail.split_at(20);
    append(&damaged, head);

    let mut config = world.config();
    config.max_record_bytes = 2048;
    let mut codex = CodexScanner::new(&config).unwrap();
    let out = scan(&mut codex);
    let bad = out
        .sessions
        .iter()
        .rev()
        .find(|s| s.meta.title == "after the damage")
        .expect("damaged session still listed");
    let kinds: Vec<_> = bad
        .warnings
        .iter()
        .map(|w| (w.key.source.clone(), w.key.kind, w.count))
        .collect();
    assert!(
        kinds
            .iter()
            .any(|(s, k, c)| s.contains("malformed") && *k == WarningKind::Parse && *c == 2),
        "malformed records reported: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|(s, _, _)| s.contains("oversized")),
        "oversized reported: {kinds:?}"
    );
    assert!(
        out.sessions
            .iter()
            .any(|s| s.meta.key.native_id == CODEX_ID),
        "healthy sessions remain"
    );
    assert!(
        out.sessions.iter().any(|s| s
            .warnings
            .iter()
            .any(|w| w.key.source.contains("duplicate"))),
        "ambiguous duplicate identity is flagged"
    );

    // The partial trailing line is held, then delivered once complete.
    let key = bad.meta.key.clone();
    let before = page(&mut codex, &key);
    assert!(!texts(&before).iter().any(|t| t == "late reply"));
    append(&damaged, &format!("{rest}\n"));
    let _ = scan(&mut codex);
    assert!(
        texts(&page(&mut codex, &key))
            .iter()
            .any(|t| t == "late reply")
    );
}

// ---- AC-06 ---------------------------------------------------------------------

#[test]
fn ac06_usage_matches_golden_vectors_and_cost_requires_an_exact_price() {
    let world = World::new();
    // A streamed Claude message repeated under one id must count once.
    let usage = serde_json::json!({"input_tokens":10,"output_tokens":4,"cache_creation_input_tokens":0,"cache_read_input_tokens":2});
    let dup = claude_line(
        "assistant",
        "dup",
        "2026-10-29T23:52:00Z",
        serde_json::json!("streamed"),
        Some(usage),
    );
    write_lines(
        &world.claude_root.join("proj").join("dup.jsonl"),
        &[dup.clone(), dup],
    );

    let mut readers = world.readers();
    let mut surface = Surface::default();
    support::world::surface_all(&mut readers, &mut surface);
    let codex = surface
        .sessions
        .iter()
        .rev()
        .find(|s| s.meta.key.harness == Harness::Codex)
        .unwrap();
    let u = codex.usage.as_ref().expect("codex usage");
    assert_eq!(u.total, Some(120), "latest cumulative snapshot, not a sum");
    // Canonical legs: Codex input includes the cached reads, which are published apart.
    assert_eq!(u.categories.input, Some(60));
    assert_eq!(u.categories.cache_read, Some(40));
    assert_eq!(u.categories.cached_input, None);
    assert_eq!(u.categories.output, Some(20));
    assert_eq!(u.class, AggregationClass::Reported);
    assert_eq!(u.cost.usd, None, "no configured price: cost stays unknown");
    assert_eq!(u.cost.class, AggregationClass::Unknown);

    let claude_usage: Vec<_> = surface
        .sessions
        .iter()
        .filter(|s| s.meta.key.harness == Harness::Claude)
        .filter_map(|s| s.usage.as_ref())
        .collect();
    assert_eq!(
        claude_usage.len(),
        2,
        "original and duplicated-record sessions"
    );
    for u in claude_usage {
        assert_eq!(
            u.categories.input,
            Some(10),
            "a repeated record counts once"
        );
        assert_eq!(u.categories.output, Some(4));
    }

    // With an exact price entry the estimate is labelled Estimated and matches the formula.
    let mut config = world.config();
    config.pricing = support::facts::price_schedule(
        r#"[{"billing_provider":"openai","model":"gpt-test","as_of":"2026-10-01",
            "rates":{"input":2.0,"output":8.0,"cache_read":1.0}}]"#,
    );
    let mut codex_reader = CodexScanner::new(&config).unwrap();
    let priced = scan(&mut codex_reader);
    let cost = &priced.sessions.last().unwrap().usage.as_ref().unwrap().cost;
    assert_eq!(cost.class, AggregationClass::Estimated);
    // (100-40)*2 + 40*1 + 20*8 = 320 micro-dollars.
    assert!((cost.usd.unwrap() - 0.000_320).abs() < 1e-12, "{cost:?}");
    assert_eq!(cost.as_of.as_deref(), Some("2026-10-01"));
}

// ---- AC-07 ---------------------------------------------------------------------

#[test]
fn ac07_missing_data_is_shown_as_unknown_or_unmatched_never_invented() {
    let world = World::new();
    let mut readers = world.readers();
    let mut surface = Surface::default();
    support::world::surface_all(&mut readers, &mut surface);
    let claude = surface
        .sessions
        .iter()
        .find(|s| s.meta.key.harness == Harness::Claude)
        .unwrap();
    assert_eq!(
        claude.lifecycle,
        Lifecycle::Unknown,
        "no persisted terminal event"
    );
    let status = synthesize(claude, NOW, &THRESHOLDS);
    assert_ne!(status.label, StatusLabel::Complete);
    assert!(!status.evidence.is_empty(), "every status explains itself");
    for s in &surface.sessions {
        if let Some(u) = &s.usage {
            assert_eq!(u.cost.usd, None, "unpriced cost is not $0.00");
            assert_ne!(u.cost.class, AggregationClass::Estimated);
        }
    }

    // Process evidence: unrelated processes -> Unmatched; failure -> Unavailable; fuzzy -> candidates only.
    let codex = surface
        .sessions
        .iter()
        .find(|s| s.meta.key.harness == Harness::Codex)
        .unwrap();
    let other = ProcessInfo::new(
        1,
        Some(NOW),
        "editor".into(),
        None,
        Some(vec!["editor".into()]),
    );
    let snap = ProcessSnapshot {
        sampled_at: NOW,
        processes: vec![other],
    };
    assert_eq!(associate(codex, &snap), ProcessAssociation::Unmatched);
    assert_eq!(
        association_from_observation(codex, &Err(ProcessError::Unavailable)),
        ProcessAssociation::Unavailable
    );
    let fuzzy = ProcessInfo::new(
        2,
        Some(NOW),
        "codex".into(),
        None,
        Some(vec!["codex".into()]),
    );
    let snap = ProcessSnapshot {
        sampled_at: NOW,
        processes: vec![fuzzy],
    };
    assert!(matches!(
        associate(codex, &snap),
        ProcessAssociation::Candidates { count: 1 }
    ));
    let exact = ProcessInfo::new(
        3,
        Some(NOW),
        "codex".into(),
        None,
        Some(vec!["codex".into(), "resume".into(), CODEX_ID.into()]),
    );
    let snap = ProcessSnapshot {
        sampled_at: NOW,
        processes: vec![exact],
    };
    assert!(matches!(
        associate(codex, &snap),
        ProcessAssociation::Matched { pid: 3, .. }
    ));
}

// ---- AC-08 / AC-09 (full suites: tests/read_only.rs, tests/privacy.rs) -------------

#[test]
fn ac08_read_only_regression_summary() {
    let world = World::new();
    let before = snapshot(world.dir.path());
    let mut readers = world.readers();
    let _ = exercise_everything(&mut readers);
    drop(readers);
    assert_eq!(snapshot(world.dir.path()).files, before.files);
    let conn = agentscope::providers::opencode::open_read_only(&world.db).unwrap();
    assert!(conn.prepare("SELECT secret FROM credential").is_err());
}

#[test]
fn ac09_privacy_summary_masks_secrets_and_neutralizes_controls() {
    let world = World::new();
    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    for e in surface.pages.iter().flat_map(|p| &p.events) {
        let Some(t) = &e.text else { continue };
        assert!(!t.preview.contains(INLINE_SECRET));
        assert!(!t.preview.contains('\u{1b}'));
    }
}

// ---- AC-10 and the umbrella flow -------------------------------------------------

#[test]
fn acceptance_scenarios_ac01_to_ac10_have_exercised_paths() {
    // Keyboard only: discover -> filter -> open -> expand tool result -> search -> back.
    let world = World::new();
    let mut h = Driver::start(&world);
    h.pump_until("three-provider roster", |a| a.total_sessions() == 3);
    assert_eq!(h.app.provider_counts().len(), 6);

    h.ch('/');
    assert!(h.app.is_editing());
    h.type_text("inventory");
    h.key(KeyCode::Enter);
    assert_eq!(
        h.app.visible_rows().len(),
        1,
        "search narrows the roster to the Codex session"
    );
    h.key(KeyCode::Enter);
    assert_eq!(h.app.screen, Screen::Inspector);
    h.pump_until("transcript", |a| a.transcript().is_some_and(|t| t.loaded()));

    // The transcript opens at its newest entry; move back to the tool interaction.
    let mut steps = 0;
    while !h
        .app
        .transcript()
        .unwrap()
        .selected_event()
        .is_some_and(|e| e.tool.is_some())
    {
        h.ch('k');
        steps += 1;
        assert!(steps < 12, "tool event reachable with k");
    }
    h.key(KeyCode::Enter);
    {
        let t = h.app.transcript().unwrap();
        assert!(t.is_expanded(t.selected_event().unwrap()));
    }
    assert!(
        h.screen().contains("inventory.rs"),
        "expanded tool output is visible"
    );

    // Search inside the transcript.
    h.ch('/');
    h.type_text("inventory");
    h.key(KeyCode::Enter);
    assert!(h.app.transcript().unwrap().match_count() >= 1);
    h.ch('n');

    // Back to the roster, same selection; quit by keyboard.
    h.key(KeyCode::Esc);
    assert_eq!(h.app.screen, Screen::Roster);
    assert_eq!(h.app.selected_key().unwrap().harness, Harness::Codex);
    assert_eq!(h.key(KeyCode::Char('q')), KeyAction::Quit);
    assert!(h.worker.shutdown());
    let _ = fs::metadata(world.codex_file()).expect("fixture still present");
}

// ---- AC-11: the integrated native world (spec §5, §6, §10) ------------------------

fn wait_for_event(
    worker: &WorkerHandle,
    what: &str,
    mut hit: impl FnMut(&WorkerEvent) -> bool,
) -> WorkerEvent {
    let deadline = Instant::now() + Duration::from_secs(60);
    while Instant::now() < deadline {
        if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(50))
            && hit(&event)
        {
            return event;
        }
    }
    panic!("never saw {what}");
}

fn roster_with(
    worker: &WorkerHandle,
    what: &str,
    pred: impl Fn(&agentscope::roster::RosterSnapshot) -> bool,
) -> agentscope::roster::RosterSnapshot {
    let WorkerEvent::Roster { snapshot, .. } = wait_for_event(
        worker,
        what,
        |e| matches!(e, WorkerEvent::Roster { snapshot, .. } if pred(snapshot)),
    ) else {
        unreachable!()
    };
    snapshot
}

fn hydrate(worker: &WorkerHandle, id: u64, key: &SessionKey) -> std::sync::Arc<SessionSummary> {
    worker
        .tx
        .send(WorkerRequest::HydratePreview {
            request_id: id,
            key: key.clone(),
        })
        .unwrap();
    let WorkerEvent::Preview { summary, .. } = wait_for_event(
        worker,
        "a hydrated preview",
        |e| matches!(e, WorkerEvent::Preview { request_id, done: true, .. } if *request_id == id),
    ) else {
        unreachable!()
    };
    summary
}

#[test]
fn ac11_native_world_pages_exact_roots_and_places_families_with_direct_usage() {
    let world = World::native();
    let g = world.grown();
    let mut config = world.config();
    config.refresh_ms = 3_600_000;
    let worker = WorkerHandle::spawn(world.readers(), config);
    let expected = g.expected_roots();
    assert_eq!(expected.len(), ROOT_COUNT);

    // The first generation freezes the 50 newest roots by recorded activity; children
    // (all newer than every root) and forks never reorder or hide a root.
    let first = roster_with(&worker, "the frozen first generation", |s| !s.provisional);
    assert_eq!(first.generation, 1);
    assert_eq!(first.roots.as_ref(), &expected[..50]);
    assert!(first.has_more);

    // A stale generation reveals nothing; the current one reveals exactly 50 more.
    for generation in [9, 1] {
        worker
            .tx
            .send(WorkerRequest::LoadRosterMore { generation })
            .unwrap();
    }
    let hundred = roster_with(&worker, "100 roots", |s| s.roots.len() >= 100);
    assert_eq!(hundred.roots.as_ref(), &expected[..100]);
    worker
        .tx
        .send(WorkerRequest::LoadRosterMore { generation: 1 })
        .unwrap();
    let all = roster_with(&worker, "125 roots", |s| s.roots.len() == ROOT_COUNT);
    assert_eq!(all.roots.as_ref(), expected.as_slice());
    assert!(!all.has_more);
    for (child, _) in &g.children {
        assert!(!all.roots.contains(child), "a child is never a root");
    }
    for (fork, _) in &g.forks {
        assert!(all.roots.contains(fork), "a fork is its own root");
    }

    // Children load under their immediate parent, one family level at a time.
    let mut id = 100;
    let mut parents: Vec<&SessionKey> = g.children.iter().map(|(_, p)| p).collect();
    parents.dedup();
    for parent in parents {
        id += 1;
        worker
            .tx
            .send(WorkerRequest::LoadChildren {
                request_id: id,
                parent: parent.clone(),
                cursor: None,
            })
            .unwrap();
        let WorkerEvent::Children { metadata, next, .. } = wait_for_event(
            &worker,
            "children",
            |e| matches!(e, WorkerEvent::Children { request_id, .. } if *request_id == id),
        ) else {
            unreachable!()
        };
        let mut got: Vec<&SessionKey> = metadata.iter().map(|m| &m.key).collect();
        let mut want: Vec<&SessionKey> = g
            .children
            .iter()
            .filter(|(_, p)| p == parent)
            .map(|(c, _)| c)
            .collect();
        got.sort_by(|a, b| agentscope::roster::cmp_keys(a, b));
        want.sort_by(|a, b| agentscope::roster::cmp_keys(a, b));
        assert_eq!(got, want, "children of {:?}", parent.native_id);
        assert!(next.is_none());
    }

    // Direct usage: every member reports only what it owns, children included.
    for (key, tokens) in &g.direct_tokens {
        id += 1;
        let summary = hydrate(&worker, id, key);
        assert_eq!(
            summary.usage.as_ref().and_then(|u| u.total),
            Some(*tokens),
            "{:?} {}",
            key.harness,
            key.native_id
        );
    }
    for (key, usd) in &g.reported_cost {
        id += 1;
        let summary = hydrate(&worker, id, key);
        assert_eq!(summary.usage.as_ref().unwrap().cost.usd, Some(*usd));
    }

    // The newest entry of each family parent is the last transcript event.
    for (key, tail) in &g.tails {
        id += 1;
        worker
            .tx
            .send(WorkerRequest::Select {
                request_id: id,
                key: key.clone(),
            })
            .unwrap();
        let WorkerEvent::Transcript { page, .. } = wait_for_event(
            &worker,
            "a transcript page",
            |e| matches!(e, WorkerEvent::Transcript { request_id, .. } if *request_id == id),
        ) else {
            unreachable!()
        };
        let last = page
            .events
            .last()
            .and_then(|e| e.text.as_ref())
            .map(|t| t.preview.as_str());
        assert_eq!(last, Some(tail.as_str()), "{:?} tail entry", key.harness);
    }
    assert!(worker.shutdown());
}

// ---- AC-12: partial, truncated, damaged and locked sources hide nothing -----------

fn opencode_members(g: &Grown) -> usize {
    g.roots
        .iter()
        .map(|(k, _)| k)
        .chain(g.children.iter().map(|(k, _)| k))
        .filter(|k| k.harness == Harness::OpenCode)
        .count()
}

#[test]
fn ac12_native_world_partial_truncated_damaged_and_locked_sources_hide_nothing() {
    let world = World::native();
    let g = world.grown();
    let mut readers = world.readers();
    let mut surface = Surface::default();
    support::world::surface_all(&mut readers, &mut surface);
    let summary = |key: &SessionKey| {
        surface
            .sessions
            .iter()
            .find(|s| &s.meta.key == key)
            .unwrap_or_else(|| panic!("{} not listed", key.native_id))
    };
    // Every root and child is listed despite the damaged neighbours.
    assert_eq!(surface.sessions.len(), ROOT_COUNT + g.children.len());

    // The truncated archive keeps its readable prefix and reports its damage once.
    assert_eq!(
        summary(&g.truncated_zstd)
            .usage
            .as_ref()
            .and_then(|u| u.total),
        Some(100)
    );
    let damaged = |needle: &str| {
        surface
            .warnings
            .iter()
            .filter(|w| w.key.source.contains(needle))
            .count()
    };
    assert_eq!(
        damaged("rollout-cdx-cut.jsonl.zst#"),
        1,
        "the truncated archive is reported once, as a source-scoped diagnostic"
    );
    assert_eq!(damaged("claude-damaged.jsonl#malformed"), 1);
    assert_eq!(damaged("rollout-cdx-zstd"), 0, "an intact archive is clean");
    assert_eq!(
        damaged("rollout-cdx-partial"),
        0,
        "a partial tail is not damage"
    );

    // Malformed middle line: its neighbours are both read; the truncated last record is
    // not an event.
    let claude_page = page(readers[1].as_mut(), &g.damaged_claude);
    assert_eq!(
        texts(&claude_page),
        ["before the damage", "after the damage"]
    );

    // A partial trailing Codex record is not an event until it is complete.
    let before = texts(&page(readers[0].as_mut(), &g.partial_codex));
    assert_eq!(before, ["Partial trailing record"]);
    let path = dated(&world.codex_root, "rollout-cdx-partial.jsonl");
    append(
        &path,
        "load\":{\"type\":\"message\",\"role\":\"user\",\"content\":[{\"type\":\"input_text\",\"text\":\"completed record\"}]}}\n",
    );
    let rescanned = scan(readers[0].as_mut());
    assert!(rescanned.warnings.is_empty());
    let after = texts(&page(readers[0].as_mut(), &g.partial_codex));
    assert_eq!(after, ["Partial trailing record", "completed record"]);

    // A locked WAL database makes OpenCode unavailable and nothing else.
    let mut locked = world.locked_readers();
    let opencode = locked.pop().unwrap();
    drop(opencode);
    let mut found = Surface::default();
    support::world::surface_all(&mut locked, &mut found);
    assert_eq!(
        found.sessions.len(),
        ROOT_COUNT + g.children.len() - opencode_members(g)
    );
}

// ---- Real-data smoke (manual; ignored by default) -------------------------------

/// Reads the real local provider stores through the adapters and prints counts only
/// (never titles, paths, or transcript text). Verifies that no provider file shrank
/// or disappeared and that no file other than live appends and SQLite sidecars
/// changed. Run: `cargo test --test acceptance -- --ignored --nocapture real_data_smoke`.
#[test]
#[ignore = "reads the developer's real provider stores"]
fn real_data_smoke_is_read_only_and_prints_counts_only() {
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    use agentscope::config::{CliOverrides, Config};
    use agentscope::providers::claude::ClaudeScanner;
    use agentscope::providers::opencode::OpenCodeScanner;

    let config = Config::load(&CliOverrides::default()).expect("default config");
    let roots: Vec<PathBuf> = [
        config.harnesses.codex.root.clone(),
        config.harnesses.claude.root.clone(),
        config
            .harnesses
            .opencode
            .db
            .as_ref()
            .and_then(|d| d.parent().map(PathBuf::from)),
    ]
    .into_iter()
    .flatten()
    .collect();

    /// Full path inventory (len, mtime, content hash). Hashing is streamed.
    fn inventory(roots: &[PathBuf]) -> BTreeMap<PathBuf, (u64, std::time::SystemTime, u64)> {
        use std::hash::Hasher;
        use std::io::Read;

        fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
            let Ok(rd) = fs::read_dir(dir) else { return };
            for e in rd.flatten() {
                let p = e.path();
                match e.metadata() {
                    Ok(m) if m.is_dir() => walk(&p, out),
                    Ok(_) => out.push(p),
                    Err(_) => {}
                }
            }
        }
        let mut paths = Vec::new();
        for r in roots {
            if r.file_name().is_some_and(|n| n == "opencode") {
                // Database directory: only the database and its sidecars.
                for name in ["opencode.db", "opencode.db-wal", "opencode.db-shm"] {
                    let p = r.join(name);
                    if p.exists() {
                        paths.push(p);
                    }
                }
            } else {
                walk(r, &mut paths);
            }
        }
        let mut out = BTreeMap::new();
        for p in paths {
            let Ok(mut f) = fs::File::open(&p) else {
                continue;
            };
            let Ok(m) = f.metadata() else { continue };
            let mut h = std::collections::hash_map::DefaultHasher::new();
            let mut buf = vec![0u8; 1 << 20];
            while let Ok(n) = f.read(&mut buf) {
                if n == 0 {
                    break;
                }
                h.write(&buf[..n]);
            }
            out.insert(
                p,
                (
                    m.len(),
                    m.modified().unwrap_or(std::time::UNIX_EPOCH),
                    h.finish(),
                ),
            );
        }
        out
    }
    let before = inventory(&roots);

    let mut readers: Vec<Box<dyn ProviderReader>> = Vec::new();
    if let Ok(r) = CodexScanner::new(&config) {
        readers.push(Box::new(r));
    }
    if let Ok(r) = ClaudeScanner::new(&config) {
        readers.push(Box::new(r));
    }
    if let Ok(r) = OpenCodeScanner::new(&config) {
        readers.push(Box::new(r));
    }
    let mut surface = Surface::default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    for r in readers.iter_mut() {
        let mut keys = Vec::new();
        loop {
            match r.discover(now, BIG) {
                Ok(d) => {
                    surface.warnings.extend(d.warnings);
                    keys.extend(d.sessions.into_iter().map(|m| m.key));
                    if d.scan.done {
                        break;
                    }
                }
                Err(e) => {
                    println!(
                        "smoke: {:?} discovery error kind={}",
                        r.harness(),
                        e.to_string().split(':').next().unwrap_or("")
                    );
                    break;
                }
            }
        }
        for key in keys {
            if let Ok(summary) =
                support::world::try_hydrate(r.as_mut(), &key, now, BIG, support::world::STEPS)
            {
                surface.warnings.extend(summary.warnings.clone());
                surface.sessions.push(summary);
            }
        }
    }
    let mut pages_ok = 0usize;
    let mut events = 0usize;
    let mut tools = 0usize;
    let mut unreadable = 0usize;
    let keys: Vec<_> = surface
        .sessions
        .iter()
        .map(|s| s.meta.key.clone())
        .collect();
    for key in keys.iter().take(60) {
        let r = readers
            .iter_mut()
            .find(|r| r.harness() == key.harness)
            .unwrap();
        let result = support::world::try_transcript(
            r.as_mut(),
            key,
            None,
            PageDirection::Backward,
            PAGE,
            support::world::BIG,
            support::world::STEPS,
        );
        match result {
            Ok(p) => {
                pages_ok += 1;
                events += p.events.len();
                tools += p.events.iter().filter(|e| e.tool.is_some()).count();
            }
            Err(e) => {
                unreadable += 1;
                println!("smoke: {:?} page error: {e}", key.harness);
            }
        }
    }
    drop(readers);

    for p in [Harness::Codex, Harness::Claude, Harness::OpenCode] {
        let n = surface
            .sessions
            .iter()
            .filter(|s| s.meta.key.harness == p)
            .count();
        let with_usage = surface
            .sessions
            .iter()
            .filter(|s| s.meta.key.harness == p && s.usage.is_some())
            .count();
        println!("smoke: {p:?} sessions={n} with_usage={with_usage}");
    }
    println!(
        "smoke: warnings={} pages_ok={pages_ok} unreadable={unreadable} events={events} tool_events={tools}",
        surface.warnings.len()
    );

    let after = inventory(&roots);
    // Classification. A silent change (same length and mtime, different bytes) can only
    // be a write by someone who preserved metadata and is a failure. Any other difference
    // (length/mtime moved, path added or removed) is a concurrent live-provider change or an
    // ADR-0002 sidecar and is reported as inconclusive, never attributed to AgentScope.
    let is_sidecar = |p: &PathBuf| {
        let n = p.to_string_lossy();
        n.ends_with("-wal") || n.ends_with("-shm")
    };
    let (mut silent, mut concurrent, mut added, mut removed, mut identical) = (0, 0, 0, 0, 0);
    for (p, (len, mtime, hash)) in &before {
        match after.get(p) {
            None => removed += 1,
            Some((l, m, h)) if l == len && m == mtime && h == hash => identical += 1,
            Some((l, m, _)) if l == len && m == mtime && !is_sidecar(p) => silent += 1,
            Some(_) => concurrent += 1,
        }
    }
    for p in after.keys() {
        if !before.contains_key(p) {
            added += 1;
        }
    }
    println!(
        "smoke: inventory={} identical={identical} silent_content_changes={silent} \
         concurrent_or_sidecar_changes(inconclusive)={concurrent} added(inconclusive)={added} removed(inconclusive)={removed}",
        before.len()
    );
    assert_eq!(silent, 0, "bytes changed without any metadata change");
}

// ---- OMP: the real reader, worker, App and renderer ----------------------------------

#[test]
fn omp_family_opens_through_the_real_worker_app_and_renderer() {
    use support::omp as om;

    let world = World::with_omp();
    let plant = world.omp_plant.as_ref().expect("planted OMP family");
    let (parent, child, nested) = (
        plant.family.parent.clone(),
        plant.family.child.clone(),
        plant.family.nested.clone(),
    );
    let mut h = Driver::start(&world);
    // Three native sessions plus the OMP parent, its fork and the secret-bearing root.
    h.pump_until("roster", |a| a.total_sessions() == 6);
    assert!(h.screen().contains("OMP"), "{}", h.screen());

    // Expand the family one level at a time: child, then grandchild.
    assert!(h.app.select_row(&parent));
    h.ch(' ');
    h.pump_until("children", |a| {
        a.children_of(&parent)
            .is_some_and(|c| !c.metadata.is_empty())
    });
    // A family level opens through its own request (the roster toggles whole families).
    assert!(h.app.load_children(&child));
    h.pump_until("grandchildren", |a| {
        a.children_of(&child)
            .is_some_and(|c| !c.metadata.is_empty())
    });
    assert!(
        h.app
            .children_of(&child)
            .is_some_and(|c| c.metadata.iter().any(|m| m.key == nested))
    );

    // The parent's preview facts: direct tokens only, the quoted child cost is not added.
    assert!(h.app.select_row(&parent));
    h.pump_until("parent facts", |a| {
        a.session(&parent).is_some_and(|r| {
            r.session
                .usage
                .as_ref()
                .is_some_and(|u| u.total == Some(1_000))
        })
    });
    let parent_row = h.app.session(&parent).unwrap();
    let summary = parent_row.session.clone();
    assert_eq!(summary.metrics.compactions.observed, 1);
    assert_eq!(
        summary.metrics.compactions.known_cost_usd, None,
        "compaction cost is Unknown"
    );
    assert_eq!(summary.displayed_event_count, 6);

    // Open the parent at its true latest event.
    h.key(KeyCode::Enter);
    assert_eq!(h.app.screen, Screen::Inspector);
    h.pump_until("latest page", |a| {
        a.transcript().is_some_and(|t| t.loaded())
    });
    let last = h
        .app
        .transcript()
        .unwrap()
        .events()
        .last()
        .and_then(|e| e.text.as_ref())
        .map(|t| t.preview.clone());
    assert_eq!(last.as_deref(), Some(support::world::OMP_TAIL));
    let screen = h.screen();
    assert!(screen.contains(support::world::OMP_TAIL), "{screen}");

    // Record-level details: the owned request with its category counters and the
    // compaction marker with its recorded summary and Unknown cost.
    assert!(h.app.request_details());
    h.pump_until("details", |a| a.details().complete());
    let page = h.app.details().page().clone();
    assert_eq!(page.requests.len(), 1);
    assert_eq!(page.requests[0].categories.input, Some(600));
    assert_eq!(page.requests[0].categories.cache_read, Some(100));
    assert_eq!(page.compactions.len(), 1);
    assert!(page.compactions[0].usage.is_none());
    assert_eq!(
        page.compactions[0]
            .summary
            .as_ref()
            .map(|s| s.preview.as_str()),
        Some("synthetic summary")
    );
    // Session info shows the usage facts and the lineage.
    h.ch('i');
    let info = h.screen();
    assert!(info.contains("Tokens: 1,000"), "{info}");
    assert!(info.contains("Provider: OMP"), "{info}");
    assert!(info.contains("Effort: Unknown"), "{info}");

    // A live append arrives through refresh and is counted, then loaded.
    h.key(KeyCode::Esc);
    let path = match &parent.source {
        SourceKey::File(path) => path.clone(),
        other => panic!("{other:?}"),
    };
    om::append(
        &path,
        &[om::user_message("u3", Some("u2"), "omp live append")],
    );
    h.worker.tx.send(WorkerRequest::Refresh).unwrap();
    h.pump_until("new events", |a| a.new_event_count() == 1);
    h.ch('G');
    h.pump_until("appended event", |a| {
        a.transcript().is_some_and(|t| {
            t.events()
                .last()
                .and_then(|e| e.text.as_ref())
                .is_some_and(|t| t.preview == "omp live append")
        })
    });

    // The source view serves the exact record.
    h.ch('v');
    h.pump_until("source", |a| {
        a.source_view()
            .is_some_and(|v| !matches!(v.state, agentscope::app::SourceState::Loading))
    });
    assert!(h.screen().contains("omp live append"), "{}", h.screen());
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    assert!(h.worker.shutdown());
}
