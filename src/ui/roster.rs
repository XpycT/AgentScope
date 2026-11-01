//! Roster screen rendering (spec §10.2, §10.6, §10.7). Pure: reads `App` and a
//! small context, draws into a `Frame`. No I/O.
//!
//! The roster is project-grouped. A group heading is one line; every session
//! is two physical lines: the primary line (marker, status, fold marker, title
//! and the right-aligned value columns) and a quiet metadata line (agent tag,
//! model, thinking level, billing provider). Children use the same two lines
//! with `├`/`└` tree prefixes.
//!
//! Narrow terminals drop value columns from the right in a fixed order (cost,
//! speed, tokens); the status column always stays. There is no horizontal
//! column scrolling.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{
    App, FilterState, LoadFocus, PREVIEW_SPLIT_WIDTH, Panel, RosterItem, RosterSession, Screen,
    SortMode, UsageAvailability,
};
use crate::domain::{AggregationClass, SessionKey, Timestamp};
use crate::ingest::text::display_safe;
use crate::time::relative_label;

use super::RosterContext;
use super::inspector::money_text;
use super::preview::render_preview;
use super::session::{cost_fact, speed_fact, tokens_fact};
use super::widgets::{
    ABSENT, Palette, agent_tag, display_title, effort_span, fit, harness_name, one_line,
    search_prompt, status_text, status_word, tokens_text,
};

pub const MIN_WIDTH: u16 = 60;
pub const MIN_HEIGHT: u16 = 20;
/// Columns reserved for the side preview at [`PREVIEW_SPLIT_WIDTH`] and wider.
pub const PREVIEW_WIDTH: u16 = 45;

/// Pinned selection/expansion marker and its trailing space.
const MARKER_W: u16 = 2;
/// Physical lines of a session or Load more row; a heading costs one.
const ROW_HEIGHT: u16 = 2;
/// Space between two value columns.
const COLUMN_GAP: u16 = 1;
/// Width left of the value columns that must remain for the marker, status,
/// fold marker and a readable title. Columns that would eat into it are
/// dropped in [`Column::DROP_ORDER`].
const MIN_LEFT: u16 = 24;
/// Wheel notches move the preview this many lines.
const WHEEL_LINES: isize = 3;

/// A right-aligned value column on the first line of a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    Status,
    Speed,
    Tokens,
    Cost,
}

impl Column {
    pub const ALL: [Column; 4] = [Column::Status, Column::Speed, Column::Tokens, Column::Cost];

    /// Order in which narrow terminals give columns up; the status stays.
    pub const DROP_ORDER: [Column; 3] = [Column::Cost, Column::Speed, Column::Tokens];

    fn width(self) -> u16 {
        match self {
            Column::Status => 12,
            Column::Speed => 7,
            Column::Tokens => 9,
            Column::Cost => 9,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Column::Status => "STATUS",
            Column::Speed => "SPEED",
            Column::Tokens => "TOKENS",
            Column::Cost => "COST",
        }
    }
}

/// One displayed value column: where it sits on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnBox {
    pub column: Column,
    pub x: u16,
    pub width: u16,
}

/// Geometry of the roster screen for one terminal size. The render and the
/// mouse both use it, so a click can only land on what is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RosterLayout {
    pub area: Rect,
    /// Column header line of the list.
    pub header: Rect,
    /// Rows region of the list; items are laid out by their own heights.
    pub rows: Rect,
    /// Preview column beside the list (terminal at least [`PREVIEW_SPLIT_WIDTH`] wide).
    pub preview: Option<Rect>,
    /// Header and rows across the whole width: where a swapped-in preview is drawn.
    pub swapped: Rect,
    pub session_x: u16,
    pub session_width: u16,
    /// Value columns kept at this width, right-aligned to the list edge.
    pub columns: Vec<ColumnBox>,
}

