//! Live updates: worker events applied to the app without moving the reader.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod support;

use agentscope::app::{App, Panel, Screen};
use agentscope::config::{CliOverrides, Config, Env};
use agentscope::domain::*;
use agentscope::processes::{ProcessInfo, ProcessSnapshot};
use agentscope::providers::{
    DiscoveryDelta, HydrationDelta, ProviderError, ProviderReader, ReadJobId, ReadProgress,
    SourceRecord,
};
use agentscope::ui::{
    KeyAction, RosterContext, apply_worker_event, drain_worker, flush_requests, handle_key, render,
    reset_for_new_worker, worker_stopped,
};
use agentscope::worker::{WorkerEvent, WorkerHandle, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::facts::{self, metadata};

const NOW: Timestamp = 1_800_000_000_000;

fn codex_key() -> SessionKey {
    SessionKey {
        harness: Harness::Codex,
        native_id: "codex-1".into(),
        source: SourceKey::File("codex-1.jsonl".into()),
    }
}

fn claude_key() -> SessionKey {
    SessionKey {
        harness: Harness::Claude,
        native_id: "claude-1".into(),
        source: SourceKey::File("claude-1.jsonl".into()),
    }
}

fn session(key: SessionKey, title: &str, age_secs: i64, count: usize) -> SessionSummary {
    let mut meta = metadata(key, title);
    meta.project = Some("RipperGame".into());
    meta.created_at = Some(NOW - 86_400_000);
    meta.activity = ActivityStamp {
        at: Some(NOW - age_secs * 1000),
        class: AggregationClass::Reported,
    };
    SessionSummary {
        last_event_at: Some(NOW - age_secs * 1000),
        last_completion: None,
        unfinished_work: true,
        displayed_event_count: count,
        hidden_records: 0,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::Unmatched,
        usage: None,
        warnings: vec![],
        ..facts::summary(meta)
    }
}

fn scan(done: bool) -> ScanProgress {
    ScanProgress {
        harness: Harness::Codex,
        scanned: 2,
        total: Some(9),
        done,
    }
}

fn roster(codex_age: i64, claude_age: i64, count: usize) -> WorkerEvent {
    roster_generation(1, codex_age, claude_age, count, vec![], true)
}

fn roster_generation(
    generation: u64,
    codex_age: i64,
    claude_age: i64,
    count: usize,
    warnings: Vec<Warning>,
    done: bool,
) -> WorkerEvent {
    WorkerEvent::Roster {
        snapshot: support::snapshot(
            generation,
            // A frozen generation lists its groups newest first.
            {
                let mut rows = vec![
                    session(codex_key(), "Inventory API", codex_age, count),
                    session(claude_key(), "Scanner refactor", claude_age, 1),
                ];
                rows.sort_by_key(|s| std::cmp::Reverse(s.meta.activity.at));
                rows
            },
            scan(done),
        ),
        warnings,
    }
}

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

fn press(app: &mut App, code: KeyCode) -> KeyAction {
    handle_key(app, KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(app: &mut App, c: char) -> KeyAction {
    press(app, KeyCode::Char(c))
}

fn draw(app: &App, ctx: &RosterContext, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render(f, app, ctx)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn provenance() -> Provenance {
    Provenance {
        harness: Harness::Codex,
        source: SourceKey::File("codex-1.jsonl".into()),
        locator: SourceLocator {
            session: codex_key(),
            kind: LocatorKind::FileOffset {
                path: "codex-1.jsonl".into(),
                offset: 0,
            },
            generation: None,
        },
        record_type: "test".into(),
        harness_version: None,
    }
}

fn event(i: u64, body: &str) -> Event {
    Event {
        id: format!("e{i}"),
        session: codex_key(),
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        kind: EventKind::AssistantMessage,
        actor: Actor::Assistant,
        text: Some(BoundedText {
            preview: body.into(),
            byte_len: body.len(),
            truncated: false,
            locator: None,
        }),
        tool: None,
        usage: None,
        provenance: provenance(),
    }
}

fn anchor(i: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        stable_id: format!("e{i}"),
    }
}

fn page(bodies: &[&str], next: PageAnchor) -> TranscriptPage {
    TranscriptPage {
        events: bodies
            .iter()
            .enumerate()
            .map(|(i, b)| event(i as u64, b))
            .collect(),
        diagnostics: vec![],
        next: Some(next),
        has_more: false,
        truncated: false,
    }
}

/// Open the first roster row (Codex) with three loaded events, selecting the middle one.
fn open_three(app: &mut App, ctx: &mut RosterContext) {
    apply_worker_event(app, ctx, roster(60, 600, 3));
    assert_eq!(app.selected_key(), Some(codex_key()));
    press(app, KeyCode::Enter);
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("expected Select, got {requests:?}");
    };
    apply_worker_event(
        app,
        ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page: page(&["first", "second", "third"], anchor(0)),
        },
    );
    // The transcript opens at its newest entry; step back to the middle.
    ch(app, 'k');
    assert_eq!(app.transcript().unwrap().selected(), 1);
}

#[test]
fn live_append_updates_roster_and_counts_new_events_without_auto_scroll() {
    let mut app = new_app();
    let mut ctx = ctx();
    open_three(&mut app, &mut ctx);
    // Opening away from the tail may queue a baseline count; settle it.
    for request in app.take_requests() {
        if let WorkerRequest::CountAfter { request_id, .. } = request {
            apply_worker_event(
                &mut app,
                &mut ctx,
                WorkerEvent::EventsAfter {
                    request_id,
                    count: 0,
                },
            );
        }
    }

    // Two events are appended; the worker publishes a newer roster.
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(2, 5, 600, 5, vec![], true),
    );
    assert_eq!(
        app.session(&codex_key())
            .unwrap()
            .session
            .displayed_event_count,
        5
    );
    let requests = app.take_requests();
    let Some(WorkerRequest::CountAfter {
        request_id,
        anchor: a,
        ..
    }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::CountAfter { .. }))
    else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert_eq!(a, &anchor(0));
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::EventsAfter {
            request_id: *request_id,
            count: 4,
        },
    );

    assert_eq!(app.new_event_count(), 2);
    assert_eq!(app.transcript().unwrap().selected(), 1, "no auto-scroll");
    assert_eq!(app.transcript().unwrap().events().len(), 3, "no auto-load");
    assert!(
        app.take_requests().is_empty(),
        "nothing is fetched on its own"
    );
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("2 new"), "{screen}");

    // G jumps to the newest page; it is requested, not assumed.
    ch(&mut app, 'G');
    assert!(matches!(
        app.take_requests().as_slice(),
        [WorkerRequest::LoadPage {
            anchor: None,
            direction: PageDirection::Backward,
            ..
        }]
    ));
}

