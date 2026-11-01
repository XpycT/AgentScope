//! Inspector screen rendering (spec §10.3, §10.6). Pure: reads `App` and a
//! small context, draws into a `Frame`. No I/O.
//!
//! The transcript is a conversation, not an event log: [`turn_rows`] folds the
//! loaded events into prompt, reply, tool, reasoning and noise rows, and each
//! row draws with the shared visual language (bars, glyphs, quiet asides).

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    App, PageKind, RowKind, Transcript, TurnRow, tool_summary as tool_argument_summary, turn_rows,
};
use crate::domain::{
    AggregationClass, BoundedText, ContextUsage, Event, EventKind, ProcessAssociation, SessionLink,
    SessionSummary, StatusLabel, Timestamp, ToolInteraction, ToolStatus, UsageScope,
};
use crate::ingest::text::display_safe;
use crate::time::format_local;

use super::RosterContext;
use super::preview::status_age;
use super::roster::{MIN_HEIGHT, MIN_WIDTH};
use super::session::{cost_fact, speed_fact, tokens_fact};
use super::widgets::{
    ABSENT, ELLIPSIS, Palette, agent_tag, display_title, effort_span, fit, search_prompt,
    status_chip, tokens_text, tool_glyph,
};

const MARKER_W: usize = 2;
const TIME_W: usize = 5;
const ACTOR_W: usize = 5;
/// Most wrapped text lines shown for a collapsed prompt or reply.
const COLLAPSED_TEXT_LINES: usize = 3;
/// Most lines one expanded row shows before deferring to Details.
const MAX_DETAIL_LINES: usize = 12;
const HEADER_LINES: u16 = 3;
const ROW_INDENT: usize = 2;
const DETAIL_INDENT: usize = 4;

pub const REASONING_LABEL: &str = "Provider reasoning (persisted)";

const FOOTER_HINTS: &str =
    "j/k move  Enter expand  o open child  / search  d details  i info  G latest  Esc back";

pub fn render_inspector(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let area = frame.area();
    let palette = Palette::new(ctx.no_color);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = format!(
            "Terminal too small: {}x{} (need at least {MIN_WIDTH}x{MIN_HEIGHT}). Resize to continue; Esc goes back.",
            area.width, area.height
        );
        frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
        return;
    }
    let Some(transcript) = app.transcript() else {
        frame.render_widget(Paragraph::new("No session is open."), area);
        return;
    };

    let [head, rule1, body, rule2, status, footer] = Layout::vertical([
        Constraint::Length(HEADER_LINES),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let width = area.width as usize;

    frame.render_widget(
        Paragraph::new(header_lines(app, transcript, width, palette)),
        head,
    );
    let rule = "─".repeat(width);
    frame.render_widget(Paragraph::new(rule.as_str()), rule1);
    frame.render_widget(Paragraph::new(rule.as_str()), rule2);
    render_timeline(frame, app, transcript, body, palette);
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(&status_line(app, transcript), width),
            palette.quiet(),
        )),
        status,
    );
    let (footer_text, footer_style) = match app.search_input() {
        Some((_, text)) => (search_prompt(&display_safe(text)), palette.title()),
        None => (FOOTER_HINTS.to_string(), palette.dim()),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(fit(&footer_text, width), footer_style)),
        footer,
    );
}

/// Exactly three lines: identity and state, quiet provenance, key numbers.
pub(super) fn header_lines(
    app: &App,
    transcript: &Transcript,
    width: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    let Some(row) = app.session(transcript.key()) else {
        return vec![
            Line::styled(
                fit("Session is no longer in the roster.", width),
                palette.title(),
            ),
            Line::raw(""),
            Line::raw(""),
        ];
    };
    let s = row.session;
    let hydrated = row.hydrated;
    let title = display_title(&s.meta);
    let age = status_age(s, app.now());
    let chip = status_chip(row.status.label, age.as_deref(), palette);
    let tag = agent_tag(s.meta.key.harness, palette);
    let chip_w = chip.content.width();
    let fixed = tag.content.width() + 1 + 2 + chip_w + 2;
    let title_text = clip(&title, width.saturating_sub(fixed).max(1));
    let evidence_w = width.saturating_sub(fixed + title_text.width());
    let evidence = clip(&flatten(&row.status.evidence), evidence_w);
    let line1 = Line::from(vec![
        tag,
        Span::raw(" "),
        Span::styled(title_text, palette.title()),
        Span::raw("  "),
        chip,
        Span::raw("  "),
        Span::styled(evidence, palette.quiet()),
    ]);
    let line2 = Line::from(line2_spans(s, transcript, palette));
    let line3 = Line::from(line3_spans(app, transcript, s, hydrated, palette));
    vec![line1, line2, line3]
}

