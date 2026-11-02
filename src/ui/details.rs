//! Details, source, and diagnostics overlays (spec §10.3, §11.3). Pure: reads
//! `App`, draws into a `Frame`. Every line is wrapped and bounded before it is
//! drawn, and only text that is already display-safe reaches the screen.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, SourceState, tool_summary};
use crate::domain::{
    BoundedText, DiagnosticEntry, Event, EventKind, LocatorKind, SessionLink, SourceLocator,
    StatusLabel, Timestamp, ToolInteraction, ToolStatus, UsageScope,
};
use crate::ingest::text::display_safe;
use crate::time::{format_local, relative_label};

use super::RosterContext;
use super::inspector::{REASONING_LABEL, money_text};
use super::roster::{MIN_HEIGHT, MIN_WIDTH};
use super::widgets::{Palette, SHELL_TOOLS, fit, tokens_text};

/// Most wrapped rows built for one overlay body.
const MAX_ROWS: usize = 4_000;
/// Most warnings listed in the diagnostics header.
const MAX_WARNINGS: usize = 3;
/// Below this width the side facts stack under the content.
const WIDE_MIN: usize = 110;
/// Width of the side facts column at wide widths.
const SIDE_WIDTH: usize = 30;
/// Widest content column, so long lines stay readable.
const CONTENT_MAX: usize = 100;
/// Left margin of the content column.
const CONTENT_MARGIN: usize = 2;
/// Width of one category bar in the side column.
const BAR_WIDTH: usize = 8;

const DETAILS_FOOTER: &str = "j/k scroll  [ ] prev/next  v source  x redaction  D diagnostics  o open child  Esc back  q quit";

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
    let back = "‹ Esc  ";
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(back, f.palette.quiet()),
            Span::styled(
                fit(title, f.width.saturating_sub(back.width())),
                f.palette.title(),
            ),
        ])),
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

/// `HH:MM:SS` in the host zone, or `—` when the event has no time.
fn clock(ts: Option<Timestamp>) -> String {
    ts.and_then(|ts| {
        format_local(ts)
            .get(11..19)
            .map(std::string::ToString::to_string)
    })
    .unwrap_or_else(|| "—".into())
}

fn age_text(app: &App, ts: Option<Timestamp>) -> String {
    ts.map_or_else(
        || "—".into(),
        |ts| format!("{} ago", relative_label(ts, app.now())),
    )
}

/// One styled row of the details body: plain text plus its style.
type Row = (String, Style);

fn styled_row(parts: Vec<(String, Style)>, width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0;
    for (text, style) in parts {
        if used >= width {
            break;
        }
        let remaining = width - used;
        let text = if UnicodeWidthStr::width(text.as_str()) > remaining {
            fit(&text, remaining)
        } else {
            text
        };
        used += UnicodeWidthStr::width(text.as_str());
        if !text.is_empty() {
            spans.push(Span::styled(text, style));
        }
    }
    Line::from(spans)
}

/// `‹ Esc  BADGE  HH:MM:SS · age · event N of M loaded · Exit: N   [ prev  ] next`.
fn header_line(
    app: &App,
    event: &Event,
    selected: usize,
    total: usize,
    width: usize,
    palette: Palette,
) -> Line<'static> {
    let (badge, badge_style) = match event.kind {
        EventKind::Tool => {
            let name = event
                .tool
                .as_ref()
                .map_or_else(|| "tool".to_string(), |tool| display_safe(&tool.name));
            (name.to_uppercase(), palette.tool_name(&name))
        }
        EventKind::UserPrompt => ("YOU".to_string(), palette.prompt_bar()),
        EventKind::AssistantMessage => ("AGENT".to_string(), palette.reply_bar()),
        EventKind::Reasoning => ("REASONING".to_string(), palette.value()),
        EventKind::System | EventKind::Lifecycle | EventKind::Compaction => {
            ("SYSTEM".to_string(), palette.value())
        }
    };
    let mut parts = vec![
        ("‹ Esc  ".to_string(), palette.quiet()),
        (
            format!(" {badge} "),
            badge_style.add_modifier(Modifier::REVERSED),
        ),
        ("  ".to_string(), Style::default()),
        (clock(event.timestamp), palette.value()),
        (" · ".to_string(), palette.quiet()),
        (age_text(app, event.timestamp), palette.quiet()),
        (
            format!(" · event {} of {} loaded", selected + 1, total),
            palette.quiet(),
        ),
    ];
    if let Some(code) = event.tool.as_ref().and_then(|tool| tool.exit_code) {
        parts.push((" · ".to_string(), palette.quiet()));
        parts.push(if code == 0 {
            ("✓ exit 0".to_string(), palette.quiet())
        } else {
            (
                format!("✗ exit {code}"),
                palette.status(StatusLabel::Failed),
            )
        });
    }
    parts.push(("   [ prev  ] next".to_string(), palette.quiet()));
    styled_row(parts, width)
}

