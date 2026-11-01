use std::collections::HashSet;
use std::time::Duration;

mod support;

use agentscope::app::{
    App, MAX_LOADED_EVENTS, NOISE_RECORD_TYPES, RowKind, tool_summary, turn_rows,
};
use agentscope::domain::*;
use agentscope::time::format_local;
use agentscope::ui::{RosterContext, render};
use agentscope::worker::WorkerRequest;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use support::facts::{self, metadata};

const NOW: Timestamp = 1_800_000_000_000;

fn key() -> SessionKey {
    SessionKey {
        harness: Harness::Codex,
        native_id: "sess-1".into(),
        source: SourceKey::File("sess-1.jsonl".into()),
    }
}

fn summary(count: usize) -> SessionSummary {
    let mut meta = metadata(key(), "Inventory API");
    meta.project = Some("RipperGame".into());
    meta.model = Some(ModelDetail {
        id: "gpt-5.1-codex".into(),
        provider: None,
        variant: None,
        effort: None,
        configured_effort: None,
    });
    meta.created_at = Some(NOW - 86_400_000);
    meta.activity = ActivityStamp {
        at: Some(NOW - 540_000),
        class: AggregationClass::Reported,
    };
    SessionSummary {
        last_event_at: Some(NOW - 540_000),
        last_completion: None,
        unfinished_work: true,
        displayed_event_count: count,
        hidden_records: 0,
        server_tools: Some(ServerToolCounts {
            web_search: 3,
            web_fetch: 1,
        }),
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::Candidates { count: 2 },
        usage: Some(UsageSummary {
            categories: TokenCategories::default(),
            total: Some(73_214),
            class: AggregationClass::Reported,
            cost: CostEstimate {
                usd: Some(0.23),
                class: AggregationClass::Estimated,
                as_of: None,
                note: None,
                ..CostEstimate::default()
            },
            ..UsageSummary::default()
        }),
        warnings: vec![],
        metrics: SessionMetrics {
            context: ContextUsage {
                capacity: Some(475_000),
                class: AggregationClass::Reported,
                ..ContextUsage::default()
            },
            ..SessionMetrics::default()
        },
        ..facts::summary(meta)
    }
}

fn app_with(s: SessionSummary) -> App {
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(vec![s], vec![]);
    app
}

fn provenance() -> Provenance {
    Provenance {
        harness: Harness::Codex,
        source: SourceKey::File("sess-1.jsonl".into()),
        locator: SourceLocator {
            session: key(),
            kind: LocatorKind::FileOffset {
                path: "sess-1.jsonl".into(),
                offset: 0,
            },
            generation: None,
        },
        record_type: "test".into(),
        harness_version: None,
    }
}

fn text(s: &str) -> BoundedText {
    BoundedText {
        preview: s.into(),
        byte_len: s.len(),
        truncated: false,
        locator: None,
    }
}

fn event(i: u64, kind: EventKind, body: &str) -> Event {
    Event {
        id: format!("e{i}"),
        session: key(),
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        kind,
        actor: match kind {
            EventKind::UserPrompt => Actor::User,
            EventKind::Tool => Actor::Tool,
            _ => Actor::Assistant,
        },
        text: Some(text(body)),
        tool: None,
        usage: None,
        provenance: provenance(),
    }
}

fn msg(i: u64) -> Event {
    event(i, EventKind::AssistantMessage, &format!("message {i}"))
}

fn anchor(i: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        stable_id: format!("e{i}"),
    }
}

fn page(events: Vec<Event>, next: Option<PageAnchor>, has_more: bool) -> TranscriptPage {
    TranscriptPage {
        events,
        diagnostics: vec![],
        next,
        has_more,
        truncated: false,
    }
}

fn forward_page(range: std::ops::Range<u64>, has_more: bool) -> TranscriptPage {
    let events: Vec<Event> = range.clone().map(msg).collect();
    page(events, Some(anchor(range.end - 1)), has_more)
}

fn backward_page(range: std::ops::Range<u64>, has_more: bool) -> TranscriptPage {
    let events: Vec<Event> = range.clone().map(msg).collect();
    page(events, Some(anchor(range.start)), has_more)
}

fn ctx() -> RosterContext {
    RosterContext {
        no_color: true,
        ..RosterContext::default()
    }
}

fn draw(app: &App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render(f, app, &ctx())).unwrap();
    let buf: &Buffer = terminal.backend().buffer();
    buf.content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn hm(ts: Timestamp) -> String {
    format_local(ts)[11..16].to_string()
}

/// Open the inspector and answer its `Select` with `first`.
fn open(app: &mut App, first: TranscriptPage) -> u64 {
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, key: k }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("expected a single Select, got {requests:?}");
    };
    assert_eq!(k, &key());
    let id = *request_id;
    assert!(app.apply_transcript(id, first));
    id
}

/// Page requests only: the baseline count requested after opening away from the
/// tail is not a page load.
fn page_requests(requests: &[WorkerRequest]) -> Vec<WorkerRequest> {
    requests
        .iter()
        .filter(|r| !matches!(r, WorkerRequest::CountAfter { .. }))
        .cloned()
        .collect()
}

fn load_page(requests: &[WorkerRequest]) -> (u64, Option<PageAnchor>, PageDirection) {
    let requests = page_requests(requests);
    match requests.as_slice() {
        [
            WorkerRequest::LoadPage {
                request_id,
                anchor,
                direction,
                ..
            },
        ] => (*request_id, anchor.clone(), *direction),
        other => panic!("expected a single LoadPage, got {other:?}"),
    }
}

fn ids(app: &App) -> Vec<String> {
    app.transcript()
        .unwrap()
        .events()
        .iter()
        .map(|e| e.id.clone())
        .collect()
}

