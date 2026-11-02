use std::process::{Command, Stdio};
use std::time::Duration;

mod support;

use agentscope::app::{ACTIVITY_DEBOUNCE_MS, App, RosterItem};
use agentscope::domain::*;
use agentscope::roster::DiscoveryLimit;
use agentscope::ui::{
    Column, RosterContext, RosterLayout, handle_mouse, render_preview, render_roster,
};
use agentscope::worker::WorkerRequest;
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};
use support::facts::{self, metadata};

const NOW: Timestamp = 100_000_000;

fn app() -> App {
    App::new(
        NOW,
        Thresholds {
            quiet: Duration::from_secs(300),
            stall: Duration::from_secs(900),
        },
    )
}

fn session(
    provider: Harness,
    id: &str,
    title: &str,
    age_secs: i64,
    tokens: Option<u64>,
    process: ProcessAssociation,
) -> SessionSummary {
    let mut meta = metadata(
        SessionKey {
            harness: provider,
            native_id: id.into(),
            source: SourceKey::File(format!("{id}.jsonl").into()),
        },
        title,
    );
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
        displayed_event_count: 1,
        hidden_records: 0,
        server_tools: None,
        lifecycle: Lifecycle::Unknown,
        process,
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

fn populated() -> App {
    let mut app = app();
    app.apply_roster(
        vec![
            session(
                Harness::Codex,
                "a",
                "Inventory API",
                12,
                Some(91_000),
                ProcessAssociation::Matched {
                    pid: 7,
                    started_at: None,
                },
            ),
            session(
                Harness::Claude,
                "b",
                "Scanner refactor",
                600,
                Some(73_000),
                ProcessAssociation::Unmatched,
            ),
            {
                let mut c = session(
                    Harness::OpenCode,
                    "c",
                    "PR review",
                    7200,
                    None,
                    ProcessAssociation::Candidates { count: 2 },
                );
                c.last_event_at = None;
                c.meta.activity.at = None;
                c.last_completion = None;
                c
            },
        ],
        vec![],
    );
    app
}

fn ctx(no_color: bool) -> RosterContext {
    RosterContext {
        no_color,
        roots: vec![
            "Codex: /home/u/.codex/sessions".into(),
            "Claude: /home/u/.claude/projects".into(),
            "OpenCode: /home/u/opencode.db".into(),
        ],
        notes: vec![],
        last_refresh: Some("12:00:00".into()),
        error: None,
    }
}

fn draw(app: &App, ctx: &RosterContext, w: u16, h: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
    terminal.draw(|f| render_roster(f, app, ctx)).unwrap();
    terminal.backend().buffer().clone()
}

fn text(buf: &Buffer) -> String {
    let w = buf.area.width as usize;
    buf.content()
        .chunks(w)
        .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn empty_state_lists_checked_roots_and_warnings() {
    let mut app = app();
    app.apply_roster(
        vec![],
        vec![Warning {
            key: WarningKey {
                harness: Harness::Claude,
                source: "projects".into(),
                kind: WarningKind::Io,
            },
            message: "permission denied".into(),
            count: 1,
        }],
    );
    let out = text(&draw(&app, &ctx(false), 100, 30));
    for want in [
        "No sessions found",
        "/home/u/.codex/sessions",
        "/home/u/.claude/projects",
        "/home/u/opencode.db",
        "Claude",
        "permission denied",
        "12:00:00",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    assert!(!out.to_lowercase().contains("start an agent"));
}

#[test]
fn no_color_mode_keeps_text_labels() {
    let app = populated();
    let plain = draw(&app, &ctx(true), 120, 40);
    for cell in plain.content() {
        assert_eq!(cell.fg, Color::Reset);
        assert_eq!(cell.bg, Color::Reset);
    }
    let out = text(&plain);
    assert!(
        out.contains("● 1 active") && out.contains("! 1 quiet") && out.contains("? 1 unknown"),
        "{out}"
    );

    let colored = draw(&app, &ctx(false), 120, 40);
    assert!(colored.content().iter().any(|c| c.fg != Color::Reset));
    assert!(
        text(&colored).contains("Live: on"),
        "the colored screen keeps its words"
    );
}

#[test]
fn too_small_terminal_shows_message() {
    let out = text(&draw(&populated(), &ctx(false), 50, 15));
    assert!(out.contains("too small"), "{out}");
    assert!(out.contains("60x20"), "{out}");
    assert!(!out.contains("STATUS"));
}

#[test]
fn untrusted_text_is_sanitized() {
    let mut app = app();
    app.apply_roster(
        vec![session(
            Harness::Codex,
            "x",
            "evil\x1b[31m title\u{202e}",
            5,
            None,
            ProcessAssociation::NotChecked,
        )],
        vec![],
    );
    let out = text(&draw(&app, &ctx(false), 120, 40));
    assert!(!out.contains('\x1b') && !out.contains('\u{202e}'));
}

#[test]
fn non_tty_exits_with_a_message_without_entering_raw_mode() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .arg("--no-processes")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(
        err.contains("terminal") && err.contains("agentscope inspect"),
        "{err}"
    );
    assert!(
        !output.stdout.contains(&0x1b),
        "no escape sequences on stdout"
    );
    assert!(!err.contains('\x1b'));
}

#[test]
fn help_and_version_exit_zero_without_starting_the_tui() {
    for (flag, want) in [
        ("--help", "--codex-sessions"),
        ("--version", "agentscope 0.1.0"),
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
            .arg(flag)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag}");
        let out = String::from_utf8_lossy(&output.stdout);
        assert!(out.contains(want), "{flag}: {out}");
        assert!(!out.contains('\x1b'));
    }
}

#[test]
fn unknown_flag_fails_with_usage() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .arg("--bogus")
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Usage"));
}

#[test]
fn every_documented_flag_parses_before_the_tty_check() {
    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .args([
            "--config",
            "c.toml",
            "--codex-sessions=a",
            "--claude-projects",
            "b",
            "--opencode-db",
            "d.db",
            "--no-processes",
            "--no-color",
        ])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&output.stderr);
    assert!(err.contains("must be a terminal"), "{err}");

    let output = Command::new(env!("CARGO_BIN_EXE_agentscope"))
        .arg("--config")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires a value"));
}

fn warning(provider: Harness, msg: &str) -> Warning {
    Warning {
        key: WarningKey {
            harness: provider,
            source: "s".into(),
            kind: WarningKind::Io,
        },
        message: msg.into(),
        count: 1,
    }
}

