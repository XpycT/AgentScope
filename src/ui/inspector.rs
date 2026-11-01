//! Inspector screen rendering (spec §10.3, §10.6). Pure: reads `App` and a
//! small context, draws into a `Frame`. No I/O.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{App, PageKind, Transcript};
use crate::domain::{
    AggregationClass, BoundedText, Event, EventKind, ProcessAssociation, SessionSummary, Timestamp,
    ToolInteraction, ToolStatus, UsageScope, UsageSummary,
};
use crate::ingest::text::display_safe;
use crate::time::format_local;

use super::RosterContext;
use super::roster::{MIN_HEIGHT, MIN_WIDTH};
use super::widgets::{ABSENT, Palette, fit, harness_name, search_prompt, status_text, tokens_text};

const MARKER_W: usize = 2;
const TIME_W: usize = 5;
const ACTOR_W: usize = 10;
/// Most detail lines shown for one expanded row.
const MAX_DETAIL_LINES: usize = 12;
const HEADER_LINES: u16 = 6;

pub const REASONING_LABEL: &str = "Provider reasoning (persisted)";

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
    render_timeline(frame, transcript, body, palette);
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(&status_line(app, transcript), width),
            palette.warning(),
        )),
        status,
    );
    let (footer_text, footer_style) = match app.search_input() {
        Some((_, text)) => (search_prompt(&display_safe(text)), palette.title()),
        None => (
            "j/k move  Enter expand  / search  n/N match  d details  i session  Esc back  ? help  q quit"
                .to_string(),
            palette.dim(),
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(fit(&footer_text, width), footer_style)),
        footer,
    );
}

fn header_lines(
    app: &App,
    transcript: &Transcript,
    width: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    let plain = |text: String| Line::from(fit(&text, width));
    let Some(row) = app.session(transcript.key()) else {
        return vec![
            Line::styled(
                fit(
                    &format!(
                        "< Back  {}",
                        harness_name(transcript.key().harness).to_uppercase()
                    ),
                    width,
                ),
                palette.title(),
            ),
            plain("Session is no longer in the roster.".into()),
        ];
    };
    let s = row.session;
    let (icon, label) = status_text(row.status.label);
    let title = display_safe(&s.meta.title);
    vec![
        Line::styled(
            fit(
                &format!(
                    "< Back  {} / {title}",
                    harness_name(s.meta.key.harness).to_uppercase()
                ),
                width,
            ),
            palette.title(),
        ),
        plain(format!(
            "Project: {}   Model: {}   Session: {}",
            s.meta
                .project
                .as_deref()
                .map_or_else(|| "Unknown".into(), display_safe),
            model_text(s),
            display_safe(&s.meta.key.native_id),
        )),
        Line::from(vec![Span::styled(
            fit(
                &format!(
                    "Status: {icon} {label} — {}",
                    display_safe(&row.status.evidence)
                ),
                width,
            ),
            palette.status(row.status.label),
        )]),
        plain(usage_text(s)),
        plain(server_tools_text(s)),
        plain(format!("Process: {}", process_line(&s.process))),
    ]
}