/// Project · model · thinking level · provider · started · loaded events.
fn line2_spans(
    s: &SessionSummary,
    transcript: &Transcript,
    palette: Palette,
) -> Vec<Span<'static>> {
    let quiet = |text: String| Span::styled(text, palette.quiet());
    let project = s
        .meta
        .project
        .as_deref()
        .map_or_else(|| ABSENT.to_string(), flatten);
    let model = s
        .meta
        .model
        .as_ref()
        .map_or_else(|| ABSENT.to_string(), |m| flatten(&m.id));
    let provider = s
        .meta
        .model
        .as_ref()
        .and_then(|m| m.provider.as_deref())
        .map_or_else(|| ABSENT.to_string(), flatten);
    let started = s
        .metrics
        .timing
        .started_at
        .or(s.meta.created_at)
        .map_or_else(|| ABSENT.to_string(), stamp);
    let effort = s
        .meta
        .model
        .as_ref()
        .and_then(|m| m.effort.as_deref().or(m.configured_effort.as_deref()));
    let mut spans = vec![
        quiet(format!("project {project}")),
        quiet(" · ".into()),
        quiet(format!("model {model}")),
        quiet(" · ".into()),
        quiet("thinking ".into()),
    ];
    match effort {
        Some(effort) => spans.push(effort_span(effort, palette)),
        None => spans.push(quiet(ABSENT.into())),
    }
    spans.push(quiet(format!(
        " · provider {provider} · started {started} · {} loaded",
        transcript.events().len()
    )));
    spans
}

/// Tokens · cost · context · speed · tool calls · children.
fn line3_spans(
    app: &App,
    transcript: &Transcript,
    s: &SessionSummary,
    hydrated: bool,
    palette: Palette,
) -> Vec<Span<'static>> {
    let tokens = tokens_fact(s.usage.as_ref(), hydrated);
    let cost = cost_fact(s.usage.as_ref(), hydrated);
    let speed = speed_fact(s, hydrated, app.now());
    let mut parts = vec![
        format!("tokens {}", tokens.head),
        format!("cost {}", cost.head),
    ];
    if let Some(context) = context_text(&s.metrics.context) {
        parts.push(format!("context {context}"));
    }
    if numeric(&speed.head) {
        parts.push(format!("speed {} tok/s", speed.head));
    }
    let tools = transcript
        .events()
        .iter()
        .filter(|e| e.tool.is_some())
        .count();
    if tools > 0 {
        parts.push(format!(
            "{tools}{} tool{}",
            if transcript.has_older() { "+" } else { "" },
            if tools == 1 { "" } else { "s" }
        ));
    }
    if let Some(children) = app.children_of(transcript.key()) {
        let count = children.metadata.len();
        if count > 0 {
            parts.push(format!(
                "{count}{} child{}",
                if children.next.is_some() { "+" } else { "" },
                if count == 1 { "" } else { "ren" }
            ));
        }
    }
    vec![Span::styled(parts.join(" · "), palette.quiet())]
}

fn stamp(ts: Timestamp) -> String {
    let local = format_local(ts);
    local
        .get(..16)
        .map_or_else(|| local.clone(), str::to_string)
}

fn flatten(text: &str) -> String {
    display_safe(text).replace(['\n', '\r'], " ")
}

/// Truncate display-safe text to `width` columns, marking the cut.
fn clip(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for ch in text.chars() {
        let w = ch.width().unwrap_or(0);
        if used + w + 1 > width {
            break;
        }
        out.push(ch);
        used += w;
    }
    out.push(ELLIPSIS);
    out
}

fn context_text(c: &ContextUsage) -> Option<String> {
    match (c.used, c.capacity) {
        (Some(used), Some(window)) => Some(format!(
            "{} of {}",
            tokens_text(Some(used)),
            tokens_text(Some(window))
        )),
        (Some(used), None) => Some(tokens_text(Some(used))),
        (None, Some(window)) => Some(format!("{} window", tokens_text(Some(window)))),
        (None, None) => None,
    }
}

