//! Roster preview: a pinned mini-dashboard for the selected session, drawn
//! beside the list or in its place on a narrow terminal. Pure: reads `App`,
//! draws into a `Frame`. The header never scrolls; the sections below it do.
//!
//! Every figure comes from the shared typed helpers in `session.rs`, so the
//! preview and the details view never price, time or rate anything separately.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use unicode_width::UnicodeWidthStr;

use crate::app::{ActivityState, App, LastActivity};
use crate::domain::{
    AggregationClass, SessionKey, SessionLink, SessionSummary, Status, Timestamp, TokenCategories,
};
use crate::ingest::text::display_safe;
use crate::time::{format_local, relative_label};

use super::RosterContext;
use super::details::wrap_into;
use super::session::{Fact, cost_fact, speed_fact, tokens_fact};
use super::widgets::{
    ABSENT, LOADING, Palette, READ_FAILED, agent_tag, display_title, fit, one_line, status_chip,
    tokens_text, tool_glyph,
};

/// Draw the preview into `area`. Beside the list a left rule separates it.
pub fn render_preview(frame: &mut Frame, app: &App, ctx: &RosterContext, area: Rect) {
    let palette = Palette::new(ctx.no_color);
    let inner = if app.preview_beside_list() {
        let block = Block::default().borders(Borders::LEFT);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        inner
    } else {
        area
    };
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let width = inner.width as usize;
    let Some(key) = app.selected_key() else {
        app.set_preview_extent(0);
        render_notice(frame, inner, "No session selected.");
        return;
    };
    let Some(row) = app.session(&key) else {
        app.set_preview_extent(0);
        render_notice(frame, inner, "Session is no longer in the roster.");
        return;
    };

    let facts_failed = app.preview_hydration_failed(&key);
    let pinned = pinned_lines(
        app,
        row.session,
        &row.status,
        row.hydrated,
        facts_failed,
        width,
        palette,
    );
    let scrolling = scrolling_rows(
        app,
        row.session,
        row.hydrated,
        facts_failed,
        &key,
        width,
        palette,
    );

    let pinned_height = pinned.len().min(inner.height as usize);
    if pinned_height > 0 {
        frame.render_widget(
            Paragraph::new(pinned.into_iter().take(pinned_height).collect::<Vec<_>>()),
            Rect::new(inner.x, inner.y, inner.width, pinned_height as u16),
        );
    }

    let body = Rect::new(
        inner.x,
        inner.y + pinned_height as u16,
        inner.width,
        inner.height - pinned_height as u16,
    );
    let height = body.height as usize;
    let max_scroll = scrolling.len().saturating_sub(height);
    app.set_preview_extent(max_scroll);
    if height == 0 {
        return;
    }
    let start = app.preview_scroll().min(max_scroll);
    let shown: Vec<Line<'static>> = scrolling.into_iter().skip(start).take(height).collect();
    frame.render_widget(Paragraph::new(shown), body);
}

fn render_notice(frame: &mut Frame, inner: Rect, text: &str) {
    let width = inner.width as usize;
    frame.render_widget(
        Paragraph::new(Line::raw(fit(text, width))),
        Rect::new(inner.x, inner.y, inner.width, 1),
    );
}