/// Split one source line into its indentation/bullet prefix and the text to wrap.
/// Bullets (`- `, `* `, `• `) and `N. ` keep a hanging indent.
fn split_list_prefix(line: &str) -> (String, &str) {
    let trimmed = line.trim_start_matches(' ');
    let lead = line.len() - trimmed.len();
    let bullet = if trimmed.starts_with("- ") || trimmed.starts_with("* ") {
        2
    } else if trimmed.starts_with("• ") {
        "• ".len()
    } else {
        let digits = trimmed.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && trimmed[digits..].starts_with(". ") {
            digits + 2
        } else {
            0
        }
    };
    let end = lead + bullet;
    (line[..end].to_string(), &line[end..])
}

/// Most columns a source line's leading indentation may occupy. Deeper
/// recorded indentation is clipped so wrapped text always stays visible.
const MAX_HANG: usize = 8;

/// The first `max` columns of `text` (used to bound a hanging indent).
fn clip_width(text: &str, max: usize) -> String {
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w > max {
            break;
        }
        out.push(ch);
        used += w;
    }
    out
}

/// Word-wrap one display-safe text into `rows`, keeping list hanging indents.
/// Stops at [`MAX_ROWS`] and reports whether it did.
fn push_wrapped(rows: &mut Vec<Row>, text: &str, width: usize, style: Style) -> bool {
    let width = width.max(1);
    for source in text.split('\n') {
        if rows.len() >= MAX_ROWS {
            return true;
        }
        let (prefix, body) = split_list_prefix(source);
        let hang = prefix.width().min(MAX_HANG).min(width.saturating_sub(1));
        let prefix = clip_width(&prefix, hang);
        let wrap = width.saturating_sub(hang).max(1);
        let mut line = prefix;
        let mut used = 0usize;
        for word in body.split_whitespace() {
            let word_w = word.width();
            if used > 0 && used + 1 + word_w > wrap {
                rows.push((line, style));
                if rows.len() >= MAX_ROWS {
                    return true;
                }
                line = " ".repeat(hang);
                used = 0;
            }
            if word_w > wrap {
                if used > 0 {
                    rows.push((line, style));
                    if rows.len() >= MAX_ROWS {
                        return true;
                    }
                    line = " ".repeat(hang);
                    used = 0;
                }
                for ch in word.chars() {
                    let ch_w = ch.width().unwrap_or(0);
                    if used + ch_w > wrap && used > 0 {
                        rows.push((line, style));
                        if rows.len() >= MAX_ROWS {
                            return true;
                        }
                        line = " ".repeat(hang);
                        used = 0;
                    }
                    line.push(ch);
                    used += ch_w;
                }
                continue;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += word_w;
        }
        rows.push((line, style));
    }
    rows.len() >= MAX_ROWS
}

fn push_section(rows: &mut Vec<Row>, label: &str, palette: Palette) {
    if !rows.is_empty() {
        rows.push((String::new(), Style::default()));
    }
    rows.push((label.to_string(), palette.section()));
}

fn push_bounded(
    rows: &mut Vec<Row>,
    text: Option<&BoundedText>,
    width: usize,
    palette: Palette,
) -> bool {
    let Some(text) = text else {
        return push_wrapped(rows, "not recorded", width, palette.quiet());
    };
    let mut capped = push_wrapped(rows, &display_safe(&text.preview), width, Style::default());
    if text.truncated && !capped {
        let note = format!(
            "[truncated: showing {} of {} bytes]",
            text.preview.len(),
            text.byte_len
        );
        capped |= push_wrapped(rows, &note, width, palette.quiet());
    }
    capped
}

fn duration_text(ms: i64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1000..=59_999 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60),
    }
}