#[test]
fn roster_selection_follows_its_key_when_live_updates_reorder_rows() {
    let mut app = new_app();
    let mut ctx = ctx();
    apply_worker_event(&mut app, &mut ctx, roster(60, 600, 3));
    ch(&mut app, 'j');
    assert_eq!(app.selected_key(), Some(claude_key()));
    // Claude becomes the most recently active; it moves to the top.
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(2, 60, 5, 3, vec![], true),
    );
    assert_eq!(app.selected_key(), Some(claude_key()));
    assert_eq!(app.selected_index(), Some(0));
    // A stale publication is ignored.
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(1, 5, 900, 3, vec![], true),
    );
    assert_eq!(app.selected_index(), Some(0));
    assert_eq!(app.generation(), 2);
}

#[test]
fn live_in_place_message_update_reloads_visible_row_without_new_event_or_selection_loss() {
    let mut app = new_app();
    let mut ctx = ctx();
    open_three(&mut app, &mut ctx);
    app.take_requests();

    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::TranscriptInvalidated { key: codex_key() },
    );
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadPage {
            request_id,
            anchor: None,
            direction: PageDirection::Backward,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected one tail reload, got {requests:?}");
    };
    // The visible rows stay on screen while the reload is in flight.
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("second"), "{screen}");

    // Search while the reload is in flight: nothing matches yet.
    ch(&mut app, '/');
    for c in "streamed".chars() {
        ch(&mut app, c);
    }
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("Search: streamed_"), "{screen}");
    assert!(screen.contains("no matches (loaded events)"), "{screen}");
    press(&mut app, KeyCode::Enter);
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page: page(
                &["first", "second, now streamed to completion", "third"],
                anchor(0),
            ),
        },
    );
    let t = app.transcript().unwrap();
    assert_eq!(t.selected(), 1);
    assert_eq!(t.selected_event().unwrap().id, "e1");
    assert_eq!(t.events().len(), 3);
    assert_eq!(app.new_event_count(), 0, "an edit is not a new event");
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("streamed to completion"), "{screen}");
    // The search follows the reloaded content, not a stale cache.
    assert!(screen.contains("match 1/1 (loaded events)"), "{screen}");
}

