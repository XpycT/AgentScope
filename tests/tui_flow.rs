//! Keyboard flow: discover, open, expand, search, filter, sort, help, and back.
//! Keys go through `ui::handle_key`; worker events through `ui::apply_worker_event`.

use std::time::Duration;

mod support;

use agentscope::app::{ACTIVITY_DEBOUNCE_MS, App, Panel, RosterItem, Screen};
use agentscope::domain::*;
use agentscope::ui::{KeyAction, RosterContext, apply_worker_event, handle_key, render};
use agentscope::worker::{WorkerEvent, WorkerRequest};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::facts::{self, metadata};

const NOW: Timestamp = 1_800_000_000_000;

fn key_of(provider: Harness, id: &str) -> SessionKey {
    SessionKey {
        harness: provider,
        native_id: id.into(),
        source: SourceKey::File(format!("{id}.jsonl").into()),
    }
}

fn session(
    provider: Harness,
    id: &str,
    title: &str,
    project: &str,
    cwd: &str,
    age_secs: i64,
    tokens: Option<u64>,
) -> SessionSummary {
    let mut meta = metadata(key_of(provider, id), title);
    meta.project = Some(project.into());
    meta.cwd = Some(cwd.into());
    meta.created_at = Some(NOW - 86_400_000);
    meta.activity = ActivityStamp {
        at: Some(NOW - age_secs * 1000),
        class: AggregationClass::Reported,
    };
    SessionSummary {
        last_event_at: Some(NOW - age_secs * 1000),
        last_completion: None,
        unfinished_work: true,
        displayed_event_count: 3,
        hidden_records: 0,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::Unmatched,
        usage: tokens.map(|t| UsageSummary {
            categories: TokenCategories::default(),
            total: Some(t),
            class: AggregationClass::Reported,
            cost: CostEstimate {
                usd: None,
                class: AggregationClass::Unknown,
                as_of: None,
                note: None,
                ..CostEstimate::default()
            },
            ..UsageSummary::default()
        }),
        warnings: vec![],
        ..facts::summary(meta)
    }
}

fn roster() -> Vec<SessionSummary> {
    vec![
        session(
            Harness::Codex,
            "codex-1",
            "Inventory API",
            "RipperGame",
            "/work/ripper",
            30,
            Some(91_000),
        ),
        session(
            Harness::Claude,
            "claude-1",
            "Scanner refactor",
            "RipperGame",
            "/work/ripper",
            600,
            Some(73_000),
        ),
        session(
            Harness::OpenCode,
            "oc-9",
            "PR review",
            "CortexSandbox",
            "/work/sandbox",
            7_200,
            None,
        ),
    ]
}