/// One quiet line of tool identity: outcome, duration, and call id.
fn tool_meta(tool: &ToolInteraction) -> String {
    let status = match tool.status {
        ToolStatus::Pending => "Pending",
        ToolStatus::Completed => "Completed",
        ToolStatus::Error => "Error",
        ToolStatus::Unknown => "Unknown",
    };
    let mut parts = vec![status.to_string()];
    if let (Some(start), Some(end)) = (tool.started_at, tool.completed_at)
        && end >= start
    {
        parts.push(duration_text(end - start));
    }
    if let Some(call) = tool.call_id.as_deref() {
        parts.push(display_safe(call));
    }
    parts.join(" · ")
}

fn tool_rows(
    rows: &mut Vec<Row>,
    tool: &ToolInteraction,
    child: Option<&SessionLink>,
    width: usize,
    palette: Palette,
) -> bool {
    let mut capped = false;
    rows.push((tool_meta(tool), palette.quiet()));
    let shell = SHELL_TOOLS
        .iter()
        .any(|name| tool.name.eq_ignore_ascii_case(name));
    push_section(
        rows,
        if shell {
            "command"
        } else {
            "input (recorded preview)"
        },
        palette,
    );
    let summary = tool_summary(tool);
    if !summary.is_empty() {
        capped |= push_wrapped(rows, &summary, width, palette.value());
    }
    let raw = tool
        .input
        .as_ref()
        .map(|input| display_safe(&input.preview));
    if summary.is_empty()
        || raw
            .as_deref()
            .is_some_and(|raw| raw.trim() != summary.trim())
    {
        if !summary.is_empty() {
            rows.push((String::new(), Style::default()));
            rows.push(("recorded arguments".into(), palette.quiet()));
        }
        capped |= push_bounded(rows, tool.input.as_ref(), width, palette);
    }

    push_section(rows, "output (recorded preview)", palette);
    match (&tool.output, tool.status) {
        (None, ToolStatus::Pending) => {
            rows.push(("pending (no result yet)".into(), palette.quiet()));
        }
        (None, _) => {
            rows.push(("not recorded".into(), palette.quiet()));
        }
        (Some(_), _) => capped |= push_bounded(rows, tool.output.as_ref(), width, palette),
    }
    if tool.orphan && !capped {
        capped |= push_wrapped(
            rows,
            "call and result could not be paired",
            width,
            palette.quiet(),
        );
    }
    if let Some(link) = child
        && !capped
    {
        capped = push_wrapped(
            rows,
            &format!(
                "Child session: {}",
                super::session::link_text(&Some(link.clone()))
            ),
            width,
            Style::default(),
        );
    }
    capped
}

fn text_rows(rows: &mut Vec<Row>, event: &Event, width: usize, palette: Palette) -> bool {
    let label = match event.kind {
        EventKind::UserPrompt => "prompt (recorded preview)",
        EventKind::AssistantMessage => "reply (recorded preview)",
        EventKind::Reasoning => "reasoning (recorded preview)",
        EventKind::System => "text (recorded preview)",
        EventKind::Compaction => "compaction (recorded preview)",
        EventKind::Lifecycle | EventKind::Tool => "event (recorded preview)",
    };
    push_section(rows, label, palette);
    push_bounded(rows, event.text.as_ref(), width, palette)
}

fn content_rows(
    event: &Event,
    child: Option<&SessionLink>,
    width: usize,
    palette: Palette,
) -> Vec<Row> {
    let mut rows = Vec::new();
    let capped = match &event.tool {
        Some(tool) => tool_rows(&mut rows, tool, child, width, palette),
        None => text_rows(&mut rows, event, width, palette),
    };
    if capped {
        rows.push((
            "[display limited: more content exists in the source record (v)]".into(),
            palette.quiet(),
        ));
    }
    rows
}

fn scope_name(scope: UsageScope) -> &'static str {
    match scope {
        UsageScope::Session => "session",
        UsageScope::Turn => "turn",
        UsageScope::Message => "message",
    }
}

/// One category bar: relative to the largest recorded category, never a total.
fn bar_cells(value: u64, max: u64) -> usize {
    if value == 0 || max == 0 {
        return 0;
    }
    let cells = (u128::from(value) * BAR_WIDTH as u128 / u128::from(max)) as usize;
    cells.clamp(1, BAR_WIDTH)
}

/// Width of one category label in the side column (`cache write 5m` is 14).
const CATEGORY_LABEL_WIDTH: usize = 14;