fn candidate_snapshot(pid: u32) -> ProcessSnapshot {
    ProcessSnapshot {
        sampled_at: NOW,
        processes: vec![
            ProcessInfo::new(
                pid,
                Some(NOW - 60_000),
                "/usr/local/bin/codex".into(),
                None,
                Some(vec![
                    "/usr/local/bin/codex".into(),
                    "--model".into(),
                    "x".into(),
                ]),
            ),
            ProcessInfo::new(
                pid + 1,
                None,
                "/usr/bin/vim".into(),
                None,
                Some(vec!["vim".into()]),
            ),
        ],
    }
}

#[test]
fn process_snapshot_and_warning_banner_update_without_blocking_input() {
    let mut app = new_app();
    let mut ctx = ctx();
    apply_worker_event(&mut app, &mut ctx, roster(60, 600, 3));

    // No sample yet: the panel says so instead of implying activity.
    assert_eq!(ch(&mut app, 'p'), KeyAction::None);
    assert_eq!(app.panel(), Panel::Processes);
    let screen = draw(&app, &ctx, 100, 30);
    assert!(screen.contains("No process sample yet"), "{screen}");

    // A sample arrives while the panel is open and shows only candidates.
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Processes {
            snapshot: candidate_snapshot(4242),
        },
    );
    let screen = draw(&app, &ctx, 100, 30);
    assert!(screen.contains("PID 4242"), "{screen}");
    assert!(!screen.contains("vim"), "{screen}");
    assert!(
        screen.contains("not matches") || screen.contains("not a match"),
        "{screen}"
    );
    // The panel stays responsive and a newer sample replaces the old one.
    ch(&mut app, 'j');
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Processes {
            snapshot: candidate_snapshot(5151),
        },
    );
    let screen = draw(&app, &ctx, 100, 30);
    assert!(
        screen.contains("PID 5151") && !screen.contains("PID 4242"),
        "{screen}"
    );
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.panel(), Panel::None);

    // A provider warning appears in the banner, without blocking input, and
    // the next publication that no longer carries it retires it.
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Warning {
            warning: Warning {
                key: WarningKey {
                    harness: Harness::Claude,
                    source: "projects".into(),
                    kind: WarningKind::Parse,
                },
                message: "unreadable record".into(),
                count: 2,
            },
        },
    );
    ch(&mut app, 'j');
    let screen = draw(&app, &ctx, 100, 30);
    assert!(
        screen.contains("Warning: Claude: unreadable record"),
        "{screen}"
    );
    assert_eq!(app.selected_key(), Some(claude_key()));
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(2, 60, 600, 3, vec![], true),
    );
    let screen = draw(&app, &ctx, 100, 30);
    assert!(!screen.contains("Warning:"), "{screen}");

    // Scan progress shows provider and counts, never paths.
    apply_worker_event(
        &mut app,
        &mut ctx,
        roster_generation(3, 60, 600, 3, vec![], false),
    );
    let screen = draw(&app, &ctx, 100, 30);
    assert!(screen.contains("Scanning Codex 2/9"), "{screen}");
}

#[test]
fn a_paused_worker_is_shown_and_r_asks_for_a_replacement() {
    let mut app = new_app();
    let mut ctx = ctx();
    apply_worker_event(&mut app, &mut ctx, roster(60, 600, 3));
    assert!(draw(&app, &ctx, 100, 30).contains("Live: on"));

    worker_stopped(&mut app, &mut ctx);
    let screen = draw(&app, &ctx, 100, 30);
    assert!(screen.contains("Live: paused"), "{screen}");
    assert!(screen.contains("Worker stopped"), "{screen}");
    // The last good roster stays; input still works.
    ch(&mut app, 'j');
    assert_eq!(app.selected_key(), Some(claude_key()));
    assert_eq!(ch(&mut app, 'r'), KeyAction::Refresh);

    reset_for_new_worker(&mut app, &mut ctx);
    assert!(draw(&app, &ctx, 100, 30).contains("Live: on"));
    assert_eq!(app.screen, Screen::Roster);
}

