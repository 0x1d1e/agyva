//! Rendering. Every function takes `&AppState` and a frame (or returns plain
//! lines); none of them changes state or touches the terminal directly.

mod accounts;
mod chrome;
mod diagnostics;
mod endpoints;
mod overlay;
mod routes;
mod usage;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::app::{AppState, View};
use super::theme;
use crate::core::format;
use crate::core::usage::WindowView;

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
        View::Endpoints => endpoints::render(state, f, body),
        View::Routes => routes::render(state, f, body),
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
    let footer_h = if area.height >= 10 || (status && area.height >= 8) {
        2
    } else {
        1
    };
    Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(chrome::banner_height(state, area)),
        Constraint::Min(0),
        Constraint::Length(footer_h),
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
        View::Usage => usage::lines_for_width(state, usize::from(body.width)),
        View::Diagnostics => diagnostics::lines_for_width(state, usize::from(body.width)),
        View::Accounts | View::Endpoints | View::Routes => return 0,
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

/// Smooth block bar (`█▓░`) enclosed in `[...]` with `cells` inner cells.
pub(super) fn smooth_bar(percent: f64, cells: usize) -> String {
    let p = if percent.is_finite() {
        percent.clamp(0.0, 100.0)
    } else {
        0.0
    };
    let half_units = ((p / 100.0) * (cells * 2) as f64).round() as usize;
    let full = (half_units / 2).min(cells);
    let half = usize::from(full < cells && half_units % 2 == 1);
    let empty = cells.saturating_sub(full + half);
    format!(
        "[{}{}{}]",
        "█".repeat(full),
        "▓".repeat(half),
        "░".repeat(empty)
    )
}

pub(super) fn eta_short(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Read precomputed burn-rate ETA from `WindowView::burn_rate` without ever
/// computing quota numbers in the TUI.
pub(super) fn window_burn_eta(w: &WindowView) -> Option<(String, bool)> {
    let br = w.burn_rate.as_ref()?;
    let eta = br.eta_exhaustion_secs?;
    Some((
        format!("~{}", eta_short(eta)),
        br.exhausts_before_reset == Some(true),
    ))
}

/// Truncate and right-pad a multi-span line to exact `target_cols` display width.
fn fit_line_spans(input: Line<'static>, target_cols: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut room = target_cols;
    for sp in input.spans {
        if room == 0 {
            break;
        }
        let clean = format::sanitize(sp.content.as_ref());
        let t = format::truncate(&clean, room);
        let used = format::width(&t);
        room = room.saturating_sub(used);
        out.push(Span::styled(t, sp.style));
    }
    if room > 0 {
        out.push(Span::raw(" ".repeat(room)));
    }
    out
}

/// Build lines for a rounded-border card of exact `width` columns.
pub(super) fn rounded_card_lines(
    title: &str,
    title_style: Style,
    body: Vec<Line<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    if width < 12 {
        return body;
    }
    let border_style = theme::border();
    let inner_w = width.saturating_sub(2);
    let content_w = inner_w.saturating_sub(2);

    let mut top_spans = vec![Span::styled("╭", border_style)];
    let clean_title = format::sanitize(title);
    if clean_title.is_empty() {
        top_spans.push(Span::styled("─".repeat(inner_w), border_style));
    } else {
        let max_title = inner_w.saturating_sub(4);
        let t = format::truncate(&clean_title, max_title);
        let t_w = format::width(&t);
        let right_dashes = inner_w.saturating_sub(3 + t_w);
        top_spans.push(Span::styled("─ ", border_style));
        top_spans.push(Span::styled(t, title_style));
        top_spans.push(Span::styled(
            format!(" {}", "─".repeat(right_dashes)),
            border_style,
        ));
    }
    top_spans.push(Span::styled("╮", border_style));

    let mut out = vec![Line::from(top_spans)];
    for l in body {
        let mut row_spans = Vec::new();
        row_spans.push(Span::styled("│ ", border_style));
        row_spans.extend(fit_line_spans(l, content_w));
        row_spans.push(Span::styled(" │", border_style));
        out.push(Line::from(row_spans));
    }
    out.push(Line::from(vec![Span::styled(
        format!("╰{}╯", "─".repeat(inner_w)),
        border_style,
    )]));
    out
}