/// The block above the scroll region: identity, values, and the token bar.
fn pinned_lines(
    app: &App,
    session: &SessionSummary,
    status: &Status,
    hydrated: bool,
    facts_failed: bool,
    width: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    let meta = &session.meta;
    let mut lines = Vec::with_capacity(6);

    // Agent tag and bold title.
    let tag = agent_tag(meta.key.harness, palette);
    let tag_width = UnicodeWidthStr::width(tag.content.as_ref());
    let title_width = width.saturating_sub(tag_width + 1);
    let mut title = vec![tag];
    if title_width > 0 {
        title.push(Span::raw(" "));
        title.push(Span::styled(
            fit(&display_title(meta), title_width),
            palette.title(),
        ));
    }
    lines.push(Line::from(title));

    // Quiet `project · model · effort · provider` plus the parent fragment.
    let mut parts: Vec<(String, Style)> = Vec::new();
    if let Some(project) = meta.project.as_deref() {
        field(&mut parts, project, palette.quiet(), palette);
    }
    if let Some(model) = meta.model.as_ref() {
        field(&mut parts, &model.id, palette.quiet(), palette);
    }
    if let Some(effort) = meta.model.as_ref().and_then(|m| m.effort.as_deref()) {
        field(&mut parts, effort, palette.quiet(), palette);
    }
    if let Some(provider) = meta.model.as_ref().and_then(|m| m.provider.as_deref()) {
        field(&mut parts, provider, palette.quiet(), palette);
    }
    match &meta.lineage.parent {
        Some(SessionLink::Resolved(key)) => {
            if let Some(parent) = app.session(key) {
                field(
                    &mut parts,
                    &format!("child of {}", display_title(&parent.session.meta)),
                    palette.quiet(),
                    palette,
                );
            }
        }
        Some(SessionLink::Ambiguous) => {
            field(&mut parts, "parent ambiguous", palette.quiet(), palette);
        }
        Some(SessionLink::Unresolved(_)) => {
            field(&mut parts, "parent not found", palette.quiet(), palette);
        }
        None => {}
    }
    if parts.is_empty() {
        parts.push((ABSENT.into(), palette.quiet()));
    }
    lines.push(styled_row(parts, width));

    // Status chip and its evidence.
    let age = status_age(session, app.now());
    let chip = status_chip(status.label, age.as_deref(), palette);
    lines.push(styled_row(
        vec![
            (chip.content.into_owned(), chip.style),
            ("  ".into(), Style::default()),
            (one_line(&status.evidence), palette.quiet()),
        ],
        width,
    ));

    // Tokens with provenance, cost, and speed. A failed hydration is not
    // "still loading": cached values stay, and otherwise the facts say so.
    let failed = facts_failed && !hydrated;
    let tokens = if failed {
        Fact::failed()
    } else {
        tokens_fact(session.usage.as_ref(), hydrated)
    };
    let cost = if failed {
        Fact::failed()
    } else {
        cost_fact(session.usage.as_ref(), hydrated)
    };
    let speed = if failed {
        Fact::failed()
    } else {
        speed_fact(session, hydrated, app.now())
    };
    let mut numbers = vec![(tokens.head.clone(), fact_style(&tokens, palette))];
    if matches!(tokens.sub.as_str(), "derived" | "reported") {
        numbers.push((" ".into(), Style::default()));
        numbers.push((tokens.sub.clone(), palette.quiet()));
    }
    numbers.push(("  ".into(), Style::default()));
    numbers.push((cost.head.clone(), fact_style(&cost, palette)));
    numbers.push(("  ".into(), Style::default()));
    let speed_text = if numeric(&speed.head) {
        format!("{} tok/s", speed.head)
    } else {
        speed.head.clone()
    };
    numbers.push((speed_text, fact_style(&speed, palette)));
    lines.push(styled_row(numbers, width));

    // A rate measured on another model says which one, quietly, right below.
    if speed.quiet && !speed.sub.is_empty() {
        lines.push(styled_row(
            vec![(one_line(&speed.sub), palette.quiet())],
            width,
        ));
    }
    if facts_failed {
        lines.push(styled_row(
            vec![(
                if hydrated {
                    "couldn't refresh session facts"
                } else {
                    "couldn't read session facts"
                }
                .into(),
                palette.quiet(),
            )],
            width,
        ));
    }

    // One-line bar and the top-three legend beside it.
    if hydrated && let Some(usage) = session.usage.as_ref() {
        let shares = token_shares(&usage.categories);
        if shares.iter().any(|share| share.value > 0) {
            lines.push(bar_line(&shares, width, palette));
            lines.push(legend_line(&shares, width, palette));
        }
    }
    lines
}

/// The scrollable block. `set_preview_extent` counts these rows only.
fn scrolling_rows(
    app: &App,
    session: &SessionSummary,
    hydrated: bool,
    facts_failed: bool,
    key: &SessionKey,
    width: usize,
    palette: Palette,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();

    rows.push(ScrollRow::Section("CONTEXT"));
    let mut context = Vec::new();
    wrap_into(
        &mut context,
        &context_text(session, hydrated, facts_failed),
        width,
    );
    rows.extend(context.into_iter().map(ScrollRow::Text));

    rows.push(ScrollRow::Section("STARTED"));
    let started = session
        .metrics
        .timing
        .started_at
        .or(session.meta.created_at)
        .map_or_else(|| ABSENT.to_string(), format_local);
    let mut started_rows = Vec::new();
    wrap_into(&mut started_rows, &started, width);
    rows.extend(started_rows.into_iter().map(ScrollRow::Text));

    rows.push(ScrollRow::Section("LAST ACTIVITY"));
    activity_rows(&mut rows, app.activity(key), width);

    rows.push(ScrollRow::Quiet("Enter open".into()));
    rows.into_iter()
        .map(|row| match row {
            ScrollRow::Section(text) => Line::styled(text, palette.section()),
            ScrollRow::Text(text) => Line::raw(text),
            ScrollRow::Quiet(text) => Line::styled(text, palette.quiet()),
        })
        .collect()
}