/// Vertical bands: top line, warnings line, table header, body, rule, footer.
fn bands(area: Rect) -> [Rect; 6] {
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

/// Total width of `columns`, including the gaps between them.
fn columns_width(columns: &[Column]) -> u16 {
    let widths: u16 = columns.iter().map(|column| column.width()).sum();
    widths + COLUMN_GAP * columns.len().saturating_sub(1) as u16
}

/// Right-aligned boxes for the columns that fit: keep the status always, drop
/// cost, then speed, then tokens until the session area has its minimum width.
fn column_boxes(rows: Rect) -> Vec<ColumnBox> {
    let mut shown: Vec<Column> = Column::ALL.to_vec();
    while shown.len() > 1 && rows.width < columns_width(&shown) + MIN_LEFT {
        let Some(at) = Column::DROP_ORDER
            .iter()
            .find_map(|drop| shown.iter().position(|column| column == drop))
        else {
            break;
        };
        shown.remove(at);
    }
    let mut x = rows.right().saturating_sub(columns_width(&shown));
    let mut boxes = Vec::with_capacity(shown.len());
    for column in shown {
        boxes.push(ColumnBox {
            column,
            x,
            width: column.width(),
        });
        x = x.saturating_add(column.width() + COLUMN_GAP);
    }
    boxes
}

impl RosterLayout {
    /// Geometry for `area`; below the minimum size everything is hidden.
    pub fn for_area(area: Rect) -> Self {
        let hidden = Self {
            area,
            header: Rect::ZERO,
            rows: Rect::ZERO,
            preview: None,
            swapped: Rect::ZERO,
            session_x: 0,
            session_width: 0,
            columns: Vec::new(),
        };
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            return hidden;
        }
        let [_, _, table_head, body, _, _] = bands(area);
        let side = area.width >= PREVIEW_SPLIT_WIDTH;
        let list_w = if side {
            area.width - PREVIEW_WIDTH
        } else {
            area.width
        };
        let header = Rect::new(area.x, table_head.y, list_w, 1);
        let rows = Rect::new(area.x, body.y, list_w, body.height);
        let preview = side.then(|| {
            Rect::new(
                area.x + list_w,
                table_head.y,
                PREVIEW_WIDTH,
                body.height + 1,
            )
        });
        let swapped = Rect::new(area.x, table_head.y, area.width, body.height + 1);
        let session_x = area.x + MARKER_W;
        let columns = column_boxes(rows);
        let session_width = columns
            .first()
            .map_or(rows.width, |first| first.x.saturating_sub(session_x));
        Self {
            area,
            header,
            rows,
            preview,
            swapped,
            session_x,
            session_width,
            columns,
        }
    }

    /// The terminal is large enough to draw the roster.
    pub fn usable(&self) -> bool {
        self.rows.height >= ROW_HEIGHT
    }

    /// Index of the first item drawn: the focused item's window fits the list.
    pub fn first_visible(&self, app: &App) -> usize {
        let Some(at) = app.focus_visual_index() else {
            return 0;
        };
        let mut total = 0u16;
        let mut count = 0usize;
        for item in app.roster_rows().take(at + 1) {
            total = total.saturating_add(item.height());
            count += 1;
        }
        if count <= at {
            return 0;
        }
        let mut first = 0;
        for item in app.roster_rows() {
            if first >= at || total <= self.rows.height {
                break;
            }
            total = total.saturating_sub(item.height());
            first += 1;
        }
        first
    }

    /// Value column displayed under screen column `x`.
    pub fn column_at(&self, x: u16) -> Option<Column> {
        self.columns
            .iter()
            .find(|column| x >= column.x && x < column.x + column.width)
            .map(|column| column.column)
    }

    /// Every item the renderer draws, with its screen rect and its index in
    /// `roster_rows()`. Render and mouse share this mapping, including the
    /// full-item check that keeps a partly drawn row out of both.
    pub fn slots(&self, app: &App) -> Vec<(Rect, usize)> {
        let mut slots = Vec::new();
        if !self.usable() {
            return slots;
        }
        let first = self.first_visible(app);
        let mut y = self.rows.y;
        for (index, item) in app.roster_rows().enumerate().skip(first) {
            let height = item.height();
            if y.saturating_add(height) > self.rows.bottom() {
                break;
            }
            slots.push((Rect::new(self.rows.x, y, self.rows.width, height), index));
            y += height;
        }
        slots
    }

    /// The preview's current screen area: beside the list, or swapped over it.
    fn preview_area(&self, app: &App) -> Option<Rect> {
        if !self.usable() {
            return None;
        }
        self.preview
            .or_else(|| app.preview_replaces_list().then_some(self.swapped))
    }
}