// ---- end to end through a real worker thread ------------------------------

#[derive(Default)]
struct State {
    sessions: Vec<SessionSummary>,
    bodies: Vec<String>,
}

struct Live(Arc<Mutex<State>>);

impl ProviderReader for Live {
    fn harness(&self) -> Harness {
        Harness::Codex
    }
    fn discover(
        &mut self,
        _now: Timestamp,
        _budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        let s = self.0.lock().unwrap();
        Ok(DiscoveryDelta {
            sessions: s.sessions.iter().map(|s| s.meta.clone()).collect(),
            removed: vec![],
            invalidated: vec![],
            warnings: vec![],
            scan: ScanProgress {
                harness: Harness::Codex,
                scanned: s.sessions.len(),
                total: None,
                done: true,
            },
        })
    }
    fn hydrate(
        &mut self,
        _job: ReadJobId,
        key: &SessionKey,
        _now: Timestamp,
        _budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        let s = self.0.lock().unwrap();
        s.sessions
            .iter()
            .find(|s| s.meta.key == *key)
            .map(|summary| HydrationDelta {
                summary: summary.clone(),
                done: true,
            })
            .ok_or_else(|| ProviderError::Unavailable("unknown".into()))
    }
    fn cancel_read(&mut self, _key: &SessionKey, _job: ReadJobId) {}
    fn details_page(
        &mut self,
        _job: ReadJobId,
        _key: &SessionKey,
        _anchor: Option<PageAnchor>,
        _request: PageRequest,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, ProviderError> {
        Ok(ReadProgress::Ready(SessionDetailPage::default()))
    }
    fn transcript_page(
        &mut self,
        _job: ReadJobId,
        _key: &SessionKey,
        from: Option<PageAnchor>,
        direction: PageDirection,
        _request: PageRequest,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
        let s = self.0.lock().unwrap();
        let at = |a: &PageAnchor| a.source_order as usize;
        let range = match (from.as_ref(), direction) {
            (None, _) => 0..s.bodies.len(),
            (Some(a), PageDirection::Forward) => at(a) + 1..s.bodies.len(),
            (Some(a), PageDirection::Backward) => 0..at(a),
        };
        let events: Vec<Event> = range
            .clone()
            .map(|i| event(i as u64, &s.bodies[i]))
            .collect();
        Ok(ReadProgress::Ready(TranscriptPage {
            next: match direction {
                PageDirection::Forward => range.end.checked_sub(1),
                PageDirection::Backward => Some(range.start),
            }
            .map(|i| anchor(i as u64)),
            events,
            diagnostics: vec![],
            has_more: false,
            truncated: false,
        }))
    }
    fn count_events_after(
        &mut self,
        _job: ReadJobId,
        _key: &SessionKey,
        anchor: &PageAnchor,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError> {
        let s = self.0.lock().unwrap();
        Ok(ReadProgress::Ready(
            s.bodies
                .len()
                .saturating_sub(anchor.source_order as usize + 1),
        ))
    }
    fn source_record(
        &mut self,
        _job: ReadJobId,
        _locator: &SourceLocator,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        Ok(ReadProgress::Ready(SourceRecord {
            raw: String::new(),
            display: String::new(),
            unavailable: Some("not available in this test".into()),
        }))
    }
}

fn pump_until(
    app: &mut App,
    ctx: &mut RosterContext,
    worker: &WorkerHandle,
    done: impl Fn(&App) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
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

#[test]
fn a_real_worker_delivers_live_appends_to_the_open_inspector() {
    let state = Arc::new(Mutex::new(State {
        sessions: vec![session(codex_key(), "Inventory API", 60, 3)],
        bodies: vec!["first".into(), "second".into(), "third".into()],
    }));
    let mut config = Config::load_with_env(&CliOverrides::default(), &Env::new()).unwrap();
    config.refresh_ms = 3_600_000;
    config.process_ms = 3_600_000;
    config.process_enabled = false;
    let worker = WorkerHandle::spawn(vec![Box::new(Live(state.clone()))], config);
    let mut app = new_app();
    let mut ctx = ctx();

    pump_until(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 1);
    press(&mut app, KeyCode::Enter);
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.transcript().is_some_and(|t| t.events().len() == 3)
    });
    // Opens at the newest entry; select the middle event for the append checks.
    ch(&mut app, 'k');

    {
        let mut s = state.lock().unwrap();
        s.bodies.push("fourth".into());
        s.bodies.push("fifth".into());
        s.sessions = vec![session(codex_key(), "Inventory API", 1, 5)];
    }
    // `r` asks the worker to refresh now.
    assert_eq!(ch(&mut app, 'r'), KeyAction::Refresh);
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    // The changed session is rehydrated after the refresh; its summary and the count of
    // events after the newest loaded one arrive independently.
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.new_event_count() == 2
            && a.session(&codex_key())
                .is_some_and(|r| r.session.displayed_event_count == 5)
    });

    assert_eq!(app.transcript().unwrap().selected(), 1, "no auto-scroll");
    assert_eq!(app.transcript().unwrap().events().len(), 3);
    assert_eq!(
        app.session(&codex_key())
            .unwrap()
            .session
            .displayed_event_count,
        5
    );
    assert!(worker.shutdown());
}

