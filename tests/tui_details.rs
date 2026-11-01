use std::time::Duration;

mod support;

use agentscope::app::{App, Screen, SourceState};
use agentscope::domain::*;
use agentscope::providers::SourceRecord;
use agentscope::ui::{RosterContext, render};
use agentscope::worker::WorkerRequest;
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use support::facts::{self, metadata};

const NOW: Timestamp = 1_800_000_000_000;
const SECRET: &str = "sk-ant-abcdefghijklmnop1234";

fn key() -> SessionKey {
    SessionKey {
        harness: Harness::Codex,
        native_id: "sess-1".into(),
        source: SourceKey::File("sess-1.jsonl".into()),
    }
}

fn locator_for(session: SessionKey, offset: u64, generation: Option<u64>) -> SourceLocator {
    SourceLocator {
        session,
        kind: LocatorKind::FileOffset {
            path: "sess-1.jsonl".into(),
            offset,
        },
        generation,
    }
}

fn locator(offset: u64) -> SourceLocator {
    locator_for(key(), offset, Some(3))
}

fn summary() -> SessionSummary {
    let mut meta = metadata(key(), "Inventory API");
    meta.project = Some("RipperGame".into());
    meta.activity = ActivityStamp {
        at: Some(NOW - 540_000),
        class: AggregationClass::Reported,
    };
    SessionSummary {
        last_event_at: Some(NOW - 540_000),
        last_completion: None,
        unfinished_work: false,
        displayed_event_count: 3,
        hidden_records: 4,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process: ProcessAssociation::Unavailable,
        usage: None,
        warnings: vec![],
        ..facts::summary(meta)
    }
}

fn app() -> App {
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(vec![summary()], vec![]);
    app.take_requests(); // the initial selection's preview request
    app
}

fn text(s: &str, truncated: bool, byte_len: usize) -> BoundedText {
    BoundedText {
        preview: s.into(),
        byte_len,
        truncated,
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
        actor: Actor::Assistant,
        text: Some(text(body, false, body.len())),
        tool: None,
        usage: None,
        provenance: Provenance {
            harness: Harness::Codex,
            source: SourceKey::File("sess-1.jsonl".into()),
            locator: locator(100 * i),
            record_type: "response_item".into(),
            harness_version: None,
        },
    }
}

fn tool_event(i: u64) -> Event {
    let mut e = event(i, EventKind::Tool, "cargo test");
    e.actor = Actor::Tool;
    e.tool = Some(ToolInteraction {
        name: "shell".into(),
        call_id: Some("call-9".into()),
        input: Some(text("cargo test --all", false, 16)),
        output: Some(text("2 failed assertions\nsecond line", true, 90_000)),
        status: ToolStatus::Error,
        exit_code: Some(1),
        started_at: Some(NOW - 10_000),
        completed_at: Some(NOW - 7_500),
        orphan: false,
        child_session: None,
    });
    e
}

fn diagnostic(offset: u64, label: &str, hidden: bool) -> DiagnosticEntry {
    DiagnosticEntry {
        locator: locator(offset),
        label: label.into(),
        hidden,
    }
}

fn page(events: Vec<Event>, diagnostics: Vec<DiagnosticEntry>) -> TranscriptPage {
    let next = events.last().map(|e| PageAnchor {
        timestamp: e.timestamp,
        source_order: e.source_order,
        stable_id: e.id.clone(),
    });
    TranscriptPage {
        events,
        diagnostics,
        next,
        has_more: false,
        truncated: false,
    }
}

fn open(app: &mut App, first: TranscriptPage) {
    assert!(app.open_inspector());
    let requests = app.take_requests();
    let Some(WorkerRequest::Select { request_id, .. }) = requests
        .iter()
        .find(|r| matches!(r, WorkerRequest::Select { .. }))
    else {
        panic!("expected a Select, got {requests:?}");
    };
    assert!(app.apply_transcript(*request_id, first));
    app.take_requests();
}