fn scan() -> ScanProgress {
    ScanProgress {
        harness: Harness::Codex,
        scanned: 3,
        total: Some(3),
        done: true,
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

fn discover(app: &mut App, ctx: &mut RosterContext, generation: u64) {
    apply_worker_event(
        app,
        ctx,
        WorkerEvent::Roster {
            snapshot: support::snapshot(generation, roster(), scan()),
            warnings: vec![],
        },
    );
}

fn press(app: &mut App, code: KeyCode) -> KeyAction {
    handle_key(app, KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(app: &mut App, c: char) -> KeyAction {
    let mods = if c.is_uppercase() {
        KeyModifiers::SHIFT
    } else {
        KeyModifiers::NONE
    };
    handle_key(app, KeyEvent::new(KeyCode::Char(c), mods))
}

fn type_text(app: &mut App, text: &str) {
    for c in text.chars() {
        assert_eq!(ch(app, c), KeyAction::None, "typing {c:?}");
    }
}

fn draw(app: &App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render(f, app, &ctx())).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn titles(app: &App) -> Vec<String> {
    app.visible_rows()
        .iter()
        .map(|r| r.session.meta.title.clone())
        .collect()
}

// ---- transcript fixtures --------------------------------------------------

fn provenance(k: &SessionKey) -> Provenance {
    Provenance {
        harness: k.harness,
        source: k.source.clone(),
        locator: SourceLocator {
            session: k.clone(),
            kind: LocatorKind::FileOffset {
                path: "x.jsonl".into(),
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

fn event(k: &SessionKey, i: u64, kind: EventKind, body: &str) -> Event {
    Event {
        id: format!("e{i}"),
        session: k.clone(),
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        kind,
        actor: Actor::Assistant,
        text: Some(text(body)),
        tool: None,
        usage: None,
        provenance: provenance(k),
    }
}

fn anchor(i: u64) -> PageAnchor {
    PageAnchor {
        timestamp: Some(NOW - 3_000_000 + i as i64 * 60_000),
        source_order: i,
        stable_id: format!("e{i}"),
    }
}

fn transcript_page(k: &SessionKey, bodies: &[&str]) -> TranscriptPage {
    let events: Vec<Event> = bodies
        .iter()
        .enumerate()
        .map(|(i, b)| event(k, i as u64, EventKind::AssistantMessage, b))
        .collect();
    TranscriptPage {
        next: Some(anchor(events.len() as u64 - 1)),
        events,
        diagnostics: vec![],
        has_more: false,
        truncated: false,
    }
}

/// Enter on the roster, then answer the resulting `Select` with `bodies`.
fn open_with(app: &mut App, ctx: &mut RosterContext, bodies: &[&str]) {
    assert_eq!(press(app, KeyCode::Enter), KeyAction::None);
    assert_eq!(app.screen, Screen::Inspector);
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, key }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("expected Select, got {requests:?}");
    };
    let page = transcript_page(key, bodies);
    apply_worker_event(
        app,
        ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page,
        },
    );
}

// ---- tests ------------------------------------------------------------------

#[test]
fn keyboard_flow_discovers_opens_expands_searches_and_returns() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(app.visible_rows().len(), 3);
    assert_eq!(app.selected_key(), Some(key_of(Harness::Codex, "codex-1")));

    open_with(
        &mut app,
        &mut ctx,
        &[
            "Implement inventory checks",
            "I will inspect the code\nsecond line of detail",
            "Running cargo test",
            "Inventory tests pass",
        ],
    );
    assert_eq!(app.transcript().unwrap().events().len(), 4);

    // The newest entry is selected on opening; step back to the second event.
    ch(&mut app, 'k');
    ch(&mut app, 'k');
    press(&mut app, KeyCode::Enter);
    let t = app.transcript().unwrap();
    assert_eq!(t.selected(), 1);
    assert!(t.is_expanded(t.selected_event().unwrap()));

    // Search the loaded events.
    assert_eq!(ch(&mut app, '/'), KeyAction::None);
    assert!(app.is_editing());
    type_text(&mut app, "inventory");
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("match 2/2 (loaded events)"), "{screen}");
    press(&mut app, KeyCode::Enter);
    assert!(!app.is_editing());
    assert_eq!(
        app.transcript().unwrap().selected(),
        3,
        "first match at or after the selection"
    );
    ch(&mut app, 'n');
    assert_eq!(app.transcript().unwrap().selected(), 0, "n wraps");
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("match 1/2 (loaded events)"), "{screen}");

    // Back to the roster; the selection is the same session.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen, Screen::Roster);
    assert!(app.transcript().is_none());
    assert_eq!(app.selected_key(), Some(key_of(Harness::Codex, "codex-1")));
    assert_eq!(ch(&mut app, 'q'), KeyAction::Quit);
}

#[test]
fn slash_searches_roster_and_loaded_transcript_with_n_and_shift_n() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);

    // Roster search covers title, project, cwd, and session id; case-insensitive.
    ch(&mut app, '/');
    type_text(&mut app, "SANDBOX");
    assert_eq!(titles(&app), vec!["PR review"]);
    assert_eq!(app.selected_key(), Some(key_of(Harness::OpenCode, "oc-9")));
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.visible_rows().len(), 3, "empty query shows everything");
    type_text(&mut app, "claude-1");
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    press(&mut app, KeyCode::Enter);
    assert!(!app.is_editing());
    assert_eq!(
        titles(&app),
        vec!["Scanner refactor"],
        "Enter keeps the query"
    );

    // Esc cancels an edit and restores the committed query.
    ch(&mut app, '/');
    type_text(&mut app, "zzz");
    assert!(app.visible_rows().is_empty());
    press(&mut app, KeyCode::Esc);
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    assert_eq!(app.screen, Screen::Roster);

    // Clear with Ctrl+u inside the input, then `q` is a character.
    ch(&mut app, '/');
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    assert_eq!(app.visible_rows().len(), 3);
    assert_eq!(
        ch(&mut app, 'q'),
        KeyAction::None,
        "q is text while editing"
    );
    assert_eq!(app.search_input().map(|(_, s)| s), Some("q"));
    press(&mut app, KeyCode::Backspace);
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.visible_rows().len(), 3);

    // Transcript search: loaded events only, n/N cycle and wrap.
    open_with(
        &mut app,
        &mut ctx,
        &["alpha one", "beta", "ALPHA two", "gamma", "alpha three"],
    );
    ch(&mut app, '/');
    type_text(&mut app, "alpha");
    press(&mut app, KeyCode::Enter);
    let t = app.transcript().unwrap();
    // The newest event already matches; search starts there.
    assert_eq!((t.selected(), t.match_count()), (4, 3));
    ch(&mut app, 'n');
    assert_eq!(app.transcript().unwrap().selected(), 0, "n wraps");
    ch(&mut app, 'n');
    assert_eq!(app.transcript().unwrap().selected(), 2);
    ch(&mut app, 'n');
    assert_eq!(app.transcript().unwrap().selected(), 4);
    ch(&mut app, 'N');
    assert_eq!(app.transcript().unwrap().selected(), 2, "N steps backwards");
    ch(&mut app, 'N');
    assert_eq!(app.transcript().unwrap().selected(), 0);
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("match 1/3 (loaded events)"), "{screen}");

    // No match is reported, and the selection stays put.
    ch(&mut app, '/');
    type_text(&mut app, "-nothing");
    assert_eq!(app.transcript().unwrap().selected(), 0);
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("no matches (loaded events)"), "{screen}");
    // Esc restores the previous query and selection.
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.screen, Screen::Inspector);
    assert_eq!(app.transcript().unwrap().search_query(), "alpha");
    assert_eq!(app.transcript().unwrap().selected(), 0);
}

#[test]
fn filter_panel_and_sort_keys_update_the_roster() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(
        titles(&app),
        vec!["Inventory API", "Scanner refactor", "PR review"]
    );

    // Provider filter: rows are Codex, Claude, OpenCode, ...
    assert_eq!(ch(&mut app, 'f'), KeyAction::None);
    assert_eq!(app.panel(), Panel::Filter);
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("Filter"), "{screen}");
    assert_eq!(
        ch(&mut app, 'q'),
        KeyAction::None,
        "q is not a quit key inside the filter panel"
    );
    assert_eq!(app.panel(), Panel::Filter);
    ch(&mut app, 'j');
    press(&mut app, KeyCode::Enter);
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    // Selection moved to the only remaining row.
    assert_eq!(
        app.selected_key(),
        Some(key_of(Harness::Claude, "claude-1"))
    );
    // Multi-select within a group.
    ch(&mut app, 'k');
    press(&mut app, KeyCode::Enter);
    assert_eq!(titles(&app), vec!["Inventory API", "Scanner refactor"]);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.panel(), Panel::None);

    // Filters compose with search.
    ch(&mut app, '/');
    type_text(&mut app, "scanner");
    press(&mut app, KeyCode::Enter);
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    let screen = draw(&app, 150, 30);
    assert!(screen.contains("provider=Codex,Claude"), "{screen}");
    assert!(screen.contains("search=scanner"), "{screen}");

    // Clear everything with `c` in the panel; the search text stays.
    ch(&mut app, 'f');
    ch(&mut app, 'c');
    press(&mut app, KeyCode::Esc);
    assert_eq!(titles(&app), vec!["Scanner refactor"]);
    assert!(app.filter().providers.is_none());
    ch(&mut app, '/');
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL),
    );
    press(&mut app, KeyCode::Enter);

    // Usage and project rows compose too.
    ch(&mut app, 'f');
    let items = app.filter_items();
    let usage_unknown = items
        .iter()
        .position(|i| {
            i.target
                == agentscope::app::FilterTarget::Usage(agentscope::app::UsageAvailability::Unknown)
        })
        .unwrap();
    for _ in 0..usage_unknown {
        ch(&mut app, 'j');
    }
    press(&mut app, KeyCode::Enter);
    assert_eq!(titles(&app), vec!["PR review"]);
    press(&mut app, KeyCode::Esc);

    // Sort: `s` cycles; the top line names the mode, `S` reverses the direction.
    ch(&mut app, 'f');
    ch(&mut app, 'c');
    press(&mut app, KeyCode::Esc);
    ch(&mut app, 's');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("sort: provider"), "{screen}");
    assert_eq!(
        titles(&app),
        vec!["Inventory API", "Scanner refactor", "PR review"]
    );
    ch(&mut app, 's');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("sort: tokens"), "{screen}");
    assert_eq!(titles(&app)[0], "Inventory API");
    ch(&mut app, 'S');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("sort: tokens"), "{screen}");
    assert_eq!(titles(&app)[0], "Scanner refactor");
    ch(&mut app, 's');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("sort: status"), "{screen}");
}