fn contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

/// What a click or wheel notch landed on.
enum Hit {
    Session(SessionKey),
    /// The selection strip or the displayed fold glyph: select, then fold.
    Fold(SessionKey),
    Roots,
    Children(SessionKey),
}

/// Apply one mouse event. Read-only: selecting, expanding, loading more and
/// scrolling only. Events that carry Shift belong to the terminal's own
/// selection and scrolling and are ignored. Hits are tested against `layout`,
/// never against stale rows, and only on the roster screen.
pub fn handle_mouse(app: &mut App, event: MouseEvent, layout: &RosterLayout) {
    if event.modifiers.contains(KeyModifiers::SHIFT)
        || app.screen != Screen::Roster
        || app.panel() != Panel::None
        || app.is_editing()
        || !layout.usable()
    {
        return;
    }
    let (x, y) = (event.column, event.row);
    match event.kind {
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            let direction = if event.kind == MouseEventKind::ScrollUp {
                -1
            } else {
                1
            };
            if layout
                .preview_area(app)
                .is_some_and(|area| contains(area, x, y))
            {
                app.scroll_preview(direction * WHEEL_LINES);
            } else if !app.preview_replaces_list() && contains(layout.rows, x, y) {
                app.move_focus(direction);
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if app.preview_replaces_list() {
                return;
            }
            let Some((slot, index)) = layout
                .slots(app)
                .into_iter()
                .find(|(rect, _)| contains(*rect, x, y))
            else {
                return;
            };
            // The displayed fold glyph lives on the first physical line; the
            // second line is metadata. The legacy selection strip stays a
            // fold target on both lines.
            let first_line = y == slot.y;
            let hit = app.roster_rows().nth(index).and_then(|item| match item {
                RosterItem::Session(row) => {
                    let on_fold = first_line
                        && fold_control(layout, &row)
                            .is_some_and(|(start, end)| x >= start && x < end);
                    if on_fold || x < layout.rows.x + MARKER_W {
                        Some(Hit::Fold(row.session.meta.key.clone()))
                    } else {
                        Some(Hit::Session(row.session.meta.key.clone()))
                    }
                }
                RosterItem::RootLoadMore => Some(Hit::Roots),
                RosterItem::ChildLoadMore { parent, .. } => Some(Hit::Children(parent.clone())),
                // A project heading selects nothing.
                RosterItem::GroupHeader(_) => None,
            });
            match hit {
                Some(Hit::Session(key)) => {
                    app.select_row(&key);
                }
                Some(Hit::Fold(key)) => {
                    if app.select_row(&key) {
                        app.toggle_children();
                    }
                }
                Some(Hit::Roots) => {
                    app.focus_load_more(LoadFocus::Roots);
                    app.activate_load_focus();
                }
                Some(Hit::Children(parent)) => {
                    app.focus_load_more(LoadFocus::Children(parent));
                    app.activate_load_focus();
                }
                None => {}
            }
        }
        _ => {}
    }
}