fn scan() -> ScanProgress {
    ScanProgress {
        harness: Harness::Codex,
        scanned: 0,
        total: None,
        done: true,
    }
}

#[test]
fn startup_notes_merge_with_live_warnings_in_the_banner() {
    let mut app = populated();
    app.apply_roster(
        app.all_rows().iter().map(|r| r.session.clone()).collect(),
        vec![warning(Harness::Claude, "permission denied")],
    );
    let mut c = ctx(false);
    c.notes = vec![
        "OpenCode unavailable: no database".into(),
        "Codex unavailable: bad root".into(),
    ];
    let out = text(&draw(&app, &c, 120, 40));
    assert!(out.contains("OpenCode unavailable"), "{out}");
    assert!(out.contains("(+2 more)"), "{out}");

    c.notes.clear();
    let out = text(&draw(&app, &c, 120, 40));
    assert!(out.contains("Claude: permission denied"), "{out}");
    assert!(!out.contains("OpenCode unavailable"));
}

#[test]
fn respawn_resets_generation_and_stale_state() {
    let mut app = populated();
    for generation in 1..=3 {
        assert!(app.apply_roster_snapshot(support::snapshot(generation, vec![], scan()), vec![]));
    }
    assert!(!app.apply_roster_snapshot(support::snapshot(1, vec![], scan()), vec![]));

    let mut c = ctx(false);
    c.error = Some("worker thread exited".into());
    c.notes = vec!["stale".into()];
    agentscope::ui::reset_for_new_worker(&mut app, &mut c);
    assert!(c.error.is_none() && c.notes.is_empty() && c.last_refresh.is_none());
    assert!(app.live);
    assert!(app.apply_roster_snapshot(
        support::snapshot(1, vec![], scan()),
        vec![warning(Harness::Codex, "fresh")]
    ));
    assert_eq!(app.warnings().len(), 1);
}

#[test]
fn lazy_worker_builds_readers_off_the_calling_thread() {
    use agentscope::config::{CliOverrides, Config};
    use agentscope::worker::{WorkerEvent, WorkerHandle};
    let config = Config::load_with_env(
        &CliOverrides {
            no_processes: true,
            ..Default::default()
        },
        &Default::default(),
    )
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let worker = WorkerHandle::spawn_lazy(
        Box::new(move || {
            tx.send(std::thread::current().id()).unwrap();
            Vec::new()
        }),
        config,
    );
    let built_on = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_ne!(built_on, std::thread::current().id());
    let event = worker.rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(matches!(
        event,
        WorkerEvent::Roster { .. }
            | WorkerEvent::RefreshFinished { .. }
            | WorkerEvent::Processes { .. }
    ));
    assert!(worker.shutdown());
}

// ---- two-line aligned layout, responsive preview, rich facts ---------------

fn model(id: &str, provider: Option<&str>, effort: Option<&str>) -> ModelDetail {
    ModelDetail {
        id: id.into(),
        provider: provider.map(Into::into),
        variant: None,
        effort: effort.map(Into::into),
        configured_effort: None,
    }
}

/// A hydrated session with a model, a complete estimate, active time and speed.
fn rich(provider: Harness, id: &str, title: &str) -> SessionSummary {
    let mut s = session(
        provider,
        id,
        title,
        12,
        Some(91_000),
        ProcessAssociation::Unmatched,
    );
    let m = model("gpt-5.5", Some("openai"), Some("high"));
    s.meta.model = Some(m.clone());
    s.usage = Some(UsageSummary {
        total: Some(91_000),
        class: AggregationClass::Reported,
        cost: CostEstimate {
            usd: Some(0.23),
            class: AggregationClass::Estimated,
            as_of: Some("2026-10-30".into()),
            source: Some(MoneySource::BundledReference),
            known_subtotal: 0.23,
            ..CostEstimate::default()
        },
        ..UsageSummary::default()
    });
    s.metrics.timing = SessionTiming {
        active_ms: Some(3_900_000),
        active_complete: true,
        ..SessionTiming::default()
    };
    s.metrics.speed.latest = Some(TimedRate {
        model: Some(m),
        request_id: None,
        started_at: Some(NOW - 190_000),
        completed_at: Some(NOW - 180_000),
        output_tokens: 1_000,
        duration_ms: 10_000.0,
        ttft_ms: Some(500.0),
        tokens_per_second: 84.2,
    });
    s
}

fn rich_app(extra: Vec<SessionSummary>) -> App {
    let mut app = app();
    let mut rows = vec![
        rich(Harness::Codex, "a", "Inventory API"),
        rich(Harness::Claude, "b", "Scanner refactor"),
    ];
    rows.extend(extra);
    app.apply_roster(rows, vec![]);
    app
}

fn lines_of(buf: &Buffer) -> Vec<String> {
    text(buf).lines().map(str::to_owned).collect()
}

/// The two physical lines of the row whose title is `title`. The match must sit
/// in the list's title column, so a preview line sharing the row never matches.
fn row_pair(lines: &[String], title: &str) -> (String, String) {
    let at = lines
        .iter()
        .position(|line| {
            line.find(title)
                .is_some_and(|byte| line[..byte].chars().count() < 40)
        })
        .unwrap_or_else(|| panic!("{title:?} not drawn in:\n{}", lines.join("\n")));
    (lines[at].clone(), lines[at + 1].clone())
}

#[test]
fn hostile_row_text_cannot_break_the_two_line_rows() {
    let mut hostile = rich(
        Harness::Codex,
        "evil",
        "evil\x1b[31m title\u{202e}\nsecond\r\nline",
    );
    hostile.meta.project = Some("proj\x1b]0;owned\x07\nname".into());
    hostile.meta.model = Some(model(
        "m\x1b[2Jodel\nx",
        Some("route\x1b[31m-x\ny"),
        Some("hi\x1b]0;x\x07gh\nz"),
    ));
    let app = {
        let mut app = app();
        app.apply_roster(
            vec![hostile, rich(Harness::Claude, "ok", "Clean row")],
            vec![],
        );
        app
    };
    let buf = draw(&app, &ctx(true), 90, 25);
    let out = text(&buf);
    assert!(
        !out.contains('\x1b') && !out.contains('\u{202e}') && !out.contains('\u{7}'),
        "{out}"
    );
    let lines = lines_of(&buf);
    assert_eq!(lines.len(), 25);
    for line in &lines {
        assert_eq!(line.chars().count(), 90, "no row grew or shrank: {line:?}");
    }
    let (first, second) = row_pair(&lines, "evil");
    assert!(first.contains("Codex") || first.contains("evil"), "{first}");
    assert!(second.contains("route-x"), "{second}");
    // The clean row's group heading follows the hostile row's two lines.
    let evil = lines.iter().position(|l| l.contains("evil")).unwrap();
    assert!(
        lines[evil + 3].contains("Clean row"),
        "{}",
        lines.join("\n")
    );
}