#[test]
fn help_is_contextual_and_closes_without_quitting() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(ch(&mut app, '?'), KeyAction::None);
    assert_eq!(app.panel(), Panel::Help);
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("Help"), "{screen}");
    assert!(screen.contains("filter"), "{screen}");
    // Other keys do not leak through the panel.
    ch(&mut app, 'j');
    ch(&mut app, 's');
    assert_eq!(app.selected_key(), Some(key_of(Harness::Codex, "codex-1")));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.panel(), Panel::None);

    open_with(&mut app, &mut ctx, &["one", "two"]);
    ch(&mut app, '?');
    let screen = draw(&app, 100, 30);
    assert!(screen.contains("Next / previous match"), "{screen}");
    ch(&mut app, '?');
    assert_eq!(app.panel(), Panel::None);
    assert_eq!(app.screen, Screen::Inspector);

    // Fits the smallest supported terminal.
    ch(&mut app, '?');
    let screen = draw(&app, 60, 20);
    assert!(screen.contains("Help"), "{screen}");
}

const LEGEND_LINES: [&str; 9] = [
    "Values: — not recorded   … loading",
    "+ partial / lower bound   ~ estimated",
    "Status: ● active   ○ recent   ! quiet",
    "▲ stalled?   ✓ complete",
    "✗ failed   × cancelled",
    "- unavailable   ? unknown",
    "Tools: ✓ completed   ✗ error",
    "… pending   ? unknown",
    "group totals cover loaded sessions",
];

fn legend_line_visible(screen: &str, want: &str) -> bool {
    screen.lines().any(|line| {
        line.trim()
            .trim_matches(|c: char| matches!(c, '│' | '─' | '┌' | '┐' | '└' | '┘'))
            .trim()
            == want
    })
}

#[test]
fn help_shows_value_legend() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(ch(&mut app, '?'), KeyAction::None);
    assert_eq!(app.panel(), Panel::Help);
    // The shared legend follows the screen-specific keys and capability notes.
    let _ = draw(&app, 100, 30);
    app.panel_move(isize::MAX);
    let screen = draw(&app, 100, 30);
    for want in LEGEND_LINES {
        assert!(
            legend_line_visible(&screen, want),
            "missing legend line {want:?} in:\n{screen}"
        );
    }
}

#[test]
fn help_legend_fits_the_smallest_terminal() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(ch(&mut app, '?'), KeyAction::None);
    let mut seen = String::new();
    loop {
        seen.push_str(&draw(&app, 60, 20));
        seen.push('\n');
        let before = app.panel_scroll();
        app.panel_move(7);
        if app.panel_scroll() == before {
            break;
        }
    }
    for want in LEGEND_LINES {
        assert!(
            legend_line_visible(&seen, want),
            "missing complete legend line {want:?} while scrolling 60x20"
        );
    }
}

#[test]
fn filter_overlay_lists_provider_counts() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(ch(&mut app, 'f'), KeyAction::None);
    assert_eq!(app.panel(), Panel::Filter);
    let screen = draw(&app, 100, 30);
    for want in ["Provider", "Codex 1", "Claude 1", "OpenCode 1"] {
        assert!(screen.contains(want), "missing {want:?} in:\n{screen}");
    }
}

#[test]
fn tab_cycles_panes_and_ctrl_c_always_quits() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    open_with(&mut app, &mut ctx, &["one", "two"]);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.screen, Screen::Details);
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.screen, Screen::Inspector);
    press(&mut app, KeyCode::BackTab);
    assert_eq!(app.screen, Screen::Roster);
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.screen, Screen::Inspector);

    ch(&mut app, '/');
    assert_eq!(
        handle_key(
            &mut app,
            KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)
        ),
        KeyAction::Quit
    );
}

fn backward_page_of(k: &SessionKey, range: std::ops::Range<u64>, has_more: bool) -> TranscriptPage {
    let events: Vec<Event> = range
        .clone()
        .map(|i| event(k, i, EventKind::AssistantMessage, &format!("message {i}")))
        .collect();
    TranscriptPage {
        next: Some(anchor(range.start)),
        events,
        diagnostics: vec![],
        has_more,
        truncated: false,
    }
}

#[test]
fn esc_restores_the_original_roster_selection_when_the_query_excluded_it() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    ch(&mut app, 'j');
    assert_eq!(
        app.selected_key(),
        Some(key_of(Harness::Claude, "claude-1"))
    );
    ch(&mut app, '/');
    type_text(&mut app, "sandbox");
    assert_eq!(app.selected_key(), Some(key_of(Harness::OpenCode, "oc-9")));
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.visible_rows().len(), 3);
    assert_eq!(
        app.selected_key(),
        Some(key_of(Harness::Claude, "claude-1")),
        "Esc returns to the session selected before the search"
    );
}

#[test]
fn transcript_search_origin_survives_an_older_page_prepending() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    press(&mut app, KeyCode::Enter);
    let k = key_of(Harness::Codex, "codex-1");
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("expected Select, got {requests:?}");
    };
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page: transcript_page(&k, &["only"]),
        },
    );
    ch(&mut app, 'G');
    let requests = app.take_requests();
    let Some(WorkerRequest::LoadPage { request_id, .. }) = requests.last() else {
        panic!("expected tail load, got {requests:?}");
    };
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page: backward_page_of(&k, 10..14, true),
        },
    );
    // Start the search at e12 and type a query that jumps elsewhere.
    ch(&mut app, 'j');
    ch(&mut app, 'j');
    let at = app
        .transcript()
        .unwrap()
        .selected_event()
        .unwrap()
        .id
        .clone();
    ch(&mut app, '/');
    type_text(&mut app, "message 13");
    // An older page arrives while the input is open and prepends events.
    let older = app
        .take_requests()
        .into_iter()
        .find_map(|r| match r {
            WorkerRequest::LoadPage {
                request_id,
                direction: PageDirection::Backward,
                ..
            } => Some(request_id),
            _ => None,
        })
        .expect("older page requested near the window edge");
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Transcript {
            request_id: older,
            page: backward_page_of(&k, 6..10, true),
        },
    );
    press(&mut app, KeyCode::Esc);
    let t = app.transcript().unwrap();
    assert_eq!(t.events().first().unwrap().id, "e6");
    assert_eq!(
        t.selected_event().unwrap().id,
        at,
        "same event after cancel"
    );
}

