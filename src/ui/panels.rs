//! Help, process-candidate, and filter panels (spec §10.1, §10.4). Pure: they
//! read `App` and draw over the screen beneath. No I/O.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, FilterItem, FilterTarget, Panel, Screen, UsageAvailability};
use crate::domain::ProcessAssociation;
use crate::ingest::text::display_safe;
use crate::time::format_local;

use super::RosterContext;
use super::inspector::process_line;
use super::roster::{MIN_HEIGHT, MIN_WIDTH};
use super::widgets::{Palette, fit, harness_name, status_text};

pub(super) fn render_panel(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return; // The screen beneath already explains the size problem.
    }
    let palette = Palette::new(ctx.no_color);
    let popup = Rect::new(2, 1, area.width - 4, area.height - 2);
    let (title, hint) = match app.panel() {
        Panel::Help => (" Help ", "j/k scroll  ? or Esc close"),
        Panel::Processes => (" Process candidates ", "j/k scroll  p or Esc close"),
        Panel::Filter => (
            " Filter ",
            "j/k move  Enter/Space toggle  c clear  Esc close",
        ),
        Panel::None => return,
    };
    frame.render_widget(Clear, popup);
    let block = Block::bordered().title(title);
    let inner = block.inner(popup);
    frame.render_widget(block, popup);
    let width = inner.width as usize;
    let body_height = inner.height.saturating_sub(1) as usize;
    let body = Rect::new(inner.x, inner.y, inner.width, body_height as u16);
    let foot = Rect::new(inner.x, inner.y + body_height as u16, inner.width, 1);

    let lines: Vec<Line<'static>> = match app.panel() {
        Panel::Help => {
            let text = help_lines(app.screen);
            scrolled(app, text, width, body_height, palette)
        }
        Panel::Processes => {
            let text = process_lines(app);
            scrolled(app, text, width, body_height, palette)
        }
        Panel::Filter => filter_lines(app, width, body_height, palette),
        Panel::None => return,
    };
    frame.render_widget(Paragraph::new(lines), body);
    frame.render_widget(
        Paragraph::new(Line::styled(fit(hint, width), palette.dim())),
        foot,
    );
}

/// Show `text` from the panel's scroll offset, reporting the scroll limit.
fn scrolled(
    app: &App,
    text: Vec<String>,
    width: usize,
    height: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    app.set_panel_extent(text.len().saturating_sub(height));
    let start = app.panel_scroll();
    text.into_iter()
        .skip(start)
        .take(height)
        .map(|l| {
            if l.starts_with('#') {
                Line::styled(
                    fit(l.trim_start_matches('#').trim(), width),
                    palette.title(),
                )
            } else {
                Line::raw(fit(&l, width))
            }
        })
        .collect()
}