pub fn render_roster(frame: &mut Frame, app: &App, ctx: &RosterContext) {
    let area = frame.area();
    let palette = Palette::new(ctx.no_color);
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        let msg = format!(
            "Terminal too small: {}x{} (need at least {MIN_WIDTH}x{MIN_HEIGHT}). Resize to continue; q quits.",
            area.width, area.height
        );
        frame.render_widget(Paragraph::new(msg).wrap(Wrap { trim: true }), area);
        return;
    }

    let layout = RosterLayout::for_area(area);
    let total = app.total_sessions();
    let [top, notes_area, table_head, _, rule2, footer] = bands(area);

    frame.render_widget(
        Paragraph::new(top_line(app, ctx, palette, area.width as usize)),
        top,
    );
    let notes = status_notes(app, ctx);
    if notes.is_empty() {
        frame.render_widget(Paragraph::new("─".repeat(area.width as usize)), notes_area);
    } else {
        frame.render_widget(Paragraph::new(Line::from(notes)), notes_area);
    }
    let swapped = layout.preview.is_none() && app.preview_replaces_list();
    let header_line = if swapped {
        Line::styled(
            fit(
                "PREVIEW  Tab list  j/k scroll  Enter open",
                layout.header.width as usize,
            ),
            palette.title(),
        )
    } else {
        table_header(&layout, palette)
    };
    frame.render_widget(Paragraph::new(header_line), table_head);
    frame.render_widget(Paragraph::new("─".repeat(area.width as usize)), rule2);

    let pane = if area.width >= PREVIEW_SPLIT_WIDTH {
        "open"
    } else {
        "preview"
    };
    let (footer_text, footer_style) = match app.search_input() {
        Some((_, text)) => (search_prompt(&display_safe(text)), palette.title()),
        None => (
            format!(
                "Enter open  ←/→ fold  Tab {pane}  / search  f filter  s sort  r refresh  m more  p procs  ? help  q quit"
            ),
            palette.dim(),
        ),
    };
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(&footer_text, area.width as usize),
            footer_style,
        )),
        footer,
    );

    let has_rows = app.has_visible_rows();
    let body = Rect::new(area.x, layout.rows.y, area.width, layout.rows.height);
    if total == 0 {
        frame.render_widget(
            Paragraph::new(empty_lines(app, ctx, palette)).wrap(Wrap { trim: false }),
            body,
        );
    } else if swapped {
        render_preview(frame, app, ctx, body);
    } else {
        if !has_rows {
            frame.render_widget(
                Paragraph::new("No sessions match the current filters.").wrap(Wrap { trim: true }),
                layout.rows,
            );
        } else {
            let selection = if ctx.no_color {
                palette.selected()
            } else {
                palette.selection_bg()
            };
            render_rows(frame, app, &layout, palette, selection);
        }
        if let Some(preview) = layout.preview {
            render_preview(frame, app, ctx, preview);
        }
    }
}

fn render_rows(
    frame: &mut Frame,
    app: &App,
    layout: &RosterLayout,
    palette: Palette,
    selection: Style,
) {
    let slots = layout.slots(app);
    let (Some(&(_, first)), Some(&(_, last))) = (slots.first(), slots.last()) else {
        return;
    };
    // Only the drawn slice is borrowed and formatted; unshown rows cost nothing.
    let items: Vec<RosterItem<'_>> = app
        .roster_rows()
        .skip(first)
        .take(last - first + 1)
        .collect();
    let load_focus = app.load_focus().and(app.focus_visual_index());
    let now = app.now();
    for (position, (rect, index)) in slots.iter().enumerate() {
        let Some(item) = items.get(index - first) else {
            continue;
        };
        match item {
            RosterItem::GroupHeader(header) => frame.render_widget(
                Paragraph::new(group_header_line(*header, layout, palette)),
                *rect,
            ),
            RosterItem::Session(row) => {
                let highlighted = row.selected && load_focus.is_none();
                let has_next = match items.get(position + 1) {
                    Some(RosterItem::Session(next)) => next.depth == 1,
                    Some(RosterItem::ChildLoadMore { .. }) => true,
                    Some(_) => false,
                    None => matches!(
                        app.roster_rows().nth(index + 1),
                        Some(RosterItem::Session(next)) if next.depth == 1
                    ),
                };
                frame.render_widget(
                    Paragraph::new(
                        row_lines(row, layout, now, highlighted, !has_next, palette, selection)
                            .to_vec(),
                    ),
                    *rect,
                );
            }
            RosterItem::RootLoadMore => frame.render_widget(
                Paragraph::new(
                    load_more_lines(
                        "Load more sessions",
                        "m or Enter loads the next page",
                        0,
                        load_focus == Some(*index),
                        palette,
                    )
                    .to_vec(),
                ),
                *rect,
            ),
            RosterItem::ChildLoadMore { depth, .. } => frame.render_widget(
                Paragraph::new(
                    load_more_lines(
                        "Load more children",
                        "Enter loads the next page",
                        *depth as usize,
                        load_focus == Some(*index),
                        palette,
                    )
                    .to_vec(),
                ),
                *rect,
            ),
        }
    }
}