// ---- B5 navigation smoke: real reader → worker → keys → render ---------------

struct SmokeState {
    sessions: Vec<SessionSummary>,
    child: SessionKey,
    parent: SessionKey,
    bodies: Vec<String>,
}

fn smoke_state() -> SmokeState {
    // The parent is the newest group; 54 more roots page 50 at a time.
    let parent = codex_key();
    let child = SessionKey {
        native_id: "codex-child".into(),
        ..codex_key()
    };
    let mut sessions = vec![session(parent.clone(), "Parent session", 1, 5)];
    for i in 0..54 {
        sessions.push(session(
            SessionKey {
                native_id: format!("codex-{i:02}"),
                ..codex_key()
            },
            &format!("Root {i:02}"),
            60 + i,
            1,
        ));
    }
    let mut child_meta = metadata(child.clone(), "Nested child");
    child_meta.lineage.parent = Some(SessionLink::Unresolved(parent.native_id.clone()));
    child_meta.activity = ActivityStamp {
        at: Some(NOW - 2_000),
        class: AggregationClass::Reported,
    };
    sessions.push(SessionSummary {
        displayed_event_count: 0,
        usage: None,
        ..facts::summary(child_meta)
    });
    SmokeState {
        sessions,
        child,
        parent,
        bodies: vec![
            "zero".into(),
            "one".into(),
            "two".into(),
            "three".into(),
            "four".into(),
        ],
    }
}

struct SmokeReader(Arc<Mutex<SmokeState>>);