/// Lines starting with `#` are headings.
fn help_lines(screen: Screen) -> Vec<String> {
    let key = |k: &str, what: &str| format!("{k:<22}{what}");
    let mut out: Vec<String> = Vec::new();
    match screen {
        Screen::Roster => {
            out.push("# Roster".into());
            out.push(key("j/k, arrows", "Move; rests on Load more rows"));
            out.push(key("g/G, Home/End", "First / last row"));
            out.push(key("PgUp/PgDn, Ctrl+u/d", "Page"));
            out.push(key("Enter", "Open the inspector / load more"));
            out.push(key("Tab", "Wide: inspector. Narrow: list/preview"));
            out.push(key("Space", "Show or hide a family's children"));
            out.push(key("Left/Right, h/l", "Scroll the trailing columns"));
            out.push(key("J/K", "Scroll the preview"));
            out.push(key("Mouse", "Click selects; marker expands; wheel"));
            out.push(key("Shift+drag", "Terminal text selection (capture)"));
            out.push(key("/", "Search title, project, cwd, session id"));
            out.push(key("f", "Filter: provider, status, project, usage"));
            out.push(key("s / S", "Cycle sort / reverse direction"));
            out.push(key("r", "Refresh now (retries a paused worker)"));
            out.push(key("p", "Process candidates for the selection"));
            out.push(key("m", "Load 50 more top-level sessions"));
            out.push("# Capability limits".into());
            out.push("Rows show \"…\" until a session's details are read; discovery".into());
            out.push("reads only file head/tail, never whole transcripts.".into());
            out.push("OpenCode: no compaction markers are recorded, and request-level".into());
            out.push("detail needs the v2 schema (legacy databases list none).".into());
            out.push("Child links exist only where the tool metadata records the".into());
            out.push("documented session id; other fields stay unread.".into());
            out.push("Codex records cumulative usage only, so it has no request list.".into());
            out.push("A Claude ID beyond the probe window is flagged approximate.".into());
            out.push("Not read: Desktop chat history (only Code manifests and".into());
            out.push("Cowork project stores), WSL, remote and cloud sessions.".into());
            out.push("Pi extension subagents: unsupported in this release (the".into());
            out.push("native format has no child-session records, so none are".into());
            out.push("listed rather than a verified zero). Pi request speed stays".into());
            out.push("Unknown unless an assistant record carries durationMs; the".into());
            out.push("producer records no session end or active interval.".into());
            out.push("OMP: compaction cost is Unknown (the producer persists no".into());
            out.push("summarization usage); child links and totals follow the".into());
            out.push("artifact tree, and a parent's quoted child cost is not added.".into());
            out.push("Hermes: compression chains are one session; in-place".into());
            out.push("compaction keeps summarized rows and collapses copies.".into());
            out.push("Request speed, live context and per-event compaction cost".into());
            out.push("stay Unknown; aggregate compression spend is one breakdown.".into());
        }
        Screen::Inspector => {
            out.push("# Inspector".into());
            out.push(key("j/k, arrows", "Move selection"));
            out.push(key("g/G, Home/End", "Oldest loaded / newest page"));
            out.push(key("PgUp/PgDn, Ctrl+u/d", "Page"));
            out.push(key("Enter", "Expand or collapse the event"));
            out.push(key("/", "Search loaded events"));
            out.push(key("n / N", "Next / previous match"));
            out.push(key("d, Tab", "Details for the event"));
            out.push(key("v", "Source record (redacted)"));
            out.push(key("x", "Toggle redaction for the selection"));
            out.push(key("D", "Diagnostics: hidden and unknown records"));
            out.push(key("i", "Session info: usage categories, pricing, lineage"));
            out.push(key("p", "Process candidates"));
            out.push(key("r", "Retry a failed load / refresh now"));
            out.push(key("Esc, Shift+Tab", "Back to the roster"));
        }
        Screen::Details => {
            out.push("# Details".into());
            out.push(key("j/k, PgUp/PgDn", "Scroll"));
            out.push(key("v", "Source record (redacted)"));
            out.push(key("x", "Toggle redaction for the selection"));
            out.push(key("D", "Diagnostics"));
            out.push(key("Esc, Shift+Tab", "Back to the inspector"));
        }
        Screen::Session => {
            out.push("# Session info".into());
            out.push(key("j/k, PgUp/PgDn", "Scroll"));
            out.push(key("m", "Load the next older detail page"));
            out.push(key("Space, Enter", "Expand compaction summaries"));
            out.push(key("Esc, Shift+Tab", "Back to the inspector"));
        }
        Screen::Source => {
            out.push("# Source record".into());
            out.push(key("j/k, PgUp/PgDn", "Scroll"));
            out.push(key("x", "Toggle redaction for this record"));
            out.push(key("Esc", "Close"));
        }
        Screen::Diagnostics => {
            out.push("# Diagnostics".into());
            out.push(key("j/k, PgUp/PgDn", "Move selection"));
            out.push(key("Enter, v", "Open the entry's source record"));
            out.push(key("x", "Toggle redaction for the entry"));
            out.push(key("Esc", "Close"));
        }
    }
    out.push(String::new());
    out.push("# Anywhere".into());
    out.push(key("?", "This help"));
    out.push(key("q", "Quit (not while typing in a search)"));
    out.push(key("Ctrl+c", "Quit, always"));
    out
}