fn screen_at(app: &App, w: u16, h: u16) -> String {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    terminal.draw(|f| render(f, app, &ctx)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn draw(app: &App) -> String {
    screen_at(app, 100, 24)
}

/// One event with recorded per-message usage and a recorded API cost.
fn usage_event(i: u64) -> Event {
    let mut e = event(i, EventKind::AssistantMessage, "hello world");
    e.usage = Some(UsageObservation {
        model: None,
        scope: UsageScope::Message,
        categories: TokenCategories {
            input: Some(1_200),
            output: Some(340),
            reasoning: Some(60),
            total: Some(1_600),
            ..TokenCategories::default()
        },
        accounting: TokenAccounting::default(),
        request: None,
        money: RecordedMoney {
            reported_api: Some(MoneyFact {
                usd: 0.0042,
                source: MoneySource::ReportedApi,
                as_of: None,
            }),
            ..RecordedMoney::default()
        },
        provenance: e.provenance.clone(),
    });
    e
}

fn source_request(app: &mut App) -> (u64, SourceLocator) {
    let requests = app.take_requests();
    match requests.as_slice() {
        [
            WorkerRequest::LoadSource {
                request_id,
                locator,
            },
        ] => (*request_id, locator.clone()),
        other => panic!("expected a single LoadSource, got {other:?}"),
    }
}

fn record(raw: &str) -> SourceRecord {
    SourceRecord {
        raw: raw.into(),
        display: agentscope::ingest::text::display_safe(raw),
        unavailable: None,
    }
}

#[test]
fn shows_tool_arguments_output_and_oversized_markers() {
    let mut app = app();
    open(&mut app, page(vec![tool_event(0)], vec![]));
    assert!(app.open_details());
    assert_eq!(app.screen, Screen::Details);
    let out = draw(&app);
    for want in [
        "SHELL",
        "call-9",
        "Error",
        "2.5s",
        "Exit: 1",
        "input (recorded preview)",
        "cargo test --all",
        "output (recorded preview)",
        "2 failed assertions",
        "second line",
        "[truncated: showing 31 of 90000 bytes]",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
}

#[test]
fn details_rendering_is_bounded_for_huge_previews() {
    let mut app = app();
    let mut e = tool_event(0);
    let huge = "x".repeat(2_000_000);
    e.tool.as_mut().unwrap().output = Some(text(&huge, true, 9_000_000));
    open(&mut app, page(vec![e], vec![]));
    assert!(app.open_details());
    let out = draw(&app);
    assert!(out.contains("input (recorded preview)"));
    assert!(out.contains("cargo test --all"));
    app.overlay_scroll(isize::MAX);
    assert!(draw(&app).contains("[display limited"), "marker at the end");
}

#[test]
fn details_wraps_at_word_boundaries() {
    let mut app = app();
    let words = "alpha bravo charlie delta echo foxtrot golf hotel india juliett kilo lima \
                 mike november oscar papa quebec romeo sierra tango uniform victor whiskey";
    let body = format!("- {words}");
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, &body)], vec![]),
    );
    assert!(app.open_details());
    let out = screen_at(&app, 100, 40);
    for word in words.split_whitespace() {
        assert!(
            out.contains(word),
            "word {word:?} was split across rows in:\n{out}"
        );
    }
    assert!(out.contains("- alpha"), "the bullet is kept:\n{out}");
    assert!(out.contains("whiskey"), "{out}");
}