#[test]
fn value_columns_drop_from_the_right_at_narrow_widths() {
    let narrow = RosterLayout::for_area(Rect::new(0, 0, 60, 20));
    assert_eq!(
        narrow
            .columns
            .iter()
            .map(|placed| placed.column)
            .collect::<Vec<_>>(),
        vec![Column::Status, Column::Speed, Column::Tokens],
        "cost goes first; the status always stays"
    );
    let wide = RosterLayout::for_area(Rect::new(0, 0, 150, 40));
    assert_eq!(
        wide.columns
            .iter()
            .map(|placed| placed.column)
            .collect::<Vec<_>>(),
        Column::ALL.to_vec()
    );
    for layout in [&narrow, &wide] {
        for pair in layout.columns.windows(2) {
            assert!(pair[0].x + pair[0].width < pair[1].x, "{pair:?}");
        }
        for placed in &layout.columns {
            assert!(placed.x + placed.width <= layout.rows.right());
            assert_eq!(layout.column_at(placed.x), Some(placed.column));
        }
    }
    assert_eq!(
        narrow.column_at(0),
        None,
        "the pinned marker and title are not value columns"
    );
}

#[test]
fn the_preview_takes_45_columns_from_150_and_never_below() {
    let app = rich_app(vec![]);
    let wide = RosterLayout::for_area(Rect::new(0, 0, 150, 30));
    let preview = wide.preview.expect("preview column at 150");
    assert_eq!((preview.width, preview.x), (45, 105));
    // One blank column separates the value columns from the preview rule.
    assert_eq!(wide.rows.width, 104);
    assert!(
        RosterLayout::for_area(Rect::new(0, 0, 149, 30))
            .preview
            .is_none()
    );

    let split = text(&draw(&app, &ctx(true), 160, 40));
    assert!(split.contains("LAST ACTIVITY"), "{split}");
    assert!(
        split.contains("RipperGame · gpt-5.5 · high · openai"),
        "{split}"
    );
    let narrow = text(&draw(&app, &ctx(true), 149, 40));
    assert!(!narrow.contains("LAST ACTIVITY"), "{narrow}");

    let small = RosterLayout::for_area(Rect::new(0, 0, 59, 19));
    assert!(!small.usable() && small.columns.is_empty());
}

fn locate(buf: &Buffer, needle: &str) -> (u16, u16) {
    let width = buf.area.width as usize;
    for y in 0..buf.area.height as usize {
        let row: String = buf.content()[y * width..(y + 1) * width]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        if let Some(byte) = row.find(needle) {
            return (row[..byte].chars().count() as u16, y as u16);
        }
    }
    panic!("{needle:?} not drawn in:\n{}", text(buf));
}

/// First occurrence whose first cell lies left of `limit`: the side preview
/// shares physical rows with the list, so list assertions must stay in it.
fn locate_left(buf: &Buffer, needle: &str, limit: u16) -> (u16, u16) {
    let width = buf.area.width as usize;
    for y in 0..buf.area.height as usize {
        let row: String = buf.content()[y * width..(y + 1) * width]
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        if let Some(byte) = row.find(needle) {
            let x = row[..byte].chars().count() as u16;
            if x < limit {
                return (x, y as u16);
            }
        }
    }
    panic!("{needle:?} not drawn left of {limit} in:\n{}", text(buf));
}

fn key_of(provider: Harness, id: &str) -> SessionKey {
    SessionKey {
        harness: provider,
        native_id: id.into(),
        source: SourceKey::File(format!("{id}.jsonl").into()),
    }
}

fn click(app: &mut App, area: Rect, x: u16, y: u16) {
    let layout = RosterLayout::for_area(area);
    handle_mouse(
        app,
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: x,
            row: y,
            modifiers: KeyModifiers::NONE,
        },
        &layout,
    );
}

/// A session with a model and speed but no recorded cost.
fn uncosted() -> SessionSummary {
    let mut s = session(
        Harness::Codex,
        "a",
        "Inventory API",
        12,
        Some(91_000),
        ProcessAssociation::Unmatched,
    );
    let m = model("gpt-5.5", Some("openai"), Some("high"));
    s.meta.model = Some(m.clone());
    s.metrics.speed.latest = Some(TimedRate {
        model: Some(m),
        request_id: None,
        started_at: Some(NOW - 190_000),
        completed_at: Some(NOW - 180_000),
        output_tokens: 1_000,
        duration_ms: 10_000.0,
        ttft_ms: Some(500.0),
        tokens_per_second: 84.2,
    });
    s
}

#[test]
fn renders_c2_rows_at_150x40() {
    let mut s = rich(Harness::Codex, "a", "Inventory API");
    s.process = ProcessAssociation::Matched {
        pid: 7,
        started_at: None,
    };
    let mut app = app();
    app.apply_roster(vec![s], vec![]);
    let lines = lines_of(&draw(&app, &ctx(true), 150, 40));
    let (first, second) = row_pair(&lines, "Inventory API");
    assert!(first.contains("active"), "status column:\n{first}");
    assert!(first.contains("91k"), "tokens column:\n{first}");
    assert!(second.contains("[CDX]"), "agent tag:\n{second}");
    assert!(second.contains("gpt-5.5"), "model id:\n{second}");
}

#[test]
fn renders_group_header_with_totals() {
    let out = text(&draw(&populated(), &ctx(true), 120, 40));
    let header = out
        .lines()
        .find(|line| line.contains("RipperGame"))
        .unwrap_or_else(|| panic!("no group header:\n{out}"));
    assert!(header.contains("3 sessions"), "{header}");
    assert!(header.contains("164k+"), "partial tokens keep +:\n{header}");
}

#[test]
fn top_line_shows_live_and_nonzero_counts_only() {
    let out = text(&draw(&populated(), &ctx(true), 120, 40));
    let top = out.lines().next().unwrap();
    assert!(top.contains("Live: on"), "{top}");
    assert!(top.contains("● 1 active"), "{top}");
    assert!(!top.contains("0 failed"), "{top}");
    assert!(!top.contains("✗ 0"), "{top}");
}

#[test]
fn live_paused_uses_warning_style() {
    let mut c = ctx(false);
    c.error = Some("worker thread exited".into());
    let buf = draw(&populated(), &c, 120, 40);
    let (x, y) = locate(&buf, "Live: paused");
    let width = buf.area.width as usize;
    let cell = &buf.content()[y as usize * width + x as usize];
    assert_eq!(cell.fg, Color::Yellow, "paused is a warning");
}