fn numeric(text: &str) -> bool {
    !text.is_empty() && text.chars().all(|c| c.is_ascii_digit() || c == '.')
}

pub(super) fn class_name(class: AggregationClass) -> &'static str {
    match class {
        AggregationClass::Reported => "reported",
        AggregationClass::Derived => "derived",
        AggregationClass::Partial => "partial",
        AggregationClass::Estimated => "estimated",
        AggregationClass::Unknown => "unknown",
    }
}

/// `73214` -> `73,214`.
pub(super) fn grouped(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// USD with enough precision that a tiny positive amount is never shown as free.
pub(super) fn money_text(usd: f64) -> String {
    if usd <= 0.0 {
        "$0.00".into()
    } else if usd < 0.0001 {
        "<$0.0001".into()
    } else if usd < 0.01 {
        format!("${usd:.4}")
    } else {
        format!("${usd:.2}")
    }
}

pub(super) fn process_line(process: &ProcessAssociation) -> String {
    match process {
        ProcessAssociation::Matched { pid, .. } => format!("matched — PID {pid}"),
        ProcessAssociation::Candidates { count } => {
            format!(
                "unmatched — {count} candidate{}",
                if *count == 1 { "" } else { "s" }
            )
        }
        ProcessAssociation::Unmatched => "unmatched — no candidates".into(),
        ProcessAssociation::NotChecked => "not checked".into(),
        ProcessAssociation::Unavailable => "unavailable".into(),
    }
}

fn status_line(app: &App, transcript: &Transcript) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(notice) = app.notice() {
        parts.push(notice.to_string());
    }
    if !transcript.search_query().trim().is_empty() {
        parts.push(match transcript.match_count() {
            0 => "no matches (loaded events)".to_string(),
            n => format!(
                "match {}/{n} (loaded events)",
                transcript
                    .match_position()
                    .map_or_else(|| "-".to_string(), |p| (p + 1).to_string())
            ),
        });
    }
    match transcript.loading() {
        Some(PageKind::Older) => parts.push("Loading older…".into()),
        Some(PageKind::Newer) => parts.push("Loading newer…".into()),
        Some(PageKind::Initial | PageKind::Tail) => parts.push("Loading…".into()),
        Some(PageKind::Reload) => parts.push("Refreshing…".into()),
        None => {}
    }
    if transcript.failed().is_some() {
        parts.push(if transcript.reload_capped() {
            "Reading position is beyond the loaded-event limit — r jumps to newest".into()
        } else {
            "Load failed — r retries".into()
        });
    }
    let new = app.new_event_count();
    if new > 0 {
        parts.push(format!("{new} new (G for newest)"));
    }
    if transcript.has_older() {
        parts.push("▲ older not loaded".into());
    }
    if transcript.has_newer() {
        parts.push("▼ newer not loaded".into());
    }
    if !transcript.events().is_empty() {
        parts.push(format!(
            "{}/{} loaded",
            transcript.selected() + 1,
            transcript.events().len()
        ));
    }
    parts.join("  ·  ")
}

fn render_timeline(
    frame: &mut Frame,
    app: &App,
    transcript: &Transcript,
    body: Rect,
    palette: Palette,
) {
    let events = transcript.events();
    if events.is_empty() {
        let text = if transcript.failed().is_some() {
            "Could not load this transcript. Press r to retry."
        } else if transcript.loaded() {
            "No events in this transcript."
        } else {
            "Loading…"
        };
        frame.render_widget(Paragraph::new(text), body);
        return;
    }
    let rows = turn_rows(events);
    if rows.is_empty() {
        frame.render_widget(Paragraph::new("No events in this transcript."), body);
        return;
    }
    let height = body.height as usize;
    let width = body.width as usize;
    let selected = transcript.selected().min(events.len() - 1);
    let selected_row = rows
        .iter()
        .position(|row| row.first <= selected && selected <= row.last)
        .unwrap_or(0);

    // Start near the middle, then advance until the selected row fits, so
    // loading pages around the selection never moves what is on screen.
    let mut start = selected_row.saturating_sub(height / 2);
    let lines = loop {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut selected_end = None;
        for (index, row) in rows.iter().enumerate().skip(start) {
            if lines.len() >= height {
                break;
            }
            lines.extend(row_lines(
                row, events, selected, transcript, app, width, palette,
            ));
            if index == selected_row {
                selected_end = Some(lines.len());
            }
        }
        match selected_end {
            Some(end) if end > height && start < selected_row => start += 1,
            None if start < selected_row => start += 1,
            _ => break lines,
        }
    };
    let shown: Vec<Line<'static>> = lines.into_iter().take(height).collect();
    frame.render_widget(Paragraph::new(shown), body);
}