fn category_line(label: &str, value: u64, max: u64) -> String {
    let cells = bar_cells(value, max);
    let mut bar = String::with_capacity(BAR_WIDTH);
    for _ in 0..cells {
        bar.push('▰');
    }
    for _ in cells..BAR_WIDTH {
        bar.push('▱');
    }
    format!(
        "{label:<CATEGORY_LABEL_WIDTH$} {:>6} {bar}",
        tokens_text(Some(value))
    )
}

fn usage_rows(rows: &mut Vec<Row>, event: &Event, width: usize, palette: Palette) {
    let usage = event.usage.as_ref();
    let label = usage.map_or_else(
        || "USAGE".to_string(),
        |usage| format!("USAGE ({})", scope_name(usage.scope)),
    );
    rows.push((label, palette.section()));
    let Some(usage) = usage else {
        push_wrapped(rows, "not recorded", width, palette.quiet());
        return;
    };
    rows.push((
        usage.categories.total.map_or_else(
            || "total not recorded".to_string(),
            |total| {
                format!(
                    "{:<CATEGORY_LABEL_WIDTH$} {:>6}",
                    "total",
                    tokens_text(Some(total))
                )
            },
        ),
        if usage.categories.total.is_some() {
            palette.value()
        } else {
            palette.quiet()
        },
    ));
    let categories = [
        ("input", usage.categories.input),
        ("cached input", usage.categories.cached_input),
        ("cache write", usage.categories.cache_write),
        ("cache write 5m", usage.categories.cache_write_5m),
        ("cache write 1h", usage.categories.cache_write_1h),
        ("cache read", usage.categories.cache_read),
        ("output", usage.categories.output),
        ("reasoning", usage.categories.reasoning),
    ];
    let max = categories
        .iter()
        .filter_map(|(_, value)| *value)
        .max()
        .unwrap_or(0);
    let mut recorded = false;
    for (label, value) in categories {
        if let Some(value) = value {
            recorded = true;
            rows.push((category_line(label, value, max), Style::default()));
        }
    }
    if !recorded {
        push_wrapped(rows, "categories not recorded", width, palette.quiet());
    }
}

fn cost_rows(rows: &mut Vec<Row>, event: &Event, width: usize, palette: Palette) {
    rows.push(("COST".to_string(), palette.section()));
    let Some(usage) = event.usage.as_ref() else {
        push_wrapped(rows, "not recorded", width, palette.quiet());
        return;
    };
    let money = &usage.money;
    let mut recorded = false;
    if let Some(fact) = &money.reported_api {
        rows.push((
            format!("{} reported", money_text(fact.usd)),
            palette.value(),
        ));
        recorded = true;
    }
    if let Some(fact) = &money.saved_estimate {
        rows.push((
            format!("~{} saved estimate", money_text(fact.usd)),
            palette.value(),
        ));
        recorded = true;
    }
    if let Some(usd) = money.unverified_usd {
        rows.push((format!("{} unverified", money_text(usd)), palette.quiet()));
        recorded = true;
    }
    if !recorded {
        push_wrapped(rows, "not recorded", width, palette.quiet());
    }
}

/// Locator class without the path: the side column names the kind.
fn locator_kind_text(locator: &SourceLocator) -> String {
    match &locator.kind {
        LocatorKind::FileOffset { offset, .. } => format!("file offset {offset}"),
        LocatorKind::CompressedRecord { ordinal, .. } => format!("compressed record {ordinal}"),
        LocatorKind::DbRow {
            table,
            row_id,
            part_index,
        } => {
            let part = part_index.map_or_else(String::new, |part| format!(" part {part}"));
            format!(
                "table row {} {}{part}",
                display_safe(table),
                display_safe(row_id)
            )
        }
    }
}

fn source_rows(rows: &mut Vec<Row>, event: &Event, app: &App, width: usize, palette: Palette) {
    rows.push(("SOURCE".to_string(), palette.section()));
    push_wrapped(
        rows,
        &display_safe(&event.provenance.record_type),
        width,
        Style::default(),
    );
    push_wrapped(
        rows,
        &locator_kind_text(&event.provenance.locator),
        width,
        palette.quiet(),
    );
    if event.kind == EventKind::Reasoning {
        push_wrapped(rows, REASONING_LABEL, width, palette.quiet());
        push_wrapped(rows, "not hidden chain-of-thought", width, palette.quiet());
    }
    push_wrapped(
        rows,
        if app.revealed_for(&event.provenance.locator) {
            "redaction OFF for this record"
        } else {
            "redacted preview"
        },
        width,
        palette.quiet(),
    );
}