#[test]
fn narrow_terminal_drops_cost_then_speed() {
    let mut app = app();
    app.apply_roster(vec![uncosted()], vec![]);
    let out = text(&draw(&app, &ctx(true), 60, 20));
    assert!(out.contains("STATUS"), "status stays:\n{out}");
    assert!(out.contains("Inventory API"), "{out}");
    assert!(!out.contains('$'), "the cost column is dropped:\n{out}");
}

#[test]
fn selection_marker_visible_without_color() {
    let app = rich_app(vec![]);
    let layout = RosterLayout::for_area(Rect::new(0, 0, 120, 40));
    let buf = draw(&app, &ctx(true), 120, 40);
    let lines = lines_of(&buf);
    let at = lines
        .iter()
        .position(|line| line.contains("Inventory API"))
        .unwrap();
    assert!(lines[at].starts_with('▌'), "{:?}", lines[at]);
    assert!(lines[at + 1].starts_with('▌'), "{:?}", lines[at + 1]);

    let width = buf.area.width as usize;
    for (_, y) in [(0, at), (0, at + 1)] {
        for x in 0..layout.rows.width as usize {
            let cell = &buf.content()[y * width + x];
            assert!(
                cell.modifier.contains(Modifier::REVERSED),
                "selected line {y} cell {x} is not reverse video"
            );
        }
    }
    let (_, other) = locate_left(&buf, "Scanner refactor", layout.rows.width);
    for x in 0..layout.rows.width as usize {
        let cell = &buf.content()[other as usize * width + x];
        assert!(
            !cell.modifier.contains(Modifier::REVERSED),
            "an unselected row is not reverse video (x={x})"
        );
    }
}

#[test]
fn click_on_group_header_selects_nothing() {
    let area = Rect::new(0, 0, 120, 40);
    let layout = RosterLayout::for_area(area);
    let mut app = populated();
    let before = app.selected_key();
    click(&mut app, area, 12, layout.rows.y);
    assert_eq!(app.selected_key(), before);
}

#[test]
fn click_on_second_line_of_session_selects_it() {
    let area = Rect::new(0, 0, 120, 40);
    let layout = RosterLayout::for_area(area);
    let mut app = populated();
    click(&mut app, area, 12, layout.rows.y + 4);
    assert_eq!(
        app.selected_key(),
        Some(key_of(Harness::Claude, "b")),
        "the second line belongs to the row"
    );
}

#[test]
fn expanded_children_draw_tree_lines() {
    let mut app = app();
    app.apply_roster(
        vec![session(
            Harness::Codex,
            "parent",
            "Parent task",
            12,
            None,
            ProcessAssociation::Unmatched,
        )],
        vec![],
    );
    let parent = key_of(Harness::Codex, "parent");
    app.load_children(&parent);
    let request_id = app
        .take_requests()
        .into_iter()
        .find_map(|request| match request {
            WorkerRequest::LoadChildren { request_id, .. } => Some(request_id),
            _ => None,
        })
        .expect("a child page was requested");
    app.apply_children(
        request_id,
        &parent,
        vec![
            std::sync::Arc::new(metadata(key_of(Harness::Codex, "child-1"), "First child")),
            std::sync::Arc::new(metadata(key_of(Harness::Codex, "child-2"), "Last child")),
        ],
        None,
    );
    assert!(app.select_row(&parent));
    assert!(app.fold_selected(true));
    let out = text(&draw(&app, &ctx(true), 120, 40));
    assert!(out.contains('├'), "a non-last child has a branch:\n{out}");
    assert!(
        out.contains('└'),
        "the last child closes the branch:\n{out}"
    );
}

#[test]
fn click_on_fold_marker_toggles_children() {
    let area = Rect::new(0, 0, 120, 40);
    let layout = RosterLayout::for_area(area);
    let mut app = populated();
    let first = key_of(Harness::Codex, "a");
    let child = key_of(Harness::Codex, "child-1");
    app.take_requests();
    assert!(app.load_children(&first));
    let request_id = app
        .take_requests()
        .into_iter()
        .find_map(|request| match request {
            WorkerRequest::LoadChildren { request_id, .. } => Some(request_id),
            _ => None,
        })
        .expect("a child page was requested");
    assert!(app.apply_children(
        request_id,
        &first,
        vec![std::sync::Arc::new(metadata(child.clone(), "Child task"))],
        None,
    ));
    assert!(app.select_row(&first));
    assert!(!app.children_expanded(&first));
    assert!(
        !app.roster_rows()
            .any(|item| matches!(item, RosterItem::Session(row) if row.depth == 1)),
        "children start folded"
    );

    // The drawn `▸ +1` starts after the marker strip and the status glyph.
    let fold_x = layout.rows.x + 4;
    // The second line at that column is quiet metadata (the agent tag), not a
    // fold control: it selects the row and leaves the family folded.
    let other = key_of(Harness::Claude, "b");
    assert!(app.select_row(&other));
    click(&mut app, area, fold_x, layout.rows.y + 2);
    assert_eq!(app.selected_key(), Some(first.clone()));
    assert!(
        !app.children_expanded(&first),
        "the second line is not a fold target"
    );

    click(&mut app, area, fold_x, layout.rows.y + 1);
    assert!(
        app.children_expanded(&first),
        "clicking the displayed fold glyph expands"
    );
    assert!(
        app.roster_rows()
            .any(|item| matches!(&item, RosterItem::Session(row) if row.session.meta.key == child)),
        "the loaded child is now drawn"
    );

    // The selection strip keeps its legacy fold action.
    click(&mut app, area, 0, layout.rows.y + 1);
    assert!(!app.children_expanded(&first));
}

