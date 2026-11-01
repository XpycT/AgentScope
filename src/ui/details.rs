//! Details, source, and diagnostics overlays (spec §10.3, §11.3). Pure: reads
//! `App`, draws into a `Frame`. Every line is wrapped and bounded before it is
//! drawn, and only text that is already display-safe reaches the screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, SourceState, Transcript};
use crate::domain::{DiagnosticEntry, Event, LocatorKind, SessionLink, SourceLocator};
use crate::ingest::text::display_safe;
use crate::time::format_local;

use super::RosterContext;
use super::inspector::{actor_label, detail_lines};
use super::roster::{MIN_HEIGHT, MIN_WIDTH};
use super::widgets::{Palette, fit};

/// Most wrapped rows built for one overlay body.
const MAX_ROWS: usize = 4_000;
/// Most warnings listed in the diagnostics header.
const MAX_WARNINGS: usize = 3;

const REDACTED_NOTICE: &str =
    "Redacted (best-effort, not a guarantee). x reveals this record from the source.";
const REVEALED_NOTICE: &str =
    "WARNING: redaction is OFF; secrets may be visible. x restores redaction.";

pub(super) struct Frame3 {
    title: Rect,
    pub(super) body: Rect,
    status: Rect,
    footer: Rect,
    pub(super) width: usize,
    palette: Palette,
}

/// Shared chrome. Returns `None` after drawing the too-small message.
pub(super) fn chrome(frame: &mut Frame, ctx: &RosterContext) -> Option<Frame3> {
    let area = frame.area();
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = format!(
            "Terminal too small: {}x{} (need at least {MIN_WIDTH}x{MIN_HEIGHT}). Resize to continue; Esc goes back.",
            area.width, area.height
        );
        frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
        return None;
    }
    let [title, rule1, body, rule2, status, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let rule = "─".repeat(area.width as usize);
    frame.render_widget(Paragraph::new(rule.as_str()), rule1);
    frame.render_widget(Paragraph::new(rule.as_str()), rule2);
    Some(Frame3 {
        title,
        body,
        status,
        footer,
        width: area.width as usize,
        palette: Palette::new(ctx.no_color),
    })
}

pub(super) fn put(
    frame: &mut Frame,
    f: &Frame3,
    title: &str,
    notice: Option<&str>,
    status: &str,
    footer: &str,
) {
    frame.render_widget(
        Paragraph::new(Line::styled(fit(title, f.width), f.palette.title())),
        f.title,
    );
    let mut spans = Vec::new();
    let mut used = 0;
    if let Some(notice) = notice {
        let text = format!("{}  ·  ", display_safe(notice));
        used = text.width();
        spans.push(Span::styled(text, f.palette.quiet()));
    }
    spans.push(Span::styled(
        fit(status, f.width.saturating_sub(used)),
        f.palette.warning(),
    ));
    frame.render_widget(Paragraph::new(Line::from(spans)), f.status);
    frame.render_widget(
        Paragraph::new(Line::styled(fit(footer, f.width), f.palette.dim())),
        f.footer,
    );
}