#[test]
fn details_side_column_at_wide_width() {
    let mut app = app();
    open(&mut app, page(vec![usage_event(0)], vec![]));
    assert!(app.open_details());
    let out = screen_at(&app, 130, 30);
    for want in [
        "USAGE (message)",
        "1,600",
        "input",
        "output",
        "reasoning",
        "COST",
        "$0.0042",
        "reported",
        "SOURCE",
        "response_item",
        "file offset",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    let side = out
        .lines()
        .find(|line| line.contains("USAGE (message)"))
        .expect("a side column");
    assert_eq!(
        side.find("USAGE (message)"),
        Some(100),
        "the side column starts after 100 content columns:\n{side}"
    );
    assert!(
        side[..100].contains("reply (recorded preview)"),
        "content and facts share the row:\n{side}"
    );
    assert!(out.contains("hello world"), "{out}");
}

#[test]
fn details_stacks_below_110_columns() {
    let mut app = app();
    open(&mut app, page(vec![usage_event(0)], vec![]));
    assert!(app.open_details());
    let out = screen_at(&app, 109, 40);
    let lines: Vec<&str> = out.lines().collect();
    let content = lines
        .iter()
        .position(|line| line.contains("hello world"))
        .expect("content");
    let usage = lines
        .iter()
        .position(|line| line.contains("USAGE (message)"))
        .expect("usage section");
    assert!(content < usage, "facts stack below the content:\n{out}");
    assert_eq!(
        lines[usage].find("USAGE (message)"),
        Some(2),
        "a stacked section uses the full width after the margin:\n{}",
        lines[usage]
    );
    for want in ["COST", "SOURCE"] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
}

#[test]
fn bracket_keys_move_between_events_and_stop_at_edge() {
    let mut app = app();
    let mut first = page(
        vec![
            event(0, EventKind::AssistantMessage, "one"),
            event(1, EventKind::AssistantMessage, "two"),
            event(2, EventKind::AssistantMessage, "three"),
        ],
        vec![],
    );
    // The page is a window: content before its first event is not loaded.
    first.has_more = true;
    first.next = Some(PageAnchor {
        timestamp: Some(NOW - 3_000_000),
        source_order: 0,
        stable_id: "e0".into(),
    });
    open(&mut app, first);
    assert_eq!(app.transcript().unwrap().selected(), 2);
    assert!(app.open_details());
    assert!(draw(&app).contains("event 3 of 3 loaded"));

    assert!(app.details_step(-1));
    assert_eq!(app.transcript().unwrap().selected(), 1);
    assert!(app.details_step(-1));
    assert_eq!(app.transcript().unwrap().selected(), 0);
    assert!(!app.details_step(-1), "stops at the loaded edge");
    assert_eq!(app.transcript().unwrap().selected(), 0);
    assert!(
        draw(&app).contains("▲ older not loaded"),
        "the existing edge hint:\n{}",
        draw(&app)
    );

    // The keys move one event at a time and never load implicitly.
    press_key(&mut app, crossterm::event::KeyCode::Char(']'));
    assert_eq!(app.transcript().unwrap().selected(), 1);
    press_key(&mut app, crossterm::event::KeyCode::Char(']'));
    assert_eq!(app.transcript().unwrap().selected(), 2);
    press_key(&mut app, crossterm::event::KeyCode::Char(']'));
    assert_eq!(app.transcript().unwrap().selected(), 2);
    assert!(
        app.take_requests().is_empty(),
        "the edge key does not page in more events"
    );

    // The keys belong to Details: the inspector selection is unchanged.
    assert!(app.close_overlay());
    let before = app.transcript().unwrap().selected();
    press_key(&mut app, crossterm::event::KeyCode::Char('['));
    press_key(&mut app, crossterm::event::KeyCode::Char(']'));
    assert_eq!(app.transcript().unwrap().selected(), before);
}

#[test]
fn tool_details_label_recorded_input_and_output_previews() {
    let mut app = app();
    open(&mut app, page(vec![tool_event(0)], vec![]));
    assert!(app.open_details());
    let out = screen_at(&app, 100, 40);
    for want in [
        "SHELL",
        "input (recorded preview)",
        "cargo test --all",
        "output (recorded preview)",
        "2 failed assertions",
        "second line",
        "[truncated: showing 31 of 90000 bytes]",
        "Exit: 1",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert!(
        !out.to_lowercase().contains("full"),
        "a bounded preview is never called full:\n{out}"
    );
}

#[test]
fn details_never_say_full() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![
                tool_event(0),
                event(1, EventKind::Reasoning, "provider reasoning text"),
            ],
            vec![],
        ),
    );
    assert!(app.open_details());
    for step in [0, -1] {
        let out = screen_at(&app, 100, 40);
        assert!(
            !out.to_lowercase().contains("full"),
            "details never claim a bounded preview is full:\n{out}"
        );
        if step != 0 {
            assert!(app.details_step(step));
        }
    }
}

#[test]
fn details_badges_use_the_specified_vocabulary() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![
                event(0, EventKind::Lifecycle, "started"),
                event(1, EventKind::Compaction, "compacted"),
            ],
            vec![],
        ),
    );
    assert!(app.open_details());
    let out = screen_at(&app, 100, 40);
    let header = out.lines().next().unwrap();
    assert!(header.contains("SYSTEM"), "{header}");
    assert!(!header.contains("COMPACTION"), "{header}");

    assert!(app.details_step(-1));
    let out = screen_at(&app, 100, 40);
    let header = out.lines().next().unwrap();
    assert!(header.contains("SYSTEM"), "{header}");
    assert!(!header.contains("LIFECYCLE"), "{header}");
}

#[test]
fn details_show_recorded_cache_write_subcategories() {
    let mut e = event(0, EventKind::AssistantMessage, "answer");
    e.usage = Some(UsageObservation {
        model: None,
        scope: UsageScope::Message,
        categories: TokenCategories {
            cache_write_5m: Some(250),
            cache_write_1h: Some(150),
            total: Some(400),
            ..TokenCategories::default()
        },
        accounting: TokenAccounting::default(),
        request: None,
        money: RecordedMoney::default(),
        provenance: e.provenance.clone(),
    });
    let mut app = app();
    open(&mut app, page(vec![e], vec![]));
    assert!(app.open_details());
    let out = screen_at(&app, 130, 40);
    for want in ["cache write 5m", "cache write 1h", "250", "150"] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert!(
        !out.contains("categories not recorded"),
        "recorded subcategories are not lost:\n{out}"
    );
}

#[test]
fn details_wide_side_facts_scroll_into_view() {
    let mut event = usage_event(0);
    let categories = &mut event.usage.as_mut().unwrap().categories;
    categories.cached_input = Some(800);
    categories.cache_write = Some(400);
    categories.cache_write_5m = Some(250);
    categories.cache_write_1h = Some(150);
    categories.cache_read = Some(900);
    let mut app = app();
    open(&mut app, page(vec![event], vec![]));
    assert!(app.open_details());
    let before = screen_at(&app, 130, 20);
    assert!(
        !before.contains("redacted preview"),
        "the side facts overflow this body:\n{before}"
    );
    app.overlay_scroll(isize::MAX);
    let after = screen_at(&app, 130, 20);
    assert!(after.contains("SOURCE"), "{after}");
    assert!(after.contains("redacted preview"), "{after}");
}