#[test]
fn roster_rows_never_print_unknown_or_provenance_words() {
    let mut s = rich(Harness::Codex, "a", "Inventory API");
    s.usage = Some(UsageSummary {
        total: Some(185_722_239),
        class: AggregationClass::Derived,
        cost: CostEstimate {
            usd: Some(1.77),
            class: AggregationClass::Estimated,
            known_subtotal: 1.77,
            ..CostEstimate::default()
        },
        ..UsageSummary::default()
    });
    let mut app = app();
    app.apply_roster(vec![s], vec![]);
    let layout = RosterLayout::for_area(Rect::new(0, 0, 150, 40));
    let lines = lines_of(&draw(&app, &ctx(true), 150, 40));
    let start = layout.rows.y as usize;
    let list = lines[start..start + layout.rows.height as usize]
        .iter()
        .map(|line| {
            line.chars()
                .take(layout.rows.width as usize)
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    for word in [
        "Unknown",
        "derived",
        "reported",
        "unpriced",
        "estimated",
        "partial",
    ] {
        assert!(
            !list.contains(word),
            "{word:?} leaked into the rows:\n{list}"
        );
    }
}

#[test]
fn warnings_line_shows_scanning_provisional_and_limited() {
    let mut app = app();
    let mut snap = support::snapshot(
        1,
        vec![session(
            Harness::Codex,
            "a",
            "Inventory API",
            12,
            Some(91_000),
            ProcessAssociation::Unmatched,
        )],
        ScanProgress {
            harness: Harness::Codex,
            scanned: 5,
            total: Some(10),
            done: false,
        },
    );
    snap.provisional = true;
    snap.has_more = true;
    snap.refresh_available = true;
    snap.limited = Some(DiscoveryLimit {
        sources: 2,
        bytes: 3 * 1024 * 1024,
    });
    assert!(app.apply_roster_snapshot(snap, vec![]));
    let lines = lines_of(&draw(&app, &ctx(true), 200, 40));
    let notes = &lines[1];
    for want in [
        "Scanning Codex 5/10",
        "provisional order",
        "more available (m)",
        "Discovery limited at 2 sources / 3 MiB",
        "refresh available (r)",
    ] {
        assert!(notes.contains(want), "missing {want:?} in:\n{notes}");
    }

    // A stopped worker does not hide the operational state it still reports.
    let mut c = ctx(true);
    c.error = Some("worker thread exited".into());
    let lines = lines_of(&draw(&app, &c, 200, 40));
    let notes = &lines[1];
    for want in [
        "Worker stopped",
        "Scanning Codex 5/10",
        "provisional order",
        "Discovery limited at 2 sources / 3 MiB",
    ] {
        assert!(notes.contains(want), "missing {want:?} in:\n{notes}");
    }
}

#[test]
fn group_header_without_values_shows_dashes() {
    let mut app = app();
    app.apply_roster(
        vec![session(
            Harness::Codex,
            "a",
            "Inventory API",
            12,
            None,
            ProcessAssociation::Unmatched,
        )],
        vec![],
    );
    let out = text(&draw(&app, &ctx(true), 120, 40));
    let header = out
        .lines()
        .find(|line| line.contains("RipperGame"))
        .unwrap_or_else(|| panic!("no header:\n{out}"));
    assert_eq!(header.matches('—').count(), 2, "{header}");
}

#[test]
fn stale_speed_renders_quiet() {
    let mut s = rich(Harness::Codex, "a", "Inventory API");
    s.meta.model = Some(model("deepseek-v4.1", None, None));
    let mut app = app();
    app.apply_roster(vec![s], vec![]);
    let layout = RosterLayout::for_area(Rect::new(0, 0, 150, 40));
    let buf = draw(&app, &ctx(false), 150, 40);
    let (x, y) = locate_left(&buf, "84.2", layout.rows.width);
    let width = buf.area.width as usize;
    let cell = &buf.content()[y as usize * width + x as usize];
    assert_eq!(cell.fg, Color::DarkGray, "speed from an old model is quiet");
    let lines = lines_of(&buf);
    let second = &lines[y as usize + 1];
    assert!(second.contains("old gpt-5.5"), "old model label:\n{second}");
    assert!(second.contains("ago"), "rate age:\n{second}");
}

#[test]
fn selected_row_has_background_in_color_mode() {
    let app = rich_app(vec![]);
    let layout = RosterLayout::for_area(Rect::new(0, 0, 150, 40));
    let buf = draw(&app, &ctx(false), 150, 40);
    let (_, y) = locate_left(&buf, "Inventory API", layout.rows.width);
    let width = buf.area.width as usize;
    let (tag_x, tag_y) = locate_left(&buf, "CDX", layout.rows.width);
    assert_eq!(tag_y, y + 1, "the tag owns the selected row's second line");
    // " CDX " is one span; the padded spaces carry the tag background too.
    let tag_start = tag_x as usize - 1;
    let tag_end = tag_x as usize + 3;
    for line in [y as usize, y as usize + 1] {
        for x in 0..layout.rows.width as usize {
            let cell = &buf.content()[line * width + x];
            if line == y as usize + 1 && (tag_start..=tag_end).contains(&x) {
                assert_eq!(cell.bg, Color::Blue, "the tag keeps its own background");
                assert_eq!(cell.fg, Color::Black, "and its chosen foreground");
                assert_ne!(cell.fg, cell.bg, "selected tag text stays readable");
            } else {
                assert_eq!(
                    cell.bg,
                    Color::Black,
                    "({x},{line}) outside the selected background"
                );
            }
        }
    }
    let (_, other) = locate_left(&buf, "Scanner refactor", layout.rows.width);
    for x in 0..layout.rows.width as usize {
        assert_eq!(
            buf.content()[other as usize * width + x].bg,
            Color::Reset,
            "an unselected row has no selection background (x={x})"
        );
    }
}

#[test]
fn a_large_roster_keeps_the_selected_row_in_the_viewport() {
    let mut app = app();
    let total = 40_000usize;
    let rows: Vec<SessionSummary> = (0..total)
        .map(|i| {
            session(
                Harness::Codex,
                &format!("s{i}"),
                &format!("Session {i}"),
                12,
                None,
                ProcessAssociation::Unmatched,
            )
        })
        .collect();
    let last = rows[total - 1].meta.key.clone();
    app.apply_roster(rows, vec![]);
    assert!(app.select_row(&last));

    let layout = RosterLayout::for_area(Rect::new(0, 0, 120, 40));
    let selected = app.visual_selected_index().expect("a selected row");
    assert!(
        layout
            .slots(&app)
            .iter()
            .any(|(_, index)| *index == selected),
        "a prefix longer than 65,535 lines still scrolls the selected row into view"
    );
}

#[test]
fn layouts_render_at_60x20_100x30_150x40() {
    let app = rich_app(vec![]);
    for (w, h) in [(60, 20), (100, 30), (150, 40)] {
        let out = text(&draw(&app, &ctx(true), w, h));
        for want in ["AgentScope", "STATUS", "▌", "Inventory API"] {
            assert!(out.contains(want), "{w}x{h} missing {want:?}:\n{out}");
        }
    }
}

/// Preview of the only row, drawn with room for unwrapped lines.
fn preview_of(summary: SessionSummary) -> String {
    let mut app = app();
    app.apply_roster(vec![summary], vec![]);
    text(&preview_buffer(&app, true))
}

/// The preview rendered against `app`, on a terminal large enough that no
/// section is clipped.
fn preview_buffer(app: &App, no_color: bool) -> Buffer {
    preview_buffer_at(app, no_color, 200, 60)
}

fn preview_buffer_at(app: &App, no_color: bool, width: u16, height: u16) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|f| render_preview(f, app, &ctx(no_color), f.area(), false))
        .unwrap();
    terminal.backend().buffer().clone()
}