fn side_rows(event: &Event, app: &App, width: usize, palette: Palette) -> Vec<Row> {
    let mut rows = Vec::new();
    usage_rows(&mut rows, event, width, palette);
    rows.push((String::new(), Style::default()));
    cost_rows(&mut rows, event, width, palette);
    rows.push((String::new(), Style::default()));
    source_rows(&mut rows, event, app, width, palette);
    rows
}

/// Status line: transient notice, redaction state, and the loaded-window edge
/// hints the inspector already uses.
fn status_text(app: &App, event: &Event, selected: usize, total: usize) -> String {
    let mut parts = Vec::new();
    if let Some(notice) = app.notice() {
        parts.push(display_safe(notice));
    }
    if let Some(transcript) = app.transcript() {
        if selected == 0 && transcript.has_older() {
            parts.push("▲ older not loaded".into());
        }
        if selected + 1 >= total && transcript.has_newer() {
            parts.push("▼ newer not loaded".into());
        }
    }
    parts.push(if app.revealed_for(&event.provenance.locator) {
        REVEALED_NOTICE.to_string()
    } else {
        "Previews are redacted (best-effort); x reloads the source unredacted.".to_string()
    });
    parts.join("  ·  ")
}

/// One body row with the content margin applied and padded to `width`.
fn margin_line(text: &str, style: Style, width: usize, margin: usize) -> Line<'static> {
    let text = fit(text, width.saturating_sub(margin));
    if margin == 0 {
        Line::styled(text, style)
    } else {
        Line::from(vec![
            Span::raw(" ".repeat(margin)),
            Span::styled(text, style),
        ])
    }
}

/// Draw the visible slice of styled rows without touching the scroll extent.
fn draw_styled_at(frame: &mut Frame, rows: &[Row], scroll: usize, body: Rect, margin: usize) {
    let height = body.height as usize;
    let max_scroll = rows.len().saturating_sub(height);
    let start = scroll.min(max_scroll);
    let shown: Vec<Line<'static>> = rows
        .iter()
        .skip(start)
        .take(height)
        .map(|(text, style)| margin_line(text, *style, body.width as usize, margin))
        .collect();
    frame.render_widget(Paragraph::new(shown), body);
}

/// Draw the visible slice of styled rows and report the scroll extent.
fn draw_styled(
    frame: &mut Frame,
    app: &App,
    rows: &[Row],
    scroll: usize,
    body: Rect,
    margin: usize,
) {
    app.set_overlay_extent(rows.len().saturating_sub(body.height as usize));
    draw_styled_at(frame, rows, scroll, body, margin);
}