#[test]
fn renders_header_usage_evidence_and_timeline() {
    let mut app = app_with(summary(3));
    let user = event(0, EventKind::UserPrompt, "Implement inventory checks");
    let reply = event(1, EventKind::AssistantMessage, "I will inspect the code.");
    let t0 = user.timestamp.unwrap();
    open(&mut app, page(vec![user, reply], None, false));

    let evidence = app.session(&key()).unwrap().status.evidence.clone();
    let out = draw(&app, 110, 30);
    for want in [
        "< Back  CODEX / Inventory API",
        "Project: RipperGame",
        "Model: gpt-5.1-codex",
        "Session: sess-1",
        "Status: ",
        evidence.as_str(),
        "Tokens: 73,214 reported",
        "Cost: ~$0.23 estimated",
        "Context: 475k window",
        "Process: unmatched — 2 candidates",
        "user",
        "Implement inventory checks",
        "assistant",
        "I will inspect the code.",
        hm(t0).as_str(),
        "Esc back",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
}

#[test]
fn renders_reported_context_window_and_server_tool_counts_and_unknown_otherwise() {
    let mut app = app_with(summary(1));
    open(&mut app, page(vec![msg(0)], None, false));
    let out = draw(&app, 110, 30);
    assert!(out.contains("Context: 475k window"), "{out}");
    assert!(
        out.contains("Server tools: web_search 3 · web_fetch 1"),
        "{out}"
    );

    let mut bare = summary(1);
    bare.metrics.context.capacity = None;
    bare.server_tools = None;
    let mut app = app_with(bare);
    open(&mut app, page(vec![msg(0)], None, false));
    let out = draw(&app, 110, 30);
    assert!(out.contains("Context: Unknown"), "{out}");
    assert!(out.contains("Server tools: Unknown"), "{out}");
}

#[test]
fn expands_a_tool_row_and_shows_status_duration_exit_and_truncation() {
    let mut app = app_with(summary(2));
    let mut tool = event(1, EventKind::Tool, "");
    tool.text = None;
    tool.tool = Some(ToolInteraction {
        name: "shell".into(),
        call_id: Some("c1".into()),
        input: Some(BoundedText {
            preview: "cargo test --all".into(),
            byte_len: 9_000,
            truncated: true,
            locator: None,
        }),
        output: Some(text("2 failed assertions\nsecond line")),
        status: ToolStatus::Error,
        exit_code: Some(1),
        started_at: Some(NOW),
        completed_at: Some(NOW + 1_200),
        orphan: false,
        child_session: None,
    });
    open(&mut app, page(vec![msg(0), tool], None, false));

    // The latest row opens expanded; collapsing it hides the details without
    // recording a manual expansion.
    let expanded = draw(&app, 110, 30);
    assert!(expanded.contains("shell cargo test --all"), "{expanded}");
    assert!(expanded.contains("[collapse]"), "{expanded}");
    assert!(expanded.contains("Duration: 1.2s"), "{expanded}");

    app.inspector_toggle();
    let collapsed = draw(&app, 110, 30);
    assert!(collapsed.contains("[expand]"), "{collapsed}");
    assert!(!collapsed.contains("Duration:"), "{collapsed}");

    app.inspector_toggle();
    let out = draw(&app, 110, 30);
    for want in [
        "[collapse]",
        "Status: Error",
        "Duration: 1.2s",
        "Exit: 1",
        "Input: cargo test --all",
        "[truncated: showing 16 of 9000 bytes]",
        "Output: 2 failed assertions",
        "second line",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }

    app.inspector_toggle();
    assert!(!draw(&app, 110, 30).contains("Duration:"));
}

#[test]
fn loads_forward_and_backward_pages_without_duplicates() {
    let mut app = app_with(summary(10));
    open(&mut app, backward_page(5..10, true));
    assert_eq!(app.transcript().unwrap().selected_event().unwrap().id, "e9");

    // Scrolling to the start prepends the older page without duplicates.
    app.inspector_move(-100);
    let (older_id, older_anchor, direction) = load_page(&app.take_requests());
    assert_eq!(direction, PageDirection::Backward);
    assert_eq!(older_anchor, Some(anchor(5)));
    // A response for another request is ignored.
    assert!(!app.apply_transcript(older_id + 99, backward_page(0..5, false)));
    assert!(app.apply_transcript(older_id, backward_page(0..5, false)));
    let expected: Vec<String> = (0..10).map(|i| format!("e{i}")).collect();
    assert_eq!(ids(&app), expected);
    assert!(page_requests(&app.take_requests()).is_empty());
    assert!(!app.transcript().unwrap().has_older());
    assert_eq!(
        app.transcript().unwrap().selected_event().unwrap().id,
        "e5",
        "selection stays on the same event after prepending"
    );

    // `G` always reloads the tail, Backward from none, even when the window
    // already ends there: content can change without the row count growing.
    app.inspector_last();
    let (tail_id, tail_anchor, direction) = load_page(&app.take_requests());
    assert_eq!((tail_anchor, direction), (None, PageDirection::Backward));
    assert!(app.apply_transcript(tail_id, backward_page(20..25, true)));
    assert_eq!(app.transcript().unwrap().selected(), 4, "lands on newest");
    let tail: Vec<String> = (20..25).map(|i| format!("e{i}")).collect();
    assert_eq!(ids(&app), tail);

    // Scrolling to the start loads the older page from the first event.
    app.inspector_move(-100);
    let (older_id, older_anchor, direction) = load_page(&app.take_requests());
    assert_eq!(direction, PageDirection::Backward);
    assert_eq!(older_anchor, Some(anchor(20)));
    assert!(app.apply_transcript(older_id, backward_page(15..20, false)));
    let all: Vec<String> = (15..25).map(|i| format!("e{i}")).collect();
    assert_eq!(ids(&app), all);
    assert_eq!(
        app.transcript().unwrap().selected(),
        5,
        "selection stays on the same event after prepending"
    );
    assert_eq!(
        app.transcript().unwrap().selected_event().unwrap().id,
        "e20"
    );
    assert!(!app.transcript().unwrap().has_older());
    let unique: HashSet<_> = ids(&app).into_iter().collect();
    assert_eq!(unique.len(), 10);
}

#[test]
fn initial_select_is_forward_from_start() {
    let mut app = app_with(summary(1));
    app.open_inspector();
    let requests = app.take_requests();
    assert!(
        matches!(requests.as_slice(), [WorkerRequest::HydratePreview { .. }, WorkerRequest::Select { key: k, .. }] if *k == key()),
        "{requests:?}"
    );
}

#[test]
fn labels_reasoning_as_provider_persisted_and_images_as_placeholders() {
    let mut app = app_with(summary(2));
    let reasoning = event(0, EventKind::Reasoning, "weighing the options");
    let image = event(1, EventKind::UserPrompt, "[image]");
    open(&mut app, page(vec![reasoning, image], None, false));
    let out = draw(&app, 110, 30);
    assert!(out.contains("Provider reasoning (persisted)"), "{out}");
    assert!(out.contains("[image]"), "{out}");
    assert!(!out.contains("weighing the options"), "collapsed: {out}");

    // The transcript opens at its newest entry; the reasoning row is first.
    app.inspector_first();
    app.inspector_toggle();
    let out = draw(&app, 110, 30);
    assert!(out.contains("weighing the options"), "{out}");
    assert!(out.contains("not hidden chain-of-thought"), "{out}");
}

#[test]
fn missing_timestamps_and_usage_show_unknown() {
    let mut bare = summary(1);
    bare.usage = None;
    let mut app = app_with(bare);
    let mut e = msg(0);
    e.timestamp = None;
    open(&mut app, page(vec![e], None, false));
    let out = draw(&app, 110, 30);
    assert!(out.contains("Tokens: Unknown · Cost: Unknown"), "{out}");
    let row = out.lines().find(|l| l.contains("message 0")).unwrap();
    assert!(row.contains('—'), "{row}");
}

#[test]
fn new_events_are_counted_without_scrolling_or_loading() {
    let mut app = app_with(summary(3));
    open(&mut app, backward_page(0..3, false));
    app.inspector_move(-1);
    assert_eq!(app.new_event_count(), 0);

    app.apply_roster(vec![summary(6)], vec![]);
    // The count goes through the worker (`count_events_after`) anchored on the
    // oldest loaded event with the already-known tail discounted.
    let requests = app.take_requests();
    let [
        WorkerRequest::CountAfter {
            request_id,
            anchor: a,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert_eq!(a, &anchor(0));
    assert!(!app.apply_events_after(request_id + 9, 5), "stale response");
    assert!(app.apply_events_after(*request_id, 5));
    assert_eq!(app.new_event_count(), 3);
    assert_eq!(app.transcript().unwrap().selected(), 1, "no auto-scroll");
    assert_eq!(ids(&app).len(), 3, "no auto-load");
    assert!(page_requests(&app.take_requests()).is_empty());
    let out = draw(&app, 110, 30);
    assert!(out.contains("3 new"), "{out}");

    // An in-place change is a reload, not a new event.
    app.invalidate_transcript(&key());
    assert!(matches!(
        app.take_requests().as_slice(),
        [WorkerRequest::LoadPage {
            direction: PageDirection::Backward,
            anchor: None,
            ..
        }]
    ));
    assert_eq!(app.new_event_count(), 3);
}

#[test]
fn counts_after_a_backward_page_exclude_the_loaded_tail() {
    let mut app = app_with(summary(60));
    open(&mut app, forward_page(0..1, false));
    app.inspector_last();
    let (id, _, _) = load_page(&app.take_requests());
    // Tail page 20..60: its anchor is the first event, so 39 loaded events
    // follow it and a count of 42 means 3 are new.
    assert!(app.apply_transcript(id, backward_page(20..60, true)));
    app.apply_roster(vec![summary(63)], vec![]);
    let requests = app.take_requests();
    let [
        WorkerRequest::CountAfter {
            request_id,
            anchor: a,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert_eq!(a, &anchor(20));
    assert!(app.apply_events_after(*request_id, 42));
    assert_eq!(app.new_event_count(), 3);
}

#[test]
fn loading_older_history_keeps_the_unread_count() {
    let mut app = app_with(summary(60));
    open(&mut app, forward_page(0..1, false));
    app.inspector_last();
    let (id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(id, backward_page(20..60, true)));
    app.apply_roster(vec![summary(63)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    // The reader scrolls off the newest row before the count lands, so the
    // arrival only raises the count instead of pulling the newest page in.
    app.inspector_move(isize::MIN);
    let (older_id, _, direction) = load_page(&app.take_requests());
    assert_eq!(direction, PageDirection::Backward);
    assert!(app.apply_events_after(*request_id, 42));
    assert_eq!(app.new_event_count(), 3);

    assert!(app.apply_transcript(older_id, backward_page(0..20, false)));
    assert_eq!(
        app.new_event_count(),
        3,
        "older pages do not refresh the tail"
    );
    assert!(draw(&app, 110, 30).contains("3 new"));

    // Reloading the newest end does clear it.
    app.inspector_last();
    let (tail_id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(tail_id, backward_page(23..63, true)));
    assert_eq!(app.new_event_count(), 0);
}

#[test]
fn invalidation_rereads_the_boundary_and_chases_the_current_selection() {
    let mut app = app_with(summary(160));
    open(&mut app, forward_page(0..1, false));
    app.inspector_last();
    let (id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(id, backward_page(100..160, true)));
    app.inspector_move(-29); // e130
    assert!(page_requests(&app.take_requests()).is_empty());
    assert_eq!(
        app.transcript().unwrap().selected_event().unwrap().id,
        "e130"
    );

    app.invalidate_transcript(&key());
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    // The first page reaches the selection but not the old window start: the
    // old window stays on screen and the next page is requested.
    assert!(app.apply_transcript(rid, backward_page(120..161, true)));
    assert_eq!(ids(&app).len(), 60, "old window shown until the swap");
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(120)), PageDirection::Backward)
    );
    // The user moves beyond the original target while staging is in flight.
    app.inspector_move(-20); // e110
    let mut older = backward_page(95..120, true);
    older.events[5].text = Some(text("BOUNDARY-CHANGED")); // e100
    assert!(app.apply_transcript(rid, older));

    let t = app.transcript().unwrap();
    assert_eq!(t.selected_event().unwrap().id, "e110");
    let loaded = ids(&app);
    assert_eq!(loaded.first().unwrap(), "e95");
    assert_eq!(loaded.last().unwrap(), "e160");
    let unique: HashSet<_> = loaded.iter().collect();
    assert_eq!(unique.len(), loaded.len());
    assert!(t.has_older() && !t.has_newer());
    let e100 = t.events().iter().find(|e| e.id == "e100").unwrap();
    assert_eq!(e100.text.as_ref().unwrap().preview, "BOUNDARY-CHANGED");
}

#[test]
fn invalidation_while_reading_history_reloads_from_the_tail() {
    let mut app = app_with(summary(200));
    // The newest window starts at e140; history before it is still unread.
    open(&mut app, backward_page(140..200, true));
    app.inspector_move(-90);
    let selected = app
        .transcript()
        .unwrap()
        .selected_event()
        .unwrap()
        .id
        .clone();
    app.take_requests(); // the near-edge older load is superseded by the reload
    app.invalidate_transcript(&key());
    // No newer page exists, so the reload re-reads backward from the tail.
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    let mut page = backward_page(140..200, true);
    for event in &mut page.events {
        if event.id == selected {
            event.text = Some(text("CHANGED"));
        }
    }
    assert!(app.apply_transcript(rid, page));
    let t = app.transcript().unwrap();
    assert_eq!(t.selected_event().unwrap().id, selected);
    assert_eq!(
        t.selected_event().unwrap().text.as_ref().unwrap().preview,
        "CHANGED"
    );
    assert!(!t.has_newer());
}

#[test]
fn a_reload_beyond_the_window_cap_is_visible_and_retry_jumps_to_the_tail() {
    let cap = MAX_LOADED_EVENTS as u64;
    let mut app = app_with(summary(40_000));
    open(&mut app, forward_page(0..1, false));
    app.inspector_last();
    let (id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(id, backward_page(10_000..10_000 + cap, true)));
    app.inspector_move(isize::MIN); // e10000, the reading position
    let (older, _, _) = load_page(&app.take_requests());
    app.transcript_load_failed(older); // keep the window as it is
    app.invalidate_transcript(&key());
    let (rid, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    // 10,000 events arrived: the position is now 30,000 events from the tail.
    assert!(app.apply_transcript(rid, backward_page(30_000..40_000, true)));
    let (rid, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(rid, backward_page(20_000..30_000, true)));

    let t = app.transcript().unwrap();
    assert!(t.failed().is_some() && t.reload_capped());
    assert!(t.loading().is_none());
    assert_eq!(t.selected_event().unwrap().id, "e10000", "position kept");
    assert_eq!(t.events().len(), MAX_LOADED_EVENTS);
    let out = draw(&app, 110, 30);
    assert!(out.contains("loaded-event limit"), "{out}");

    app.retry_transcript();
    let (tail, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    assert!(app.apply_transcript(tail, backward_page(30_000..40_000, true)));
    let t = app.transcript().unwrap();
    assert!(t.failed().is_none() && !t.reload_capped());
    assert_eq!(t.selected_event().unwrap().id, "e39999");
}

#[test]
fn opening_expands_latest_row() {
    let mut app = app_with(summary(3));
    open(&mut app, backward_page(0..3, false));

    let t = app.transcript().unwrap();
    assert_eq!(t.selected_event().unwrap().id, "e2", "opens on the newest");
    assert!(t.following());
    assert!(t.is_expanded(&t.events()[2]), "the latest row is expanded");
    assert!(!t.is_expanded(&t.events()[0]));
    assert!(!t.is_expanded(&t.events()[1]));
}

#[test]
fn new_events_while_following_reload_tail_and_move_expansion() {
    let mut app = app_with(summary(3));
    open(&mut app, backward_page(0..3, false));

    app.apply_roster(vec![summary(5)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    // The count alone pulls the newest page in: no `G` keypress.
    assert!(app.apply_events_after(*request_id, 4));
    let (id, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    assert!(app.apply_transcript(id, backward_page(2..5, false)));

    let t = app.transcript().unwrap();
    assert_eq!(t.selected_event().unwrap().id, "e4", "lands on the newest");
    assert!(t.following());
    assert!(
        t.is_expanded(&t.events()[2]),
        "the new latest row is expanded"
    );
    assert!(!t.is_expanded(&t.events()[0]), "the old one collapses");
    assert_eq!(app.new_event_count(), 0);
}

#[test]
fn moving_up_stops_following() {
    let mut app = app_with(summary(3));
    open(&mut app, backward_page(0..3, false));
    assert!(app.transcript().unwrap().following());

    app.inspector_move(-1);
    let t = app.transcript().unwrap();
    assert!(!t.following());
    assert_eq!(t.selected_event().unwrap().id, "e1");

    // An arrival only raises the count once the reader has scrolled away.
    app.apply_roster(vec![summary(5)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert!(app.apply_events_after(*request_id, 4));
    assert!(page_requests(&app.take_requests()).is_empty());
    assert_eq!(app.new_event_count(), 2);

    // Moving back onto the newest row follows again.
    app.inspector_move(1);
    assert!(app.transcript().unwrap().following());
}

#[test]
fn manual_collapse_of_latest_sticks_until_next_append() {
    let mut app = app_with(summary(3));
    open(&mut app, backward_page(0..3, false));
    app.inspector_toggle();
    let t = app.transcript().unwrap();
    assert!(!t.is_expanded(&t.events()[2]), "collapsed by hand");
    assert!(t.following(), "collapsing does not stop following");

    // Moving within the newest row does not expand it again.
    app.inspector_move(0);
    let t = app.transcript().unwrap();
    assert!(!t.is_expanded(&t.events()[2]));

    // The next append expands the new latest row, not the collapsed one.
    app.apply_roster(vec![summary(4)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert!(app.apply_events_after(*request_id, 3));
    let (id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(id, backward_page(2..4, false)));

    let t = app.transcript().unwrap();
    assert_eq!(t.selected_event().unwrap().id, "e3");
    assert!(
        t.is_expanded(&t.events()[1]),
        "the new latest row is expanded"
    );
    assert!(
        !t.is_expanded(&t.events()[0]),
        "the collapsed row stays shut"
    );
}

#[test]
fn reload_keeps_follow_state() {
    let mut app = app_with(summary(3));
    open(&mut app, backward_page(0..3, false));

    app.invalidate_transcript(&key());
    let (id, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    let mut page = backward_page(0..3, false);
    page.events[2].text = Some(text("CHANGED"));
    assert!(app.apply_transcript(id, page));
    let t = app.transcript().unwrap();
    assert!(t.following());
    assert_eq!(t.selected_event().unwrap().id, "e2", "the position is kept");
    assert!(t.is_expanded(&t.events()[2]));
    assert_eq!(
        t.selected_event().unwrap().text.as_ref().unwrap().preview,
        "CHANGED"
    );

    // A reload that reaches the tail while following lands on the newest row but
    // leaves the automatic expansion where it was (only an append moves it).
    app.invalidate_transcript(&key());
    let (id, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    assert!(app.apply_transcript(id, backward_page(2..5, false)));
    let t = app.transcript().unwrap();
    assert!(t.following());
    assert_eq!(t.selected_event().unwrap().id, "e4");
    assert!(
        t.is_expanded(&t.events()[0]),
        "the automatic expansion stays on the row it named (e2)"
    );
    assert!(
        !t.is_expanded(&t.events()[2]),
        "the reload adds no expansion"
    );
}

#[test]
fn manual_collapse_of_latest_survives_an_unchanged_reload() {
    let mut app = app_with(summary(3));
    open(&mut app, backward_page(0..3, false));
    app.inspector_toggle();
    assert!(
        !app.transcript()
            .unwrap()
            .is_expanded(&app.transcript().unwrap().events()[2])
    );

    app.invalidate_transcript(&key());
    let (id, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    assert!(app.apply_transcript(id, backward_page(0..3, false)));
    let t = app.transcript().unwrap();
    assert!(t.following(), "the reload keeps following");
    assert_eq!(t.selected_event().unwrap().id, "e2");
    assert!(
        !t.is_expanded(&t.events()[2]),
        "an ordinary reload preserves the manual collapse"
    );
}

#[test]
fn a_count_during_a_page_load_is_fetched_once_that_load_settles() {
    let mut app = app_with(summary(60));
    // A short tail window still has older history: opening it requests the older
    // page automatically while following.
    open(&mut app, backward_page(57..60, true));
    let (older_id, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(57)), PageDirection::Backward)
    );

    // Two events are appended while that page is in flight.
    app.apply_roster(vec![summary(62)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert!(app.apply_events_after(*request_id, 4));
    assert!(
        page_requests(&app.take_requests()).is_empty(),
        "no page load while another is in flight"
    );

    // The older page lands: the arrivals were not loaded, so the count stays
    // and the owed newest page is requested as soon as the load settles.
    assert!(app.apply_transcript(older_id, backward_page(20..57, true)));
    let t = app.transcript().unwrap();
    assert!(t.following());
    assert_eq!(
        t.selected_event().unwrap().id,
        "e59",
        "still on the newest row"
    );
    assert_eq!(app.new_event_count(), 2, "the arrivals were not loaded");
    let (tail_id, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));

    // The newest page arrives: the arrivals are on screen and the new latest
    // row is expanded.
    assert!(app.apply_transcript(tail_id, backward_page(22..62, true)));
    let t = app.transcript().unwrap();
    assert_eq!(t.selected_event().unwrap().id, "e61");
    assert!(t.following());
    assert!(t.is_expanded(&t.events()[39]));
    assert_eq!(app.new_event_count(), 0);
}

#[test]
fn a_failed_page_load_still_fetches_the_owed_newest_page() {
    let mut app = app_with(summary(60));
    open(&mut app, backward_page(57..60, true));
    let (older_id, _, _) = load_page(&app.take_requests());

    // Two events are appended while the older page is in flight.
    app.apply_roster(vec![summary(62)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert!(app.apply_events_after(*request_id, 4));
    assert!(page_requests(&app.take_requests()).is_empty());

    // The older page fails: the catch-up is still owed, so the newest page is
    // read now instead of waiting for another arrival or a keypress.
    app.transcript_load_failed(older_id);
    assert_eq!(app.new_event_count(), 2, "the arrivals are still unread");
    let (tail_id, from, direction) = load_page(&app.take_requests());
    assert_eq!((from, direction), (None, PageDirection::Backward));
    assert!(app.apply_transcript(tail_id, backward_page(22..62, true)));
    let t = app.transcript().unwrap();
    assert!(t.failed().is_none());
    assert_eq!(t.selected_event().unwrap().id, "e61");
    assert!(t.is_expanded(&t.events()[39]));
    assert_eq!(app.new_event_count(), 0);
}

#[test]
fn restart_keeps_following_on_the_new_latest_row() {
    let mut app = app_with(summary(3));
    open(&mut app, backward_page(0..3, false));
    assert!(app.transcript().unwrap().following());

    app.restart_transcript();
    let requests = app.take_requests();
    let [WorkerRequest::Select { request_id, .. }] = requests.as_slice() else {
        panic!("expected Select after restart, got {requests:?}");
    };
    // An event arrived while the worker was replaced.
    assert!(app.apply_transcript(*request_id, backward_page(1..4, false)));
    let t = app.transcript().unwrap();
    assert!(t.following(), "a follower keeps following across a restart");
    assert_eq!(t.selected_event().unwrap().id, "e3");
    assert!(
        t.is_expanded(&t.events()[2]),
        "the new latest row is expanded"
    );
}

#[test]
fn search_navigation_updates_follow_state() {
    let mut app = app_with(summary(3));
    let events = vec![
        event(0, EventKind::UserPrompt, "NEEDLE one"),
        msg(1),
        event(2, EventKind::AssistantMessage, "NEEDLE two"),
    ];
    open(&mut app, page(events, Some(anchor(0)), false));
    assert!(app.transcript().unwrap().following());

    // The search starts on the newest row and its first match keeps following.
    assert!(app.begin_search());
    for c in "NEEDLE".chars() {
        app.search_push(c);
    }
    assert_eq!(app.transcript().unwrap().selected_event().unwrap().id, "e2");
    assert!(app.transcript().unwrap().following());

    // Stepping to an older match stops following, so an arrival is only
    // counted instead of pulling the newest page in.
    app.search_step(false);
    assert_eq!(app.transcript().unwrap().selected_event().unwrap().id, "e0");
    assert!(!app.transcript().unwrap().following());
    app.search_commit();
    app.apply_roster(vec![summary(5)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("expected CountAfter, got {requests:?}");
    };
    assert!(app.apply_events_after(*request_id, 4));
    assert!(page_requests(&app.take_requests()).is_empty());
    assert_eq!(app.new_event_count(), 2);

    // Searching forward wraps onto the newest row and follows again.
    app.search_step(true);
    assert_eq!(app.transcript().unwrap().selected_event().unwrap().id, "e2");
    assert!(app.transcript().unwrap().following());
}

#[test]
fn moves_skip_folded_events() {
    let events = vec![
        event(0, EventKind::UserPrompt, "go"),
        auxiliary(1, "title"),
        auxiliary(2, "title"),
        auxiliary(3, "title"),
        msg(4),
        auxiliary(5, "title"),
        auxiliary(6, "title"),
    ];
    let mut app = app_with(summary(7));
    open(&mut app, page(events, None, false));

    let t = app.transcript().unwrap();
    assert_eq!(t.selected(), 5, "the latest row starts at its first event");
    assert!(t.following());
    assert!(t.is_expanded(&t.events()[5]));
    assert!(!t.is_expanded(&t.events()[6]), "only the row's first event");

    app.inspector_move(-1);
    assert_eq!(app.transcript().unwrap().selected(), 4);
    app.inspector_move(-1);
    assert_eq!(
        app.transcript().unwrap().selected(),
        1,
        "a folded run is one row"
    );
    app.inspector_move(-1);
    assert_eq!(app.transcript().unwrap().selected(), 0);
    app.inspector_move(-1);
    assert_eq!(app.transcript().unwrap().selected(), 0, "clamped");
    assert!(!app.transcript().unwrap().following());

    app.inspector_move(1);
    assert_eq!(app.transcript().unwrap().selected(), 1);
    app.inspector_move(1);
    assert_eq!(app.transcript().unwrap().selected(), 4);
    app.inspector_move(1);
    assert_eq!(app.transcript().unwrap().selected(), 5);
    assert!(app.transcript().unwrap().following());
}

fn tail_at_60() -> App {
    let mut app = app_with(summary(60));
    open(&mut app, backward_page(20..60, true));
    app
}

#[test]
fn an_outstanding_count_survives_an_older_load() {
    let mut app = tail_at_60();
    app.apply_roster(vec![summary(63)], vec![]);
    let requests = app.take_requests();
    let [WorkerRequest::CountAfter { request_id, .. }] = requests.as_slice() else {
        panic!("{requests:?}");
    };
    app.inspector_move(isize::MIN);
    let (older_id, _, _) = load_page(&app.take_requests());
    assert!(
        app.apply_events_after(*request_id, 42),
        "count not discarded"
    );
    assert_eq!(app.new_event_count(), 3);
    assert!(app.apply_transcript(older_id, backward_page(0..20, false)));
    assert_eq!(app.new_event_count(), 3);
}

#[test]
fn arrivals_are_counted_while_older_history_is_displayed() {
    let mut app = tail_at_60();
    app.inspector_move(isize::MIN);
    let (older_id, _, _) = load_page(&app.take_requests());
    assert!(app.apply_transcript(older_id, backward_page(0..20, false)));

    app.apply_roster(vec![summary(66)], vec![]);
    let requests = app.take_requests();
    let [
        WorkerRequest::CountAfter {
            request_id,
            anchor: a,
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("{requests:?}");
    };
    assert_eq!(a, &anchor(20));
    assert!(app.apply_events_after(*request_id, 45));
    assert_eq!(app.new_event_count(), 6);
}

#[test]
fn r_retries_a_failed_older_or_newer_load() {
    let mut app = tail_at_60();
    app.inspector_move(isize::MIN);
    let (id, from, _) = load_page(&app.take_requests());
    assert_eq!(from, Some(anchor(20)));
    app.transcript_load_failed(id);
    assert!(app.transcript().unwrap().failed().is_some());
    app.inspector_move(1);
    assert!(
        page_requests(&app.take_requests()).is_empty(),
        "failed edge load is not auto-retried"
    );

    app.retry_transcript();
    let (retry, from, direction) = load_page(&app.take_requests());
    assert_eq!(
        (from, direction),
        (Some(anchor(20)), PageDirection::Backward)
    );
    assert_ne!(retry, id);
    assert!(app.transcript().unwrap().failed().is_none());
    assert!(app.apply_transcript(retry, backward_page(0..20, false)));
    assert_eq!(app.transcript().unwrap().events().len(), 60);
}

#[test]
fn g_reloads_the_tail_even_when_the_row_count_is_unchanged() {
    let mut app = app_with(summary(3));
    open(&mut app, forward_page(0..3, false));
    app.inspector_last();
    let (_, anchor_sent, direction) = load_page(&app.take_requests());
    assert_eq!((anchor_sent, direction), (None, PageDirection::Backward));
}

#[test]
fn keeps_the_selected_row_visible_when_expanded_rows_fill_the_viewport() {
    let mut app = app_with(summary(5));
    let long: String = (0..8).map(|i| format!("line {i}\n")).collect();
    let events: Vec<Event> = (0..4)
        .map(|i| event(i, EventKind::AssistantMessage, &format!("{long}tail {i}")))
        .chain([event(4, EventKind::UserPrompt, "SELECTED-ROW")])
        .collect();
    open(&mut app, page(events, None, false));
    for _ in 0..4 {
        app.inspector_toggle();
        app.inspector_move(1);
    }
    assert_eq!(app.transcript().unwrap().selected(), 4);
    let out = draw(&app, 80, 20);
    assert!(out.contains("> "), "{out}");
    assert!(out.contains("SELECTED-ROW"), "{out}");
}

#[test]
fn a_failed_initial_load_is_shown_and_retried() {
    let mut app = app_with(summary(1));
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("{requests:?}");
    };
    app.transcript_load_failed(*request_id);
    let out = draw(&app, 110, 30);
    assert!(!out.contains("Loading…"), "{out}");
    assert!(out.contains("Could not load this transcript"), "{out}");
    assert!(out.contains("r retries"), "{out}");

    app.retry_transcript();
    let requests = app.take_requests();
    let [
        WorkerRequest::Select {
            request_id: retry, ..
        },
    ] = requests.as_slice()
    else {
        panic!("expected a new Select, got {requests:?}");
    };
    assert_ne!(retry, request_id);
    assert!(app.apply_transcript(*retry, page(vec![msg(0)], None, false)));
    let out = draw(&app, 110, 30);
    assert!(out.contains("message 0"), "{out}");
    assert!(!out.contains("r retries"), "{out}");
}

#[test]
fn a_superseded_load_failure_does_not_abort_the_current_request() {
    let mut app = app_with(summary(20));
    open(&mut app, forward_page(0..5, true));
    let (newer_id, _, _) = load_page(&app.take_requests());
    app.inspector_last(); // supersedes the newer-page load
    let (tail_id, _, _) = load_page(&app.take_requests());
    assert_ne!(newer_id, tail_id);

    app.transcript_load_failed(newer_id);
    assert!(app.transcript().unwrap().failed().is_none());
    assert!(app.transcript().unwrap().loading().is_some());
    assert!(app.apply_transcript(tail_id, backward_page(15..20, false)));
    assert_eq!(ids(&app).len(), 5);

    // A failure for the request that is current does abort it.
    app.inspector_last();
    let (current, _, _) = load_page(&app.take_requests());
    app.transcript_load_failed(current);
    assert!(app.transcript().unwrap().failed().is_some());
}

#[test]
fn a_replacement_page_larger_than_the_window_is_rejected() {
    let mut app = app_with(summary(1));
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("{requests:?}");
    };
    let big = backward_page(0..(MAX_LOADED_EVENTS as u64 + 1), true);
    assert!(app.apply_transcript(*request_id, big));
    assert!(ids(&app).is_empty());
    assert!(app.transcript().unwrap().failed().is_some());
    // A page of exactly the cap fits and keeps its continuation anchor.
    app.retry_transcript();
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("{requests:?}");
    };
    let full = backward_page(0..MAX_LOADED_EVENTS as u64, true);
    assert!(app.apply_transcript(*request_id, full));
    assert_eq!(ids(&app).len(), MAX_LOADED_EVENTS);
    assert!(app.transcript().unwrap().has_older());
}

#[test]
fn loaded_events_stay_bounded_and_pages_stay_contiguous() {
    let cap = MAX_LOADED_EVENTS as u64;
    let half = cap / 2;
    let mut app = app_with(summary(cap as usize));
    open(&mut app, backward_page(half..cap, true));
    app.inspector_move(isize::MIN);
    let (id, from, direction) = load_page(&app.take_requests());
    assert_eq!(direction, PageDirection::Backward);
    assert_eq!(from, Some(anchor(half)));
    assert!(app.apply_transcript(id, backward_page(0..half, false)));

    let loaded = ids(&app);
    assert!(loaded.len() <= MAX_LOADED_EVENTS, "{}", loaded.len());
    // The window stays one contiguous run after trimming to the cap.
    let first: u64 = loaded[0][1..].parse().unwrap();
    for (i, id) in loaded.iter().enumerate() {
        assert_eq!(id, &format!("e{}", first + i as u64));
    }
    let unique: HashSet<_> = loaded.iter().collect();
    assert_eq!(unique.len(), loaded.len());
}

#[test]
fn escape_returns_to_the_roster() {
    let mut app = app_with(summary(1));
    open(&mut app, forward_page(0..1, false));
    app.close_inspector();
    assert!(app.transcript().is_none());
    let out = draw(&app, 110, 30);
    assert!(out.contains("AgentScope"), "{out}");
}

fn populated_usage() -> UsageSummary {
    UsageSummary {
        models: vec![ModelUsageSummary {
            model: ModelDetail {
                id: "claude-sonnet-test".into(),
                provider: None,
                variant: None,
                effort: None,
                configured_effort: None,
            },
            categories: TokenCategories::default(),
            total: None,
            cost: CostEstimate::default(),
            coverage: UsageCoverage::default(),
        }],
        categories: TokenCategories {
            input: Some(1_200),
            cached_input: None,
            cache_write: Some(300),
            cache_write_5m: Some(200),
            cache_write_1h: Some(100),
            cache_read: Some(40_000),
            output: Some(5_555),
            reasoning: Some(77),
            total: Some(47_055),
            ..TokenCategories::default()
        },
        total: Some(47_055),
        class: AggregationClass::Reported,
        cost: CostEstimate {
            usd: Some(1.25),
            class: AggregationClass::Estimated,
            as_of: Some("2026-10-01".into()),
            note: Some("server-tool fees excluded".into()),
            ..CostEstimate::default()
        },
        ..UsageSummary::default()
    }
}

fn open_empty(app: &mut App) {
    open(app, page(vec![msg(0)], None, false));
}

#[test]
fn session_info_shows_categories_dated_pricing_and_fee_exclusion_note() {
    let mut s = summary(1);
    s.usage = Some(populated_usage());
    let mut app = app_with(s);
    open_empty(&mut app);
    assert!(
        !draw(&app, 120, 30).contains("Cache read"),
        "hidden until opened"
    );
    assert!(app.open_session_info());
    assert_eq!(app.screen, agentscope::app::Screen::Session);
    let screen = draw(&app, 140, 40);
    for needle in [
        "Input: 1,200",
        "Cached input: —",
        "Cache write: 300",
        "Cache write 5m: 200",
        "Cache write 1h: 100",
        "Cache read: 40,000",
        "Output: 5,555",
        "Reasoning: 77",
        "Total: 47,055",
        "Pricing as of: 2026-10-01",
        "Cost note: server-tool fees excluded",
        "Priced model: claude-sonnet-test",
        "web_search 3",
    ] {
        assert!(screen.contains(needle), "missing {needle:?}:\n{screen}");
    }
    // Esc returns to the inspector.
    assert!(app.close_overlay());
    assert_eq!(app.screen, agentscope::app::Screen::Inspector);
}

#[test]
fn session_info_is_reachable_by_key_and_listed_in_the_inspector_footer() {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let mut app = app_with(summary(1));
    open_empty(&mut app);
    assert!(draw(&app, 120, 30).contains("i session"));
    agentscope::ui::handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('i'), KeyModifiers::NONE),
    );
    assert_eq!(app.screen, agentscope::app::Screen::Session);
    agentscope::ui::handle_key(&mut app, KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    assert_eq!(app.screen, agentscope::app::Screen::Inspector);
}

#[test]
fn session_info_renders_resolved_ambiguous_and_unresolved_lineage() {
    let parent = SessionKey {
        harness: Harness::Codex,
        native_id: "parent-7".into(),
        source: SourceKey::File("parent-7.jsonl".into()),
    };
    let cases = [
        (
            Some(SessionLink::Resolved(parent)),
            None,
            true,
            [
                "Parent: resolved — Codex parent-7",
                "Fork: none",
                "Sidechain: yes",
            ],
        ),
        (
            Some(SessionLink::Ambiguous),
            Some(SessionLink::Unresolved("fork-9".into())),
            false,
            [
                "Parent: ambiguous",
                "Fork: unresolved — id fork-9",
                "Sidechain: no",
            ],
        ),
    ];
    for (parent, fork, sidechain, expected) in cases {
        let mut s = summary(1);
        s.meta.lineage = Lineage {
            parent,
            fork,
            sidechain,
            ..Lineage::default()
        };
        let mut app = app_with(s);
        open_empty(&mut app);
        assert!(app.open_session_info());
        let screen = draw(&app, 120, 40);
        for needle in expected {
            assert!(screen.contains(needle), "missing {needle:?}:\n{screen}");
        }
    }
}

#[test]
fn event_details_show_per_event_usage_categories() {
    let mut e = event(0, EventKind::AssistantMessage, "answer");
    e.usage = Some(UsageObservation {
        model: None,
        scope: UsageScope::Message,
        categories: populated_usage().categories,
        accounting: TokenAccounting::default(),
        request: None,
        money: RecordedMoney::default(),
        provenance: provenance(),
    });
    let mut app = app_with(summary(1));
    open(&mut app, page(vec![e], None, false));
    assert!(app.open_details());
    let screen = draw(&app, 160, 30);
    assert!(screen.contains("Usage (message):"), "{screen}");
    assert!(screen.contains("cache read 40,000"), "{screen}");
}

fn measured_model(id: &str) -> ModelDetail {
    ModelDetail {
        id: id.into(),
        provider: None,
        variant: None,
        effort: None,
        configured_effort: None,
    }
}

#[test]
fn session_info_renders_measured_speed_context_timing_and_compaction_facts() {
    let mut s = summary(1);
    s.metrics = SessionMetrics {
        timing: SessionTiming {
            started_at: Some(NOW - 3_600_000),
            ended_at: None,
            elapsed_ms: None,
            active_ms: Some(1_500),
            model_ms: Some(70_000.0),
            active_complete: false,
        },
        speed: SpeedSummary {
            latest: Some(TimedRate {
                model: Some(measured_model("gpt-5.0-codex")),
                request_id: Some("req-9".into()),
                started_at: Some(NOW - 20_000),
                completed_at: Some(NOW - 1_000),
                output_tokens: 1_600,
                duration_ms: 19_000.0,
                ttft_ms: Some(400.0),
                tokens_per_second: 1_600.0 / 19.0,
            }),
            weighted_tokens_per_second: Some(5_500.0 / 70.0),
            measured_output: 5_500,
            measured_duration_ms: 70_000.0,
            measured_requests: 5,
            total_requests: 8,
            complete: false,
        },
        context: ContextUsage {
            used: Some(215_000),
            capacity: Some(1_000_000),
            class: AggregationClass::Reported,
            recorded_at: Some(NOW - 5_000),
            model: Some(measured_model("gpt-5.1-codex")),
            compaction_epoch: Some("c2".into()),
        },
        compactions: CompactionSummary {
            observed: 2,
            history_complete: true,
            known_cost_usd: None,
            cost_scope: CompactionScope::Unknown,
            unpriced: 1,
            latest: Some(CompactionObservation {
                id: "compact-1".into(),
                timestamp: Some(NOW - 10_000),
                method: None,
                trigger: Some("auto".into()),
                summary: None,
                before_tokens: Some(180_000),
                after_tokens: Some(42_000),
                duration_ms: None,
                usage: None,
                scope: CompactionScope::Event,
                provenance: provenance(),
            }),
        },
    };
    let mut app = app_with(s);
    open_empty(&mut app);
    assert!(app.open_session_info());
    let screen = draw(&app, 160, 60);
    for needle in [
        "# Measured",
        "Elapsed: Unknown",
        "Active: 1s (incomplete coverage)",
        "Model time: 1m 10s",
        "Latest speed: 84.2 tok/s — gpt-5.0-codex (measured on an earlier model) at ",
        "Latest request: 1,600 output in 19s · TTFT 400 ms",
        "Weighted speed: 78.6 tok/s — 5,500 output over 1m 10s across 5 of 8 requests (incomplete coverage)",
        "Context: 215,000 of 1.0M window (reported, recorded ",
        "epoch c2",
        "Compactions: 2 observed · cost Unknown · 1 unpriced",
        "Latest compaction: ",
        "trigger auto · tokens removed 138,000",
        "Cost by category: input Unknown",
    ] {
        assert!(screen.contains(needle), "missing {needle:?} in:\n{screen}");
    }
}

#[test]
fn session_info_shows_unmeasured_facts_as_unknown_not_zero() {
    let mut s = summary(1);
    s.metrics = SessionMetrics::default();
    let mut app = app_with(s);
    open_empty(&mut app);
    assert!(app.open_session_info());
    let screen = draw(&app, 160, 60);
    for needle in [
        "Started: Unknown",
        "Active: Unknown (incomplete coverage)",
        "Latest speed: Unknown",
        "Weighted speed: Unknown — no measured requests",
        "Context: Unknown of Unknown window",
        "Compactions: 0 observed (history incomplete) · cost Unknown · 0 unpriced",
    ] {
        assert!(screen.contains(needle), "missing {needle:?} in:\n{screen}");
    }
    assert!(!screen.contains("0 tok/s"), "{screen}");
}

#[test]
fn session_info_never_shows_a_tiny_positive_cost_as_free() {
    let mut s = summary(1);
    let usage = s.usage.as_mut().expect("usage fixture");
    usage.cost.usd = Some(0.00003);
    usage.cost.breakdown.input = Some(0.00003);
    s.metrics.compactions = CompactionSummary {
        observed: 1,
        history_complete: true,
        known_cost_usd: Some(0.00003),
        cost_scope: CompactionScope::Event,
        unpriced: 0,
        latest: None,
    };
    let mut app = app_with(s);
    open_empty(&mut app);
    assert!(app.open_session_info());
    let screen = draw(&app, 160, 60);
    for needle in [
        "Cost by category: input <$0.0001",
        "Compactions: 1 observed · cost <$0.0001 · 0 unpriced",
    ] {
        assert!(screen.contains(needle), "missing {needle:?} in:\n{screen}");
    }
    assert!(!screen.contains("input $0.00"), "{screen}");
}

#[test]
fn incomplete_compaction_history_labels_money_as_a_subtotal_and_unknown_model_is_not_stale() {
    let mut s = summary(1);
    s.meta.model = None;
    s.metrics.speed = SpeedSummary {
        latest: Some(TimedRate {
            model: Some(measured_model("gpt-5.0-codex")),
            request_id: None,
            started_at: None,
            completed_at: Some(NOW - 1_000),
            output_tokens: 100,
            duration_ms: 2_000.0,
            ttft_ms: None,
            tokens_per_second: 50.0,
        }),
        weighted_tokens_per_second: Some(50.0),
        measured_output: 100,
        measured_duration_ms: 2_000.0,
        measured_requests: 1,
        total_requests: 1,
        complete: true,
    };
    s.metrics.compactions = CompactionSummary {
        observed: 256,
        history_complete: false,
        known_cost_usd: Some(0.5),
        cost_scope: CompactionScope::Event,
        unpriced: 0,
        latest: None,
    };
    let mut app = app_with(s);
    open_empty(&mut app);
    assert!(app.open_session_info());
    let screen = draw(&app, 160, 60);
    assert!(
        !screen.contains("measured on an earlier model"),
        "an unknown current model is not evidence of a stale rate:\n{screen}"
    );
    assert!(
        screen.contains("Compactions: 256 observed (history incomplete) · cost $0.50 known subtotal (not a total) · 0 unpriced"),
        "{screen}"
    );
}

fn usage_with_purpose(purpose: RequestPurpose) -> UsageObservation {
    UsageObservation {
        model: None,
        scope: UsageScope::Message,
        categories: TokenCategories::default(),
        accounting: TokenAccounting::default(),
        request: Some(RequestMetadata {
            purpose,
            ..RequestMetadata::default()
        }),
        money: RecordedMoney::default(),
        provenance: provenance(),
    }
}

/// A System notice shaped like an OMP housekeeping record.
fn notice(i: u64, record_type: &str, body: &str) -> Event {
    let mut e = event(i, EventKind::System, body);
    e.provenance.record_type = record_type.into();
    e
}

/// An auxiliary model call: usages with an `Auxiliary` purpose.
fn auxiliary(i: u64, purpose: &str) -> Event {
    let mut e = event(
        i,
        EventKind::System,
        &format!("auxiliary model call: {purpose}"),
    );
    e.usage = Some(usage_with_purpose(RequestPurpose::Auxiliary(
        purpose.into(),
    )));
    e
}

fn empty_reasoning(i: u64) -> Event {
    let mut e = event(i, EventKind::Reasoning, "");
    e.text = None;
    e
}

fn tool_fixture(input: Option<&str>) -> ToolInteraction {
    ToolInteraction {
        name: "shell".into(),
        call_id: None,
        input: input.map(text),
        output: None,
        status: ToolStatus::Completed,
        exit_code: None,
        started_at: None,
        completed_at: None,
        orphan: false,
        child_session: None,
    }
}

#[test]
fn prompt_starts_turn_and_work_is_indented() {
    let events = [
        event(0, EventKind::System, "preamble"),
        event(1, EventKind::UserPrompt, "fix the build"),
        event(2, EventKind::Reasoning, "reading files"),
        event(3, EventKind::Tool, ""),
        event(4, EventKind::AssistantMessage, "done"),
        event(5, EventKind::Lifecycle, "session exit"),
    ];
    let rows = turn_rows(&events);
    assert_eq!(rows.len(), 6);
    assert_eq!(rows[0].kind, RowKind::Other);
    assert_eq!((rows[0].first, rows[0].last, rows[0].depth), (0, 0, 0));
    assert_eq!(rows[1].kind, RowKind::Prompt);
    assert_eq!((rows[1].first, rows[1].last, rows[1].depth), (1, 1, 0));
    assert_eq!(rows[2].kind, RowKind::Reasoning);
    assert_eq!(rows[2].depth, 1);
    assert_eq!(rows[3].kind, RowKind::Tool);
    assert_eq!(rows[3].depth, 1);
    assert_eq!(rows[4].kind, RowKind::Reply);
    assert_eq!((rows[4].first, rows[4].last, rows[4].depth), (4, 4, 0));
    assert_eq!(rows[5].kind, RowKind::Other);
    assert_eq!(rows[5].depth, 1);
}

#[test]
fn auxiliary_runs_fold_into_one_row() {
    let events = [
        event(0, EventKind::UserPrompt, "summarize"),
        auxiliary(1, "title"),
        auxiliary(2, "suggestion"),
        auxiliary(3, "title"),
    ];
    let rows = turn_rows(&events);
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[1].first, rows[1].last, rows[1].depth), (1, 3, 1));
    match &rows[1].kind {
        RowKind::Noise { count, label } => {
            assert_eq!(*count, 3);
            assert_eq!(label, "3 auxiliary model calls (title)");
        }
        other => panic!("expected one folded noise row, got {other:?}"),
    }

    // A visible row ends the run: two separate calls must not merge.
    let separated = turn_rows(&[auxiliary(0, "title"), msg(1), auxiliary(2, "title")]);
    assert_eq!(separated.len(), 3);
    assert!(matches!(separated[0].kind, RowKind::Noise { count: 1, .. }));
    assert_eq!(separated[1].kind, RowKind::Reply);
    assert!(matches!(separated[2].kind, RowKind::Noise { count: 1, .. }));
}

#[test]
fn noise_fold_never_hides_errors() {
    let mut broken_text = auxiliary(1, "title");
    broken_text.text = Some(text("error: quota exceeded"));
    let mut broken_tool = auxiliary(3, "title");
    broken_tool.tool = Some(ToolInteraction {
        status: ToolStatus::Error,
        ..tool_fixture(None)
    });
    let events = [
        auxiliary(0, "title"),
        broken_text,
        auxiliary(2, "title"),
        broken_tool,
        auxiliary(4, "title"),
    ];
    let rows = turn_rows(&events);
    assert_eq!(rows.len(), 5);
    assert!(matches!(rows[0].kind, RowKind::Noise { count: 1, .. }));
    assert_eq!(rows[1].kind, RowKind::Other);
    assert_eq!((rows[1].first, rows[1].last), (1, 1));
    assert!(matches!(rows[2].kind, RowKind::Noise { count: 1, .. }));
    assert_eq!(rows[3].kind, RowKind::Other);
    assert_eq!((rows[3].first, rows[3].last), (3, 3));
    assert!(matches!(rows[4].kind, RowKind::Noise { count: 1, .. }));
}

#[test]
fn noise_fold_never_hides_system_error_without_tool() {
    let events = [
        notice(0, "model_usage", "auxiliary model call: title"),
        notice(1, "model_usage", "error: quota exceeded"),
        notice(2, "model_usage", "auxiliary model call: title"),
    ];
    let rows = turn_rows(&events);
    assert_eq!(rows.len(), 3);
    assert!(matches!(rows[0].kind, RowKind::Noise { count: 1, .. }));
    assert_eq!(rows[1].kind, RowKind::Other);
    assert_eq!((rows[1].first, rows[1].last), (1, 1));
    assert!(matches!(rows[2].kind, RowKind::Noise { count: 1, .. }));
}

#[test]
fn each_noise_record_type_folds() {
    assert_eq!(
        NOISE_RECORD_TYPES,
        &["model_usage", "service_tier_change", "mode_change"]
    );
    for &record_type in NOISE_RECORD_TYPES {
        let events = [
            notice(0, record_type, "notice"),
            notice(1, record_type, "another notice"),
        ];
        let rows = turn_rows(&events);
        assert_eq!(rows.len(), 1, "{record_type}");
        match &rows[0].kind {
            RowKind::Noise { count, label } => {
                assert_eq!(*count, 2, "{record_type}");
                assert_eq!(label, "2 system notices", "{record_type}");
            }
            other => panic!("{record_type} did not fold: {other:?}"),
        }
    }
}

#[test]
fn events_before_first_prompt_are_other_at_depth_zero() {
    let mut compacted = event(2, EventKind::Compaction, "compacted");
    // Even auxiliary-tagged usage never turns a compaction into noise.
    compacted.usage = Some(usage_with_purpose(RequestPurpose::Auxiliary(
        "title".into(),
    )));
    let events = [
        event(0, EventKind::Lifecycle, "session started"),
        event(1, EventKind::System, "developer instructions"),
        compacted,
        event(3, EventKind::UserPrompt, "go"),
        event(4, EventKind::System, "developer instructions"),
        event(5, EventKind::Lifecycle, "session exit"),
    ];
    let rows = turn_rows(&events);
    assert_eq!(rows.len(), 6);
    for row in &rows[..3] {
        assert_eq!(row.kind, RowKind::Other);
        assert_eq!(row.depth, 0);
    }
    assert_eq!(rows[3].kind, RowKind::Prompt);
    assert_eq!(rows[3].depth, 0);
    assert_eq!(rows[4].kind, RowKind::Other);
    assert_eq!(rows[4].depth, 1);
    assert_eq!(rows[5].kind, RowKind::Other);
    assert_eq!(rows[5].depth, 1);

    // Agent work is indented even when the window starts after its prompt.
    let isolated = turn_rows(&[event(0, EventKind::Tool, "")]);
    assert_eq!(isolated[0].kind, RowKind::Tool);
    assert_eq!(isolated[0].depth, 1);
}

#[test]
fn turn_row_debug_omits_labels() {
    let rows = turn_rows(&[auxiliary(0, "private-purpose-token")]);
    assert_eq!(rows.len(), 1);
    let debug = format!("{:?}", rows[0]);
    assert!(debug.contains("Noise"), "{debug}");
    assert!(debug.contains("count: 1"), "{debug}");
    assert!(debug.contains("first: 0"), "{debug}");
    assert!(debug.contains("last: 0"), "{debug}");
    assert!(!debug.contains("private-purpose-token"), "{debug}");
    assert!(!debug.contains("label"), "{debug}");
    assert!(!debug.contains("auxiliary model calls"), "{debug}");
}

#[test]
fn empty_reasoning_rows_collapse_with_count() {
    let events = [
        event(0, EventKind::UserPrompt, "go"),
        empty_reasoning(1),
        empty_reasoning(2),
        empty_reasoning(3),
        event(4, EventKind::Reasoning, "thinking"),
        empty_reasoning(5),
        empty_reasoning(6),
        msg(7),
    ];
    let rows = turn_rows(&events);
    assert_eq!(rows.len(), 5);
    match &rows[1].kind {
        RowKind::ReasoningEmpty { count } => assert_eq!(*count, 3),
        other => panic!("expected collapsed empty reasoning, got {other:?}"),
    }
    assert_eq!((rows[1].first, rows[1].last, rows[1].depth), (1, 3, 1));
    assert_eq!(rows[2].kind, RowKind::Reasoning);
    match &rows[3].kind {
        RowKind::ReasoningEmpty { count } => assert_eq!(*count, 2),
        other => panic!("expected collapsed empty reasoning, got {other:?}"),
    }
    assert_eq!((rows[3].first, rows[3].last), (5, 6));
    assert_eq!(rows[4].kind, RowKind::Reply);
}

#[test]
fn failure_bearing_empty_reasoning_stays_standalone() {
    let mut by_type = empty_reasoning(2);
    by_type.provenance.record_type = "reasoning_error".into();

    let mut by_tool = empty_reasoning(2);
    by_tool.usage = Some(usage_with_purpose(RequestPurpose::Auxiliary(
        "title".into(),
    )));
    by_tool.tool = Some(ToolInteraction {
        status: ToolStatus::Error,
        ..tool_fixture(None)
    });

    for failure in [by_type, by_tool] {
        let events = [
            empty_reasoning(0),
            empty_reasoning(1),
            failure,
            empty_reasoning(3),
            empty_reasoning(4),
        ];
        let rows = turn_rows(&events);
        assert_eq!(rows.len(), 3);
        match &rows[0].kind {
            RowKind::ReasoningEmpty { count } => assert_eq!(*count, 2),
            other => panic!("expected the leading run, got {other:?}"),
        }
        assert_eq!((rows[0].first, rows[0].last), (0, 1));
        match &rows[1].kind {
            RowKind::ReasoningEmpty { count } => assert_eq!(*count, 1),
            other => panic!("expected the failure row alone, got {other:?}"),
        }
        assert_eq!((rows[1].first, rows[1].last), (2, 2));
        match &rows[2].kind {
            RowKind::ReasoningEmpty { count } => assert_eq!(*count, 2),
            other => panic!("expected the trailing run, got {other:?}"),
        }
        assert_eq!((rows[2].first, rows[2].last), (3, 4));
    }
}

#[test]
fn tool_summary_prefers_command_then_description() {
    let command = tool_fixture(Some("{\"command\":\"set -e\\ngit add -A\"}"));
    assert_eq!(tool_summary(&command), "set -e git add -A");

    let description = tool_fixture(Some(
        "{\"description\":\"Scoped re-review\",\"context\":\"ignored\"}",
    ));
    assert_eq!(tool_summary(&description), "Scoped re-review");

    let both = tool_fixture(Some("{\"command\":\"run tests\",\"description\":\"desc\"}"));
    assert_eq!(tool_summary(&both), "run tests");

    assert_eq!(tool_summary(&tool_fixture(None)), "");
}

#[test]
fn tool_summary_falls_back_without_json_braces() {
    let truncated = tool_fixture(Some("{\"command\": \"cargo test"));
    assert_eq!(tool_summary(&truncated), "command: cargo test");

    let plain = tool_fixture(Some("cargo test --all"));
    assert_eq!(tool_summary(&plain), "cargo test --all");

    let quoted = tool_fixture(Some("\"quoted value\""));
    assert_eq!(tool_summary(&quoted), "quoted value");
}

#[test]
fn tool_summary_sanitizes() {
    let escaped = tool_fixture(Some(r#"{"command":"echo \u001b[31mred\u001b[0m"}"#));
    let summary = tool_summary(&escaped);
    assert_eq!(summary, "echo red");
    assert!(!summary.contains('\u{1b}'), "{summary:?}");

    let raw = tool_fixture(Some("\u{1b}[31mred\u{1b}[0m"));
    assert_eq!(tool_summary(&raw), "red");
}

#[test]
fn model_and_thinking_changes_stay_visible() {
    for record_type in ["model_change", "thinking_level_change", "session_exit"] {
        let rows = turn_rows(&[notice(0, record_type, "model changed: gpt-5")]);
        assert_eq!(rows.len(), 1, "{record_type}");
        assert_eq!(rows[0].kind, RowKind::Other, "{record_type}");
    }
}