#[test]
fn tab_focus_cycling_retains_the_transcript_and_its_state() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    open_with(&mut app, &mut ctx, &["alpha", "beta", "alpha two"]);
    ch(&mut app, 'k');
    press(&mut app, KeyCode::Enter);
    ch(&mut app, '/');
    type_text(&mut app, "alpha");
    press(&mut app, KeyCode::Enter);
    let selected = app.transcript().unwrap().selected();

    press(&mut app, KeyCode::Tab); // details
    press(&mut app, KeyCode::Tab); // roster
    assert_eq!(app.screen, Screen::Roster);
    assert!(app.transcript().is_some());
    press(&mut app, KeyCode::Tab); // back to the same transcript
    assert_eq!(app.screen, Screen::Inspector);
    assert!(app.take_requests().is_empty(), "no reload on focus change");
    press(&mut app, KeyCode::BackTab);
    press(&mut app, KeyCode::Tab);
    let t = app.transcript().unwrap();
    assert_eq!(t.events().len(), 3);
    assert_eq!(t.selected(), selected);
    assert_eq!(t.search_query(), "alpha");
    assert!(t.is_expanded(&t.events()[1]));
    // Esc is the explicit close.
    press(&mut app, KeyCode::Esc);
    assert!(app.transcript().is_none());
}

#[test]
fn search_opened_while_the_first_page_loads_works_once_it_arrives() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    press(&mut app, KeyCode::Enter);
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, key }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("expected Select, got {requests:?}");
    };
    ch(&mut app, '/');
    assert!(app.is_editing(), "search opens while loading");
    apply_worker_event(
        &mut app,
        &mut ctx,
        WorkerEvent::Transcript {
            request_id: *request_id,
            page: transcript_page(key, &["alpha", "beta", "gamma target"]),
        },
    );
    type_text(&mut app, "target");
    assert_eq!(app.transcript().unwrap().selected(), 2);
    press(&mut app, KeyCode::Esc);
    assert_eq!(app.transcript().unwrap().selected(), 2, "Esc restores");
}

#[test]
fn process_panel_follows_the_selected_roster_row_after_leaving_the_inspector() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    open_with(&mut app, &mut ctx, &["one"]);
    ch(&mut app, 'p');
    assert_eq!(
        app.process_view().session.unwrap().meta.key,
        key_of(Harness::Codex, "codex-1")
    );
    press(&mut app, KeyCode::Esc); // close the panel
    press(&mut app, KeyCode::BackTab); // roster, transcript retained
    assert_eq!(app.screen, Screen::Roster);
    ch(&mut app, 'j');
    ch(&mut app, 'p');
    assert_eq!(
        app.process_view().session.unwrap().meta.key,
        key_of(Harness::Claude, "claude-1")
    );
}

// ---- responsive layout, Load more focus and read-only mouse -----------------

use agentscope::app::{LoadFocus, PreviewState};
use agentscope::ui::{RosterLayout, handle_event, handle_mouse};
use crossterm::event::{Event as TermEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

fn lines(screen: &str) -> Vec<&str> {
    screen.lines().collect()
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    }
}

fn click(app: &mut App, area: Rect, column: u16, row: u16) {
    let layout = RosterLayout::for_area(area);
    handle_mouse(
        app,
        mouse(MouseEventKind::Down(MouseButton::Left), column, row),
        &layout,
    );
}

/// Three loaded roots and an explicit root Load more row.
fn paged_app() -> App {
    let mut app = new_app();
    let mut snap = support::snapshot(1, roster(), scan());
    snap.has_more = true;
    assert!(app.apply_roster_snapshot(snap, vec![]));
    app.take_requests();
    app
}

/// `roots` two-line sessions in one project, plus an optional root Load more.
fn uniform_app(roots: usize, has_more: bool) -> App {
    let mut app = new_app();
    let sessions: Vec<SessionSummary> = (0..roots)
        .map(|i| {
            session(
                Harness::Codex,
                &format!("s-{i:02}"),
                "Same project row",
                "Uniform",
                "/work/uniform",
                60 + i as i64,
                None,
            )
        })
        .collect();
    let mut snap = support::snapshot(1, sessions, scan());
    snap.has_more = has_more;
    assert!(app.apply_roster_snapshot(snap, vec![]));
    app.take_requests();
    app
}

fn load_more_requested(app: &mut App) -> bool {
    app.take_requests()
        .iter()
        .any(|r| matches!(r, WorkerRequest::LoadRosterMore { .. }))
}

#[test]
fn tab_swaps_list_and_preview_on_narrow_terminals_and_keeps_the_selection() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    handle_event(&mut app, TermEvent::Resize(90, 25), Rect::new(0, 0, 90, 25));
    ch(&mut app, 'j');
    let selected = app.selected_key();
    assert_eq!(selected, Some(key_of(Harness::Claude, "claude-1")));
    assert!(draw(&app, 90, 25).contains("STATUS"));

    press(&mut app, KeyCode::Tab);
    assert!(app.preview_replaces_list());
    assert_eq!(
        app.screen,
        Screen::Roster,
        "Tab swaps panes, it does not navigate"
    );
    let swapped = draw(&app, 90, 25);
    assert!(swapped.contains("LAST ACTIVITY"), "{swapped}");
    assert!(
        swapped.contains("Scanner refactor"),
        "the selected session's preview:\n{swapped}"
    );
    assert!(!swapped.contains("STATUS"), "{swapped}");
    assert_eq!(app.selected_key(), selected);

    // Vertical keys scroll the preview, not the selection.
    ch(&mut app, 'j');
    assert_eq!(app.selected_key(), selected);
    press(&mut app, KeyCode::Tab);
    assert!(!app.preview_replaces_list());
    assert!(draw(&app, 90, 25).contains("STATUS"));
    assert_eq!(app.selected_key(), selected);

    // From 150 columns the preview has its own column and Tab keeps its old job.
    handle_event(
        &mut app,
        TermEvent::Resize(160, 40),
        Rect::new(0, 0, 160, 40),
    );
    press(&mut app, KeyCode::Tab);
    assert_eq!(app.screen, Screen::Inspector);
    assert!(!app.preview_replaces_list());
}