/// Presentation inputs shared by every row renderer.
#[derive(Clone, Copy)]
struct RowCtx {
    selected: bool,
    matched: bool,
    width: usize,
    palette: Palette,
}

fn row_lines(
    row: &TurnRow,
    events: &[Event],
    selected_event: usize,
    transcript: &Transcript,
    app: &App,
    width: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    let selected = row.first <= selected_event && selected_event <= row.last;
    let matched = (row.first..=row.last).any(|index| transcript.is_match(index));
    let index = if selected { selected_event } else { row.first };
    let event = &events[index.min(row.last)];
    let expanded = transcript.is_expanded(event);
    let ctx = RowCtx {
        selected,
        matched,
        width,
        palette,
    };
    match &row.kind {
        RowKind::Prompt => turn_text_lines(event, expanded, true, ctx),
        RowKind::Reply => turn_text_lines(event, expanded, false, ctx),
        RowKind::Tool => tool_lines(row, event, expanded, app, ctx),
        RowKind::Reasoning => reasoning_lines(row, event, expanded, ctx),
        RowKind::ReasoningEmpty { count } => {
            let mut text = "reasoning (no text recorded)".to_string();
            if *count > 1 {
                text.push_str(&format!(" ×{count}"));
            }
            vec![quiet_row_line(row.depth, &text, ctx)]
        }
        RowKind::Noise { label, .. } => noise_lines(row, event, expanded, label, ctx),
        RowKind::Other => other_lines(row, event, expanded, ctx),
    }
}

fn marker_span(selected: bool, matched: bool, palette: Palette) -> Span<'static> {
    if selected {
        Span::styled(
            format!("{} ", Palette::SELECTION_MARKER),
            palette.selected(),
        )
    } else if matched {
        Span::styled("* ", palette.title())
    } else {
        Span::raw("  ")
    }
}

fn indent(depth: u8) -> String {
    " ".repeat(ROW_INDENT * usize::from(depth))
}

/// One quiet line in the row's indentation: reasoning notes and folded noise.
fn quiet_row_line(depth: u8, text: &str, ctx: RowCtx) -> Line<'static> {
    let prefix = MARKER_W + ROW_INDENT * usize::from(depth);
    let body = clip(text, ctx.width.saturating_sub(prefix));
    Line::from(vec![
        marker_span(ctx.selected, ctx.matched, ctx.palette),
        Span::raw(indent(depth)),
        Span::styled(body, ctx.palette.quiet()),
    ])
}

/// The quiet tail of an expanded row: a bounded preview note, never a claim
/// that the excerpt is the full record.
fn detail_line(text: &str, ctx: RowCtx) -> Line<'static> {
    let prefix = MARKER_W + DETAIL_INDENT;
    let body = clip(text, ctx.width.saturating_sub(prefix));
    Line::from(vec![
        marker_span(ctx.selected, false, ctx.palette),
        Span::raw(" ".repeat(DETAIL_INDENT)),
        Span::styled(body, ctx.palette.quiet()),
    ])
}