fn model_text(s: &SessionSummary) -> String {
    s.meta
        .model
        .as_ref()
        .map_or_else(|| "Unknown".into(), |m| display_safe(&m.id))
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

fn usage_text(s: &SessionSummary) -> String {
    let tokens = match s.usage.as_ref().and_then(|u| u.total.map(|t| (t, u))) {
        Some((total, usage)) => format!("{} {}", grouped(total), class_name(usage.class)),
        None => "Unknown".into(),
    };
    let usage = &s.metrics.context;
    let context = match (usage.used, usage.capacity) {
        (Some(used), Some(window)) => format!(
            "{} of {} window",
            tokens_text(Some(used)),
            tokens_text(Some(window))
        ),
        (Some(used), None) => format!("{} used — window Unknown", tokens_text(Some(used))),
        (None, Some(window)) => format!("{} window", tokens_text(Some(window))),
        (None, None) => "Unknown".to_string(),
    };
    format!(
        "Tokens: {tokens} · Cost: {} · Context: {context}",
        cost_text(s.usage.as_ref())
    )
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

pub(super) fn cost_text(usage: Option<&UsageSummary>) -> String {
    let fact = super::session::cost_fact(usage, true);
    if fact.head == ABSENT {
        "Unknown".into()
    } else if fact.sub.is_empty() {
        fact.head
    } else {
        format!("{} {}", fact.head, fact.sub)
    }
}

fn server_tools_text(s: &SessionSummary) -> String {
    match s.server_tools {
        Some(t) => format!(
            "Server tools: web_search {} · web_fetch {}",
            t.web_search, t.web_fetch
        ),
        None => "Server tools: Unknown".into(),
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

fn render_timeline(frame: &mut Frame, transcript: &Transcript, body: Rect, palette: Palette) {
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
    let height = body.height as usize;
    let width = body.width as usize;
    let selected = transcript.selected().min(events.len() - 1);

    // Start near the middle, then advance until the selected row fits, so
    // loading pages around the selection never moves what is on screen.
    let mut start = selected.saturating_sub(height / 2);
    let lines = loop {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut selected_end = None;
        for (i, event) in events.iter().enumerate().skip(start) {
            if lines.len() >= height {
                break;
            }
            lines.extend(event_lines(
                event,
                transcript.is_expanded(event),
                i == selected,
                transcript.is_match(i),
                width,
                palette,
            ));
            if i == selected {
                selected_end = Some(lines.len());
            }
        }
        match selected_end {
            // Advance while the selection is cut off, or not reached at all
            // because rows above it (expanded) fill the viewport.
            Some(end) if end > height && start < selected => start += 1,
            None if start < selected => start += 1,
            _ => break lines,
        }
    };
    let shown: Vec<Line<'static>> = lines.into_iter().take(height).collect();
    frame.render_widget(Paragraph::new(shown), body);
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
            .map_or_else(|| "—".to_string(), str::to_string),
        None => "—".to_string(),
    }
}

fn first_line(text: &BoundedText) -> String {
    display_safe(text.preview.lines().next().unwrap_or(""))
}

fn row_body(event: &Event) -> String {
    match (&event.tool, event.kind) {
        (Some(tool), _) => {
            let name = display_safe(&tool.name);
            match tool
                .input
                .as_ref()
                .map(first_line)
                .filter(|l| !l.is_empty())
            {
                Some(input) => format!("{name} {input}"),
                None => name,
            }
        }
        (None, EventKind::Reasoning) => REASONING_LABEL.to_string(),
        (None, _) => match &event.text {
            Some(text) if !text.preview.is_empty() => first_line(text),
            _ => "(no text)".to_string(),
        },
    }
}

/// Whether Enter has more to show for this row.
fn expandable(event: &Event) -> bool {
    if event.tool.is_some() || event.kind == EventKind::Reasoning {
        return true;
    }
    event
        .text
        .as_ref()
        .is_some_and(|t| t.truncated || t.preview.lines().nth(1).is_some())
}

fn event_lines(
    event: &Event,
    expanded: bool,
    selected: bool,
    matched: bool,
    width: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    let suffix = match (expandable(event), expanded) {
        (false, _) => "",
        (true, false) => " [expand]",
        (true, true) => " [collapse]",
    };
    let fixed = MARKER_W + TIME_W + 2 + ACTOR_W + suffix.chars().count();
    let body = fit(&row_body(event), width.saturating_sub(fixed));
    let text = format!(
        "{}{}  {}{body}{suffix}",
        match (selected, matched) {
            (true, _) => "> ",
            (false, true) => "* ",
            (false, false) => "  ",
        },
        fit(&clock(event.timestamp), TIME_W),
        fit(actor_label(event), ACTOR_W),
    );
    let head = Line::from(fit(&text, width));
    let mut lines = vec![if selected {
        head.style(palette.selected())
    } else {
        head
    }];
    if expanded {
        let mut detail = detail_lines(event);
        let overflow = detail.len() > MAX_DETAIL_LINES;
        detail.truncate(MAX_DETAIL_LINES);
        if overflow {
            detail.push("… more in Details".to_string());
        }
        lines.extend(
            detail
                .into_iter()
                .map(|l| Line::styled(fit(&format!("      {l}"), width), palette.dim())),
        );
    }
    lines
}

pub(super) fn detail_lines(event: &Event) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(tool) = &event.tool {
        out.push(tool_summary(tool));
        push_block(&mut out, "Input", tool.input.as_ref());
        match (&tool.output, tool.status) {
            (None, ToolStatus::Pending) => out.push("Output: pending (no result yet)".into()),
            (None, _) => out.push("Output: none recorded".into()),
            (Some(_), _) => push_block(&mut out, "Output", tool.output.as_ref()),
        }
        if tool.orphan {
            out.push("Note: call and result could not be paired".into());
        }
    } else {
        if event.kind == EventKind::Reasoning {
            out.push(format!("{REASONING_LABEL}; not hidden chain-of-thought"));
        }
        push_block(&mut out, "Text", event.text.as_ref());
    }
    if let Some(usage) = &event.usage {
        out.push(format!(
            "Usage ({}): {}",
            match usage.scope {
                UsageScope::Session => "session",
                UsageScope::Turn => "turn",
                UsageScope::Message => "message",
            },
            super::session::categories_text(&usage.categories)
        ));
    }
    out
}

fn tool_summary(tool: &ToolInteraction) -> String {
    let status = match tool.status {
        ToolStatus::Pending => "Pending",
        ToolStatus::Completed => "Completed",
        ToolStatus::Error => "Error",
        ToolStatus::Unknown => "Unknown",
    };
    let duration = match (tool.started_at, tool.completed_at) {
        (Some(a), Some(b)) if b >= a => duration_text(b - a),
        _ => "—".to_string(),
    };
    let exit = tool
        .exit_code
        .map_or_else(|| "—".to_string(), |c| c.to_string());
    format!("Status: {status}   Duration: {duration}   Exit: {exit}")
}

fn duration_text(ms: i64) -> String {
    match ms {
        0..=999 => format!("{ms}ms"),
        1000..=59_999 => format!("{:.1}s", ms as f64 / 1000.0),
        _ => format!("{}m{:02}s", ms / 60_000, (ms / 1000) % 60),
    }
}

fn push_block(out: &mut Vec<String>, label: &str, text: Option<&BoundedText>) {
    let Some(text) = text else {
        out.push(format!("{label}: —"));
        return;
    };
    let mut lines = text.preview.lines();
    out.push(format!(
        "{label}: {}",
        display_safe(lines.next().unwrap_or(""))
    ));
    out.extend(lines.map(|l| format!("  {}", display_safe(l))));
    if text.truncated {
        out.push(format!(
            "  [truncated: showing {} of {} bytes]",
            text.preview.len(),
            text.byte_len
        ));
    }
}