impl ProviderReader for SmokeReader {
    fn harness(&self) -> Harness {
        Harness::Codex
    }
    fn discover(
        &mut self,
        _now: Timestamp,
        _budget: RefreshBudget,
    ) -> Result<DiscoveryDelta, ProviderError> {
        let s = self.0.lock().unwrap();
        Ok(DiscoveryDelta {
            sessions: s.sessions.iter().map(|s| s.meta.clone()).collect(),
            removed: vec![],
            invalidated: vec![],
            warnings: vec![],
            scan: ScanProgress {
                harness: Harness::Codex,
                scanned: s.sessions.len(),
                total: Some(s.sessions.len()),
                done: true,
            },
        })
    }
    fn hydrate(
        &mut self,
        _job: ReadJobId,
        key: &SessionKey,
        _now: Timestamp,
        _budget: RefreshBudget,
    ) -> Result<HydrationDelta, ProviderError> {
        let s = self.0.lock().unwrap();
        s.sessions
            .iter()
            .find(|s| s.meta.key == *key)
            .map(|summary| HydrationDelta {
                summary: summary.clone(),
                done: true,
            })
            .ok_or_else(|| ProviderError::Unavailable("unknown".into()))
    }
    fn cancel_read(&mut self, _key: &SessionKey, _job: ReadJobId) {}
    fn details_page(
        &mut self,
        _job: ReadJobId,
        _key: &SessionKey,
        _anchor: Option<PageAnchor>,
        _request: PageRequest,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<SessionDetailPage>, ProviderError> {
        Ok(ReadProgress::Ready(SessionDetailPage {
            recent: vec![event(0, "detail record")],
            has_more: false,
            ..SessionDetailPage::default()
        }))
    }
    fn transcript_page(
        &mut self,
        _job: ReadJobId,
        _key: &SessionKey,
        anchor: Option<PageAnchor>,
        direction: PageDirection,
        _request: PageRequest,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<TranscriptPage>, ProviderError> {
        let s = self.0.lock().unwrap();
        let len = s.bodies.len();
        let at = anchor.as_ref().map_or(0, |a| a.source_order as usize);
        let (start, end) = match (anchor.as_ref(), direction) {
            (None, _) => (len.saturating_sub(3), len),
            (Some(_), PageDirection::Backward) => (at.saturating_sub(3), at),
            (Some(_), PageDirection::Forward) => ((at + 1).min(len), len.min(at + 4)),
        };
        let events: Vec<Event> = (start..end)
            .map(|i| event(i as u64, &s.bodies[i]))
            .collect();
        let next = match direction {
            PageDirection::Forward => events.last(),
            PageDirection::Backward => events.first(),
        }
        .map(|e| PageAnchor {
            timestamp: e.timestamp,
            source_order: e.source_order,
            stable_id: e.id.clone(),
        });
        Ok(ReadProgress::Ready(TranscriptPage {
            events,
            diagnostics: vec![],
            next,
            has_more: match direction {
                PageDirection::Backward => start > 0,
                PageDirection::Forward => end < len,
            },
            truncated: false,
        }))
    }
    fn count_events_after(
        &mut self,
        _job: ReadJobId,
        _key: &SessionKey,
        anchor: &PageAnchor,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<usize>, ProviderError> {
        let s = self.0.lock().unwrap();
        Ok(ReadProgress::Ready(
            s.bodies
                .len()
                .saturating_sub(anchor.source_order as usize + 1),
        ))
    }
    fn source_record(
        &mut self,
        _job: ReadJobId,
        _locator: &SourceLocator,
        _budget: RefreshBudget,
    ) -> Result<ReadProgress<SourceRecord>, ProviderError> {
        Ok(ReadProgress::Ready(SourceRecord {
            raw: String::new(),
            display: String::new(),
            unavailable: Some("synthetic".into()),
        }))
    }
}

#[test]
fn b5_navigation_smoke_through_reader_worker_keys_and_render() {
    let state = Arc::new(Mutex::new(smoke_state()));
    let mut config = Config::load_with_env(&CliOverrides::default(), &Env::new()).unwrap();
    config.refresh_ms = 3_600_000;
    config.process_ms = 3_600_000;
    config.process_enabled = false;
    let worker = WorkerHandle::spawn(vec![Box::new(SmokeReader(state.clone()))], config);
    let mut app = new_app();
    let mut ctx = ctx();

    // First root page: 50 of the 55 groups.
    pump_until(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 50);
    assert!(app.roster_status().has_more);
    let parent = state.lock().unwrap().parent.clone();
    assert_eq!(app.selected_key(), Some(parent.clone()));

    // `m` reveals the rest without moving the selection.
    press(&mut app, KeyCode::Char('m'));
    pump_until(&mut app, &mut ctx, &worker, |a| a.total_sessions() == 55);
    assert_eq!(app.selected_key(), Some(parent.clone()));

    // Space asks for the fast preview and the first child page.
    press(&mut app, KeyCode::Char(' '));
    pump_until(&mut app, &mut ctx, &worker, |a| {
        matches!(
            a.preview(),
            agentscope::app::PreviewState::Ready { complete: true, .. }
        ) && a
            .children_of(&parent)
            .is_some_and(|c| c.metadata.len() == 1)
    });
    let rows: Vec<String> = app
        .roster_rows()
        .filter_map(|item| match item {
            agentscope::app::RosterItem::Session(row) => {
                Some(row.session.meta.key.native_id.clone())
            }
            _ => None,
        })
        .take(2)
        .collect();
    assert_eq!(rows, ["codex-1".to_string(), "codex-child".to_string()]);
    // The selected identity is still the parent; one loaded move reaches the child.
    assert_eq!(app.selected_key(), Some(parent.clone()));
    press(&mut app, KeyCode::Char('j'));
    let child = state.lock().unwrap().child.clone();
    assert_eq!(app.selected_key(), Some(child.clone()));
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("Nested child"), "{screen}");

    // Back to the parent and open its newest entry.
    press(&mut app, KeyCode::Char('k'));
    press(&mut app, KeyCode::Enter);
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.transcript().is_some_and(|t| t.events().len() == 3)
    });
    assert_eq!(
        app.transcript().unwrap().selected_event().unwrap().id,
        "e4",
        "the newest event is selected on opening"
    );
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("four"), "{screen}");

    // Page back to the previous content; the anchor stays on e2.
    app.inspector_move(-2);
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.transcript().is_some_and(|t| t.events().len() == 5)
    });
    assert_eq!(app.transcript().unwrap().selected_event().unwrap().id, "e2");

    // Two live events increment the indicator without moving the reader.
    {
        let mut s = state.lock().unwrap();
        s.bodies.push("five".into());
        s.bodies.push("six".into());
        s.sessions[0] = session(s.parent.clone(), "Parent session", 1, 7);
    }
    // `r` in the inspector retries and asks for a refresh; the run loop turns
    // that action into an App-queued Refresh.
    assert_eq!(press(&mut app, KeyCode::Char('r')), KeyAction::Refresh);
    app.refresh_roster();
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.session(&parent)
            .is_some_and(|r| r.session.displayed_event_count == 7)
    });
    pump_until(&mut app, &mut ctx, &worker, |a| a.new_event_count() == 2);
    assert_eq!(app.transcript().unwrap().selected_event().unwrap().id, "e2");
    let screen = draw(&app, &ctx, 110, 30);
    assert!(screen.contains("2 new"), "{screen}");

    // Explicit End/follow reloads the newest page and lands on the new last event.
    press(&mut app, KeyCode::Char('G'));
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.transcript()
            .and_then(|t| t.selected_event())
            .is_some_and(|e| e.id == "e6")
    });
    assert_eq!(app.new_event_count(), 0);

    // Session info requests paged detailed facts; the worker answers them.
    press(&mut app, KeyCode::Char('i'));
    pump_until(&mut app, &mut ctx, &worker, |a| {
        a.details().key() == Some(&parent) && a.details().page().recent.len() == 1
    });
    assert!(!app.details().page().has_more);
    let _ = draw(&app, &ctx, 110, 30);
    worker.shutdown();
}