/// A prompt or reply: bar, actor, time, wrapped recorded preview. A reply's
/// token count is shown only when the usage belongs to that message or turn.
fn turn_text_lines(event: &Event, expanded: bool, prompt: bool, ctx: RowCtx) -> Vec<Line<'static>> {
    let actor = if prompt { "You" } else { "Agent" };
    let bar_style = if prompt {
        ctx.palette.prompt_bar()
    } else {
        ctx.palette.reply_bar()
    };
    let width = ctx.width;
    let prefix = MARKER_W + 2 + ACTOR_W + 2 + TIME_W + 1;
    let token = if prompt {
        None
    } else {
        reply_tokens(event, ctx.palette)
    };
    let token_w = token.as_ref().map_or(0, |t| t.content.width());
    let text = event.text.as_ref().map_or_else(String::new, |t| {
        display_safe(&t.preview).trim_end_matches('\n').to_string()
    });
    let first_width = width
        .saturating_sub(prefix + token_w + usize::from(token_w > 0))
        .max(1);
    let rest_width = width.saturating_sub(prefix).max(1);
    let wrapped = wrap_text_widths(&text, first_width, rest_width);
    let (mut body, cut) = if expanded {
        capped(wrapped, MAX_DETAIL_LINES)
    } else {
        capped(wrapped, COLLAPSED_TEXT_LINES)
    };

    let head_text = body.first().cloned().unwrap_or_default();
    let head_w = head_text.width();
    if !body.is_empty() {
        body.remove(0);
    }
    let head = {
        let mut spans = vec![
            marker_span(ctx.selected, ctx.matched, ctx.palette),
            Span::styled("▌", bar_style),
            Span::raw(" "),
            Span::styled(
                format!("{actor:<width$}", width = ACTOR_W),
                ctx.palette.title(),
            ),
            Span::raw("  "),
            Span::styled(clock(event.timestamp), ctx.palette.dim()),
            Span::raw(" "),
            Span::raw(head_text),
        ];
        if let Some(token) = token {
            let used = prefix + head_w;
            let spaces = width.saturating_sub(used + token_w);
            spans.push(Span::raw(" ".repeat(spaces)));
            spans.push(token);
        }
        Line::from(spans)
    };
    let mut lines = vec![head];
    let continuation = " ".repeat(prefix - MARKER_W);
    for line in body {
        lines.push(Line::from(vec![
            marker_span(ctx.selected, false, ctx.palette),
            Span::raw(continuation.clone()),
            Span::raw(line),
        ]));
    }
    if cut {
        lines.push(Line::from(vec![
            marker_span(ctx.selected, false, ctx.palette),
            Span::raw(continuation),
            Span::raw(ELLIPSIS.to_string()),
        ]));
    }
    if expanded {
        if cut {
            lines.push(detail_line("more in details (d)", ctx));
        }
        lines.push(detail_line("recorded preview · v opens the source", ctx));
    }
    lines
}

/// A tool interaction: status glyph, name colored by kind, one-line summary,
/// and the right-aligned outcome or duration plus a confirmed child marker.
fn tool_lines(
    row: &TurnRow,
    event: &Event,
    expanded: bool,
    app: &App,
    ctx: RowCtx,
) -> Vec<Line<'static>> {
    let Some(tool) = event.tool.as_ref() else {
        return vec![quiet_row_line(
            row.depth,
            &format!("· tool  {}", event_first_line(event)),
            ctx,
        )];
    };
    let name = flatten(&tool.name);
    let summary = tool_argument_summary(tool);
    let mut right: Vec<Span<'static>> = Vec::new();
    if let Some(code) = tool.exit_code {
        let text = format!("exit {code}");
        right.push(Span::styled(
            text,
            if code == 0 {
                ctx.palette.quiet()
            } else {
                ctx.palette.status(StatusLabel::Failed)
            },
        ));
    } else if let (Some(start), Some(end)) = (tool.started_at, tool.completed_at)
        && end >= start
    {
        right.push(Span::styled(
            duration_text(end - start),
            ctx.palette.quiet(),
        ));
    }
    if child_marker(app, event, tool) {
        if !right.is_empty() {
            right.push(Span::raw("  "));
        }
        right.push(Span::styled("▸ child", ctx.palette.quiet()));
    }
    let right_w: usize = right.iter().map(|s| s.content.width()).sum();
    let prefix = MARKER_W + ROW_INDENT * usize::from(row.depth) + 2;
    let avail = ctx
        .width
        .saturating_sub(prefix + right_w + usize::from(right_w > 0))
        .max(1);
    let name_text = clip(&name, avail);
    let name_w = name_text.width();
    let summary_avail = avail.saturating_sub(name_w + 1);
    let summary_text = clip(&summary, summary_avail);
    let mut spans = vec![
        marker_span(ctx.selected, ctx.matched, ctx.palette),
        Span::raw(indent(row.depth)),
        Span::styled(
            tool_glyph(tool.status),
            ctx.palette.tool_status(tool.status),
        ),
        Span::raw(" "),
        Span::styled(name_text, ctx.palette.tool_name(&tool.name)),
    ];
    let mut used = prefix + name_w;
    if !summary_text.is_empty() {
        spans.push(Span::raw(" "));
        used += 1 + summary_text.width();
        spans.push(Span::raw(summary_text));
    }
    let spaces = ctx.width.saturating_sub(used + right_w);
    spans.push(Span::raw(" ".repeat(spaces)));
    spans.extend(right);
    let mut lines = vec![Line::from(spans)];
    if expanded {
        let detail = tool_detail_lines(tool);
        let (detail, cut) = capped(detail, MAX_DETAIL_LINES);
        for line in detail {
            lines.push(detail_line(&line, ctx));
        }
        if cut {
            lines.push(detail_line("more in details (d)", ctx));
        }
        lines.push(detail_line("recorded preview · v opens the source", ctx));
    }
    lines
}