enum ScrollRow {
    Section(&'static str),
    Text(String),
    Quiet(String),
}

fn activity_rows(rows: &mut Vec<ScrollRow>, state: Option<&ActivityState>, width: usize) {
    match state {
        None | Some(ActivityState::Loading) => rows.push(ScrollRow::Quiet(LOADING.into())),
        Some(ActivityState::Nothing) => {
            rows.push(ScrollRow::Quiet("no activity recorded".into()));
        }
        Some(ActivityState::Failed) => {
            rows.push(ScrollRow::Quiet("couldn't read latest activity".into()));
        }
        Some(ActivityState::Ready(last)) => ready_rows(rows, last, width),
    }
}

fn ready_rows(rows: &mut Vec<ScrollRow>, last: &LastActivity, width: usize) {
    if let Some(tool) = &last.tool {
        let mut text = format!("{} {}", tool_glyph(tool.status), one_line(&tool.name));
        let summary = one_line(&tool.summary);
        if !summary.is_empty() {
            text.push(' ');
            text.push_str(&summary);
        }
        let mut wrapped = Vec::new();
        wrap_into(&mut wrapped, &text, width);
        rows.extend(wrapped.into_iter().map(ScrollRow::Text));
    }
    match &last.reply {
        Some(reply) => {
            let text = display_safe(&reply.preview);
            let mut wrapped = Vec::new();
            wrap_into(&mut wrapped, &text, width.saturating_sub(2).max(1));
            for line in wrapped.into_iter().take(3) {
                rows.push(ScrollRow::Text(format!("▌ {line}")));
            }
        }
        None => rows.push(ScrollRow::Quiet("no reply in recent events".into())),
    }
}

/// Recorded window occupancy. A missing window has no percentage, and the
/// preview never falls back to the word `Unknown`. A failed hydration is a
/// read failure, not an unfinished load.
fn context_text(session: &SessionSummary, hydrated: bool, facts_failed: bool) -> String {
    if facts_failed && !hydrated {
        return READ_FAILED.into();
    }
    if !hydrated {
        return LOADING.into();
    }
    let context = &session.metrics.context;
    match (context.used, context.capacity) {
        (Some(used), Some(window)) if window > 0 => format!(
            "{}% of window · {} of {}",
            used.saturating_mul(100) / window,
            tokens_text(Some(used)),
            tokens_text(Some(window)),
        ),
        (Some(used), _) => format!("{} recorded · window not recorded", tokens_text(Some(used))),
        _ => ABSENT.into(),
    }
}

pub(super) fn status_age(session: &SessionSummary, now: Timestamp) -> Option<String> {
    let at = [
        session.meta.activity.at,
        session.last_event_at,
        session.last_completion,
    ]
    .into_iter()
    .flatten()
    .max()?;
    let approximate = session.meta.activity.at == Some(at)
        && session.meta.activity.class == AggregationClass::Derived;
    Some(format!(
        "{}{}",
        if approximate { "~" } else { "" },
        relative_label(at, now)
    ))
}

fn field(parts: &mut Vec<(String, Style)>, text: &str, style: Style, palette: Palette) {
    let text = one_line(text);
    if text.is_empty() {
        return;
    }
    separator(parts, palette);
    parts.push((text, style));
}

fn separator(parts: &mut Vec<(String, Style)>, palette: Palette) {
    if !parts.is_empty() {
        parts.push((" · ".into(), palette.quiet()));
    }
}

fn fact_style(fact: &Fact, palette: Palette) -> Style {
    if fact.quiet || matches!(fact.head.as_str(), ABSENT | LOADING) {
        palette.quiet()
    } else {
        palette.value()
    }
}

fn numeric(head: &str) -> bool {
    head.chars().next().is_some_and(|ch| ch.is_ascii_digit())
}

/// Fit styled parts into `width` columns, truncating the one that overflows.
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

/// One category of the token bar, in fixed order.
struct Share {
    label: &'static str,
    value: u64,
    color: Color,
    plain: char,
}

fn token_shares(categories: &TokenCategories) -> [Share; 4] {
    let split_write = categories
        .cache_write_5m
        .unwrap_or(0)
        .saturating_add(categories.cache_write_1h.unwrap_or(0));
    let cache_write = categories
        .cache_write
        .filter(|write| *write > 0)
        .unwrap_or(split_write);
    [
        Share {
            label: "cache read",
            value: categories.cache_read.unwrap_or(0),
            color: Color::Blue,
            plain: '#',
        },
        Share {
            label: "input",
            value: categories.input.unwrap_or(0),
            color: Color::Green,
            plain: '=',
        },
        Share {
            label: "output",
            value: categories.output.unwrap_or(0),
            color: Color::Yellow,
            plain: '-',
        },
        Share {
            label: "cache write",
            value: cache_write,
            color: Color::Magenta,
            plain: '+',
        },
    ]
}

fn bar_line(shares: &[Share; 4], width: usize, palette: Palette) -> Line<'static> {
    let widths = bar_widths(shares, width);
    let mut spans = Vec::new();
    let mut used = 0;
    for (share, cells) in shares.iter().zip(widths) {
        if cells == 0 {
            continue;
        }
        used += cells;
        let (glyph, style) = if palette.is_no_color() {
            (share.plain, Style::default())
        } else {
            ('█', Style::default().fg(share.color))
        };
        spans.push(Span::styled(
            std::iter::repeat_n(glyph, cells).collect::<String>(),
            style,
        ));
    }
    if used < width {
        spans.push(Span::raw(" ".repeat(width - used)));
    }
    Line::from(spans)
}