#[test]
fn details_bound_deep_hanging_indentation() {
    let mut app = app();
    let body = format!("{}deeply indented text stays visible", " ".repeat(100));
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, &body)], vec![]),
    );
    assert!(app.open_details());
    let out = screen_at(&app, 100, 40);
    assert!(
        out.contains("deeply indented text stays visible"),
        "oversized indentation must not hide the text:\n{out}"
    );
}

#[test]
fn source_view_loads_by_locator_and_rejects_foreign_locators() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );

    let other = SessionKey {
        native_id: "other".into(),
        ..key()
    };
    // Another session, a superseded generation, and an unreferenced offset.
    for foreign in [
        locator_for(other, 0, Some(3)),
        locator_for(key(), 0, Some(2)),
        locator_for(key(), 12_345, Some(3)),
    ] {
        assert!(!app.open_source(foreign));
        assert!(app.take_requests().is_empty());
        assert_eq!(app.screen, Screen::Inspector);
        assert!(app.source_view().is_none());
    }

    assert!(app.open_selected_source());
    assert_eq!(app.screen, Screen::Source);
    let (id, loc) = source_request(&mut app);
    assert_eq!(loc, locator(0));
    assert!(draw(&app).contains("Loading source record"));

    assert!(!app.apply_source(id + 99, record("wrong request")));
    assert!(app.apply_source(id, record(r#"{"type":"message","text":"hello"}"#)));
    let out = draw(&app);
    assert!(
        out.contains(r#"{"type":"message","text":"hello"}"#),
        "{out}"
    );
    assert!(out.contains("sess-1.jsonl"), "{out}");
    assert!(out.contains("Redacted"), "{out}");

    // A second response for the same request is ignored.
    assert!(!app.apply_source(id, record("again")));

    assert!(app.close_overlay());
    assert_eq!(app.screen, Screen::Inspector);
    assert!(app.source_view().is_none());
}

#[test]
fn unavailable_and_failed_sources_are_shown() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    assert!(app.open_selected_source());
    let (id, _) = source_request(&mut app);
    assert!(app.apply_source(
        id,
        SourceRecord {
            raw: String::new(),
            display: String::new(),
            unavailable: Some("record exceeds limit".into()),
        }
    ));
    assert!(draw(&app).contains("record exceeds limit"));
    app.close_overlay();

    assert!(app.open_selected_source());
    let (id, _) = source_request(&mut app);
    app.transcript_load_failed(id);
    assert!(draw(&app).contains("could not be loaded"));
}

#[test]
fn diagnostics_lists_hidden_unknown_and_source_only_records() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![
                diagnostic(500, "suppressed call record", true),
                diagnostic(600, "unknown record type", false),
                diagnostic(700, "source-only record", false),
            ],
        ),
    );
    assert!(app.open_diagnostics());
    assert_eq!(app.screen, Screen::Diagnostics);
    let out = draw(&app);
    for want in [
        "Diagnostics",
        "Hidden records: 4",
        "[hidden] suppressed call record",
        "[listed] unknown record type",
        "[listed] source-only record",
        "sess-1.jsonl @ byte 500",
        "sess-1.jsonl @ byte 700",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
}

#[test]
fn diagnostics_with_no_entries_says_so() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::UserPrompt, "hi")], vec![]),
    );
    assert!(app.open_diagnostics());
    assert!(draw(&app).contains("No hidden, unknown, or source-only records"));
    assert!(!app.open_diagnostic_source());
    assert!(app.take_requests().is_empty());
}