fn reply_tokens(event: &Event, palette: Palette) -> Option<Span<'static>> {
    let usage = event.usage.as_ref()?;
    if !matches!(usage.scope, UsageScope::Message | UsageScope::Turn) {
        return None;
    }
    let total = usage.categories.total?;
    Some(Span::styled(tokens_text(Some(total)), palette.quiet()))
}

fn child_marker(app: &App, event: &Event, tool: &ToolInteraction) -> bool {
    let Some(SessionLink::Resolved(key)) = app.resolved_tool_child(&event.session, tool) else {
        return false;
    };
    app.session(&key).is_some()
}

/// Reasoning: quiet first words, or the bounded preview once expanded.
fn reasoning_lines(
    row: &TurnRow,
    event: &Event,
    expanded: bool,
    ctx: RowCtx,
) -> Vec<Line<'static>> {
    let text = event.text.as_ref().map_or_else(String::new, |t| {
        display_safe(&t.preview).trim_end_matches('\n').to_string()
    });
    let label = "reasoning: ";
    let indent = indent(row.depth);
    let prefix = MARKER_W + indent.len() + label.len();
    let avail = ctx.width.saturating_sub(prefix).max(1);
    if !expanded {
        let body = clip(&text, avail);
        return vec![Line::from(vec![
            marker_span(ctx.selected, ctx.matched, ctx.palette),
            Span::raw(indent),
            Span::styled(format!("{label}{body}"), ctx.palette.quiet()),
        ])];
    }
    let wrapped = wrap_text_widths(&text, avail, avail);
    let (body, cut) = capped(wrapped, MAX_DETAIL_LINES);
    let pad = " ".repeat(label.len());
    let mut lines = Vec::new();
    for (i, line) in body.into_iter().enumerate() {
        let head = if i == 0 {
            format!("{label}{line}")
        } else {
            format!("{pad}{line}")
        };
        lines.push(Line::from(vec![
            marker_span(ctx.selected, i == 0 && ctx.matched, ctx.palette),
            Span::raw(indent.clone()),
            Span::styled(head, ctx.palette.quiet()),
        ]));
    }
    if cut {
        lines.push(detail_line("more in details (d)", ctx));
    }
    lines.push(detail_line("recorded preview · v opens the source", ctx));
    lines
}

/// System, lifecycle and compaction events: the smallest row that still names
/// what happened.
fn other_lines(row: &TurnRow, event: &Event, expanded: bool, ctx: RowCtx) -> Vec<Line<'static>> {
    let text = event_first_line(event);
    let label = if text.is_empty() {
        format!("· {}", actor_label(event))
    } else {
        format!("· {}  {text}", actor_label(event))
    };
    let mut lines = vec![quiet_row_line(row.depth, &label, ctx)];
    if expanded {
        lines.extend(text_preview_lines(event, ctx));
    }
    lines
}

/// Folded noise keeps its collapsed label; expanding the row shows the selected
/// event's recorded text instead of leaving Enter without a visible effect.
fn noise_lines(
    row: &TurnRow,
    event: &Event,
    expanded: bool,
    label: &str,
    ctx: RowCtx,
) -> Vec<Line<'static>> {
    let mut lines = vec![quiet_row_line(row.depth, &format!("· {label}"), ctx)];
    if expanded {
        lines.extend(text_preview_lines(event, ctx));
    }
    lines
}