/// Exact proportional widths; every category with a value keeps one column
/// when the bar is wide enough, and the largest categories win a too-narrow bar.
fn bar_widths(shares: &[Share; 4], width: usize) -> [usize; 4] {
    let mut out = [0usize; 4];
    let active: Vec<usize> = (0..4).filter(|&i| shares[i].value > 0).collect();
    if active.is_empty() || width == 0 {
        return out;
    }
    if width < active.len() {
        let mut order = active;
        order.sort_by(|&a, &b| shares[b].value.cmp(&shares[a].value).then(a.cmp(&b)));
        for &i in order.iter().take(width) {
            out[i] = 1;
        }
        return out;
    }
    for &i in &active {
        out[i] = 1;
    }
    let remaining = width - active.len();
    let total: u128 = active.iter().map(|&i| shares[i].value as u128).sum();
    let mut used = 0;
    let mut fractions = Vec::with_capacity(active.len());
    for &i in &active {
        let exact = shares[i].value as u128 * remaining as u128;
        let cells = (exact / total) as usize;
        out[i] += cells;
        used += cells;
        fractions.push((i, exact % total));
    }
    let mut leftover = remaining - used;
    fractions.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (i, _) in fractions.iter().cycle() {
        if leftover == 0 {
            break;
        }
        out[*i] += 1;
        leftover -= 1;
    }
    out
}

/// A stacked token bar over the recorded categories, shared with the session
/// dashboard so both surfaces use one visual grammar.
pub(super) fn token_bar(
    categories: &TokenCategories,
    width: usize,
    palette: Palette,
) -> Line<'static> {
    bar_line(&token_shares(categories), width, palette)
}

fn legend_line(shares: &[Share; 4], width: usize, palette: Palette) -> Line<'static> {
    let mut order: Vec<usize> = (0..4).filter(|&i| shares[i].value > 0).collect();
    order.sort_by(|&a, &b| shares[b].value.cmp(&shares[a].value).then(a.cmp(&b)));
    let mut parts: Vec<(String, Style)> = Vec::new();
    for (rank, &i) in order.iter().take(3).enumerate() {
        if rank > 0 {
            parts.push(("  ".into(), Style::default()));
        }
        let share = &shares[i];
        let (glyph, style) = if palette.is_no_color() {
            (share.plain, Style::default())
        } else {
            ('█', Style::default().fg(share.color))
        };
        parts.push((glyph.to_string(), style));
        parts.push((format!(" {} ", share.label), palette.quiet()));
        parts.push((tokens_text(Some(share.value)), palette.value()));
    }
    styled_row(parts, width)
}