#[test]
fn diagnostic_entry_selects_and_opens_its_source_record() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![
                diagnostic(500, "unknown record type", false),
                diagnostic(600, "suppressed call record", true),
            ],
        ),
    );
    assert!(app.open_diagnostics());
    app.diagnostics_move(1);
    assert_eq!(app.diagnostics_selected(), 1);
    app.diagnostics_move(10);
    assert_eq!(app.diagnostics_selected(), 1, "clamped to the last entry");

    assert!(app.open_diagnostic_source());
    assert_eq!(app.screen, Screen::Source);
    let (id, loc) = source_request(&mut app);
    assert_eq!(loc, locator(600));
    assert!(app.apply_source(id, record(r#"{"type":"function_call","name":"shell"}"#)));
    assert!(draw(&app).contains("function_call"));

    // Esc returns to the diagnostics list, then to the inspector.
    assert!(app.close_overlay());
    assert_eq!(app.screen, Screen::Diagnostics);
    assert!(app.close_overlay());
    assert_eq!(app.screen, Screen::Inspector);
}

#[test]
fn redaction_toggle_reloads_source_and_never_retains_raw_text() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    let raw = format!(r#"{{"text":"key {SECRET} end"}}"#);

    assert!(app.open_selected_source());
    let (first, _) = source_request(&mut app);
    assert!(app.apply_source(first, record(&raw)));
    let out = draw(&app);
    assert!(!out.contains(SECRET), "redacted by default:\n{out}");
    assert!(out.contains("key"), "{out}");
    assert!(!app.redaction_revealed());

    // x clears the shown text and asks the worker for the record again.
    app.toggle_redaction();
    assert!(app.redaction_revealed());
    assert_eq!(app.source_view().unwrap().state, SourceState::Loading);
    let (second, loc) = source_request(&mut app);
    assert_ne!(first, second);
    assert_eq!(loc, locator(0));
    assert!(draw(&app).contains("redaction is OFF"));
    // The superseded response cannot fill the view.
    assert!(!app.apply_source(first, record(&raw)));
    assert!(app.apply_source(second, record(&raw)));
    assert!(draw(&app).contains(SECRET), "revealed after reload");

    // Back to redacted: the revealed text is gone and rebuilt from a reload.
    app.toggle_redaction();
    assert!(!app.redaction_revealed());
    assert_eq!(app.source_view().unwrap().state, SourceState::Loading);
    let (third, _) = source_request(&mut app);
    assert!(app.apply_source(third, record(&raw)));
    let out = draw(&app);
    assert!(!out.contains(SECRET), "{out}");
    let dbg = format!("{:?}", app.source_view().unwrap());
    assert!(!dbg.contains(SECRET) && !dbg.contains("key"), "{dbg}");

    // Reveal, close, reopen: redaction is back on and nothing was kept.
    app.toggle_redaction();
    let (fourth, _) = source_request(&mut app);
    assert!(app.apply_source(fourth, record(&raw)));
    assert!(app.close_overlay());
    assert!(app.source_view().is_none());
    assert!(!app.redaction_revealed());
    assert!(app.open_selected_source());
    let (fifth, _) = source_request(&mut app);
    assert!(app.apply_source(fifth, record(&raw)));
    assert!(!draw(&app).contains(SECRET));
}

#[test]
fn revealed_source_keeps_terminal_sanitization_and_is_bounded() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    app.toggle_redaction();
    assert_eq!(app.screen, Screen::Source);
    let (id, _) = source_request(&mut app);
    let raw = format!("\u{1b}[31mred{}", "a".repeat(200_000));
    assert!(app.apply_source(id, record(&raw)));
    let view = app.source_view().unwrap();
    let SourceState::Ready { text, truncated } = &view.state else {
        panic!("expected ready");
    };
    assert!(*truncated);
    assert!(text.len() <= agentscope::app::MAX_SOURCE_BYTES);
    assert!(!text.contains('\u{1b}'));
    assert!(!draw(&app).contains('\u{1b}'));
}

#[test]
fn toggling_redaction_after_the_record_left_the_window_does_not_request() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![diagnostic(500, "unknown record type", false)],
        ),
    );
    assert!(app.open_diagnostics());
    assert!(app.open_diagnostic_source());
    let (id, _) = source_request(&mut app);
    assert!(app.apply_source(id, record("x")));

    // Reload the transcript with a new source generation: old locators are stale.
    app.invalidate_transcript(&key());
    let requests = app.take_requests();
    let reload_id = requests
        .iter()
        .find_map(|r| match r {
            WorkerRequest::LoadPage { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .expect("reload page request");
    let mut moved = event(0, EventKind::AssistantMessage, "hello");
    moved.provenance.locator = locator_for(key(), 0, Some(4));
    assert!(app.apply_transcript(reload_id, page(vec![moved], vec![])));
    app.take_requests();

    app.toggle_redaction();
    assert!(
        app.take_requests().is_empty(),
        "stale locator is not requested"
    );
    assert!(draw(&app).contains("no longer part of the loaded transcript"));
}

#[test]
fn x_in_inspector_reloads_the_selected_record_and_reveal_is_scoped_to_it() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![
                event(0, EventKind::AssistantMessage, "a"),
                event(1, EventKind::AssistantMessage, "b"),
            ],
            vec![],
        ),
    );
    let raw = format!("key {SECRET}");
    app.inspector_first();
    app.toggle_redaction();
    assert_eq!(app.screen, Screen::Source);
    let (id, loc) = source_request(&mut app);
    assert_eq!(loc, locator(0));
    assert!(app.apply_source(id, record(&raw)));
    assert!(draw(&app).contains(SECRET));
    assert!(app.close_overlay());
    assert!(!app.redaction_revealed());

    // Another selection is never revealed by the earlier x.
    app.inspector_move(1);
    assert!(app.open_selected_source());
    let (id, loc) = source_request(&mut app);
    assert_eq!(loc, locator(100));
    assert!(app.apply_source(id, record(&raw)));
    assert!(!draw(&app).contains(SECRET));
    assert!(!app.revealed_for(&locator(0)));

    // x in Details and Diagnostics also reloads immediately.
    app.close_overlay();
    assert!(app.open_details());
    app.toggle_redaction();
    assert_eq!(app.screen, Screen::Source);
    assert_eq!(source_request(&mut app).1, locator(100));
    assert!(app.close_overlay());
    assert_eq!(app.screen, Screen::Details);
}

