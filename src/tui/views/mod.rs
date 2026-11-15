//! Rendering. Every function takes `&AppState` and a frame (or returns plain
//! lines); none of them changes state or touches the terminal directly.

mod accounts;
mod chrome;
mod diagnostics;
mod overlay;
mod usage;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::Style;
use ratatui::text::{Line, Span};

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
    let banner_h = chrome::banner_height(state, area);
    let [head, banner, body, foot] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(banner_h),
        Constraint::Min(0),
        Constraint::Length(1),
    ])
    .areas(area);
    chrome::header(state, f, head);
    if banner_h > 0 {
        chrome::banner(state, f, banner);
    }
    match state.view {
        View::Accounts => accounts::render(state, f, body),
        View::Usage => usage::render(state, f, body),
        View::Diagnostics => diagnostics::render(state, f, body),
    }
    chrome::footer(state, f, foot);
    overlay::render(state, f, area);
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
