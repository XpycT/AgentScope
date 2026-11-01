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
        Self::start_with(world, world.config())
    }

    fn start_with(world: &World, config: agentscope::config::Config) -> Self {
        let worker = WorkerHandle::spawn(world.readers(), config);
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
    assert!(info.contains("Total: 1,000"), "{info}");
    assert!(info.contains("[OMP]"), "{info}");
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

#[test]
fn ac_hermes_profiles_chains_and_compaction_surface_end_to_end() {
    let world = World::with_hermes();
    let plant = world.hermes_plant.as_ref().expect("hermes plant");
    let mut config = world.config();
    config.refresh_ms = 3_600_000;
    let worker = WorkerHandle::spawn(world.readers(), config);

    // The roster keeps one root for the compression chain and one for the profile
    // database; the delegate is a child of the parent chain.
    let snapshot = roster_with(&worker, "the Hermes roster", |s| {
        s.roots.iter().any(|key| key.native_id == "h-parent")
            && s.roots.iter().any(|key| key.native_id == "h-profile")
    });
    let parent = plant.parent.clone();
    let profile = plant.profile.clone();
    let child = plant.child.clone();
    assert!(snapshot.roots.contains(&parent));
    assert!(snapshot.roots.contains(&profile));
    assert!(!snapshot.roots.contains(&child), "a delegate is a child");
    assert_eq!(parent.native_id, "h-parent");
    assert_ne!(parent.source, profile.source, "profiles stay distinct");

    let summary = hydrate(&worker, 900, &parent);
    assert_eq!(summary.meta.key, parent);
    assert_eq!(
        summary
            .meta
            .members
            .iter()
            .map(|key| key.native_id.as_str())
            .collect::<Vec<_>>(),
        ["h-parent", "h-cont"]
    );
    assert!(summary.metrics.compactions.observed >= 2);
    assert_eq!(
        summary.metrics.compactions.cost_scope,
        CompactionScope::Unknown,
        "no compression ledger row is invented"
    );
    assert!(summary.usage.is_some(), "main usage rows reconcile");

    let mut readers = world.readers();
    let surface = exercise_everything(&mut readers);
    let texts: Vec<String> = surface
        .pages
        .iter()
        .flat_map(|page| page.events.iter())
        .filter_map(|event| event.text.as_ref().map(|text| text.preview.clone()))
        .collect();
    assert!(texts.iter().any(|text| text == &plant.tail));
    assert!(texts.iter().any(|text| text == &plant.summary));
    assert!(!texts.iter().any(|text| text.contains(INLINE_SECRET)));
    assert!(!surface.warnings.iter().any(|warning| {
        warning.message.contains(INLINE_SECRET) || warning.message.contains(SENTINEL)
    }));
    worker.shutdown();
}

#[test]
fn pi_family_opens_through_the_real_worker_app_and_renderer() {
    use support::pi as px;

    let world = World::with_pi();
    let plant = world.pi_plant.as_ref().expect("planted Pi family");
    let (parent, fork, secret) = (
        plant.parent.clone(),
        plant.fork.clone(),
        plant.secret.clone(),
    );
    let mut h = Driver::start(&world);
    // Three native sessions plus the Pi parent, its fork root and the secret-bearing
    // root. Native Pi has no child-session record, so nothing is nested.
    h.pump_until("roster", |a| a.total_sessions() == 6);
    let screen = h.screen();
    assert!(screen.contains("Pi"), "{screen}");

    // The family has no children: a Pi row toggles nothing and invents no subagent.
    assert!(h.app.select_row(&parent));
    h.ch(' ');
    assert!(
        h.app
            .children_of(&parent)
            .is_none_or(|c| c.metadata.is_empty())
    );

    // Direct lifetime usage only: the fork's copied history belongs to its parent.
    h.pump_until("parent facts", |a| {
        a.session(&parent)
            .and_then(|r| r.session.usage.as_ref().map(|u| u.total))
            == Some(Some(1_000))
    });
    let parent_row = h.app.session(&parent).unwrap();
    let summary = parent_row.session.clone();
    assert_eq!(
        summary.meta.model.as_ref().map(|m| m.provider.as_deref()),
        Some(Some("anthropic"))
    );
    assert!(
        summary.metrics.speed.latest.is_none(),
        "no recorded duration means Unknown speed"
    );
    assert_eq!(summary.metrics.timing.ended_at, None);
    assert_eq!(summary.lifecycle, Lifecycle::Unknown);

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
    assert_eq!(last.as_deref(), Some(support::world::PI_TAIL));
    let screen = h.screen();
    assert!(screen.contains(support::world::PI_TAIL), "{screen}");

    // Record-level details: the owned request's categories, and the model change.
    assert!(h.app.request_details());
    h.pump_until("details", |a| a.details().complete());
    let page = h.app.details().page().clone();
    assert_eq!(page.requests.len(), 1);
    assert_eq!(page.requests[0].categories.input, Some(600));
    assert_eq!(page.requests[0].categories.cache_read, Some(400));
    assert!(page.compactions.is_empty());
    h.ch('i');
    let info = h.screen();
    assert!(info.contains("Total: 1,000"), "{info}");
    assert!(info.contains("[PI]"), "{info}");
    assert!(info.contains("Effort: Unknown"), "{info}");

    // The capability text says extension children are unsupported instead of
    // claiming a verified zero.
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);
    assert_eq!(h.app.screen, Screen::Roster);
    h.ch('?');
    assert_eq!(h.app.panel(), agentscope::app::Panel::Help);
    let help = h.screen();
    assert!(
        help.contains("Pi extension subagents: unsupported in this release"),
        "{help}"
    );

    // A live append arrives through refresh, is counted once and is loaded.
    h.key(KeyCode::Esc);
    assert!(h.app.select_row(&parent));
    h.key(KeyCode::Enter);
    h.pump_until("reopened transcript", |a| {
        a.transcript().is_some_and(|t| t.loaded())
    });
    let path = match &parent.source {
        SourceKey::File(path) => path.clone(),
        other => panic!("{other:?}"),
    };
    px::append(
        &path,
        &[px::message(
            "pu2",
            Some("pa1"),
            "user",
            "Pi live append",
            None,
        )],
    );
    h.worker.tx.send(WorkerRequest::Refresh).unwrap();
    h.pump_until("new events", |a| a.new_event_count() == 1);
    h.ch('G');
    h.pump_until("appended event", |a| {
        a.transcript().is_some_and(|t| {
            t.events()
                .last()
                .and_then(|e| e.text.as_ref())
                .is_some_and(|t| t.preview == "Pi live append")
        })
    });

    // The source view serves the exact record.
    h.ch('v');
    h.pump_until("source", |a| {
        a.source_view()
            .is_some_and(|v| !matches!(v.state, agentscope::app::SourceState::Loading))
    });
    assert!(h.screen().contains("Pi live append"), "{}", h.screen());
    h.key(KeyCode::Esc);
    h.key(KeyCode::Esc);

    // The fork lists only its own request; the copied parent call is not charged.
    assert!(h.app.select_row(&fork));
    h.pump_until("fork facts", |a| {
        a.session(&fork)
            .and_then(|r| r.session.usage.as_ref().map(|u| u.total))
            == Some(Some(15))
    });
    // The secret-bearing root is listed, and its title is masked.
    let masked = h.app.session(&secret).unwrap().session.meta.title.clone();
    assert!(masked.contains("[REDACTED]"), "{masked}");
    assert!(h.worker.shutdown());
}