/// A session with a known token breakdown, so the bar has every share.
fn categorized(mut summary: SessionSummary) -> SessionSummary {
    summary.usage.as_mut().expect("usage").categories = TokenCategories {
        input: Some(10_000),
        cache_read: Some(40_000),
        cache_write: Some(5_000),
        output: Some(3_000),
        ..TokenCategories::default()
    };
    summary
}

fn preview_app(summary: SessionSummary) -> (App, SessionKey) {
    let mut app = app();
    let key = summary.meta.key.clone();
    app.apply_roster(vec![summary], vec![]);
    app.take_requests();
    (app, key)
}

/// Settle the selection and return the activity request it issued.
fn begin_activity(app: &mut App) -> u64 {
    app.settle_selection(0);
    app.settle_selection(ACTIVITY_DEBOUNCE_MS);
    app.take_requests()
        .into_iter()
        .find_map(|request| match request {
            WorkerRequest::PreviewTail { request_id, .. } => Some(request_id),
            _ => None,
        })
        .expect("the settled selection reads its latest activity")
}

fn event_base(key: &SessionKey, id: &str, at: Timestamp) -> Event {
    Event {
        id: id.into(),
        session: key.clone(),
        timestamp: Some(at),
        source_order: 0,
        kind: EventKind::AssistantMessage,
        actor: Actor::Assistant,
        text: None,
        tool: None,
        usage: None,
        provenance: Provenance {
            harness: key.harness,
            source: key.source.clone(),
            locator: SourceLocator {
                session: key.clone(),
                kind: LocatorKind::FileOffset {
                    path: "activity.jsonl".into(),
                    offset: 0,
                },
                generation: None,
            },
            record_type: "test".into(),
            harness_version: None,
        },
    }
}

fn reply_event(key: &SessionKey, at: Timestamp, text: &str) -> Event {
    let mut event = event_base(key, "reply", at);
    event.text = Some(BoundedText {
        preview: text.into(),
        byte_len: text.len(),
        truncated: false,
        locator: None,
    });
    event
}

fn tool_event(key: &SessionKey, at: Timestamp, input: &str) -> Event {
    let mut event = event_base(key, "tool", at);
    event.kind = EventKind::Tool;
    event.tool = Some(ToolInteraction {
        name: "exec".into(),
        call_id: None,
        input: Some(BoundedText {
            preview: input.into(),
            byte_len: input.len(),
            truncated: false,
            locator: None,
        }),
        output: None,
        status: ToolStatus::Completed,
        exit_code: None,
        started_at: None,
        completed_at: None,
        orphan: false,
        child_session: None,
    });
    event
}

fn activity_page(events: Vec<Event>, has_more: bool) -> TranscriptPage {
    let next = events.first().map(|event| PageAnchor {
        timestamp: event.timestamp,
        source_order: event.source_order,
        stable_id: event.id.clone(),
    });
    TranscriptPage {
        events,
        diagnostics: vec![],
        next,
        has_more,
        truncated: false,
    }
}

#[test]
fn preview_pins_header_numbers_and_bar() {
    let mut s = categorized(rich(Harness::Codex, "p", "Inventory API"));
    s.usage.as_mut().unwrap().total = Some(91_000);
    let mut app = app();
    app.apply_roster(vec![s], vec![]);
    let buf = preview_buffer_at(&app, true, 60, 10);
    let lines = lines_of(&buf);
    let first = &lines[0];
    assert!(
        first.contains("[CDX]") && first.contains("Inventory API"),
        "{first}"
    );
    let at = first.find("Inventory API").expect("title");
    assert!(
        buf.content()[at].modifier.contains(Modifier::BOLD),
        "the title is bold"
    );
    assert!(
        lines[1].contains("RipperGame · gpt-5.5 · high · openai"),
        "{}",
        lines[1]
    );
    let out = lines.join("\n");
    for want in [
        "recent",
        "recent writes; process unmatched",
        "91k reported",
        "~$0.23",
        "84.2 tok/s",
        "cache read 40k",
        "input 10k",
        "cache write 5.0k",
    ] {
        assert!(out.contains(want), "missing {want:?}:\n{out}");
    }
    assert!(
        !out.contains("output 3k"),
        "the legend keeps the top three:\n{out}"
    );

    // Scrolling moves only the sections below the pinned block.
    let _ = preview_buffer_at(&app, true, 60, 13); // records the scroll extent
    app.scroll_preview(100);
    let scrolled = text(&preview_buffer_at(&app, true, 60, 13));
    assert!(
        scrolled
            .lines()
            .next()
            .is_some_and(|l| l.contains("Inventory API")),
        "{scrolled}"
    );
    for want in ["91k reported", "LAST ACTIVITY", "Enter open"] {
        assert!(
            scrolled.contains(want),
            "scrolled past pinned {want:?}:\n{scrolled}"
        );
    }
}