#[test]
fn resizing_between_sizes_keeps_selection_and_a_swapped_preview_only_while_narrow() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    ch(&mut app, 'j');
    let selected = app.selected_key();
    handle_event(&mut app, TermEvent::Resize(90, 25), Rect::new(0, 0, 90, 25));
    press(&mut app, KeyCode::Tab);
    assert!(app.preview_replaces_list());
    for (w, h) in [(60, 20), (160, 40), (90, 25), (60, 20)] {
        handle_event(&mut app, TermEvent::Resize(w, h), Rect::new(0, 0, w, h));
        let screen = draw(&app, w, h);
        assert_eq!(app.selected_key(), selected, "{w}x{h}");
        if w >= 150 {
            assert!(!app.preview_replaces_list(), "the swap ends once both fit");
            assert!(
                screen.contains("STATUS") && screen.contains("LAST ACTIVITY"),
                "{screen}"
            );
        } else {
            assert!(screen.contains("Scanner refactor"), "{w}x{h}:\n{screen}");
        }
        for line in lines(&screen) {
            assert_eq!(line.chars().count(), w as usize, "{w}x{h} row width");
        }
    }
}

#[test]
fn right_expands_and_left_collapses_children() {
    let mut app = new_app();
    assert!(app.apply_roster_snapshot(support::snapshot(1, roster(), scan()), vec![]));
    let parent = key_of(Harness::Codex, "codex-1");
    assert!(app.select_row(&parent));
    assert!(!app.children_expanded(&parent));

    press(&mut app, KeyCode::Right);
    assert!(app.children_expanded(&parent), "Right expands");
    let requests = app.take_requests();
    assert!(
        requests.iter().any(|request| matches!(
            request,
            WorkerRequest::LoadChildren { parent: asked, .. } if *asked == parent
        )),
        "the first page is requested once: {requests:?}"
    );
    let request_id = requests
        .iter()
        .find_map(|request| match request {
            WorkerRequest::LoadChildren { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .unwrap();
    assert!(app.apply_children(
        request_id,
        &parent,
        vec![std::sync::Arc::new(metadata(
            key_of(Harness::Codex, "child-1"),
            "Child task"
        ))],
        None,
    ));

    press(&mut app, KeyCode::Left);
    assert!(!app.children_expanded(&parent), "Left collapses");
    assert!(
        !app.roster_rows()
            .any(|item| matches!(item, RosterItem::Session(row) if row.depth == 1)),
        "the child is hidden"
    );
    press(&mut app, KeyCode::Right);
    assert!(app.children_expanded(&parent));
    assert!(
        app.roster_rows()
            .any(|item| matches!(item, RosterItem::Session(row) if row.depth == 1)),
        "the loaded child returns without another read"
    );
}

#[test]
fn left_on_child_selects_parent() {
    let mut app = new_app();
    assert!(app.apply_roster_snapshot(support::snapshot(1, roster(), scan()), vec![]));
    let parent = key_of(Harness::Codex, "codex-1");
    let child = key_of(Harness::Codex, "child-1");
    assert!(app.select_row(&parent));
    press(&mut app, KeyCode::Right);
    let request_id = app
        .take_requests()
        .into_iter()
        .find_map(|request| match request {
            WorkerRequest::LoadChildren { request_id, .. } => Some(request_id),
            _ => None,
        })
        .unwrap();
    assert!(app.apply_children(
        request_id,
        &parent,
        vec![std::sync::Arc::new(metadata(child.clone(), "Child task"))],
        None,
    ));
    assert!(app.select_row(&child));
    assert_eq!(app.selected_key(), Some(child));

    press(&mut app, KeyCode::Left);
    assert_eq!(app.selected_key(), Some(parent.clone()));
    assert!(
        app.children_expanded(&parent),
        "moving to the parent collapses nothing"
    );
}

#[test]
fn down_arrow_skips_group_header() {
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    assert_eq!(app.selected_key(), Some(key_of(Harness::Codex, "codex-1")));
    assert!(
        app.roster_rows()
            .any(|item| matches!(item, RosterItem::GroupHeader(_))),
        "the roster is grouped"
    );

    press(&mut app, KeyCode::Down);
    assert_eq!(
        app.selected_key(),
        Some(key_of(Harness::Claude, "claude-1"))
    );
    // A heading separates the RipperGame and CortexSandbox groups; Down lands
    // on the next session, never on the heading.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.selected_key(), Some(key_of(Harness::OpenCode, "oc-9")));
}

#[test]
fn root_load_more_is_keyboard_focusable_and_enter_activates_it() {
    let mut app = paged_app();
    ch(&mut app, 'G');
    assert_eq!(app.selected_key(), Some(key_of(Harness::OpenCode, "oc-9")));
    assert!(app.load_focus().is_none());
    ch(&mut app, 'j');
    assert_eq!(app.load_focus(), Some(&LoadFocus::Roots));
    let screen = draw(&app, 90, 25);
    let row = lines(&screen)
        .into_iter()
        .find(|l| l.contains("Load more sessions"))
        .unwrap_or_else(|| panic!("{screen}"));
    assert!(
        row.starts_with("> "),
        "the focus marker is on the row: {row:?}"
    );
    // The session row loses its marker while Load more holds the focus.
    assert!(
        !lines(&screen)
            .iter()
            .any(|l| l.starts_with("> ") && l.contains("PR review"))
    );
    assert!(!load_more_requested(&mut app), "moving never loads");

    press(&mut app, KeyCode::Enter);
    assert!(load_more_requested(&mut app));
    assert_eq!(
        app.screen,
        Screen::Roster,
        "Enter on Load more does not open a session"
    );

    ch(&mut app, 'k');
    assert!(app.load_focus().is_none());
    assert_eq!(app.selected_key(), Some(key_of(Harness::OpenCode, "oc-9")));
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.screen, Screen::Inspector);
}

#[test]
fn child_load_more_is_keyboard_focusable_and_asks_for_its_own_family() {
    let mut app = new_app();
    let snap = support::snapshot(1, roster(), scan());
    assert!(app.apply_roster_snapshot(snap, vec![]));
    app.take_requests();
    let parent = key_of(Harness::Codex, "codex-1");
    assert!(app.load_children(&parent));
    let requests = app.take_requests();
    let [WorkerRequest::LoadChildren { request_id, .. }] = requests.as_slice() else {
        panic!("expected a child page request, got {requests:?}");
    };
    let child = key_of(Harness::Codex, "child-1");
    assert!(app.apply_children(
        *request_id,
        &parent,
        vec![std::sync::Arc::new(metadata(child.clone(), "Child task"))],
        Some(key_of(Harness::Codex, "child-2")),
    ));
    ch(&mut app, ' ');
    app.take_requests();
    ch(&mut app, 'j');
    assert_eq!(app.selected_key(), Some(child));
    ch(&mut app, 'j');
    assert_eq!(app.load_focus(), Some(&LoadFocus::Children(parent.clone())));
    let screen = draw(&app, 90, 25);
    assert!(screen.contains("Load more children"), "{screen}");
    press(&mut app, KeyCode::Enter);
    let requests = app.take_requests();
    assert!(
        requests.iter().any(|r| matches!(
            r,
            WorkerRequest::LoadChildren { parent: p, .. } if *p == parent
        )),
        "{requests:?}"
    );
    // Space does nothing on a Load more row.
    ch(&mut app, ' ');
    assert!(app.take_requests().is_empty());
}

#[test]
fn mouse_selects_expands_and_loads_more_only_where_rows_are_drawn() {
    let area = Rect::new(0, 0, 90, 25);
    let mut app = paged_app();
    handle_event(&mut app, TermEvent::Resize(90, 25), area);
    let layout = RosterLayout::for_area(area);
    let first_y = layout.rows.y;

    // A click on the second row (either of its two physical lines) selects it;
    // the first line is the project heading.
    click(&mut app, area, 12, first_y + 3);
    assert_eq!(
        app.selected_key(),
        Some(key_of(Harness::Claude, "claude-1"))
    );
    click(&mut app, area, 12, first_y + 6);
    assert_eq!(app.selected_key(), Some(key_of(Harness::OpenCode, "oc-9")));

    // The marker column is the Space-equivalent expander.
    app.take_requests();
    click(&mut app, area, 0, first_y + 1);
    assert_eq!(app.selected_key(), Some(key_of(Harness::Codex, "codex-1")));
    let requests = app.take_requests();
    assert!(
        requests
            .iter()
            .any(|r| matches!(r, WorkerRequest::LoadChildren { .. })),
        "{requests:?}"
    );

    // Load more rows are activated by a click, and only when they are shown.
    let _ = app.take_requests();
    let before = app.selected_key();
    click(&mut app, area, 12, first_y + 8);
    assert!(load_more_requested(&mut app));
    assert_eq!(app.selected_key(), before, "Load more is not a session");
    assert_eq!(app.load_focus(), Some(&LoadFocus::Roots));

    // Nothing outside the drawn rows is a hit: header, footer, beyond the list.
    for (x, y) in [
        (12, first_y - 1),
        (12, area.height - 1),
        (12, first_y + 20),
        (95, first_y),
    ] {
        let selected = app.selected_key();
        click(&mut app, area, x, y);
        assert_eq!(app.selected_key(), selected, "({x},{y}) is not a row");
        assert!(app.take_requests().is_empty(), "({x},{y})");
    }
}

#[test]
fn mouse_never_hits_rows_the_list_did_not_draw() {
    let area = Rect::new(0, 0, 90, 25);
    let layout = RosterLayout::for_area(area);
    let blank = layout.rows.y + layout.rows.height - 1;
    assert_eq!(
        layout.rows.height, 20,
        "one heading plus nine rows fills 19 of 20 lines"
    );

    // One heading plus nine two-line rows fill 19 of 20 lines; the undrawn
    // tenth session must not be selected through the leftover blank line.
    let mut app = uniform_app(10, false);
    let before = app.selected_key();
    click(&mut app, area, 12, blank);
    assert_eq!(
        app.selected_key(),
        before,
        "an undrawn session is not a mouse target"
    );
    assert!(app.take_requests().is_empty());

    // Nine rows leave the two-line root Load more row undrawn: the same blank
    // line must not request another page.
    let mut app = uniform_app(9, true);
    click(&mut app, area, 12, blank);
    assert!(
        app.take_requests().is_empty(),
        "a blank line must not activate an undrawn Load more row"
    );
    assert_eq!(app.load_focus(), None);
}

#[test]
fn mouse_is_ignored_for_hidden_modified_and_stale_geometry() {
    let area = Rect::new(0, 0, 90, 25);
    let layout = RosterLayout::for_area(area);
    let y = layout.rows.y + 2;
    let down = |app: &mut App, layout: &RosterLayout, modifiers| {
        let mut event = mouse(MouseEventKind::Down(MouseButton::Left), 12, y);
        event.modifiers = modifiers;
        handle_mouse(app, event, layout);
    };

    // Shift belongs to the terminal's own selection.
    let mut app = paged_app();
    let first = app.selected_key();
    down(&mut app, &layout, KeyModifiers::SHIFT);
    assert_eq!(app.selected_key(), first);

    // A terminal below the minimum size draws no rows, so there is nothing to hit.
    let tiny = RosterLayout::for_area(Rect::new(0, 0, 59, 19));
    down(&mut app, &tiny, KeyModifiers::NONE);
    assert_eq!(app.selected_key(), first);

    // A swapped-in preview hides the list.
    handle_event(&mut app, TermEvent::Resize(90, 25), area);
    press(&mut app, KeyCode::Tab);
    assert!(app.preview_replaces_list());
    down(&mut app, &layout, KeyModifiers::NONE);
    assert_eq!(app.selected_key(), first);
    press(&mut app, KeyCode::Tab);

    // A panel owns the screen.
    ch(&mut app, '?');
    assert_ne!(app.panel(), Panel::None);
    down(&mut app, &layout, KeyModifiers::NONE);
    assert_eq!(app.selected_key(), first);
    press(&mut app, KeyCode::Esc);

    // The inspector's transcript scrollback is not roster geometry.
    let mut ctx = ctx();
    open_with(&mut app, &mut ctx, &["one", "two"]);
    let selected_event = app.transcript().unwrap().selected();
    down(&mut app, &layout, KeyModifiers::NONE);
    handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 12, y), &layout);
    assert_eq!(app.screen, Screen::Inspector);
    assert_eq!(app.selected_key(), first);
    assert_eq!(app.transcript().unwrap().selected(), selected_event);

    // Pointer motion carries no action.
    let mut moved = paged_app();
    let before = moved.selected_key();
    handle_event(
        &mut moved,
        TermEvent::Mouse(mouse(MouseEventKind::Moved, 12, y)),
        area,
    );
    assert_eq!(moved.selected_key(), before);
}

