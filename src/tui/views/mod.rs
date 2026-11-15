//! Rendering. Every function takes `&AppState` and a frame (or returns plain
//! lines); none of them changes state or touches the terminal directly.

mod accounts;
mod chrome;
mod diagnostics;
mod overlay;
mod usage;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::app::{AppState, View};
use crate::core::format;

pub use accounts::{ColumnPlan, column_plan};

/// Smallest area worth drawing anything into.
const MIN_W: u16 = 4;
const MIN_H: u16 = 3;

pub fn render(state: &AppState, f: &mut Frame<'_>) {
    let area = f.area();
    if area.width < MIN_W || area.height < MIN_H {
        return;
    }
    let [head, banner, body, foot] = areas(state, area);
    chrome::header(state, f, head);
    if banner.height > 0 {
        chrome::banner(state, f, banner);
    }
    match state.view {
        View::Accounts => accounts::render(state, f, body),
        View::Usage => usage::render(state, f, body),
        View::Diagnostics => diagnostics::render(state, f, body),
    }
    chrome::footer(state, f, foot);
    overlay::render(state, f, area);
    if !state.color {
        for cell in &mut f.buffer_mut().content {
            cell.fg = Color::Reset;
            cell.bg = Color::Reset;
        }
    }
}

fn areas(state: &AppState, area: Rect) -> [Rect; 4] {
    let status = state.error.is_some()
        || state.message.is_some()
        || !state.filter.is_empty()
        || state.filter_editing;
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(chrome::banner_height(state, area)),
        Constraint::Min(0),
        Constraint::Length(if status && area.height >= 8 { 2 } else { 1 }),
    ])
    .areas(area)
}

pub(super) fn paragraph_max(paragraph: &Paragraph<'_>, area: Rect) -> usize {
    paragraph
        .line_count(area.width)
        .saturating_sub(usize::from(area.height))
}

pub(crate) fn max_scroll(state: &AppState) -> usize {
    let area = Rect::new(0, 0, state.size.0, state.size.1);
    if state.overlay.is_some() {
        return overlay::max_scroll(state, area);
    }
    let body = areas(state, area)[2];
    let lines = match state.view {
        View::Usage => usage::lines(state),
        View::Diagnostics => diagnostics::lines(state),
        View::Accounts => return 0,
    };
    paragraph_max(&Paragraph::new(lines).wrap(Wrap { trim: false }), body)
}

/// Append `text` to `spans`, never exceeding `room` columns in total.
/// Returns the columns left.
pub(super) fn push_fit(
    spans: &mut Vec<Span<'static>>,
    text: &str,
    style: Style,
    room: usize,
) -> usize {
    if room == 0 {
        return 0;
    }
    let t = format::truncate(text, room);
    let used = format::width(&t);
    spans.push(Span::styled(t, style));
    room - used.min(room)
}

pub(super) fn rect_u16(n: usize) -> u16 {
    u16::try_from(n).unwrap_or(u16::MAX)
}

pub(super) fn line(text: impl Into<String>, style: Style) -> Line<'static> {
    Line::from(Span::styled(format::sanitize(&text.into()), style))
}