#[test]
fn a_transcript_reload_invalidates_the_open_source_and_stale_responses() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    assert!(app.open_selected_source());
    let (pending, _) = source_request(&mut app);

    app.invalidate_transcript(&key());
    let reload_id = app
        .take_requests()
        .iter()
        .find_map(|r| match r {
            WorkerRequest::LoadPage { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .unwrap();
    let mut moved = event(0, EventKind::AssistantMessage, "hello");
    moved.provenance.locator = locator_for(key(), 0, Some(4));
    assert!(app.apply_transcript(reload_id, page(vec![moved], vec![])));
    app.take_requests();

    assert!(!app.apply_source(pending, record("generation 3 content")));
    let out = draw(&app);
    assert!(!out.contains("generation 3 content"));
    assert!(out.contains("no longer part of the loaded transcript"));
    assert!(matches!(
        app.source_view().unwrap().state,
        SourceState::Unavailable(_)
    ));
}

#[test]
fn diagnostics_selection_is_clamped_after_the_list_shrinks() {
    let mut app = app();
    open(
        &mut app,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![
                diagnostic(500, "a", false),
                diagnostic(600, "b", false),
                diagnostic(700, "c", false),
            ],
        ),
    );
    assert!(app.open_diagnostics());
    app.diagnostics_move(2);
    app.invalidate_transcript(&key());
    let reload_id = app
        .take_requests()
        .iter()
        .find_map(|r| match r {
            WorkerRequest::LoadPage { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .unwrap();
    assert!(app.apply_transcript(
        reload_id,
        page(
            vec![event(0, EventKind::AssistantMessage, "hello")],
            vec![diagnostic(500, "a", false)],
        )
    ));
    app.take_requests();
    assert_eq!(app.diagnostics_selected(), 0);
    assert!(app.open_diagnostic_source());
    assert_eq!(source_request(&mut app).1, locator(500));
}

#[test]
fn o_opens_a_confirmed_tool_child_and_ignores_unresolved_links() {
    use agentscope::domain::SessionLink;
    use agentscope::ui::handle_key;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let child_key = SessionKey {
        harness: Harness::Codex,
        native_id: "child-1".into(),
        source: SourceKey::File("child-1.jsonl".into()),
    };
    let mut child = summary();
    child.meta.key = child_key.clone();
    child.meta.title = "Child session".into();
    let mut opened = app();
    opened.apply_roster(vec![summary(), child], vec![]);

    let mut tool = tool_event(0);
    tool.tool.as_mut().unwrap().child_session = Some(SessionLink::Unresolved("child-1".into()));
    open(&mut opened, page(vec![tool], vec![]));
    assert!(opened.open_details());
    handle_key(
        &mut opened,
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
    );
    assert_eq!(opened.screen, Screen::Inspector);
    assert_eq!(opened.selected_key(), Some(child_key.clone()));
    assert_eq!(opened.transcript().unwrap().key(), &child_key);
    let requests = opened.take_requests();
    assert!(
        requests.iter().any(|r| matches!(
            r,
            WorkerRequest::Select { key, .. } if key == &child_key
        )),
        "the child transcript is requested: {requests:?}"
    );

    // An unresolved or ambiguous link leaves the details screen unchanged.
    for link in [
        SessionLink::Unresolved("missing-child".into()),
        SessionLink::Ambiguous,
    ] {
        let mut fresh = app();
        let mut tool = tool_event(0);
        tool.tool.as_mut().unwrap().child_session = Some(link);
        open(&mut fresh, page(vec![tool], vec![]));
        assert!(fresh.open_details());
        handle_key(
            &mut fresh,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
        );
        assert_eq!(fresh.screen, Screen::Details, "no guessed navigation");
        assert_eq!(fresh.selected_key(), Some(key()));
    }
}

// ---- paged record-level facts: requests and compactions ---------------------

fn compaction(
    id: &str,
    scope: CompactionScope,
    summary: Option<&str>,
    reported_usd: Option<f64>,
) -> CompactionObservation {
    let mut usage = facts::usage(
        id,
        Some("openai"),
        "gpt-5.5",
        TokenCategories::default(),
        TokenAccounting::default(),
    );
    if let Some(usd) = reported_usd {
        usage.money.reported_api = Some(MoneyFact {
            usd,
            source: MoneySource::ReportedApi,
            as_of: None,
        });
    }
    CompactionObservation {
        id: id.into(),
        timestamp: Some(NOW - 120_000),
        method: Some("auto".into()),
        trigger: Some("limit".into()),
        summary: summary.map(|s| text(s, false, s.len())),
        before_tokens: Some(9_000),
        after_tokens: Some(2_500),
        duration_ms: None,
        usage: Some(usage),
        scope,
        provenance: Provenance {
            harness: Harness::Codex,
            source: SourceKey::File("sess-1.jsonl".into()),
            locator: locator(7),
            record_type: "compacted".into(),
            harness_version: None,
        },
    }
}

fn session_screen(app: &App) -> String {
    let (w, h) = (130u16, 60u16);
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    let ctx = RosterContext {
        no_color: true,
        ..RosterContext::default()
    };
    terminal.draw(|f| render(f, app, &ctx)).unwrap();
    terminal
        .backend()
        .buffer()
        .content()
        .chunks(w as usize)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

fn press_key(app: &mut App, code: crossterm::event::KeyCode) {
    agentscope::ui::handle_key(
        app,
        crossterm::event::KeyEvent::new(code, crossterm::event::KeyModifiers::NONE),
    );
}

#[test]
fn session_details_list_compactions_with_separate_event_cost_and_reduction() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    assert!(app.open_session_info());
    let requests = app.take_requests();
    let [WorkerRequest::LoadDetails { request_id, .. }] = requests.as_slice() else {
        panic!("expected one details request, got {requests:?}");
    };
    let hostile = format!(
        "keep this\x1b[31m red\nsecond {SECRET} line {}",
        "tail ".repeat(60)
    );
    let detail = SessionDetailPage {
        requests: vec![facts::usage(
            "req-1",
            Some("openai"),
            "gpt-5.5",
            TokenCategories {
                input: Some(1_200),
                output: Some(340),
                ..TokenCategories::default()
            },
            TokenAccounting::default(),
        )],
        compactions: vec![
            compaction(
                "c-event",
                CompactionScope::Event,
                Some(&hostile),
                Some(0.42),
            ),
            compaction("c-none", CompactionScope::Event, None, None),
            compaction(
                "c-agg",
                CompactionScope::Aggregate,
                Some("aggregate note"),
                Some(9.99),
            ),
        ],
        recent: vec![],
        next: Some(PageAnchor {
            timestamp: Some(NOW),
            source_order: 1,
            stable_id: "older".into(),
        }),
        has_more: true,
        truncated: false,
    };
    assert!(app.apply_details(*request_id, key(), detail));

    let collapsed = session_screen(&app);
    assert!(
        collapsed.contains("1 requests · 3 compactions · 0 events loaded"),
        "{collapsed}"
    );
    assert!(
        collapsed.contains("older records exist (m loads the next page)"),
        "{collapsed}"
    );
    assert!(collapsed.contains("Request"), "{collapsed}");
    assert!(
        collapsed.contains("input 1,200 · output 340"),
        "{collapsed}"
    );
    // Context reduction and event cost are separate facts on separate lines.
    assert!(
        collapsed.contains("context 9,000 → 2,500 (removed 6,500)"),
        "{collapsed}"
    );
    assert!(
        collapsed.contains("Event cost: $0.42 (reported)"),
        "{collapsed}"
    );
    assert!(
        collapsed.contains("Event cost: not recorded for this event"),
        "{collapsed}"
    );
    // An aggregate is never attributed to an event, even when money was recorded.
    assert!(
        collapsed.contains("Event cost: session aggregate only, not attributed to this event"),
        "{collapsed}"
    );
    assert!(!collapsed.contains("9.99"), "{collapsed}");
    // Summaries are sanitized, redacted and collapsed until expanded.
    assert!(
        !collapsed.contains('\x1b') && !collapsed.contains(SECRET),
        "{collapsed}"
    );
    assert!(collapsed.contains("(Space expands)"), "{collapsed}");
    assert!(
        collapsed.matches("tail").count() < 20,
        "collapsed summary is cut"
    );

    press_key(&mut app, crossterm::event::KeyCode::Char(' '));
    assert!(app.summaries_expanded());
    let expanded = session_screen(&app);
    assert!(
        !expanded.contains('\x1b') && !expanded.contains(SECRET),
        "{expanded}"
    );
    assert!(expanded.contains("keep this"), "{expanded}");
    assert!(!expanded.contains("(Space expands)"), "{expanded}");

    // `m` asks for the next older page; the shown pages stay.
    press_key(&mut app, crossterm::event::KeyCode::Char('m'));
    let requests = app.take_requests();
    assert!(
        matches!(
            requests.as_slice(),
            [WorkerRequest::LoadDetails {
                anchor: Some(_),
                ..
            }]
        ),
        "{requests:?}"
    );
    assert!(session_screen(&app).contains("Loading detailed facts"));
}

#[test]
fn partial_category_costs_keep_their_subtotal_and_coverage_in_session_details() {
    let mut s = summary();
    s.usage = Some(UsageSummary {
        total: Some(5_000),
        class: AggregationClass::Reported,
        categories: TokenCategories {
            input: Some(4_000),
            output: Some(1_000),
            ..TokenCategories::default()
        },
        cost: CostEstimate {
            usd: None,
            class: AggregationClass::Partial,
            known_subtotal: 0.05,
            breakdown: CostBreakdown {
                input: Some(0.05),
                ..CostBreakdown::default()
            },
            ..CostEstimate::default()
        },
        coverage: UsageCoverage {
            observed_requests: 2,
            priced_requests: 1,
            complete: false,
            reason: None,
        },
        ..UsageSummary::default()
    });
    let mut app = App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    );
    app.apply_roster(vec![s], vec![]);
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hi")], vec![]),
    );
    assert!(app.open_session_info());
    let screen = session_screen(&app);
    assert!(
        screen.contains("Known subtotal: $0.05 (incomplete, not a total)"),
        "{screen}"
    );
    assert!(screen.contains("Unpriced coverage: output"), "{screen}");
    assert!(screen.contains("Priced requests: 1 of 2"), "{screen}");
    assert!(screen.contains("Cost: $0.05+ partial 1/2"), "{screen}");
    assert!(
        !screen.contains("Cost: $0.05 "),
        "no complete total is claimed:\n{screen}"
    );
}

#[test]
fn m_on_a_session_without_request_rows_pages_visible_recent_events() {
    let mut app = app();
    open(
        &mut app,
        page(vec![event(0, EventKind::AssistantMessage, "hello")], vec![]),
    );
    assert!(app.open_session_info());
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadDetails {
            request_id: first, ..
        },
    ] = requests.as_slice()
    else {
        panic!("{requests:?}");
    };
    // A Codex-style session: cumulative usage only, so pages carry events alone.
    let anchor = |i: u64| PageAnchor {
        timestamp: Some(NOW),
        source_order: i,
        stable_id: format!("e{i}"),
    };
    let page_of = |events: Vec<Event>, next: Option<PageAnchor>| SessionDetailPage {
        recent: events,
        next: next.clone(),
        has_more: next.is_some(),
        ..SessionDetailPage::default()
    };
    let newest = vec![
        event(9, EventKind::AssistantMessage, "newest reply one"),
        event(
            8,
            EventKind::UserPrompt,
            "newest prompt\x1b[31m red\nsecond line",
        ),
    ];
    assert!(app.apply_details(*first, key(), page_of(newest, Some(anchor(8)))));
    let one = session_screen(&app);
    assert!(
        one.contains("0 requests · 0 compactions · 2 events loaded"),
        "{one}"
    );
    assert!(
        one.contains("newest reply one") && one.contains("newest prompt"),
        "{one}"
    );
    assert!(!one.contains('\x1b'), "{one}");
    assert!(
        one.contains("older records exist (m loads the next page)"),
        "{one}"
    );
    assert!(!one.contains("older reply"));

    press_key(&mut app, crossterm::event::KeyCode::Char('m'));
    let requests = app.take_requests();
    let [
        WorkerRequest::LoadDetails {
            request_id: second,
            anchor: Some(_),
            ..
        },
    ] = requests.as_slice()
    else {
        panic!("{requests:?}");
    };
    let older = vec![event(7, EventKind::AssistantMessage, "older reply two")];
    assert!(app.apply_details(*second, key(), page_of(older, None)));
    let two = session_screen(&app);
    assert!(
        two.contains("0 requests · 0 compactions · 3 events loaded"),
        "{two}"
    );
    assert!(
        two.contains("older reply two") && two.contains("newest reply one"),
        "{two}"
    );
    assert!(
        !two.contains("older records exist"),
        "the last page ends the hint:\n{two}"
    );
}

#[test]
fn o_on_unloaded_child_from_details_shows_notice() {
    use agentscope::domain::SessionLink;
    use agentscope::ui::handle_key;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let unloaded = SessionKey {
        harness: Harness::Codex,
        native_id: "missing-child".into(),
        source: SourceKey::File("missing-child.jsonl".into()),
    };
    let mut app = app();
    let mut tool = tool_event(0);
    tool.tool.as_mut().unwrap().child_session = Some(SessionLink::Resolved(unloaded));
    open(&mut app, page(vec![tool], vec![]));
    assert!(app.open_details());

    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('o'), KeyModifiers::NONE),
    );
    assert_eq!(app.screen, Screen::Details, "no navigation");
    assert_eq!(app.notice(), Some("child session not loaded"));
    let screen = draw(&app);
    assert!(screen.contains("child session not loaded"), "{screen}");

    // The next key still clears it.
    handle_key(
        &mut app,
        KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE),
    );
    assert_eq!(app.notice(), None);
    assert!(!draw(&app).contains("child session not loaded"));
}