// ---- OMP: the real reader behind the real worker --------------------------------------

/// A live OMP session grows, then its source is replaced: the open transcript follows the
/// append, reloads at the replacement's newest event, and a locator taken before the
/// replacement is refused.
#[test]
fn omp_open_transcript_follows_appends_and_a_replaced_source() {
    use support::omp as om;
    use support::world::{OMP_TAIL, World};

    let world = World::with_omp();
    let plant = world.omp_plant.as_ref().unwrap();
    let parent = plant.family.parent.clone();
    let path = match &parent.source {
        SourceKey::File(path) => path.clone(),
        other => panic!("{other:?}"),
    };
    let worker = WorkerHandle::spawn(world.readers(), world.config());
    let mut app = new_app();
    let mut ctx = ctx();
    let pump = |app: &mut App, ctx: &mut RosterContext, what: &str, done: &dyn Fn(&App) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            flush_requests(app, &worker);
            if done(app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(25)) {
                apply_worker_event(app, ctx, event);
            }
        }
    };
    let last_text = |app: &App| {
        app.transcript().and_then(|t| {
            t.events()
                .last()
                .and_then(|e| e.text.as_ref())
                .map(|t| t.preview.clone())
        })
    };
    pump(&mut app, &mut ctx, "the roster", &|a| {
        a.total_sessions() == 6
    });
    assert!(app.select_row(&parent));
    press(&mut app, KeyCode::Enter);
    pump(&mut app, &mut ctx, "the latest page", &|a| {
        last_text(a).as_deref() == Some(OMP_TAIL)
    });
    let old_locator = app
        .transcript()
        .unwrap()
        .events()
        .last()
        .unwrap()
        .provenance
        .locator
        .clone();

    // A live append is counted and, once requested, shown at the end.
    om::append(&path, &[om::user_message("u3", Some("u2"), "live append")]);
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    pump(&mut app, &mut ctx, "the new event count", &|a| {
        a.new_event_count() == 1
    });
    ch(&mut app, 'G');
    pump(&mut app, &mut ctx, "the appended event", &|a| {
        last_text(a).as_deref() == Some("live append")
    });

    // The source is replaced by a shorter file: the transcript reloads from it.
    om::write_session(
        &world.omp_root,
        "work",
        "t1_omp-fam",
        &[
            om::header("omp-fam", None, None),
            om::user_message("r1", None, "replacement tail"),
        ],
    );
    support::set_modified(&path, 1_900_000_000);
    worker.tx.send(WorkerRequest::Refresh).unwrap();
    pump(&mut app, &mut ctx, "the replaced transcript", &|a| {
        last_text(a).as_deref() == Some("replacement tail")
    });
    assert!(
        app.transcript()
            .unwrap()
            .events()
            .iter()
            .all(|e| e.text.as_ref().is_none_or(|t| t.preview != "live append")),
        "nothing of the old generation stays loaded"
    );
    worker
        .tx
        .send(WorkerRequest::LoadSource {
            request_id: 9_999,
            locator: old_locator,
        })
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        assert!(
            Instant::now() < deadline,
            "the stale locator was not answered"
        );
        if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(25)) {
            match &event {
                WorkerEvent::Source {
                    request_id: 9_999,
                    record,
                } => {
                    assert!(
                        !record.raw.contains(OMP_TAIL),
                        "the replaced generation never serves old bytes"
                    );
                    break;
                }
                WorkerEvent::LoadFailed { request_id: 9_999 } => break,
                _ => {}
            }
            apply_worker_event(&mut app, &mut ctx, event);
        }
    }
    assert!(worker.shutdown());
}