// ---- R1: every harness in one roster ----------------------------------------------

/// Discovery-and-hydrate one family's children through the real worker.
fn children_of(
    worker: &WorkerHandle,
    request_id: u64,
    parent: &SessionKey,
    cursor: Option<SessionKey>,
) -> (
    Vec<std::sync::Arc<agentscope::domain::SessionMetadata>>,
    Option<SessionKey>,
) {
    worker
        .tx
        .send(WorkerRequest::LoadChildren {
            request_id,
            parent: parent.clone(),
            cursor,
        })
        .unwrap();
    let WorkerEvent::Children { metadata, next, .. } = wait_for_event(
        worker,
        "a child page",
        |e| matches!(e, WorkerEvent::Children { request_id: id, .. } if *id == request_id),
    ) else {
        unreachable!()
    };
    (metadata, next)
}

fn keys_of(metadata: &[std::sync::Arc<agentscope::domain::SessionMetadata>]) -> Vec<SessionKey> {
    metadata.iter().map(|m| m.key.clone()).collect()
}

#[test]
fn six_harness_roster_preserves_direct_usage_and_generation() {
    let world = World::with_all_harnesses();
    let g = world.grown();
    let hermes = world.hermes_plant.as_ref().expect("the Hermes home");
    let mut config = world.config();
    config.refresh_ms = 3_600_000;
    let worker = WorkerHandle::spawn(world.readers(), config);

    // Explicit, distinct activity stamps make the expected root order unambiguous.
    let expected = g.expected_roots();
    assert_eq!(expected.len(), ROOT_COUNT);
    let mut stamps: Vec<i64> = g.roots.iter().map(|(_, at)| *at).collect();
    stamps.sort_unstable();
    stamps.dedup();
    assert_eq!(stamps.len(), ROOT_COUNT, "every root has its own stamp");

    // All six harnesses own roots. Five native child families exist; Pi records no
    // child session at all.
    for harness in [
        Harness::Codex,
        Harness::Claude,
        Harness::OpenCode,
        Harness::Omp,
        Harness::Hermes,
        Harness::Pi,
    ] {
        assert!(
            expected.iter().any(|key| key.harness == harness),
            "{harness:?} owns a root"
        );
    }
    let child_of = |child: &SessionKey| {
        g.children
            .iter()
            .find(|(c, _)| c == child)
            .map(|(_, p)| p.clone())
    };
    for family in [&g.codex, &g.claude, &g.opencode] {
        assert_eq!(child_of(&family.child), Some(family.parent.clone()));
        assert_eq!(child_of(&family.nested), Some(family.child.clone()));
    }
    assert_eq!(
        child_of(&g.omp.family.child),
        Some(g.omp.family.parent.clone())
    );
    assert_eq!(
        child_of(&g.omp.family.nested),
        Some(g.omp.family.child.clone())
    );
    assert_eq!(child_of(&hermes.child), Some(hermes.parent.clone()));
    assert_eq!(child_of(&g.desktop_child), Some(g.desktop_code.clone()));
    assert_eq!(child_of(&g.pi.parent), None, "native Pi has no children");
    assert_eq!(child_of(&g.pi.fork), None);

    // Pages 50 -> 100 -> 125, exact and duplicate-free; a stale generation reveals
    // nothing and children/continuations never become extra root slots.
    let first = roster_with(&worker, "the frozen first generation", |s| !s.provisional);
    assert_eq!(first.generation, 1);
    assert_eq!(first.roots.as_ref(), &expected[..50]);
    assert!(first.has_more);

    worker
        .tx
        .send(WorkerRequest::LoadRosterMore { generation: 9 })
        .unwrap();
    let until = Instant::now() + Duration::from_millis(400);
    while Instant::now() < until {
        if let Ok(WorkerEvent::Roster { snapshot, .. }) =
            worker.rx.recv_timeout(Duration::from_millis(20))
        {
            assert!(
                snapshot.roots.len() <= 50,
                "a stale generation revealed more roots"
            );
        }
    }
    worker
        .tx
        .send(WorkerRequest::LoadRosterMore { generation: 1 })
        .unwrap();
    let hundred = roster_with(&worker, "100 roots", |s| s.roots.len() >= 100);
    assert_eq!(hundred.roots.as_ref(), &expected[..100]);
    worker
        .tx
        .send(WorkerRequest::LoadRosterMore { generation: 1 })
        .unwrap();
    let all = roster_with(&worker, "125 roots", |s| s.roots.len() == ROOT_COUNT);
    assert_eq!(all.roots.as_ref(), expected.as_slice());
    assert!(!all.has_more);
    let distinct: std::collections::HashSet<&SessionKey> = all.roots.iter().collect();
    assert_eq!(distinct.len(), ROOT_COUNT, "no root is listed twice");

    let mut id = 100;
    for (child, _) in &g.children {
        assert!(!all.roots.contains(child), "a child is never a root slot");
    }
    for (fork, _) in &g.forks {
        assert!(all.roots.contains(fork), "a fork is its own root");
    }
    assert!(
        !all.roots.contains(&hermes.continuation),
        "a compression continuation is a member, not a slot"
    );

    // Children load under their immediate parent, one family level at a time.
    let mut parents: Vec<&SessionKey> = g.children.iter().map(|(_, p)| p).collect();
    parents.dedup();
    for parent in parents {
        id += 1;
        let (metadata, next) = children_of(&worker, id, parent, None);
        let mut got = keys_of(&metadata);
        let mut want: Vec<SessionKey> = g
            .children
            .iter()
            .filter(|(_, p)| p == parent)
            .map(|(c, _)| c.clone())
            .collect();
        got.sort_by(agentscope::roster::cmp_keys);
        want.sort_by(agentscope::roster::cmp_keys);
        assert_eq!(got, want, "children of {}", parent.native_id);
        assert!(next.is_none());
    }
    // A fresh publication after every family was expanded still holds exactly the
    // frozen 125 roots: children never consume or duplicate a root slot.
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    let refreshed = roster_with(&worker, "the refreshed roster", |s| {
        s.generation == 2 && !s.provisional && s.roots.len() == ROOT_COUNT
    });
    assert_eq!(refreshed.roots.as_ref(), expected.as_slice());
    let fresh: std::collections::HashSet<&SessionKey> = refreshed.roots.iter().collect();
    assert_eq!(fresh.len(), ROOT_COUNT, "no root is listed twice");
    for (child, _) in &g.children {
        assert!(
            !refreshed.roots.contains(child),
            "a child never takes a root slot"
        );
    }
    assert!(
        !refreshed.roots.contains(&hermes.continuation),
        "a continuation never takes a root slot"
    );
    // The Pi root still has no family.
    id += 1;
    let (pi_children, _) = children_of(&worker, id, &g.pi.parent, None);
    assert!(pi_children.is_empty(), "Pi lists no children");

    // Direct usage: every member reports only what it owns.
    let mut hydrated: std::collections::HashMap<SessionKey, std::sync::Arc<SessionSummary>> =
        std::collections::HashMap::new();
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
        hydrated.insert(key.clone(), summary);
    }
    for (key, usd) in &g.reported_cost {
        id += 1;
        let summary = hydrate(&worker, id, key);
        assert_eq!(summary.usage.as_ref().unwrap().cost.usd, Some(*usd));
        hydrated.insert(key.clone(), summary);
    }

    // Direct dollar ownership beyond OpenCode: catalog estimates only where the
    // bundled/saved evidence prices every used leg, Unknown everywhere else. The
    // OMP parent's untrusted saved 99.0 and its child's quoted $9.99 are never money.
    let expect_cost = |key: &SessionKey,
                       usd: Option<f64>,
                       class: AggregationClass,
                       source: Option<MoneySource>| {
        let summary = hydrated
            .get(key)
            .unwrap_or_else(|| panic!("{} not hydrated", key.native_id));
        let usage = summary.usage.as_ref().expect("usage");
        match usd {
            Some(expected) => {
                let actual = usage.cost.usd.expect("a complete estimate");
                assert!(
                    (actual - expected).abs() < 1e-9,
                    "{} cost {actual} != {expected}",
                    key.native_id
                );
            }
            None => assert_eq!(usage.cost.usd, None, "{} must stay Unknown", key.native_id),
        }
        assert_eq!(usage.cost.class, class, "{}", key.native_id);
        assert_eq!(usage.cost.source, source, "{}", key.native_id);
    };
    // Claude: bundled reference estimates for the priced family members.
    expect_cost(
        &g.claude.parent,
        Some(0.0014),
        AggregationClass::Estimated,
        Some(MoneySource::BundledReference),
    );
    expect_cost(
        &g.claude.child,
        Some(0.0007),
        AggregationClass::Estimated,
        Some(MoneySource::BundledReference),
    );
    // Hermes: the source's saved estimates, split per logical session.
    expect_cost(
        &hermes.parent,
        Some(0.02),
        AggregationClass::Estimated,
        Some(MoneySource::SavedCatalogEstimate),
    );
    expect_cost(
        &hermes.child,
        Some(0.01),
        AggregationClass::Estimated,
        Some(MoneySource::SavedCatalogEstimate),
    );
    expect_cost(
        &hermes.profile,
        Some(0.005),
        AggregationClass::Estimated,
        Some(MoneySource::SavedCatalogEstimate),
    );
    assert_eq!(
        hydrated[&hermes.parent]
            .usage
            .as_ref()
            .and_then(|u| u.saved_estimate_usd),
        Some(0.02)
    );
    // Codex, OMP, Pi, unknown-route OpenCode and the unpriced Claude Desktop/Cowork
    // sessions report no complete dollar amount.
    for key in [
        &g.codex.parent,
        &g.codex.child,
        &g.zstd,
        &g.desktop_code,
        &g.cowork,
        &g.omp.family.parent,
        &g.omp.family.child,
        &g.omp.fork,
        &g.unknown_billing,
        &g.pi.parent,
        &g.pi.fork,
    ] {
        expect_cost(key, None, AggregationClass::Unknown, None);
    }
    // The OMP parent keeps its untrusted saved figure out of money, and the child's
    // quoted task-result cost is not added to the parent.
    let omp_parent = hydrated[&g.omp.family.parent].usage.as_ref().unwrap();
    assert_eq!(omp_parent.saved_estimate_usd, None);
    assert_eq!(omp_parent.reported_cost_usd, None);
    assert_ne!(omp_parent.cost.usd, Some(99.0));
    assert_ne!(omp_parent.cost.usd, Some(9.99));

    // The Hermes chain is one logical root; its members and the delegate stay out of
    // the root slots, and the archived Codex rollout is a native root.
    id += 1;
    let chain = hydrate(&worker, id, &hermes.parent);
    assert_eq!(
        chain
            .meta
            .members
            .iter()
            .map(|m| m.native_id.as_str())
            .collect::<Vec<_>>(),
        ["h-parent", "h-cont"]
    );
    assert_eq!(chain.usage.as_ref().and_then(|u| u.total), Some(240));
    assert!(all.roots.contains(&g.zstd), "the zstd rollout is a root");
    id += 1;
    worker
        .tx
        .send(WorkerRequest::Select {
            request_id: id,
            key: g.zstd.clone(),
        })
        .unwrap();
    let WorkerEvent::Transcript { page, .. } = wait_for_event(
        &worker,
        "the archived transcript",
        |e| matches!(e, WorkerEvent::Transcript { request_id, .. } if *request_id == id),
    ) else {
        unreachable!()
    };
    assert!(
        texts(&page).iter().any(|t| t == "Archived prompt"),
        "the archived rollout keeps its readable records"
    );

    // Selecting each parent and each child reaches the actual newest transcript event.
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
        assert_eq!(
            last,
            Some(tail.as_str()),
            "{:?} {}",
            key.harness,
            key.native_id
        );
    }
    assert!(worker.shutdown());

    // A broken source and privacy sentinels coexist with every healthy root.
    let mut readers = world.readers();
    let mut surface = Surface::default();
    surface_all(&mut readers, &mut surface);
    assert_eq!(surface.sessions.len(), ROOT_COUNT + g.children.len());
    assert!(
        surface
            .warnings
            .iter()
            .any(|w| w.key.source.contains("rollout-cdx-cut.jsonl.zst#")),
        "the truncated archive is reported beside healthy roots"
    );
    for forbidden in g.forbidden_texts() {
        assert!(
            !surface
                .warnings
                .iter()
                .any(|w| w.message.contains(forbidden)),
            "a warning leaked a secret"
        );
        for summary in &surface.sessions {
            assert!(
                !summary.meta.title.contains(forbidden),
                "a title leaked a secret"
            );
        }
    }
    assert!(surface.sessions.iter().any(|s| s.meta.key == g.zstd));
}

