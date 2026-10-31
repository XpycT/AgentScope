//! Roster preview: facts of the selected session, drawn beside the list or in
//! its place on a narrow terminal. Pure: reads `App`, draws into a `Frame`.
//!
//! Every figure comes from the shared typed helpers in `session.rs`, so the
//! preview and the details view never price, time or rate anything separately.
//! The preview scrolls vertically on its own, independent of the list.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};

use crate::app::{App, PreviewState};

use super::RosterContext;
use super::details::wrap_into;
use super::session::preview_lines;
use super::widgets::{Palette, fit};

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
    if inner.width == 0 || inner.height < 2 {
        return;
    }
    let width = inner.width as usize;
    let (state, rows) = preview_rows(app, width);
    let title = Rect::new(inner.x, inner.y, inner.width, 1);
    frame.render_widget(
        Paragraph::new(Line::styled(fit(&state, width), palette.title())),
        title,
    );

    let body = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
    let height = body.height as usize;
    let max_scroll = rows.len().saturating_sub(height);
    app.set_preview_extent(max_scroll);
    let start = app.preview_scroll().min(max_scroll);
    let shown: Vec<Line<'_>> = rows
        .iter()
        .skip(start)
        .take(height)
        .map(|row| Line::raw(row.as_str()))
        .collect();
    frame.render_widget(Paragraph::new(shown), body);
}

/// The state line and the wrapped fact rows of the selected session.
fn preview_rows(app: &App, width: usize) -> (String, Vec<String>) {
    let mut rows = Vec::new();
    let Some(key) = app.selected_key() else {
        wrap_into(&mut rows, "No session selected.", width);
        return ("Preview".into(), rows);
    };
    let preview = app.preview();
    let current = preview.key() == Some(&key);
    let state = match preview {
        PreviewState::Loading { progress, .. } if current => {
            format!("Preview — loading ({} records read)", progress.records)
        }
        PreviewState::Ready {
            complete: false, ..
        } if current => "Preview — partial: more is still being read".into(),
        PreviewState::Ready { .. } if current => "Preview".into(),
        PreviewState::Failed { .. } if current => "Preview — failed to read".into(),
        _ => "Preview — pending".into(),
    };
    match app.session(&key) {
        Some(row) => {
            for line in preview_lines(row.session, row.hydrated, app.now()) {
                if wrap_into(&mut rows, &line, width) {
                    break;
                }
            }
        }
        None => {
            wrap_into(&mut rows, "Session is no longer in the roster.", width);
        }
    }
    (state, rows)
}