pub fn render_details(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let Some(f) = chrome(frame, ctx) else { return };
    let Some(transcript) = app.transcript() else {
        frame.render_widget(Paragraph::new("No session is open."), f.body);
        return;
    };
    let Some(event) = transcript.selected_event() else {
        frame.render_widget(Paragraph::new("No event is selected."), f.body);
        return;
    };
    let palette = f.palette;
    let total = transcript.events().len();
    let selected = transcript.selected();
    frame.render_widget(
        Paragraph::new(header_line(app, event, selected, total, f.width, palette)),
        f.title,
    );
    let child = event
        .tool
        .as_ref()
        .and_then(|tool| app.resolved_tool_child(&event.session, tool));
    let wide = f.width >= WIDE_MIN;
    let content_width = if wide {
        CONTENT_MAX.min(f.width - SIDE_WIDTH - 2 - CONTENT_MARGIN * 2)
    } else {
        CONTENT_MAX.min(f.width - CONTENT_MARGIN)
    };
    let content = content_rows(event, child.as_ref(), content_width, palette);
    if wide {
        let [left, rule, _, right] = Layout::horizontal([
            Constraint::Min(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(SIDE_WIDTH as u16),
        ])
        .areas(f.body);
        let bar: Vec<Line<'static>> = (0..rule.height)
            .map(|_| Line::styled("│", palette.quiet()))
            .collect();
        frame.render_widget(Paragraph::new(bar), rule);
        let side = side_rows(event, app, SIDE_WIDTH, palette);
        let content_max = content.len().saturating_sub(left.height as usize);
        let side_max = side.len().saturating_sub(f.body.height as usize);
        app.set_overlay_extent(content_max.saturating_add(side_max));
        let scroll = app.details_scroll();
        let content_scroll = scroll.min(content_max);
        let side_scroll = scroll.saturating_sub(content_max).min(side_max);
        draw_styled_at(frame, &content, content_scroll, left, CONTENT_MARGIN);
        let shown: Vec<Line<'static>> = side
            .iter()
            .skip(side_scroll)
            .take(f.body.height as usize)
            .map(|(text, style)| margin_line(text, *style, SIDE_WIDTH, 0))
            .collect();
        frame.render_widget(Paragraph::new(shown), right);
    } else {
        let mut rows = content;
        rows.push((String::new(), Style::default()));
        rows.extend(side_rows(event, app, f.width - CONTENT_MARGIN, palette));
        draw_styled(
            frame,
            app,
            &rows,
            app.details_scroll(),
            f.body,
            CONTENT_MARGIN,
        );
    }
    let status = status_text(app, event, selected, total);
    frame.render_widget(
        Paragraph::new(Line::styled(fit(&status, f.width), palette.warning())),
        f.status,
    );
    frame.render_widget(
        Paragraph::new(Line::styled(fit(DETAILS_FOOTER, f.width), palette.dim())),
        f.footer,
    );
}

pub fn render_source(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let Some(f) = chrome(frame, ctx) else { return };
    let Some(view) = app.source_view() else {
        frame.render_widget(Paragraph::new("No source record is open."), f.body);
        return;
    };
    let palette = f.palette;
    let mut rows: Vec<Row> = Vec::new();
    push_section(&mut rows, "LOCATOR", palette);
    push_wrapped(
        &mut rows,
        &locator_text(&view.locator),
        f.width,
        Style::default(),
    );
    push_section(&mut rows, "STATE", palette);
    let mut note = if view.revealed {
        REVEALED_NOTICE.to_string()
    } else {
        REDACTED_NOTICE.to_string()
    };
    match &view.state {
        SourceState::Loading => rows.push(("Loading source record…".into(), palette.quiet())),
        SourceState::Failed => rows.push((
            "The source record could not be loaded.".into(),
            palette.warning(),
        )),
        SourceState::Unavailable(reason) => {
            let reason = display_safe(reason);
            let text = if reason.trim().is_empty() {
                "Source record unavailable: —".to_string()
            } else {
                format!("Source record unavailable: {reason}")
            };
            push_wrapped(&mut rows, &text, f.width, palette.warning());
        }
        SourceState::Ready { text, truncated } => {
            push_section(&mut rows, "CONTENT", palette);
            let mut wrapped = Vec::new();
            if wrap_into(&mut wrapped, text, f.width) || *truncated {
                note = format!("{note}  [record truncated for display]");
            }
            rows.extend(wrapped.into_iter().map(|line| (line, Style::default())));
        }
    }
    draw_styled(frame, app, &rows, view.scroll, f.body, 0);
    put(
        frame,
        &f,
        "SOURCE RECORD",
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
    let row = app.session(transcript.key());
    let mut head: Vec<Row> = Vec::new();
    push_section(&mut head, "OVERVIEW", f.palette);
    head.push((
        format!(
            "Hidden records: {}",
            row.as_ref().map_or_else(
                || "—".to_string(),
                |row| row.session.hidden_records.to_string()
            )
        ),
        Style::default(),
    ));
    head.push((
        format!(
            "Source available: {}",
            row.as_ref().map_or_else(
                || "—".to_string(),
                |row| if row.session.meta.source_available {
                    "yes".to_string()
                } else {
                    "no".to_string()
                }
            )
        ),
        Style::default(),
    ));
    head.push((
        format!("Entries listed: {}", entries.len()),
        Style::default(),
    ));
    push_section(&mut head, "WARNINGS", f.palette);
    match row.as_ref() {
        Some(row) if !row.session.warnings.is_empty() => {
            for w in row.session.warnings.iter().take(MAX_WARNINGS) {
                head.push((
                    format!("Warning ×{}: {}", w.count, display_safe(&w.message)),
                    f.palette.warning(),
                ));
            }
        }
        Some(_) => head.push(("— none".into(), f.palette.quiet())),
        None => head.push(("—".into(), f.palette.quiet())),
    }
    push_section(&mut head, "RECORDS", f.palette);

    let height = f.body.height as usize;
    let mut lines: Vec<Line<'static>> = head
        .iter()
        .take(height)
        .map(|(text, style)| margin_line(text, *style, f.width, 0))
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
        "DIAGNOSTICS",
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