/// An explicit Load more row: it is a keyboard- and mouse-focusable action,
/// never a session.
fn load_more_lines(
    label: &str,
    hint: &str,
    depth: usize,
    focused: bool,
    palette: Palette,
) -> [Line<'static>; 2] {
    let indent = "  ".repeat(depth + 1);
    let marker = if focused { "> " } else { "  " };
    let first = Line::from(format!("{marker}{}{label}", indent.get(2..).unwrap_or("")));
    let second = Line::styled(format!("  {indent}{hint}"), palette.dim());
    if focused {
        [
            first.style(palette.selected()),
            second.style(palette.selected()),
        ]
    } else {
        [first, second]
    }
}

/// Append a styled fragment and track the physical width drawn so far.
fn push(spans: &mut Vec<Span<'static>>, used: &mut usize, text: impl Into<String>, style: Style) {
    let text = text.into();
    *used += UnicodeWidthStr::width(text.as_str());
    spans.push(Span::styled(text, style));
}

/// Append a pre-built span (an agent tag) and track its width.
fn push_span(spans: &mut Vec<Span<'static>>, used: &mut usize, span: Span<'static>) {
    *used += UnicodeWidthStr::width(span.content.as_ref());
    spans.push(span);
}

/// Left-pad `text` to `width` columns; a value wider than its column is
/// truncated with the shared visible marker.
fn fit_right(text: &str, width: usize) -> String {
    let width_used = UnicodeWidthStr::width(text);
    if width_used > width {
        fit(text, width)
    } else {
        format!("{}{text}", " ".repeat(width - width_used))
    }
}

/// Pad a line with unstyled spaces up to the list width. The selected-row
/// style is patched on afterwards, so the whole row carries the background.
fn pad_to(spans: &mut Vec<Span<'static>>, used: &mut usize, width: usize) {
    if *used < width {
        spans.push(Span::raw(" ".repeat(width - *used)));
        *used = width;
    }
}

/// Status word and age for the STATUS column. A mtime-derived activity keeps
/// its `~` prefix so it never reads as a recorded event time.
fn status_cell(row: &RosterSession<'_>, now: Timestamp) -> String {
    let word = status_word(row.status.label);
    let activity = [
        row.session.meta.activity.at,
        row.session.last_event_at,
        row.session.last_completion,
    ]
    .into_iter()
    .flatten()
    .max();
    let approximate = activity == row.session.meta.activity.at
        && row.session.meta.activity.class == AggregationClass::Derived;
    match activity {
        Some(at) => format!(
            "{word} {}{}",
            if approximate { "~" } else { "" },
            relative_label(at, now)
        ),
        None => word.to_string(),
    }
}

/// Visible fold text: expanded, collapsed with known children, or a blank cell.
fn fold_text(row: &RosterSession<'_>) -> String {
    if row.expanded {
        "▾".to_string()
    } else if row.child_count > 0 {
        format!("▸ +{}", row.child_count)
    } else {
        " ".to_string()
    }
}

/// First screen column of a session's fold cell: after the marker strip, the
/// tree prefix (`4` columns per depth level), and the status glyph.
fn fold_start(layout: &RosterLayout, depth: u8) -> u16 {
    layout.rows.x + MARKER_W + u16::from(depth) * 4 + 2
}

/// Screen span of the displayed fold control, when there is one. A collapsed
/// family with no loaded children draws a blank cell, which is not a control.
fn fold_control(layout: &RosterLayout, row: &RosterSession<'_>) -> Option<(u16, u16)> {
    if !row.expanded && row.child_count == 0 {
        return None;
    }
    let start = fold_start(layout, row.depth);
    let width = UnicodeWidthStr::width(fold_text(row).as_str()) as u16;
    Some((start, start + width))
}