fn process_lines(app: &App) -> Vec<String> {
    let view = app.process_view();
    let mut out = Vec::new();
    let Some(session) = view.session else {
        out.push("No session selected.".into());
        return out;
    };
    out.push(format!(
        "# {} / {}",
        harness_name(session.meta.key.harness),
        display_safe(&session.meta.title)
    ));
    out.push(format!(
        "Association: {}",
        match session.process {
            ProcessAssociation::NotChecked => "not checked (observation off or not run yet)".into(),
            ProcessAssociation::Unavailable =>
                "unavailable (the OS withheld process details)".into(),
            other => process_line(&other),
        }
    ));
    match view.sampled_at {
        None => out.push("No process sample yet.".into()),
        Some(at) => {
            out.push(format!("Sampled: {}", clock(at)));
            out.push(String::new());
            if view.candidates.is_empty() {
                out.push(format!(
                    "No {} processes in the latest sample.",
                    harness_name(session.meta.key.harness)
                ));
            }
            let matched = match session.process {
                ProcessAssociation::Matched { pid, .. } => Some(pid),
                _ => None,
            };
            for p in &view.candidates {
                let started = p.started_at.map_or_else(|| "unknown".into(), clock);
                let mark = if matched == Some(p.pid) {
                    "  (matched by session id)"
                } else {
                    ""
                };
                out.push(format!(
                    "PID {}  started {started}  {}{mark}",
                    p.pid,
                    display_safe(&p.executable)
                ));
                out.push(format!(
                    "    cwd: {}",
                    p.cwd
                        .as_ref()
                        .map_or_else(|| "unknown".into(), |c| display_safe(&c.to_string_lossy()))
                ));
                if let Some(cmd) = &p.command_line {
                    out.push(format!("    args: {}", display_safe(cmd)));
                }
            }
        }
    }
    out.push(String::new());
    out.push("Candidates are not matches: only an identity-bearing argument".into());
    out.push("ties a process to a session; names, folders, and timing never do.".into());
    out
}

/// `HH:MM:SS` local time.
fn clock(ts: i64) -> String {
    format_local(ts)
        .get(11..19)
        .map_or_else(|| "—".to_string(), str::to_string)
}

fn filter_lines(app: &App, width: usize, height: usize, palette: Palette) -> Vec<Line<'static>> {
    let items = app.filter_items();
    let cursor = app.panel_cursor();
    let mut rows: Vec<(Option<usize>, String)> = Vec::new();
    let mut section = "";
    for (i, item) in items.iter().enumerate() {
        let heading = match item.target {
            FilterTarget::Provider(_) => "Provider",
            FilterTarget::Status(_) => "Status",
            FilterTarget::Usage(_) => "Usage",
            FilterTarget::Project(_) => "Project",
            FilterTarget::Released => "",
            FilterTarget::Clear => "",
        };
        if heading != section {
            section = heading;
            rows.push((None, format!("# {heading}")));
        }
        rows.push((Some(i), item_text(item, i == cursor)));
    }
    let cursor_row = rows
        .iter()
        .position(|(i, _)| *i == Some(cursor))
        .unwrap_or(0);
    let start = (cursor_row + 1).saturating_sub(height);
    rows.into_iter()
        .skip(start)
        .take(height)
        .map(|(i, text)| match i {
            Some(i) if i == cursor => Line::styled(fit(&text, width), palette.selected()),
            Some(_) => Line::raw(fit(&text, width)),
            None => Line::styled(
                fit(text.trim_start_matches('#').trim(), width),
                palette.title(),
            ),
        })
        .collect()
}

fn item_text(item: &FilterItem, at_cursor: bool) -> String {
    let marker = if at_cursor { "> " } else { "  " };
    let (checkbox, label) = match &item.target {
        FilterTarget::Provider(p) => (true, harness_name(*p).to_string()),
        FilterTarget::Status(l) => {
            let (icon, text) = status_text(*l);
            (true, format!("{icon} {text}"))
        }
        FilterTarget::Usage(u) => (
            false,
            match u {
                UsageAvailability::Any => "Any",
                UsageAvailability::Known => "Token total reported",
                UsageAvailability::Unknown => "Token total unknown",
                UsageAvailability::HasCost => "Cost estimate available",
            }
            .to_string(),
        ),
        FilterTarget::Project(None) => (false, "Any project".to_string()),
        FilterTarget::Project(Some(name)) => (false, display_safe(name)),
        FilterTarget::Released => (true, "Include released sessions".to_string()),
        FilterTarget::Clear => return format!("{marker}Clear all filters (c)"),
    };
    let box_ = match (checkbox, item.selected) {
        (true, true) => "[x]",
        (true, false) => "[ ]",
        (false, true) => "(*)",
        (false, false) => "( )",
    };
    format!("{marker}{box_} {label}")
}