/// Give the selected session enough recent activity that the preview's
/// scrolling block overflows a 160x20 terminal.
fn seed_long_preview_activity(app: &mut App) {
    let Some(key) = app.selected_key() else {
        return;
    };
    app.take_requests();
    app.settle_selection(0);
    app.settle_selection(ACTIVITY_DEBOUNCE_MS);
    let id = app
        .take_requests()
        .into_iter()
        .find_map(|request| match request {
            WorkerRequest::PreviewTail { request_id, .. } => Some(request_id),
            _ => None,
        })
        .expect("the selection reads its latest activity");
    let mut tool = event(&key, 10, EventKind::Tool, "");
    tool.text = None;
    tool.tool = Some(ToolInteraction {
        name: "exec".into(),
        call_id: None,
        input: Some(text(&format!(r#"{{"command":"{}"}}"#, "a".repeat(200)))),
        output: None,
        status: ToolStatus::Completed,
        exit_code: None,
        started_at: None,
        completed_at: None,
        orphan: false,
        child_session: None,
    });
    let reply = event(&key, 11, EventKind::AssistantMessage, &"reply ".repeat(20));
    let page = TranscriptPage {
        events: vec![tool, reply],
        diagnostics: vec![],
        next: None,
        has_more: false,
        truncated: false,
    };
    assert!(app.apply_activity(id, key, page));
}

#[test]
fn the_wheel_scrolls_the_preview_independently_of_the_list() {
    let area = Rect::new(0, 0, 160, 20);
    let mut app = new_app();
    let mut ctx = ctx();
    discover(&mut app, &mut ctx, 1);
    seed_long_preview_activity(&mut app);
    handle_event(&mut app, TermEvent::Resize(160, 20), area);
    let before = app.selected_key();
    let screen = draw(&app, 160, 20);
    assert!(screen.contains("LAST ACTIVITY"), "{screen}");

    let layout = RosterLayout::for_area(area);
    let preview = layout.preview.expect("preview column");
    let (px, py) = (preview.x + 5, preview.y + 5);
    handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, px, py), &layout);
    assert!(app.preview_scroll() > 0, "the preview scrolled");
    assert_eq!(app.selected_key(), before, "the list did not move");
    let scrolled = draw(&app, 160, 20);
    assert_ne!(scrolled, screen);
    handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp, px, py), &layout);
    assert_eq!(app.preview_scroll(), 0);

    // A wheel notch over the list moves the selection and resets the preview scroll.
    handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown, px, py), &layout);
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::ScrollDown, 10, layout.rows.y + 1),
        &layout,
    );
    assert_eq!(
        app.selected_key(),
        Some(key_of(Harness::Claude, "claude-1"))
    );
    assert_eq!(app.preview_scroll(), 0, "a new selection starts at the top");

    // `J`/`K` scroll the preview from the keyboard.
    assert!(app.select_row(&key_of(Harness::Codex, "codex-1")));
    draw(&app, 160, 20);
    ch(&mut app, 'J');
    assert_eq!(app.preview_scroll(), 1);
    ch(&mut app, 'K');
    assert_eq!(app.preview_scroll(), 0);
}