/// Opening a large OMP transcript and leaving it cancels its indexing; another session
/// opens at once, and the large one still opens completely afterwards.
#[test]
fn omp_indexing_is_cancelled_by_leaving_and_input_never_waits_for_it() {
    use support::omp as om;
    use support::world::World;

    let world = World::with_omp();
    let plant = world.omp_plant.as_ref().unwrap();
    let mut entries = vec![om::header("big", None, None)];
    let mut last: Option<String> = None;
    for n in 0..30_000 {
        let id = format!("e{n}");
        entries.push(om::user_message(&id, last.as_deref(), &"word ".repeat(30)));
        last = Some(id);
    }
    entries.push(om::user_message("final", last.as_deref(), "the big tail"));
    om::write_session(&world.omp_root, "work", "t8_big", &entries);

    let worker = WorkerHandle::spawn(world.readers(), world.config());
    let mut app = new_app();
    let mut ctx = ctx();
    let pump = |app: &mut App, ctx: &mut RosterContext, what: &str, done: &dyn Fn(&App) -> bool| {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            flush_requests(app, &worker);
            if done(app) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(5)) {
                apply_worker_event(app, ctx, event);
            }
        }
    };
    pump(&mut app, &mut ctx, "the roster", &|a| {
        a.total_sessions() == 7
    });
    let big = app
        .all_rows()
        .iter()
        .find(|r| r.session.meta.key.native_id == "big")
        .map(|r| r.session.meta.key.clone())
        .expect("the large session is listed");

    assert!(app.select_row(&big));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen, Screen::Inspector);
    flush_requests(&mut app, &worker);
    // Leave before the index can possibly be finished, then open another session.
    press(&mut app, KeyCode::Esc);
    flush_requests(&mut app, &worker);
    assert!(app.select_row(&plant.fork));
    press(&mut app, KeyCode::Enter);
    pump(&mut app, &mut ctx, "the other session", &|a| {
        a.transcript()
            .is_some_and(|t| t.loaded() && t.key() == &plant.fork)
    });
    press(&mut app, KeyCode::Esc);

    // The large session opens completely on its own turn.
    assert!(app.select_row(&big));
    press(&mut app, KeyCode::Enter);
    pump(&mut app, &mut ctx, "the large session", &|a| {
        a.transcript().is_some_and(|t| {
            t.loaded()
                && t.events()
                    .last()
                    .and_then(|e| e.text.as_ref())
                    .is_some_and(|t| t.preview == "the big tail")
        })
    });
    assert!(worker.shutdown());
}