/// The two physical lines of one session.
fn row_lines(
    row: &RosterSession<'_>,
    layout: &RosterLayout,
    now: Timestamp,
    highlighted: bool,
    is_last_child: bool,
    palette: Palette,
    selection: Style,
) -> [Line<'static>; 2] {
    let s = row.session;
    let (icon, _) = status_text(row.status.label);
    let marker = if highlighted {
        Palette::SELECTION_MARKER
    } else {
        " "
    };
    let (tree_first, tree_second) = if row.depth == 0 {
        ("", "")
    } else if is_last_child {
        ("  └ ", "    ")
    } else {
        ("  ├ ", "  │ ")
    };
    let fold = fold_text(row);
    let speed = speed_fact(s, row.hydrated, now);
    let column_start = layout
        .columns
        .first()
        .map_or(layout.rows.right(), |first| first.x) as usize;
    let width = layout.rows.width as usize;

    // Line 1: marker, tree, status glyph, fold marker, bold title, columns.
    let mut first = Vec::new();
    let mut used = 0;
    push(&mut first, &mut used, marker, Style::default());
    push(&mut first, &mut used, " ", Style::default());
    push(&mut first, &mut used, tree_first, palette.quiet());
    push(
        &mut first,
        &mut used,
        icon,
        palette.status(row.status.label),
    );
    push(&mut first, &mut used, " ", Style::default());
    push(&mut first, &mut used, fold, palette.quiet());
    push(&mut first, &mut used, " ", Style::default());
    let title_width = column_start.saturating_sub(used + 1);
    push(
        &mut first,
        &mut used,
        fit(&display_title(&s.meta), title_width),
        palette.value(),
    );
    for placed in &layout.columns {
        if (placed.x as usize) > used {
            let padding = " ".repeat(placed.x as usize - used);
            push(&mut first, &mut used, padding, Style::default());
        }
        let (text, style) = match placed.column {
            Column::Status => (status_cell(row, now), palette.status(row.status.label)),
            Column::Speed => {
                let style = if speed.quiet {
                    palette.quiet()
                } else {
                    palette.value()
                };
                (speed.head.clone(), style)
            }
            Column::Tokens => (
                tokens_fact(s.usage.as_ref(), row.hydrated).head,
                palette.value(),
            ),
            Column::Cost => (
                cost_fact(s.usage.as_ref(), row.hydrated).head,
                palette.value(),
            ),
        };
        push(
            &mut first,
            &mut used,
            fit_right(&text, placed.width as usize),
            style,
        );
    }
    pad_to(&mut first, &mut used, width);

    // Line 2: marker, tree continuation, agent tag, model, effort, provider.
    let mut second = Vec::new();
    let mut used = 0;
    push(&mut second, &mut used, marker, Style::default());
    push(&mut second, &mut used, " ", Style::default());
    push(&mut second, &mut used, tree_second, palette.quiet());
    push_span(
        &mut second,
        &mut used,
        agent_tag(s.meta.key.harness, palette),
    );
    push(&mut second, &mut used, " ", Style::default());
    let model = s.meta.model.as_ref();
    let model_id = model.map_or_else(|| ABSENT.to_string(), |m| one_line(&m.id));
    push(&mut second, &mut used, model_id, palette.quiet());
    if let Some(effort) = model.and_then(|m| m.effort.as_deref()) {
        push(&mut second, &mut used, " ", Style::default());
        push_span(&mut second, &mut used, effort_span(effort, palette));
    }
    push(&mut second, &mut used, " ", Style::default());
    let provider = model
        .and_then(|m| m.provider.as_deref())
        .map_or_else(|| ABSENT.to_string(), one_line);
    push(&mut second, &mut used, provider, palette.quiet());
    // A rate measured on another model keeps its explanation beside the quiet
    // number; the color cue alone is absent under no_color.
    if speed.quiet && !speed.sub.is_empty() {
        push(&mut second, &mut used, " · ", Style::default());
        push(
            &mut second,
            &mut used,
            one_line(&speed.sub),
            palette.quiet(),
        );
    }
    pad_to(&mut second, &mut used, width);

    let lines = [Line::from(first), Line::from(second)];
    if !highlighted {
        return lines;
    }
    // Patch every span of the selected row: a span's own style (the agent tag
    // background) would otherwise win over the line style and break the band.
    let select = |line: Line<'static>| {
        Line::from(
            line.spans
                .into_iter()
                .map(|span| span.patch_style(selection))
                .collect::<Vec<_>>(),
        )
    };
    lines.map(select)
}

