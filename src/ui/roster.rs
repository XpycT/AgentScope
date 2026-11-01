//! Roster screen rendering (spec §10.2, §10.6, §10.7). Pure: reads `App` and a
//! small context, draws into a `Frame`. No I/O.
//!
//! Every session is two physical lines. The marker and session columns are
//! pinned; the trailing columns (harness/provider, model/effort, tokens, API
//! estimate, active time, latest tok/s) scroll as whole columns. A column's
//! second line starts at the same x as its first, so provider sits under the
//! harness and effort under the model.

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};
use unicode_width::UnicodeWidthStr;

use crate::app::{
    App, COLUMN_COUNT, FilterState, LoadFocus, PREVIEW_SPLIT_WIDTH, Panel, RosterItem,
    RosterSession, Screen, SortMode, UsageAvailability,
};
use crate::domain::{SessionKey, SessionSummary, Timestamp};
use crate::ingest::text::display_safe;
use crate::time::relative_label;

use super::RosterContext;
use super::preview::render_preview;
use super::session::{Fact, active_fact, cost_fact, speed_fact, tokens_fact};
use super::widgets::{Palette, fit, harness_name, one_line, search_prompt, status_text};

pub const MIN_WIDTH: u16 = 60;
pub const MIN_HEIGHT: u16 = 20;
/// Columns reserved for the side preview at [`PREVIEW_SPLIT_WIDTH`] and wider.
pub const PREVIEW_WIDTH: u16 = 45;

/// Pinned selection/expansion marker.
const MARKER_W: u16 = 2;
/// Physical lines per roster row.
const ROW_HEIGHT: u16 = 2;
/// Lines above the table header: title, filters, counts.
const HEAD_H: u16 = 2;
/// Wheel notches move the preview this many lines.
const WHEEL_LINES: isize = 3;

/// A scrollable trailing column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Column {
    /// Harness over its recorded billing provider.
    Provider,
    /// Model over its reasoning effort.
    Model,
    Tokens,
    Cost,
    Active,
    Speed,
}

impl Column {
    pub const ALL: [Column; COLUMN_COUNT] = [
        Column::Provider,
        Column::Model,
        Column::Tokens,
        Column::Cost,
        Column::Active,
        Column::Speed,
    ];

    fn width(self) -> u16 {
        match self {
            Column::Provider => 12,
            Column::Model => 18,
            Column::Tokens => 9,
            Column::Cost => 12,
            Column::Active => 9,
            Column::Speed => 18,
        }
    }

    fn title(self) -> &'static str {
        match self {
            Column::Provider => "HARNESS/PROV",
            Column::Model => "MODEL/EFFORT",
            Column::Tokens => "TOKENS",
            Column::Cost => "API ESTIMATE",
            Column::Active => "ACTIVE",
            Column::Speed => "LATEST TOK/S",
        }
    }
}

/// One displayed trailing column: where it sits on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnBox {
    pub column: Column,
    pub x: u16,
    pub width: u16,
}

/// Geometry of the roster screen for one terminal size and column scroll. The
/// render and the mouse both use it, so a click can only land on what is drawn.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RosterLayout {
    pub area: Rect,
    /// Table header line of the list.
    pub header: Rect,
    /// Rows region of the list: [`ROW_HEIGHT`] lines per row.
    pub rows: Rect,
    /// Preview column beside the list (terminal at least [`PREVIEW_SPLIT_WIDTH`] wide).
    pub preview: Option<Rect>,
    /// Header and rows across the whole width: where a swapped-in preview is drawn.
    pub swapped: Rect,
    pub session_x: u16,
    pub session_width: u16,
    /// Trailing columns shown from `horizontal_column`; the last may be cut.
    pub columns: Vec<ColumnBox>,
    /// First visible trailing column after clamping.
    pub horizontal_column: usize,
    /// Header hint that earlier columns are scrolled out.
    pub more_left: Option<Rect>,
    /// Header hint that later columns are hidden.
    pub more_right: Option<Rect>,
}