/// Real readers over the synthetic three-harness store, through the worker into
/// `App`, then Crossterm events into the Ratatui test backend.
#[test]
fn real_store_reader_worker_app_event_and_terminal_surface() {
    use support::world::World;
    let world = World::new();
    let worker = agentscope::worker::WorkerHandle::spawn(world.readers(), world.config());
    let mut app = App::new(
        support::NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    let mut ctx = ctx();
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(50)) {
            apply_worker_event(&mut app, &mut ctx, event);
        }
        if app.total_sessions() == 3 && matches!(app.preview(), PreviewState::Empty) {
            // The first row is under the cursor: ask for its preview like a key move does.
            let first = app.selected_key().expect("a selected row");
            app.select_session(&first);
        }
        agentscope::ui::flush_requests(&mut app, &worker);
        if app.total_sessions() == 3
            && matches!(app.preview(), PreviewState::Ready { complete: true, .. })
        {
            break;
        }
    }
    assert_eq!(app.total_sessions(), 3);
    assert!(matches!(
        app.preview(),
        PreviewState::Ready { complete: true, .. }
    ));

    for (w, h) in [(60u16, 20u16), (90, 25), (160, 40), (90, 25)] {
        let area = Rect::new(0, 0, w, h);
        handle_event(&mut app, TermEvent::Resize(w, h), area);
        let screen = draw(&app, w, h);
        assert!(!screen.contains('\x1b'), "{w}x{h}");
        let all = lines(&screen);
        assert_eq!(all.len(), h as usize);
        for row in app.visible_rows() {
            let title: String = row.session.meta.title.chars().take(10).collect();
            let first = all
                .iter()
                .position(|line| line.chars().take(40).collect::<String>().contains(&title))
                .unwrap_or_else(|| panic!("{title} not drawn at {w}x{h}:\n{screen}"));
            let tag = match row.session.meta.key.harness {
                Harness::Codex => "[CDX]",
                Harness::Claude => "[CLD]",
                Harness::OpenCode => "[OC]",
                Harness::Omp => "[OMP]",
                Harness::Hermes => "[HRM]",
                Harness::Pi => "[PI]",
            };
            assert!(
                all[first + 1].contains(tag),
                "{w}x{h}: the quiet line carries the agent tag:\n{}\n{}",
                all[first],
                all[first + 1]
            );
        }
    }

    // Click the second row, then walk to the swap and the scrolled columns.
    let area = Rect::new(0, 0, 90, 25);
    handle_event(&mut app, TermEvent::Resize(90, 25), area);
    let layout = RosterLayout::for_area(area);
    let second = app.visible_rows()[1].session.meta.key.clone();
    // Walk the row model so a project heading above the row cannot shift the click.
    let mut y = layout.rows.y;
    for item in app.roster_rows() {
        if matches!(&item, RosterItem::Session(row) if row.session.meta.key == second) {
            break;
        }
        y += item.height();
    }
    handle_event(
        &mut app,
        TermEvent::Mouse(mouse(MouseEventKind::Down(MouseButton::Left), 12, y)),
        area,
    );
    assert_eq!(app.selected_key(), Some(second.clone()));
    agentscope::ui::flush_requests(&mut app, &worker);
    handle_event(
        &mut app,
        TermEvent::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
        area,
    );
    assert!(app.preview_replaces_list());
    let title = app.session(&second).unwrap().session.meta.title.clone();
    assert!(draw(&app, 90, 25).contains(&title));
    assert_eq!(app.selected_key(), Some(second));
    let _ = worker.tx.send(WorkerRequest::Shutdown);
}