/// Group heading: bold project name, then quiet loaded-member totals.
fn group_header_line(
    header: crate::app::GroupHeader<'_>,
    layout: &RosterLayout,
    palette: Palette,
) -> Line<'static> {
    let total = header.totals;
    let tokens = match total.tokens {
        Some(tokens) => format!(
            "{}{}",
            tokens_text(Some(tokens)),
            if total.tokens_partial { "+" } else { "" }
        ),
        None => ABSENT.to_string(),
    };
    let cost = match total.cost_usd {
        Some(usd) => format!(
            "{}{}{}",
            if total.cost_estimated { "~" } else { "" },
            money_text(usd),
            if total.cost_partial { "+" } else { "" }
        ),
        None => ABSENT.to_string(),
    };
    let summary = format!("{} sessions · {tokens} · {cost}", total.sessions);
    let width = layout.rows.width as usize;
    let summary_width = UnicodeWidthStr::width(summary.as_str());
    let name_width = width.saturating_sub(summary_width + 2);
    let name = header
        .project
        .map_or_else(|| "No project".to_string(), display_safe);
    let mut spans = Vec::new();
    let mut used = 0;
    push(
        &mut spans,
        &mut used,
        fit(&name, name_width),
        palette.title(),
    );
    push(&mut spans, &mut used, "  ", Style::default());
    push(&mut spans, &mut used, summary, palette.quiet());
    pad_to(&mut spans, &mut used, width);
    Line::from(spans)
}

/// Column labels aligned over their value columns.
fn table_header(layout: &RosterLayout, palette: Palette) -> Line<'static> {
    let width = layout.rows.width as usize;
    let mut out = String::new();
    let mut used = 0usize;
    let label_x = layout.session_x as usize;
    if label_x > used {
        out.push_str(&" ".repeat(label_x - used));
        used = label_x;
    }
    out.push_str(&fit("SESSION / PROJECT", layout.session_width as usize));
    used += layout.session_width as usize;
    for placed in &layout.columns {
        let x = placed.x as usize;
        if x > used {
            out.push_str(&" ".repeat(x - used));
            used = x;
        }
        out.push_str(&fit(placed.column.title(), placed.width as usize));
        used += placed.width as usize;
    }
    if used < width {
        out.push_str(&" ".repeat(width - used));
    }
    Line::styled(out, palette.section())
}

/// Top line: identity, counts, live state, sort and active filters, with a
/// right-aligned help hint.
fn top_line(app: &App, ctx: &RosterContext, palette: Palette, width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let mut used = 0;
    push(&mut spans, &mut used, "AgentScope", palette.title());
    push(
        &mut spans,
        &mut used,
        format!(" · {} sessions", app.total_sessions()),
        Style::default(),
    );
    let paused = !app.live || ctx.error.is_some();
    push(&mut spans, &mut used, " · ", palette.quiet());
    push(
        &mut spans,
        &mut used,
        if paused { "Live: paused" } else { "Live: on" },
        if paused {
            palette.warning()
        } else {
            Style::default()
        },
    );
    for (label, count) in app
        .status_counts()
        .into_iter()
        .filter(|(_, count)| *count > 0)
    {
        let (glyph, _) = status_text(label);
        push(&mut spans, &mut used, " · ", palette.quiet());
        push(
            &mut spans,
            &mut used,
            format!("{glyph} {count} {}", status_word(label)),
            palette.status(label),
        );
    }
    push(&mut spans, &mut used, " · ", palette.quiet());
    push(
        &mut spans,
        &mut used,
        format!("sort: {}", sort_name(app.sort_mode())),
        Style::default(),
    );
    if app.filter_active() {
        push(&mut spans, &mut used, " · ", palette.quiet());
        push(
            &mut spans,
            &mut used,
            format!("filters: {}", filters_text(app.filter())),
            Style::default(),
        );
    }
    let hint = "? help";
    let hint_width = UnicodeWidthStr::width(hint);
    if used + 1 + hint_width <= width {
        let padding = " ".repeat(width - used - hint_width);
        push(&mut spans, &mut used, padding, Style::default());
        push(&mut spans, &mut used, hint, palette.quiet());
    }
    Line::from(spans)
}

/// Append one note, separated from the previous one by a quiet middle dot.
fn push_note(spans: &mut Vec<Span<'static>>, text: String, style: Style) {
    if !spans.is_empty() {
        spans.push(Span::styled(" · ", Style::default()));
    }
    spans.push(Span::styled(text, style));
}