#[test]
fn six_harness_refresh_keeps_selection_and_truthful_unknowns() {
    use support::facts::file_key;

    let world = World::with_all_harnesses();
    let g = world.grown();
    let hermes = world.hermes_plant.as_ref().expect("the Hermes home");
    let mut config = world.config();
    config.refresh_ms = 3_600_000;
    let mut h = Driver::start_with(&world, config);

    // The first page freezes while newer sources are on disk but not yet admitted.
    h.pump_until("the frozen first page", |a| {
        a.roster_status().generation == 1
            && !a.roster_status().provisional
            && a.all_rows().len() == 50
    });
    assert_eq!(
        h.app
            .all_rows()
            .iter()
            .map(|r| r.session.meta.key.clone())
            .collect::<Vec<_>>(),
        g.expected_roots()[..50]
    );

    // A timed OMP child is the selection under a preview that a refresh will obsolete.
    let omp_parent = g.omp.family.parent.clone();
    let omp_child = g.omp.family.child.clone();
    assert!(h.app.select_row(&omp_parent));
    h.ch(' ');
    h.pump_until("the OMP children", |a| {
        a.children_of(&omp_parent)
            .is_some_and(|c| c.metadata.iter().any(|m| m.key == omp_child))
    });
    assert!(h.app.select_row(&omp_child));
    h.pump_until("the timed child preview", |a| {
        a.preview()
            .summary()
            .is_some_and(|s| s.meta.key == omp_child && s.metrics.speed.latest.is_some())
    });
    // A replacement preview request is genuinely outstanding at the generation
    // change: send it to the worker now and withhold its answer.
    assert!(h.app.retry_preview());
    let pending_preview = h
        .app
        .preview()
        .request_id()
        .expect("the replacement request");
    flush_requests(&mut h.app, &h.worker);
    let mut late_previews: Vec<WorkerEvent> = Vec::new();
    let mut terminal_answer = false;

    // Newer source data and a child whose parent does not exist yet land on disk.
    let newest_path = dated(&world.codex_root, "rollout-six-newest.jsonl");
    write_lines(
        &newest_path,
        &[
            cx::session_meta(&iso(NOW + 60_000), "six-newest", "/work/native"),
            cx::user(&iso(NOW + 60_000), "newest root prompt"),
        ],
    );
    let kid_path = dated(&world.codex_root, "rollout-six-late-kid.jsonl");
    write_lines(
        &kid_path,
        &[
            cx::child_session_meta(
                &iso(NOW + 30_000),
                "six-late-kid",
                "six-late-kid",
                "six-late-parent",
                None,
                "0.159.2",
            ),
            cx::turn_context(&iso(NOW + 30_000), "gpt-5.5", "/work/native"),
        ],
    );
    // Every answer for the replacement request is withheld, including its terminal
    // `done: true`, and no new source is admitted before the explicit refresh.
    let answer_by = Instant::now() + Duration::from_secs(20);
    while !terminal_answer {
        assert!(Instant::now() < answer_by, "the terminal preview answer");
        if let Ok(ev) = h.worker.rx.recv_timeout(Duration::from_millis(25)) {
            if matches!(
                &ev,
                WorkerEvent::Preview { request_id, key, .. }
                    if *request_id == pending_preview && key == &omp_child
            ) {
                terminal_answer = matches!(&ev, WorkerEvent::Preview { done: true, .. });
                late_previews.push(ev);
            } else {
                apply_worker_event(&mut h.app, &mut h.ctx, ev);
            }
        }
    }
    // The replacement request is genuinely outstanding immediately before the
    // generation change: the worker answered it, the App has not applied anything.
    assert_eq!(h.app.roster_status().generation, 1, "no refresh yet");
    assert_eq!(
        h.app.preview().request_id(),
        Some(pending_preview),
        "the replacement request is still current"
    );
    assert!(h.app.preview().is_loading(), "its answer is still withheld");
    assert_eq!(h.app.all_rows().len(), 50, "pages stay frozen");
    assert!(
        !h.app
            .all_rows()
            .iter()
            .any(|r| r.session.meta.key.native_id == "six-newest"),
        "a new source waits for the explicit refresh"
    );

    // The explicit refresh is a new generation: the newest root appears, the orphan
    // child is still its own root, and the withheld answer cannot own the new state.
    h.worker.tx.send(WorkerRequest::Refresh).unwrap();
    let generation_by = Instant::now() + Duration::from_secs(20);
    loop {
        if h.app.roster_status().generation == 2
            && h.app
                .all_rows()
                .iter()
                .any(|r| r.session.meta.key.native_id == "six-newest")
            && h.app
                .all_rows()
                .iter()
                .any(|r| r.session.meta.key.native_id == "six-late-kid")
        {
            break;
        }
        assert!(Instant::now() < generation_by, "generation 2");
        if let Ok(ev) = h.worker.rx.recv_timeout(Duration::from_millis(25)) {
            if matches!(
                &ev,
                WorkerEvent::Preview { request_id, key, .. }
                    if *request_id == pending_preview && key == &omp_child
            ) {
                late_previews.push(ev);
            } else {
                apply_worker_event(&mut h.app, &mut h.ctx, ev);
            }
        }
    }
    assert_eq!(h.app.all_rows().len(), 50, "the revealed capacity is kept");
    assert_eq!(
        h.app.all_rows()[0].session.meta.key.native_id,
        "six-newest",
        "the newest root is first"
    );
    // The new generation answers its own preview request; its facts are the ones
    // the delayed terminal answer must not replace.
    h.pump_until("the refreshed preview", |a| a.preview().summary().is_some());
    let selected = h.app.selected_key().expect("a selection survives");
    let request = h.app.preview().request_id().expect("a current request");
    assert_ne!(
        request, pending_preview,
        "the generation change released the old request"
    );
    let facts = h
        .app
        .preview()
        .summary()
        .map(|s| std::sync::Arc::as_ptr(s) as usize);
    assert!(facts.is_some(), "current preview facts exist");
    assert!(
        !late_previews.is_empty(),
        "at least one withheld answer is delivered late"
    );
    for ev in late_previews {
        apply_worker_event(&mut h.app, &mut h.ctx, ev);
    }
    assert_eq!(h.app.selected_key(), Some(selected), "selection is kept");
    assert_eq!(
        h.app.preview().request_id(),
        Some(request),
        "the late preview answers are ignored"
    );
    assert_eq!(
        h.app
            .preview()
            .summary()
            .map(|s| std::sync::Arc::as_ptr(s) as usize),
        facts,
        "current preview facts are not replaced"
    );

    // The late parent arrives: the next generation reparents the child instead of
    // leaving it as a duplicate root.
    let parent_path = dated(&world.codex_root, "rollout-six-late-parent.jsonl");
    write_lines(
        &parent_path,
        &[
            cx::session_meta(&iso(NOW + 40_000), "six-late-parent", "/work/native"),
            cx::user(&iso(NOW + 40_000), "late parent prompt"),
        ],
    );
    let late_parent = file_key(Harness::Codex, "six-late-parent", &parent_path);
    let late_kid = file_key(Harness::Codex, "six-late-kid", &kid_path);
    h.worker.tx.send(WorkerRequest::Refresh).unwrap();
    h.pump_until("generation 3", |a| {
        a.roster_status().generation == 3
            && a.all_rows()
                .iter()
                .any(|r| r.session.meta.key == late_parent)
            && a.all_rows().iter().all(|r| r.session.meta.key != late_kid)
    });
    assert!(h.app.select_row(&late_parent));
    h.ch(' ');
    h.pump_until("the late child", |a| {
        a.children_of(&late_parent)
            .is_some_and(|c| c.metadata.iter().any(|m| m.key == late_kid))
    });
    assert_eq!(h.app.all_rows().len(), 50, "a child never adds a root slot");

    // Search reads only loaded roots: an unloaded root is not invented.
    let unloaded = g.expected_roots()[60].clone();
    h.app.set_query(&unloaded.native_id);
    assert!(
        h.app.visible_rows().is_empty(),
        "an unloaded root cannot match a loaded-only search"
    );
    h.app.set_query("");

    // A child-only match keeps the parent context on the roster. The generation
    // change dropped loaded families, so the OMP family is opened again first.
    assert!(h.app.select_row(&omp_parent));
    h.ch(' ');
    h.pump_until("the OMP children again", |a| {
        a.children_of(&omp_parent)
            .is_some_and(|c| c.metadata.iter().any(|m| m.key == omp_child))
    });
    h.app.set_query("omp-kid");
    let visible: Vec<SessionKey> = h
        .app
        .visible_rows()
        .iter()
        .map(|r| r.session.meta.key.clone())
        .collect();
    assert!(
        visible.contains(&omp_parent),
        "a child match keeps its parent row"
    );
    assert!(!visible.contains(&omp_child), "children are not root rows");
    let shown: Vec<(SessionKey, u8)> = h
        .app
        .roster_rows()
        .filter_map(|item| match item {
            agentscope::app::RosterItem::Session(row) => {
                Some((row.session.meta.key.clone(), row.depth))
            }
            _ => None,
        })
        .collect();
    assert!(
        shown
            .iter()
            .any(|(key, depth)| key == &omp_child && *depth == 1),
        "the matched child is shown under its parent"
    );
    h.app.set_query("");

    // Manual historical reading is not jumped by a refresh, and the older anchor
    // still resumes from where the reader was. Reveal the rest of the frozen
    // generation first so the long archived rollout is loaded.
    while h.app.roster_status().has_more {
        let loaded = h.app.all_rows().len();
        assert!(h.app.load_more_roots());
        h.pump_until("a longer frozen page", |a| a.all_rows().len() > loaded);
    }
    let archived = g.truncated_zstd.clone();
    assert!(h.app.select_row(&archived), "the archived root is loaded");
    h.key(KeyCode::Enter);
    h.pump_until("the archived transcript", |a| {
        a.transcript().is_some_and(|t| t.loaded())
    });
    h.ch('k');
    let reading = h
        .app
        .transcript()
        .unwrap()
        .selected_event()
        .unwrap()
        .id
        .clone();
    let loaded_before = h.app.transcript().unwrap().events().len();
    assert!(h.app.transcript().unwrap().has_older());
    h.worker.tx.send(WorkerRequest::Refresh).unwrap();
    h.pump_until("generation 4", |a| a.roster_status().generation == 4);
    assert_eq!(
        h.app.transcript().unwrap().selected_event().unwrap().id,
        reading,
        "the reader is not moved"
    );
    assert_eq!(h.app.transcript().unwrap().events().len(), loaded_before);
    assert!(h.app.transcript().unwrap().has_older());
    let boundary = h.app.transcript().unwrap().events()[0].id.clone();
    h.ch('g');
    h.pump_until("the older page", |a| {
        a.transcript()
            .is_some_and(|t| t.events().len() > loaded_before)
    });
    let resumed = h.app.transcript().unwrap();
    assert_eq!(
        resumed.selected_event().unwrap().id,
        boundary,
        "focus stays on the boundary event the older page was resumed from"
    );
    assert_ne!(
        resumed.events()[0].id,
        boundary,
        "older events were prepended"
    );
    assert!(
        !resumed.has_older(),
        "the older page reached the readable start of the truncated archive"
    );

    // Aggregate rows and missing timing stay Unknown; they generate no request speed.
    let chain = hydrate(&h.worker, 901, &hermes.parent);
    assert!(chain.usage.is_some(), "recorded aggregate rows reconcile");
    assert!(chain.metrics.speed.latest.is_none(), "no aggregate speed");
    assert!(chain.metrics.speed.weighted_tokens_per_second.is_none());
    assert_eq!(chain.metrics.speed.total_requests, 0);
    assert_eq!(
        chain.usage.as_ref().unwrap().cost.usd,
        Some(0.02),
        "the chain's estimated rows reconcile exactly"
    );
    let pi = hydrate(&h.worker, 902, &g.pi.parent);
    assert!(pi.usage.is_some());
    assert!(pi.metrics.speed.latest.is_none(), "no recorded duration");
    assert_eq!(pi.metrics.speed.measured_requests, 0);
    assert_eq!(pi.metrics.speed.total_requests, 1, "its call is known");
    assert!(!pi.metrics.speed.complete);
    assert_eq!(pi.metrics.timing.ended_at, None);
    assert!(h.worker.shutdown());
}