#[test]
fn quit_keys_work_in_every_roster_mode_so_the_terminal_is_always_released() {
    let area = Rect::new(0, 0, 90, 25);
    let ctrl_c = TermEvent::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    let q = TermEvent::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE));

    let mut app = paged_app();
    handle_event(&mut app, TermEvent::Resize(90, 25), area);
    assert_eq!(
        handle_event(&mut app, ctrl_c.clone(), area),
        KeyAction::Quit
    );
    assert_eq!(handle_event(&mut app, q.clone(), area), KeyAction::Quit);

    // Swapped-in preview, Load more focus, search editing and an open panel.
    press(&mut app, KeyCode::Tab);
    assert!(app.preview_replaces_list());
    assert_eq!(handle_event(&mut app, q.clone(), area), KeyAction::Quit);
    assert_eq!(
        handle_event(&mut app, ctrl_c.clone(), area),
        KeyAction::Quit
    );
    press(&mut app, KeyCode::Tab);
    ch(&mut app, 'G');
    ch(&mut app, 'j');
    assert!(app.load_focus().is_some());
    assert_eq!(handle_event(&mut app, q, area), KeyAction::Quit);
    ch(&mut app, '/');
    assert!(app.is_editing());
    assert_eq!(
        handle_event(&mut app, ctrl_c.clone(), area),
        KeyAction::Quit
    );
    press(&mut app, KeyCode::Esc);
    ch(&mut app, '?');
    assert_eq!(handle_event(&mut app, ctrl_c, area), KeyAction::Quit);
}

// ---- B7: the integrated native world through the real keyboard path ----------------

#[test]
fn native_world_keyboard_pages_roots_expands_families_opens_at_the_tail_and_drops_stale_replies() {
    use support::world::{ROOT_COUNT, World};

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
    let mut ctx = ctx();
    let mut first_generation: Option<WorkerEvent> = None;
    let mut pump_until = |app: &mut App, ctx: &mut RosterContext, done: &dyn Fn(&App) -> bool| {
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        loop {
            agentscope::ui::flush_requests(app, &worker);
            if done(app) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "the app stalled");
            if let Ok(event) = worker.rx.recv_timeout(Duration::from_millis(20)) {
                if first_generation.is_none()
                    && matches!(&event, WorkerEvent::Roster { snapshot, .. } if !snapshot.provisional && snapshot.generation == 1)
                {
                    first_generation = Some(event.clone());
                }
                apply_worker_event(app, ctx, event);
            }
        }
    };
    let expected = g.expected_roots();
    let visible = |app: &App| -> Vec<SessionKey> {
        app.visible_rows()
            .iter()
            .filter(|r| g.is_root(&r.session.meta.key))
            .map(|r| r.session.meta.key.clone())
            .collect()
    };
    let hidden_released = |keys: &[SessionKey]| -> Vec<SessionKey> {
        keys.iter()
            .filter(|k| **k != g.desktop_released)
            .cloned()
            .collect()
    };

    // 50, then 100, then all 125 roots, each only by the keyboard's Load more row.
    pump_until(&mut app, &mut ctx, &|a| a.total_sessions() == 50);
    assert_eq!(visible(&app), hidden_released(&expected[..50]));
    for loaded in [100, ROOT_COUNT] {
        ch(&mut app, 'G');
        ch(&mut app, 'j');
        assert_eq!(app.load_focus(), Some(&agentscope::app::LoadFocus::Roots));
        press(&mut app, KeyCode::Enter);
        pump_until(&mut app, &mut ctx, &|a| a.total_sessions() == loaded);
        assert_eq!(visible(&app), hidden_released(&expected[..loaded]));
    }
    ch(&mut app, 'G');
    ch(&mut app, 'j');
    assert!(
        app.load_focus().is_none(),
        "every root is loaded: no Load more"
    );

    // Families expand in place; a fork is a root of its own and never a child.
    for (parent, child) in [
        (&g.codex.parent, &g.codex.child),
        (&g.claude.parent, &g.claude.child),
        (&g.opencode.parent, &g.opencode.child),
        (&g.desktop_code, &g.desktop_child),
    ] {
        assert!(app.select_row(parent));
        ch(&mut app, ' ');
        pump_until(&mut app, &mut ctx, &|a| {
            a.children_of(parent)
                .is_some_and(|c| c.metadata.iter().any(|m| &m.key == child))
        });
        assert!(app.children_expanded(parent));
        let loaded: Vec<&SessionKey> = app
            .children_of(parent)
            .unwrap()
            .metadata
            .iter()
            .map(|m| &m.key)
            .collect();
        assert_eq!(loaded, [child]);
        assert!(
            !app.children_of(parent)
                .unwrap()
                .metadata
                .iter()
                .any(|m| g.is_root(&m.key))
        );
    }
    for (fork, _) in &g.forks {
        assert!(
            app.visible_rows()
                .iter()
                .any(|r| &r.session.meta.key == fork)
        );
    }

    // Every session opens at its newest entry.
    for (key, tail) in &g.tails {
        assert!(app.select_row(key));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.screen, Screen::Inspector);
        pump_until(&mut app, &mut ctx, &|a| {
            a.transcript().is_some_and(|t| t.loaded() && t.key() == key)
        });
        let t = app.transcript().unwrap();
        let selected = t.selected_event().expect("a selected entry");
        assert_eq!(
            selected.text.as_ref().map(|x| x.preview.as_str()),
            Some(tail.as_str()),
            "{:?} opens at its newest entry",
            key.harness
        );
        assert!(draw(&app, 120, 40).contains(tail.as_str()));
        // A reply to a request that is not the current one changes nothing.
        let page = transcript_page(key, &["a reply for another request"]);
        assert!(!app.apply_transcript(0, page));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.screen, Screen::Roster);
    }

    // A refresh opens a new generation; the old first generation can no longer replace it.
    assert!(app.refresh_roster());
    pump_until(&mut app, &mut ctx, &|a| a.generation() >= 2);
    let generation = app.generation();
    let shown = visible(&app);
    let stale = first_generation.expect("the first generation was observed");
    apply_worker_event(&mut app, &mut ctx, stale);
    assert_eq!(app.generation(), generation);
    assert_eq!(visible(&app), shown, "a stale generation changes nothing");
    let _ = worker.tx.send(WorkerRequest::Shutdown);
}