/// The warnings line: operational states in quiet style, warnings, errors and
/// discovery limits in the warning style. Empty when a rule should be drawn.
fn status_notes(app: &App, ctx: &RosterContext) -> Vec<Span<'static>> {
    let palette = Palette::new(ctx.no_color);
    let mut spans = Vec::new();
    if let Some(error) = &ctx.error {
        push_note(
            &mut spans,
            format!("Worker stopped: {}; r retries", one_line(error)),
            palette.warning(),
        );
    }
    let status = app.roster_status();
    for scan in status.scan.iter().filter(|scan| !scan.done) {
        let total = scan
            .total
            .map_or(String::new(), |total| format!("/{total}"));
        push_note(
            &mut spans,
            format!(
                "Scanning {} {}{total}",
                harness_name(scan.harness),
                scan.scanned
            ),
            palette.quiet(),
        );
    }
    if status.provisional {
        push_note(
            &mut spans,
            "provisional order (discovery running)".into(),
            palette.quiet(),
        );
    }
    if status.has_more {
        push_note(&mut spans, "more available (m)".into(), palette.quiet());
    }
    if let Some(limit) = status.limited {
        push_note(
            &mut spans,
            format!(
                "Discovery limited at {} sources / {} MiB: raise max_metadata_sources / max_metadata_bytes",
                limit.sources,
                limit.bytes / (1024 * 1024)
            ),
            palette.warning(),
        );
    }
    if status.refresh_available {
        push_note(&mut spans, "refresh available (r)".into(), palette.quiet());
    }
    // The empty state lists every warning, so the line stays operational there.
    if app.total_sessions() > 0 {
        let mut items =
            ctx.notes
                .iter()
                .map(|note| one_line(note))
                .chain(app.warnings().iter().map(|warning| {
                    format!(
                        "{}: {}",
                        harness_name(warning.key.harness),
                        one_line(&warning.message)
                    )
                }));
        if let Some(first) = items.next() {
            let more = items.count();
            let suffix = if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            };
            push_note(
                &mut spans,
                format!("Warning: {first}{suffix}"),
                palette.warning(),
            );
        }
    }
    spans
}

fn empty_lines(app: &App, ctx: &RosterContext, palette: Palette) -> Vec<Line<'static>> {
    let mut lines = vec![Line::styled("No sessions found.", palette.title())];
    for note in &ctx.notes {
        lines.push(Line::styled(display_safe(note), palette.warning()));
    }
    lines.push(Line::from("Checked:"));
    for root in &ctx.roots {
        lines.push(Line::from(format!("  {}", display_safe(root))));
    }
    lines.push(Line::from(format!(
        "Last refresh: {}",
        ctx.last_refresh.as_deref().unwrap_or("not yet completed")
    )));
    if !app.warnings().is_empty() {
        lines.push(Line::from("Warnings:"));
        for warning in app.warnings() {
            lines.push(Line::styled(
                format!(
                    "  {}: {}",
                    harness_name(warning.key.harness),
                    display_safe(&warning.message)
                ),
                palette.warning(),
            ));
        }
    }
    lines
}

fn sort_name(mode: SortMode) -> &'static str {
    match mode {
        SortMode::LastActivity => "last activity",
        SortMode::Provider => "provider",
        SortMode::Tokens => "tokens",
        SortMode::Status => "status",
    }
}

fn filters_text(filter: &FilterState) -> String {
    let mut parts = Vec::new();
    if let Some(providers) = &filter.providers {
        let names: Vec<_> = providers.iter().map(|&p| harness_name(p)).collect();
        parts.push(format!("provider={}", names.join(",")));
    }
    if let Some(statuses) = &filter.statuses {
        let names: Vec<_> = statuses.iter().map(|&s| status_text(s).1).collect();
        parts.push(format!("status={}", names.join(",")));
    }
    if let Some(project) = &filter.project {
        parts.push(format!("project={}", display_safe(project)));
    }
    match filter.usage {
        UsageAvailability::Any => {}
        UsageAvailability::Known => parts.push("usage=known".into()),
        UsageAvailability::Unknown => parts.push("usage=unknown".into()),
        UsageAvailability::HasCost => parts.push("usage=has cost".into()),
    }
    if !filter.query.is_empty() {
        parts.push(format!("search={}", display_safe(&filter.query)));
    }
    if filter.include_released {
        parts.push("released=shown".into());
    }
    if parts.is_empty() {
        "none".into()
    } else {
        parts.join(" ")
    }
}