/// The bounded recorded text of one event, as quiet detail lines ending with
/// the source hint. Shared by expanded noise and unrecognized events.
fn text_preview_lines(event: &Event, ctx: RowCtx) -> Vec<Line<'static>> {
    let body = event.text.as_ref().map_or_else(String::new, |t| {
        display_safe(&t.preview).trim_end_matches('\n').to_string()
    });
    let width = ctx.width.saturating_sub(MARKER_W + DETAIL_INDENT).max(1);
    let wrapped = wrap_text_widths(&body, width, width);
    let (body, cut) = capped(wrapped, MAX_DETAIL_LINES);
    let mut lines: Vec<Line<'static>> = body.iter().map(|line| detail_line(line, ctx)).collect();
    if cut {
        lines.push(detail_line("more in details (d)", ctx));
    }
    lines.push(detail_line("recorded preview · v opens the source", ctx));
    lines
}

pub(super) fn actor_label(event: &Event) -> &'static str {
    match event.kind {
        EventKind::UserPrompt => "user",
        EventKind::AssistantMessage => "assistant",
        EventKind::Reasoning => "reasoning",
        EventKind::Tool => "tool",
        EventKind::System => "system",
        EventKind::Lifecycle => "lifecycle",
        EventKind::Compaction => "compaction",
    }
}

fn clock(ts: Option<Timestamp>) -> String {
    match ts {
        // `YYYY-MM-DD HH:MM:SS +zz:zz`; keep `HH:MM`.
        Some(ts) => format_local(ts)
            .get(11..16)
            .map_or_else(|| ABSENT.to_string(), str::to_string),
        None => ABSENT.to_string(),
    }
}

fn first_line(text: &BoundedText) -> String {
    display_safe(text.preview.lines().next().unwrap_or(""))
}

fn event_first_line(event: &Event) -> String {
    event.text.as_ref().map_or_else(String::new, first_line)
}

/// Word-wrap display-safe text: the first output line uses `first` columns,
/// every later line uses `rest`. Explicit line breaks are kept, and a word
/// wider than the column is split rather than allowed to overflow.
fn wrap_text_widths(text: &str, first: usize, rest: usize) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let rest = rest.max(1);
    let mut width = first.max(1);
    let mut out = Vec::new();
    for source in text.split('\n') {
        let mut line = String::new();
        let mut used = 0usize;
        for word in source.split_whitespace() {
            let word_w = word.width();
            if word_w > width {
                // The current line keeps its own budget, including the first
                // line's narrower one. Only a line emitted past it uses `rest`.
                if used > 0 {
                    out.push(std::mem::take(&mut line));
                    used = 0;
                    width = rest;
                }
                for ch in word.chars() {
                    let w = ch.width().unwrap_or(0);
                    if used + w > width && used > 0 {
                        out.push(std::mem::take(&mut line));
                        used = 0;
                        width = rest;
                    }
                    line.push(ch);
                    used += w;
                }
                continue;
            }
            if used > 0 && used + 1 + word_w > width {
                out.push(std::mem::take(&mut line));
                used = 0;
                width = rest;
            }
            if used > 0 {
                line.push(' ');
                used += 1;
            }
            line.push_str(word);
            used += word_w;
        }
        out.push(line);
        width = rest;
    }
    out
}

fn capped(lines: Vec<String>, cap: usize) -> (Vec<String>, bool) {
    if lines.len() > cap {
        (lines.into_iter().take(cap).collect(), true)
    } else {
        (lines, false)
    }
}

/// Recorded preview blocks for an expanded row, bounded to what the row has.
fn tool_detail_lines(tool: &ToolInteraction) -> Vec<String> {
    let mut out = Vec::new();
    push_preview(&mut out, "input", tool.input.as_ref());
    match (&tool.output, tool.status) {
        (None, ToolStatus::Pending) => out.push("output: pending (no result yet)".into()),
        (None, _) => out.push("output: none recorded".into()),
        (Some(_), _) => push_preview(&mut out, "output", tool.output.as_ref()),
    }
    if tool.orphan {
        out.push("note: call and result could not be paired".into());
    }
    out
}

fn push_preview(out: &mut Vec<String>, label: &str, text: Option<&BoundedText>) {
    let Some(text) = text else {
        out.push(format!("{label}: {ABSENT}"));
        return;
    };
    let mut lines = text.preview.lines();
    out.push(format!(
        "{label}: {}",
        display_safe(lines.next().unwrap_or(""))
    ));
    out.extend(lines.map(|line| format!("  {}", display_safe(line))));
    if text.truncated {
        out.push(format!(
            "  [truncated: showing {} of {} bytes]",
            text.preview.len(),
            text.byte_len
        ));
    }
}

fn duration_text(ms: i64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1000..=59_999 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60),
    }
}
