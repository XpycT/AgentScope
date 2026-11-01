use std::process::{Command, Stdio};
use std::time::Duration;

mod support;

use agentscope::app::App;
use agentscope::domain::*;
use agentscope::ui::{Column, RosterContext, RosterLayout, render_preview, render_roster};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
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
fn renders_session_harness_model_tokens_and_status_lines_at_120x40() {
    let out = text(&draw(&populated(), &ctx(false), 120, 40));
    for want in [
        "AgentScope",
        "Codex 1",
        "Claude 1",
        "OpenCode 1",
        "Live: on",
        "Sort: last activity",
        "SESSION / PROJECT",
        "HARNESS/PROV",
        "MODEL/EFFORT",
        "TOKENS",
        "API ESTIMATE",
        "ACTIVE",
        "LATEST TOK/S",
        "● Active",
        "! Quiet",
        "? Unknown",
        "Inventory API",
        "12s",
        "10m",
        "RipperGame",
        "91k",
        "73k",
        "q quit",
    ] {
        assert!(out.contains(want), "missing {want:?} in:\n{out}");
    }
    // Process association moved to the preview and `p`; it is not a roster column.
    assert!(!out.contains("PROC"), "{out}");
}

#[test]
fn narrow_terminals_pin_the_session_column_and_scroll_the_rest() {
    let mut app = populated();
    let w60 = text(&draw(&app, &ctx(false), 60, 20));
    for want in [
        "SESSION / PROJECT",
        "HARNESS/PROV",
        "● Active",
        "Inventory API",
        "Codex",
        "12s",
    ] {
        assert!(w60.contains(want), "missing {want:?} in:\n{w60}");
    }
    assert!(
        !w60.contains("TOKENS"),
        "later columns are scrolled out:\n{w60}"
    );
    for line in w60.lines() {
        assert!(line.chars().count() <= 60);
    }

    app.scroll_columns(2);
    let scrolled = text(&draw(&app, &ctx(false), 60, 20));
    assert!(
        scrolled.contains("TOKENS") && scrolled.contains("91k"),
        "{scrolled}"
    );
    assert!(
        scrolled.contains('‹'),
        "a hint marks the hidden left columns"
    );
    assert!(
        scrolled.contains("Inventory API"),
        "the session column stays pinned:\n{scrolled}"
    );
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
    assert!(out.contains("● Active") && out.contains("! Quiet") && out.contains("? Unknown"));

    let colored = draw(&app, &ctx(false), 120, 40);
    assert!(colored.content().iter().any(|c| c.fg != Color::Reset));
    assert_eq!(out, text(&colored));
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

/// Cell column where `needle` first starts on `line`.
fn col_of(line: &str, needle: &str) -> usize {
    let byte = line
        .find(needle)
        .unwrap_or_else(|| panic!("{needle:?} not found in {line:?}"));
    line[..byte].chars().count()
}

/// The two physical lines of the row whose title is `title`.
fn row_pair(lines: &[String], title: &str) -> (String, String) {
    let at = lines
        .iter()
        .position(|l| l.contains(title))
        .unwrap_or_else(|| panic!("{title:?} not drawn in:\n{}", lines.join("\n")));
    (lines[at].clone(), lines[at + 1].clone())
}

#[test]
fn provider_sits_under_harness_and_effort_under_model_on_every_size() {
    let mut app = rich_app(vec![]);
    // Resizing between sizes keeps the selected identity and the alignment.
    let selected = app.selected_key();
    for (w, h) in [(90, 25), (160, 40), (60, 20), (90, 25)] {
        // 60 columns hold only the first columns: scroll to the model column too.
        let lines = lines_of(&draw(&app, &ctx(true), w, h));
        let (first, second) = row_pair(&lines, "Inventory API");
        assert_eq!(
            col_of(&first, "Codex"),
            col_of(&second, "openai"),
            "{w}x{h} provider under harness:\n{first}\n{second}"
        );
        let header = lines.iter().find(|l| l.contains("HARNESS/PROV")).unwrap();
        assert_eq!(col_of(header, "HARNESS/PROV"), col_of(&first, "Codex"));
        app.scroll_columns(1);
        let lines = lines_of(&draw(&app, &ctx(true), w, h));
        let (first, second) = row_pair(&lines, "Inventory API");
        assert_eq!(
            col_of(&first, "gpt-5.5"),
            col_of(&second, "high"),
            "{w}x{h} effort under model:\n{first}\n{second}"
        );
        app.scroll_columns(-1);
        assert_eq!(app.selected_key(), selected);
    }
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
    assert_eq!(col_of(&first, "Codex"), col_of(&second, "route-x"));
    // The clean row still starts right after the hostile row's two lines.
    let evil = lines.iter().position(|l| l.contains("evil")).unwrap();
    assert!(
        lines[evil + 2].contains("Clean row"),
        "{}",
        lines.join("\n")
    );
}

#[test]
fn horizontal_scroll_reaches_tokens_cost_active_and_speed_at_60x20() {
    let mut app = rich_app(vec![]);
    let mut seen = Vec::new();
    for step in 0..COLUMNS {
        let out = text(&draw(&app, &ctx(true), 60, 20));
        for want in ["91k", "~$0.23", "1h 5m", "84.2"] {
            if out.contains(want) {
                seen.push(want);
            }
        }
        assert!(
            out.contains("Inventory API"),
            "step {step}: the session column stays pinned"
        );
        app.scroll_columns(1);
    }
    for want in ["91k", "~$0.23", "1h 5m", "84.2"] {
        assert!(seen.contains(&want), "{want:?} never reachable");
    }
    // Scrolling stops at the last column instead of running off the table.
    for _ in 0..10 {
        app.scroll_columns(1);
    }
    assert_eq!(app.horizontal_column(), COLUMNS - 1);
    app.scroll_columns(-100);
    assert_eq!(app.horizontal_column(), 0);
}

const COLUMNS: usize = agentscope::app::COLUMN_COUNT;

#[test]
fn the_preview_takes_45_columns_from_150_and_never_below() {
    let app = rich_app(vec![]);
    let wide = RosterLayout::for_area(Rect::new(0, 0, 150, 30), 0);
    let preview = wide.preview.expect("preview column at 150");
    assert_eq!((preview.width, preview.x), (45, 105));
    assert_eq!(wide.rows.width, 105);
    assert!(
        RosterLayout::for_area(Rect::new(0, 0, 149, 30), 0)
            .preview
            .is_none()
    );

    let split = text(&draw(&app, &ctx(true), 160, 40));
    assert!(split.contains("Preview"), "{split}");
    assert!(split.contains("Billing provider: openai"), "{split}");
    let narrow = text(&draw(&app, &ctx(true), 149, 40));
    assert!(!narrow.contains("Preview"), "{narrow}");

    let small = RosterLayout::for_area(Rect::new(0, 0, 59, 19), 0);
    assert!(!small.usable() && small.columns.is_empty());
}

#[test]
fn column_hitboxes_follow_the_scroll_and_never_overlap() {
    let area = Rect::new(0, 0, 90, 25);
    let first = RosterLayout::for_area(area, 0);
    assert_eq!(first.columns[0].column, Column::Provider);
    assert!(first.more_left.is_none() && first.more_right.is_some());
    let scrolled = RosterLayout::for_area(area, 3);
    assert_eq!(scrolled.columns[0].column, Column::Cost);
    assert!(scrolled.more_left.is_some());
    for layout in [&first, &scrolled] {
        for pair in layout.columns.windows(2) {
            assert!(pair[0].x + pair[0].width < pair[1].x, "{pair:?}");
        }
        for placed in &layout.columns {
            assert!(placed.x + placed.width <= layout.rows.right());
            assert_eq!(layout.column_at(placed.x), Some(placed.column));
        }
    }
    assert_eq!(
        first.column_at(0),
        None,
        "the pinned columns are not trailing columns"
    );
    assert_eq!(
        RosterLayout::for_area(area, 99).horizontal_column,
        COLUMNS - 1
    );
}

/// Preview of the only row, drawn with room for unwrapped lines.
fn preview_of(summary: SessionSummary) -> String {
    let mut app = app();
    app.apply_roster(vec![summary], vec![]);
    let mut terminal = Terminal::new(TestBackend::new(200, 60)).unwrap();
    terminal
        .draw(|f| render_preview(f, &app, &ctx(true), f.area()))
        .unwrap();
    text(terminal.backend().buffer())
}

#[test]
fn partial_category_costs_show_a_subtotal_and_missing_coverage_not_a_total() {
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
    assert!(out.contains("partial 3/4"), "{out}");
    assert!(out.contains("Unpriced coverage: output"), "{out}");
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
    assert!(out.contains("Latest speed: —"), "{out}");
    assert!(!out.contains("0.0 tok/s"), "{out}");

    let mut zero = rich(Harness::Codex, "z", "Zero speed");
    zero.metrics
        .speed
        .latest
        .as_mut()
        .unwrap()
        .tokens_per_second = 0.0;
    let out = preview_of(zero);
    assert!(
        out.contains("Latest speed: —") && !out.contains("0.0 tok/s"),
        "{out}"
    );

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
    assert!(out.contains("Unknown (window not recorded)"), "{out}");
    assert!(!out.contains('%'), "no percentage without a window:\n{out}");

    let mut known = rich(Harness::Codex, "k", "Known window");
    known.metrics.context.used = Some(215_000);
    known.metrics.context.capacity = Some(1_000_000);
    let out = preview_of(known);
    assert!(out.contains("21% of window"), "{out}");
}

fn compaction(scope: CompactionScope, summary: Option<&str>) -> CompactionObservation {
    let s = rich(Harness::Codex, "c", "c");
    let key = s.meta.key.clone();
    CompactionObservation {
        id: "c1".into(),
        timestamp: Some(NOW - 60_000),
        method: Some("auto".into()),
        trigger: Some("limit".into()),
        summary: summary.map(|t| BoundedText {
            preview: t.into(),
            byte_len: t.len(),
            truncated: false,
            locator: None,
        }),
        before_tokens: Some(2_000),
        after_tokens: Some(500),
        duration_ms: None,
        usage: None,
        scope,
        provenance: Provenance {
            harness: Harness::Codex,
            source: key.source.clone(),
            locator: SourceLocator {
                session: key,
                kind: LocatorKind::FileOffset {
                    path: "c.jsonl".into(),
                    offset: 0,
                },
                generation: None,
            },
            record_type: "compaction".into(),
            harness_version: None,
        },
    }
}

#[test]
fn compaction_event_cost_and_context_reduction_are_separate_facts() {
    let mut s = rich(Harness::Codex, "e", "Event scope");
    s.metrics.compactions = CompactionSummary {
        observed: 1,
        history_complete: true,
        known_cost_usd: None,
        cost_scope: CompactionScope::Event,
        unpriced: 1,
        latest: Some(compaction(CompactionScope::Event, None)),
    };
    let out = preview_of(s);
    assert!(out.contains("context 2,000 → 500 (removed 1,500)"), "{out}");
    assert!(
        out.contains("Event cost: not recorded for this event"),
        "{out}"
    );

    // An aggregate cost is never attributed to the single latest event.
    let mut agg = rich(Harness::Codex, "g", "Aggregate scope");
    agg.metrics.compactions = CompactionSummary {
        observed: 2,
        history_complete: true,
        known_cost_usd: Some(1.5),
        cost_scope: CompactionScope::Aggregate,
        unpriced: 0,
        latest: Some(compaction(CompactionScope::Aggregate, None)),
    };
    let out = preview_of(agg);
    assert!(
        out.contains("$1.50 aggregate (not attributed per event)"),
        "{out}"
    );
    assert!(
        out.contains("session aggregate only, not attributed to this event"),
        "{out}"
    );
    let event_line = out.lines().find(|l| l.contains("Event cost:")).unwrap();
    assert!(!event_line.contains("$1.50"), "{event_line}");
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