/// Vertical bands: head, counts, banner-or-rule, table header, body, rule, footer.
fn bands(area: Rect) -> [Rect; 7] {
    Layout::vertical([
        Constraint::Length(HEAD_H),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area)
}

impl RosterLayout {
    /// Geometry for `area`; below the minimum size everything is hidden.
    pub fn for_area(area: Rect, horizontal_column: usize) -> Self {
        let horizontal = horizontal_column.min(COLUMN_COUNT - 1);
        let hidden = Self {
            area,
            header: Rect::ZERO,
            rows: Rect::ZERO,
            preview: None,
            swapped: Rect::ZERO,
            session_x: 0,
            session_width: 0,
            columns: Vec::new(),
            horizontal_column: horizontal,
            more_left: None,
            more_right: None,
        };
        if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
            return hidden;
        }
        let [_, _, _, table_head, body, _, _] = bands(area);
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
        let session_width = (list_w / 4).clamp(20, 34);
        let mut columns = Vec::with_capacity(COLUMN_COUNT);
        let mut cursor = session_x + session_width;
        let right = rows.right();
        for column in Column::ALL.into_iter().skip(horizontal) {
            let x = cursor + 1;
            if x >= right || right - x < 4 {
                break;
            }
            let width = column.width().min(right - x);
            columns.push(ColumnBox { column, x, width });
            cursor = x + width;
        }
        let shown_all = columns.last().is_some_and(|last| {
            last.column == Column::Speed && last.width == Column::Speed.width()
        });
        let more_left = (horizontal > 0)
            .then(|| columns.first().map(|c| Rect::new(c.x - 1, header.y, 1, 1)))
            .flatten();
        let more_right = (!shown_all).then(|| Rect::new(right - 1, header.y, 1, 1));
        Self {
            area,
            header,
            rows,
            preview,
            swapped,
            session_x,
            session_width,
            columns,
            horizontal_column: horizontal,
            more_left,
            more_right,
        }
    }

    /// The terminal is large enough to draw the roster.
    pub fn usable(&self) -> bool {
        self.rows.height >= ROW_HEIGHT
    }

    /// Rows that fit in the list.
    pub fn capacity(&self) -> usize {
        (self.rows.height / ROW_HEIGHT) as usize
    }

    /// Index of the first row drawn: the focused row stays on screen.
    pub fn first_visible(&self, app: &App) -> usize {
        app.focus_visual_index()
            .map_or(0, |at| (at + 1).saturating_sub(self.capacity()))
    }

    /// Trailing column displayed under screen column `x`.
    pub fn column_at(&self, x: u16) -> Option<Column> {
        self.columns
            .iter()
            .find(|c| x >= c.x && x < c.x + c.width)
            .map(|c| c.column)
    }

    /// Displayed row slot (0-based from the first drawn row) under `(x, y)`.
    pub fn slot_at(&self, x: u16, y: u16) -> Option<usize> {
        if !self.usable() || !contains(self.rows, x, y) {
            return None;
        }
        let slot = ((y - self.rows.y) / ROW_HEIGHT) as usize;
        (slot < self.capacity()).then_some(slot)
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
            if layout.more_left.is_some_and(|r| contains(r, x, y)) {
                app.scroll_columns(-1);
                return;
            }
            if layout.more_right.is_some_and(|r| contains(r, x, y)) {
                app.scroll_columns(1);
                return;
            }
            let Some(slot) = layout.slot_at(x, y) else {
                return;
            };
            let hit =
                app.roster_rows()
                    .nth(layout.first_visible(app) + slot)
                    .map(|item| match item {
                        RosterItem::Session(row) => Hit::Session(row.session.meta.key.clone()),
                        RosterItem::RootLoadMore => Hit::Roots,
                        RosterItem::ChildLoadMore { parent, .. } => Hit::Children(parent.clone()),
                    });
            match hit {
                Some(Hit::Session(key)) => {
                    if app.select_row(&key) && x < layout.rows.x + MARKER_W {
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

    let layout = RosterLayout::for_area(area, app.horizontal_column());
    let total = app.total_sessions();
    let banner = banner_line(app, ctx, total);
    let [head, counts, banner_area, table_head, _, rule2, footer] = bands(area);

    frame.render_widget(Paragraph::new(header_lines(app, ctx, palette)), head);
    frame.render_widget(
        Paragraph::new(Line::styled(
            fit(&counts_text(app), area.width as usize),
            palette.dim(),
        )),
        counts,
    );
    match banner {
        Some(line) => frame.render_widget(
            Paragraph::new(Line::styled(
                fit(&line, area.width as usize),
                palette.warning(),
            )),
            banner_area,
        ),
        None => frame.render_widget(Paragraph::new("─".repeat(area.width as usize)), banner_area),
    }
    frame.render_widget(Paragraph::new("─".repeat(area.width as usize)), rule2);

    let swapped = layout.preview.is_none() && app.preview_replaces_list();
    let header_text = if swapped {
        fit(
            "PREVIEW  Tab list  j/k scroll  Enter open",
            area.width as usize,
        )
    } else {
        table_header(&layout)
    };
    frame.render_widget(
        Paragraph::new(Line::styled(header_text, palette.title())),
        table_head,
    );
    let (footer_text, footer_style) = match app.search_input() {
        Some((_, text)) => (search_prompt(&display_safe(text)), palette.title()),
        None => (
            if app.roster_status().has_more {
                "Enter open  ←/→ columns  Tab preview  / search  f filter  s sort  r refresh  m more  p procs  ? help  q quit"
            } else {
                "Enter open  ←/→ columns  Tab preview  / search  f filter  s sort  r refresh  p procs  ? help  q quit"
            }
            .to_string(),
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
            render_rows(frame, app, &layout, palette);
        }
        if let Some(preview) = layout.preview {
            render_preview(frame, app, ctx, preview);
        }
    }
}

fn render_rows(frame: &mut Frame, app: &App, layout: &RosterLayout, palette: Palette) {
    let capacity = layout.capacity();
    if capacity == 0 {
        return;
    }
    let first = layout.first_visible(app);
    let load_focus = app.load_focus().and(app.focus_visual_index());
    let now = app.now();
    // Only the visible slice is borrowed and formatted; unshown rows cost nothing.
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(capacity * 2);
    for (index, item) in app.roster_rows().enumerate().skip(first).take(capacity) {
        match item {
            RosterItem::Session(row) => {
                let highlighted = row.selected && load_focus.is_none();
                lines.extend(row_lines(&row, layout, now, highlighted, palette));
            }
            RosterItem::RootLoadMore => lines.extend(load_more_lines(
                "Load more sessions",
                "m or Enter loads the next page",
                0,
                load_focus == Some(index),
                palette,
            )),
            RosterItem::ChildLoadMore { depth, .. } => lines.extend(load_more_lines(
                "Load more children",
                "Enter loads the next page",
                depth as usize,
                load_focus == Some(index),
                palette,
            )),
        }
    }
    frame.render_widget(Paragraph::new(lines), layout.rows);
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

/// The two cells (first and second physical line) of a trailing column.
fn cells(column: Column, s: &SessionSummary, hydrated: bool, now: Timestamp) -> (String, String) {
    let pair = |fact: Fact| (fact.head, fact.sub);
    match column {
        Column::Provider => (
            harness_name(s.meta.key.harness).to_string(),
            s.meta
                .model
                .as_ref()
                .and_then(|m| m.provider.as_deref())
                .map_or_else(|| "Unknown".into(), one_line),
        ),
        Column::Model => (
            s.meta
                .model
                .as_ref()
                .map_or_else(|| "Unknown".into(), |m| one_line(&m.id)),
            s.meta
                .model
                .as_ref()
                .and_then(|m| m.effort.as_deref())
                .map_or_else(|| "Unknown".into(), one_line),
        ),
        Column::Tokens => pair(tokens_fact(s.usage.as_ref(), hydrated)),
        Column::Cost => pair(cost_fact(s.usage.as_ref(), hydrated)),
        Column::Active => pair(active_fact(s, hydrated)),
        Column::Speed => pair(speed_fact(s, hydrated, now)),
    }
}

/// One session as two aligned physical lines.
fn row_lines(
    row: &RosterSession<'_>,
    layout: &RosterLayout,
    now: Timestamp,
    highlighted: bool,
    palette: Palette,
) -> [Line<'static>; 2] {
    let s = row.session;
    let (icon, label) = status_text(row.status.label);
    let indent = "  ".repeat(row.depth as usize);
    let session_w = (layout.session_width as usize).saturating_sub(indent.len());
    let title = one_line(&s.meta.title);
    let activity = [s.meta.activity.at, s.last_event_at, s.last_completion]
        .into_iter()
        .flatten()
        .max();
    // A mtime-derived activity is approximate and must not read as an exact
    // recorded event time.
    let approximate = activity == s.meta.activity.at
        && s.meta.activity.class == crate::domain::AggregationClass::Derived;
    let last = activity.map_or_else(
        || "—".to_string(),
        |t| {
            format!(
                "{}{}",
                if approximate { "~" } else { "" },
                relative_label(t, now)
            )
        },
    );
    let project = s
        .meta
        .project
        .as_deref()
        .map(one_line)
        .filter(|p| !p.is_empty());

    let status = format!("{icon} {label}");
    let used = status.width();
    let mut rest = format!(" · {last}");
    if let Some(project) = project {
        rest.push_str(" · ");
        rest.push_str(&project);
    }

    let expander = if row.expanded {
        '-'
    } else if row.child_count > 0 {
        '+'
    } else {
        ' '
    };
    let mut first: Vec<Span<'static>> = vec![
        Span::raw(if highlighted { "> " } else { "  " }),
        Span::raw(format!("{indent}{}", fit(&title, session_w))),
    ];
    let mut second: Vec<Span<'static>> = vec![
        Span::raw(format!("{expander} ")),
        Span::raw(indent),
        Span::styled(status, palette.status(row.status.label)),
        Span::raw(fit(&rest, session_w.saturating_sub(used))),
    ];
    for placed in &layout.columns {
        let (head, sub) = cells(placed.column, s, row.hydrated, now);
        let width = placed.width as usize;
        first.push(Span::raw(" "));
        first.push(Span::raw(fit(&head, width)));
        second.push(Span::raw(" "));
        second.push(Span::raw(fit(&sub, width)));
    }
    let lines = [Line::from(first), Line::from(second)];
    if highlighted {
        lines.map(|line| line.style(palette.selected()))
    } else {
        lines
    }
}

/// Header text for the displayed columns, with `‹`/`›` hints when columns are
/// scrolled out of view.
fn table_header(layout: &RosterLayout) -> String {
    let mut out = format!(
        "{}{}",
        " ".repeat(MARKER_W as usize),
        fit("SESSION / PROJECT", layout.session_width as usize),
    );
    for (i, placed) in layout.columns.iter().enumerate() {
        out.push(if i == 0 && layout.more_left.is_some() {
            '‹'
        } else {
            ' '
        });
        out.push_str(&fit(placed.column.title(), placed.width as usize));
    }
    let mut chars: Vec<char> = fit(&out, layout.header.width as usize).chars().collect();
    if layout.more_right.is_some()
        && let Some(last) = chars.last_mut()
    {
        *last = '›';
    }
    chars.into_iter().collect()
}

fn header_lines(app: &App, ctx: &RosterContext, palette: Palette) -> Vec<Line<'static>> {
    let mut title = vec![
        Span::styled("AgentScope", palette.title()),
        Span::raw("  LOCAL"),
    ];
    for (provider, count) in app.provider_counts() {
        title.push(Span::raw(format!("  {} {count}", harness_name(provider))));
    }
    let live = if app.live && ctx.error.is_none() {
        "Live: on"
    } else {
        "Live: paused"
    };
    title.push(Span::raw(format!("  {live}")));

    let order = if app.sort_ascending() { "asc" } else { "desc" };
    let shown = app.visible_rows().len();
    let filters = Line::from(format!(
        "Filters: {}   Sort: {} ({order})   Showing {shown} of {}",
        filters_text(app.filter()),
        sort_name(app.sort_mode()),
        app.total_sessions(),
    ));
    vec![Line::from(title), filters]
}

fn counts_text(app: &App) -> String {
    let mut parts: Vec<String> = app
        .status_counts()
        .into_iter()
        .filter(|&(_, n)| n > 0)
        .map(|(label, n)| {
            let (icon, text) = status_text(label);
            format!("{icon} {text} {n}")
        })
        .collect();
    if parts.is_empty() {
        parts.push("No sessions yet".into());
    }
    let mut out = parts.join("  ");
    let status = app.roster_status();
    for scan in status.scan.iter().filter(|s| !s.done) {
        let total = scan.total.map_or(String::new(), |t| format!("/{t}"));
        out.push_str(&format!(
            "  Scanning {} {}{total}",
            harness_name(scan.harness),
            scan.scanned
        ));
    }
    if status.provisional {
        out.push_str("  · provisional order (discovery running)");
    }
    if status.has_more {
        out.push_str("  · more available (m)");
    }
    if let Some(limit) = status.limited {
        out.push_str(&format!(
            "  · Discovery limited at {} sources / {} MiB: raise max_metadata_sources / max_metadata_bytes",
            limit.sources,
            limit.bytes / (1024 * 1024)
        ));
    }
    if status.refresh_available {
        out.push_str("  · refresh available (r)");
    }
    out
}

fn banner_line(app: &App, ctx: &RosterContext, total: usize) -> Option<String> {
    if let Some(error) = &ctx.error {
        return Some(format!("Worker stopped: {}; r retries", one_line(error)));
    }
    if total == 0 {
        return None; // The empty state lists every warning.
    }
    let warnings = app.warnings();
    let mut items = ctx.notes.iter().map(|n| one_line(n)).chain(
        warnings
            .iter()
            .map(|w| format!("{}: {}", harness_name(w.key.harness), one_line(&w.message))),
    );
    let first = items.next()?;
    let more = items.count();
    let suffix = if more > 0 {
        format!(" (+{more} more)")
    } else {
        String::new()
    };
    Some(format!("Warning: {first}{suffix}"))
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
        for w in app.warnings() {
            lines.push(Line::styled(
                format!(
                    "  {}: {}",
                    harness_name(w.key.harness),
                    display_safe(&w.message)
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
        SortMode::Project => "project",
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