#[test]
fn preview_shows_last_activity_states() {
    let (mut app, key) = preview_app(rich(Harness::Codex, "a", "Activity"));
    let id = begin_activity(&mut app);
    let out = text(&preview_buffer(&app, true));
    let at = out.find("LAST ACTIVITY").expect("section header");
    assert!(
        out[at..]
            .lines()
            .nth(1)
            .is_some_and(|line| line.trim() == "…"),
        "loading shows an ellipsis:\n{out}"
    );

    let page = activity_page(
        vec![
            tool_event(&key, NOW - 2_000, r#"{"command":"cargo test"}"#),
            reply_event(&key, NOW - 1_000, "the reply is wrapped here"),
        ],
        false,
    );
    assert!(app.apply_activity(id, key.clone(), page));
    let out = text(&preview_buffer(&app, true));
    assert!(out.contains("✓ exec cargo test"), "{out}");
    assert!(out.contains("▌ the reply is wrapped here"), "{out}");
    assert!(!out.contains("no reply in recent events"), "{out}");

    let (mut app, key) = preview_app(rich(Harness::Codex, "t", "Tool only"));
    let id = begin_activity(&mut app);
    assert!(app.apply_activity(
        id,
        key.clone(),
        activity_page(
            vec![tool_event(&key, NOW - 1_000, r#"{"command":"cargo test"}"#)],
            false,
        )
    ));
    let out = text(&preview_buffer(&app, true));
    assert!(out.contains("no reply in recent events"), "{out}");

    let (mut app, key) = preview_app(rich(Harness::Codex, "n", "Nothing"));
    let id = begin_activity(&mut app);
    assert!(app.apply_activity(id, key, activity_page(vec![], false)));
    let out = text(&preview_buffer(&app, true));
    assert!(out.contains("no activity recorded"), "{out}");

    let (mut app, _) = preview_app(rich(Harness::Codex, "f", "Failed"));
    let id = begin_activity(&mut app);
    app.load_failed(id);
    let out = text(&preview_buffer(&app, true));
    assert!(out.contains("couldn't read latest activity"), "{out}");
}

#[test]
fn preview_hydration_failure_is_distinct_from_activity_failure() {
    let mut app = app();
    let summary = rich(Harness::Codex, "h", "Failed facts");
    let key = summary.meta.key.clone();
    let mut snap = support::snapshot(
        1,
        vec![SessionSummary::unhydrated(summary.meta.clone())],
        ScanProgress {
            harness: Harness::Codex,
            scanned: 1,
            total: Some(1),
            done: true,
        },
    );
    snap.summaries = Vec::new();
    assert!(app.apply_roster_snapshot(snap, vec![]));
    let hydration = app
        .take_requests()
        .into_iter()
        .find_map(|request| match request {
            WorkerRequest::HydratePreview { request_id, .. } => Some(request_id),
            _ => None,
        })
        .expect("the selection requests preview facts");
    app.load_failed(hydration);
    assert!(app.preview_hydration_failed(&key));

    // A successful activity read is independent: it cannot fill failed facts.
    let activity = begin_activity(&mut app);
    assert!(app.apply_activity(
        activity,
        key.clone(),
        activity_page(
            vec![tool_event(&key, NOW - 1_000, r#"{"command":"cargo test"}"#)],
            false,
        )
    ));
    let out = text(&preview_buffer(&app, true));
    assert!(out.contains("couldn't read session facts"), "{out}");
    assert!(
        out.contains("✓ exec cargo test"),
        "the activity read still succeeds:\n{out}"
    );
}

#[test]
fn preview_reply_keeps_line_breaks_and_stops_at_three_rows() {
    let (mut app, key) = preview_app(rich(Harness::Codex, "r", "Reply lines"));
    let id = begin_activity(&mut app);
    let reply = format!("one\ntwo\n{}OMITTED", "X".repeat(70));
    assert!(app.apply_activity(
        id,
        key.clone(),
        activity_page(vec![reply_event(&key, NOW - 1_000, &reply)], false)
    ));
    // Narrow enough that the long third line wraps: the first three rendered
    // reply rows are kept and the tail is dropped.
    let out = text(&preview_buffer_at(&app, true, 30, 40));
    assert!(out.contains("▌ one"), "{out}");
    assert!(out.contains("▌ two"), "{out}");
    let shown = out.lines().filter(|line| line.starts_with("▌ ")).count();
    assert_eq!(shown, 3, "only three reply rows:\n{out}");
    assert!(!out.contains("OMITTED"), "the tail is not shown:\n{out}");
}

#[test]
fn preview_effort_keeps_its_level_color() {
    for (effort, color) in [("high", Color::Yellow), ("max", Color::Red)] {
        let mut s = rich(Harness::Codex, "e", "Effort");
        s.meta.model = Some(model("gpt-5.5", Some("openai"), Some(effort)));
        let mut app = app();
        app.apply_roster(vec![s], vec![]);
        let buf = preview_buffer(&app, false);
        let (x, y) = locate_left(&buf, effort, buf.area.width);
        let width = buf.area.width as usize;
        assert_eq!(
            buf.content()[y as usize * width + x as usize].fg,
            color,
            "{effort} uses the shared thinking-level color"
        );
    }
}

#[test]
fn preview_token_bar_has_no_color_fallback() {
    let mut app = app();
    app.apply_roster(vec![categorized(rich(Harness::Codex, "b", "Bar"))], vec![]);
    let plain = text(&preview_buffer(&app, true));
    let bar = plain
        .lines()
        .find(|line| ['#', '=', '-', '+'].iter().all(|ch| line.contains(*ch)))
        .unwrap_or_else(|| panic!("no fallback bar:\n{plain}"));
    for ch in ['#', '=', '-', '+'] {
        assert!(bar.contains(ch), "the bar is missing {ch:?}: {bar}");
    }

    let colored = preview_buffer(&app, false);
    let fg: Vec<Color> = colored
        .content()
        .iter()
        .filter(|cell| cell.symbol() == "█")
        .map(|cell| cell.fg)
        .collect();
    for color in [Color::Blue, Color::Green, Color::Yellow, Color::Magenta] {
        assert!(fg.contains(&color), "the bar is missing {color:?}");
    }
}

#[test]
fn preview_never_prints_unknown_word() {
    let mut s = session(
        Harness::Codex,
        "u",
        "No facts",
        12,
        None,
        ProcessAssociation::NotChecked,
    );
    s.meta.project = None;
    s.meta.model = None;
    s.usage = None;
    let out = preview_of(s);
    assert!(
        !out.to_lowercase().contains("unknown"),
        "the preview never prints the word:\n{out}"
    );
    assert!(!out.contains("$0.00"), "no fake price:\n{out}");
    assert!(out.contains('—'), "absent values keep their glyph:\n{out}");
}

#[test]
fn preview_parent_link_only_when_resolved() {
    let parent = rich(Harness::Claude, "parent", "Parent work");
    let parent_key = parent.meta.key.clone();
    let with = |link: SessionLink| {
        let mut child = rich(Harness::Codex, "child", "Child work");
        child.meta.lineage.parent = Some(link);
        let key = child.meta.key.clone();
        let mut app = app();
        app.apply_roster(vec![child, parent.clone()], vec![]);
        assert!(app.select_row(&key));
        text(&preview_buffer(&app, true))
    };

    let out = with(SessionLink::Resolved(parent_key));
    assert!(out.contains("child of Parent work"), "{out}");

    let out = with(SessionLink::Ambiguous);
    assert!(out.contains("parent ambiguous"), "{out}");
    assert!(!out.contains("child of"), "{out}");

    let out = with(SessionLink::Unresolved("ghost".into()));
    assert!(out.contains("parent not found"), "{out}");
    assert!(!out.contains("child of"), "{out}");

    let missing = SessionKey {
        harness: Harness::Pi,
        native_id: "not-loaded".into(),
        source: SourceKey::File("missing.jsonl".into()),
    };
    let out = with(SessionLink::Resolved(missing));
    assert!(
        !out.contains("child of")
            && !out.contains("parent ambiguous")
            && !out.contains("parent not found"),
        "an unloaded parent has no name to show:\n{out}"
    );
}

#[test]
fn preview_shows_old_model_speed_note() {
    let mut s = rich(Harness::Codex, "o", "Old model");
    s.metrics.speed.latest.as_mut().unwrap().model = Some(model("gpt-4.1", Some("openai"), None));
    let mut app = app();
    app.apply_roster(vec![s], vec![]);
    let buf = preview_buffer(&app, false);
    let out = text(&buf);
    assert!(out.contains("old gpt-4.1"), "{out}");
    assert!(out.contains("3m ago"), "{out}");
    let (x, y) = locate_left(&buf, "84.2", buf.area.width);
    let width = buf.area.width as usize;
    assert_eq!(
        buf.content()[y as usize * width + x as usize].fg,
        Color::DarkGray,
        "a stale speed is quiet"
    );
}

#[test]
fn preview_absent_speed_has_no_unit() {
    let mut s = rich(Harness::Codex, "s", "No speed");
    s.metrics.speed.latest = None;
    let out = preview_of(s);
    assert!(!out.contains("tok/s"), "no unit without a rate:\n{out}");
    assert!(out.contains('—'), "the absent glyph stays:\n{out}");
}

#[test]
fn partial_cost_keeps_a_lower_bound_and_never_reads_as_free() {
    let mut s = rich(Harness::Claude, "p", "Partial cost");
    s.usage = Some(UsageSummary {
        total: Some(91_000),
        class: AggregationClass::Reported,
        categories: TokenCategories {
            input: Some(80_000),
            output: Some(11_000),
            ..TokenCategories::default()
        },
        cost: CostEstimate {
            usd: None,
            class: AggregationClass::Partial,
            known_subtotal: 0.12,
            breakdown: CostBreakdown {
                input: Some(0.12),
                output: None,
                ..CostBreakdown::default()
            },
            ..CostEstimate::default()
        },
        coverage: UsageCoverage {
            observed_requests: 4,
            priced_requests: 3,
            complete: false,
            reason: None,
        },
        ..UsageSummary::default()
    });
    let out = preview_of(s);
    assert!(
        out.contains("$0.12+"),
        "known subtotal, marked as a lower bound:\n{out}"
    );
    assert!(
        !out.contains("~$"),
        "no complete estimate is claimed:\n{out}"
    );

    // A tiny positive amount is never shown as free.
    let mut tiny = rich(Harness::Claude, "t", "Tiny cost");
    tiny.usage.as_mut().unwrap().cost.usd = Some(0.00004);
    let out = preview_of(tiny);
    assert!(out.contains("<$0.0001"), "{out}");
}

#[test]
fn unknown_speed_is_not_zero_and_an_old_model_keeps_its_qualifier() {
    let mut none = rich(Harness::Codex, "n", "No speed");
    none.metrics.speed.latest = None;
    let out = preview_of(none);
    assert!(out.contains('—') && !out.contains("tok/s"), "{out}");
    assert!(!out.contains("0.0"), "{out}");

    let mut zero = rich(Harness::Codex, "z", "Zero speed");
    zero.metrics
        .speed
        .latest
        .as_mut()
        .unwrap()
        .tokens_per_second = 0.0;
    let out = preview_of(zero);
    assert!(out.contains('—') && !out.contains("tok/s"), "{out}");

    let mut old = rich(Harness::Codex, "o", "Old model");
    old.metrics.speed.latest.as_mut().unwrap().model = Some(model("gpt-4.1", Some("openai"), None));
    let out = preview_of(old);
    assert!(out.contains("old gpt-4.1"), "model qualifier:\n{out}");
    assert!(out.contains("3m ago"), "age qualifier:\n{out}");
    assert!(out.contains("84.2"), "{out}");

    // The reported rate is shown as measured: TTFT (500 ms of 10 s) is not subtracted.
    let out = preview_of(rich(Harness::Codex, "r", "Fresh"));
    assert!(out.contains("84.2") && !out.contains("105.3"), "{out}");
}

#[test]
fn context_percentage_needs_a_known_window() {
    let mut unknown = rich(Harness::Codex, "u", "No window");
    unknown.metrics.context.used = Some(215_000);
    unknown.metrics.context.capacity = None;
    let out = preview_of(unknown);
    assert!(out.contains("215k in use"), "{out}");
    assert!(!out.contains('%'), "no percentage without a window:\n{out}");

    let mut known = rich(Harness::Codex, "k", "Known window");
    known.metrics.context.used = Some(215_000);
    known.metrics.context.capacity = Some(1_000_000);
    let out = preview_of(known);
    assert!(out.contains("21% of window"), "{out}");
    assert!(out.contains("215k of 1.0M"), "{out}");
}

#[test]
fn loading_rows_show_an_ellipsis_never_zero() {
    let mut app = app();
    let loading = SessionSummary::unhydrated(
        session(
            Harness::Codex,
            "l",
            "Loading row",
            5,
            None,
            ProcessAssociation::NotChecked,
        )
        .meta,
    );
    let snapshot = {
        let mut snap = support::snapshot(
            1,
            vec![loading],
            ScanProgress {
                harness: Harness::Codex,
                scanned: 1,
                total: Some(1),
                done: true,
            },
        );
        snap.summaries = Vec::new();
        snap
    };
    assert!(app.apply_roster_snapshot(snapshot, vec![]));
    let out = text(&draw(&app, &ctx(true), 120, 30));
    assert!(out.contains("Loading row"), "{out}");
    assert!(out.contains('…'), "{out}");
    assert!(!out.contains("0 tok/s") && !out.contains("$0.00"), "{out}");
}

#[test]
fn mtime_derived_activity_is_marked_approximate() {
    let mut derived = rich(Harness::Codex, "d", "Approximate archive");
    derived.meta.activity.at = Some(NOW - 180_000);
    derived.meta.activity.class = AggregationClass::Derived;
    derived.last_event_at = None;
    derived.last_completion = None;
    let app = rich_app(vec![derived]);
    let out = text(&draw(&app, &ctx(false), 120, 40));
    assert!(out.contains("~3m"), "approximate marker missing:\n{out}");
}