/// Wrap `text` to `width` columns, one row per `\n`-separated line at least.
/// Stops at [`MAX_ROWS`] and reports whether it did.
pub(super) fn wrap_into(rows: &mut Vec<String>, text: &str, width: usize) -> bool {
    let width = width.max(1);
    for line in text.split('\n') {
        let mut row = String::new();
        let mut used = 0;
        for ch in line.chars() {
            let w = ch.width().unwrap_or(0);
            if used + w > width && !row.is_empty() {
                rows.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(ch);
            used += w;
            if rows.len() >= MAX_ROWS {
                return true;
            }
        }
        rows.push(row);
        if rows.len() >= MAX_ROWS {
            return true;
        }
    }
    false
}

/// Draw the visible slice of `rows` and report the scroll extent to `app`.
pub(super) fn draw_rows(frame: &mut Frame, app: &App, rows: &[String], scroll: usize, body: Rect) {
    let height = body.height as usize;
    let max_scroll = rows.len().saturating_sub(height);
    app.set_overlay_extent(max_scroll);
    let start = scroll.min(max_scroll);
    let shown: Vec<Line<'static>> = rows
        .iter()
        .skip(start)
        .take(height)
        .map(|r| Line::raw(r.clone()))
        .collect();
    frame.render_widget(Paragraph::new(shown), body);
}

fn time_text(event: &Event) -> String {
    event
        .timestamp
        .map_or_else(|| "—".to_string(), format_local)
}

fn event_rows(event: &Event, width: usize, child: Option<&SessionLink>) -> Vec<String> {
    let mut rows = Vec::new();
    let head = format!(
        "Event: {}   Time: {}   Record: {}",
        actor_label(event),
        time_text(event),
        display_safe(&event.provenance.record_type)
    );
    let mut capped = wrap_into(&mut rows, &head, width);
    if let Some(tool) = &event.tool {
        let line = format!(
            "Tool: {}   Call: {}",
            display_safe(&tool.name),
            tool.call_id
                .as_deref()
                .map_or_else(|| "—".to_string(), display_safe)
        );
        capped |= wrap_into(&mut rows, &line, width);
        if let Some(link) = child {
            let line = format!(
                "Child session: {}",
                super::session::link_text(&Some(link.clone()))
            );
            capped |= wrap_into(&mut rows, &line, width);
        }
    }
    rows.push(String::new());
    for line in detail_lines(event) {
        if capped {
            break;
        }
        capped = wrap_into(&mut rows, &line, width);
    }
    if capped {
        rows.push("[display limited: more content exists in the source record (v)]".into());
    }
    rows
}

pub fn render_details(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let Some(f) = chrome(frame, ctx) else { return };
    let Some(event) = app.transcript().and_then(Transcript::selected_event) else {
        frame.render_widget(Paragraph::new("No event is selected."), f.body);
        return;
    };
    let child = event
        .tool
        .as_ref()
        .and_then(|tool| app.resolved_tool_child(&event.session, tool));
    let rows = event_rows(event, f.width, child.as_ref());
    draw_rows(frame, app, &rows, app.details_scroll(), f.body);
    let status = if app.revealed_for(&event.provenance.locator) {
        REVEALED_NOTICE
    } else {
        "Previews are redacted (best-effort); x reloads this record's source unredacted."
    };
    put(
        frame,
        &f,
        "< Details",
        app.notice(),
        status,
        "j/k scroll  v source  x redaction  D diagnostics  o open child  Esc back  q quit",
    );
}

pub fn render_source(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let Some(f) = chrome(frame, ctx) else { return };
    let Some(view) = app.source_view() else {
        frame.render_widget(Paragraph::new("No source record is open."), f.body);
        return;
    };
    let mut rows = vec![locator_text(&view.locator), String::new()];
    let mut note = if view.revealed {
        REVEALED_NOTICE.to_string()
    } else {
        REDACTED_NOTICE.to_string()
    };
    match &view.state {
        SourceState::Loading => rows.push("Loading source record…".into()),
        SourceState::Failed => rows.push("The source record could not be loaded.".into()),
        SourceState::Unavailable(reason) => {
            rows.push(format!("Source record unavailable: {reason}"));
        }
        SourceState::Ready { text, truncated } => {
            if wrap_into(&mut rows, text, f.width) || *truncated {
                note = format!("{note}  [record truncated for display]");
            }
        }
    }
    draw_rows(frame, app, &rows, view.scroll, f.body);
    put(
        frame,
        &f,
        "< Source record",
        app.notice(),
        &note,
        "j/k scroll  x redaction  Esc back  q quit",
    );
}

/// `file.jsonl @ 1234` or `table row id #part`. Paths show the file name only.
fn locator_text(locator: &SourceLocator) -> String {
    match &locator.kind {
        LocatorKind::FileOffset { path, offset } => {
            let name = path
                .file_name()
                .map_or_else(|| path.to_string_lossy(), |n| n.to_string_lossy());
            format!("Source: {} @ byte {offset}", display_safe(&name))
        }
        LocatorKind::CompressedRecord { path, ordinal } => {
            let name = path
                .file_name()
                .map_or_else(|| path.to_string_lossy(), |n| n.to_string_lossy());
            format!("Source: {} record {ordinal}", display_safe(&name))
        }
        LocatorKind::DbRow {
            table,
            row_id,
            part_index,
        } => {
            let part = part_index.map_or_else(String::new, |p| format!(" part {p}"));
            format!(
                "Source: {} row {}{part}",
                display_safe(table),
                display_safe(row_id)
            )
        }
    }
}

fn diagnostic_row(entry: &DiagnosticEntry) -> String {
    let kind = if entry.hidden { "hidden" } else { "listed" };
    format!(
        "[{kind}] {}  —  {}",
        display_safe(&entry.label),
        locator_text(&entry.locator)
    )
}

pub fn render_diagnostics(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let Some(f) = chrome(frame, ctx) else { return };
    let Some(transcript) = app.transcript() else {
        frame.render_widget(Paragraph::new("No session is open."), f.body);
        return;
    };
    let entries = transcript.diagnostics();
    let mut head: Vec<String> = Vec::new();
    if let Some(row) = app.session(transcript.key()) {
        head.push(format!(
            "Hidden records: {}   Source available: {}   Entries listed: {}",
            row.session.hidden_records,
            if row.session.meta.source_available {
                "yes"
            } else {
                "no"
            },
            entries.len()
        ));
        for w in row.session.warnings.iter().take(MAX_WARNINGS) {
            head.push(format!(
                "Warning ×{}: {}",
                w.count,
                display_safe(&w.message)
            ));
        }
    }
    head.push(String::new());

    let height = f.body.height as usize;
    let mut lines: Vec<Line<'static>> = head
        .iter()
        .take(height)
        .map(|h| Line::raw(fit(h, f.width)))
        .collect();
    let room = height.saturating_sub(lines.len());
    if entries.is_empty() {
        if room > 0 {
            lines.push(Line::raw(fit(
                "No hidden, unknown, or source-only records in the loaded range.",
                f.width,
            )));
        }
    } else if room > 0 {
        let selected = app.diagnostics_selected().min(entries.len() - 1);
        let start = (selected + 1).saturating_sub(room);
        for (i, entry) in entries.iter().enumerate().skip(start).take(room) {
            let marker = if i == selected { "> " } else { "  " };
            let text = fit(&format!("{marker}{}", diagnostic_row(entry)), f.width);
            lines.push(if i == selected {
                Line::styled(text, f.palette.selected())
            } else {
                Line::raw(text)
            });
        }
    }
    frame.render_widget(Paragraph::new(lines), f.body);
    put(
        frame,
        &f,
        "< Diagnostics: hidden, unknown, and source-only records",
        app.notice(),
        if entries
            .get(app.diagnostics_selected())
            .is_some_and(|e| app.revealed_for(&e.locator))
        {
            REVEALED_NOTICE
        } else {
            "Source records open redacted; x reveals the selected record."
        },
        "j/k move  Enter/v source  Esc back  q quit",
    );
}
