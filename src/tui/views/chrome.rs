//! Header (title and tabs), suspension banner and footer (keys, filter,
//! messages).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use super::{push_fit, rect_u16};
use crate::core::format;
use crate::tui::app::{AppState, View};
use crate::tui::theme;

fn banner_text(state: &AppState) -> Option<String> {
    let s = state.suspended();
    if s.is_empty() {
        return None;
    }
    let names: Vec<String> = s.iter().map(|a| format::sanitize(&a.alias)).collect();
    Some(format!(
        "SUSPENDED: {}. Google reported a Terms of Service suspension. agyva does not retry it or work around it. \
         See the Antigravity FAQ (https://antigravity.google/docs/faq) and contact Google support via the account's suspension notice.",
        names.join(", ")
    ))
}

/// Rows the banner needs (0 when nothing is suspended); never more than a
/// third of the screen so the list stays usable.
pub(super) fn banner_height(state: &AppState, area: Rect) -> u16 {
    let Some(text) = banner_text(state) else {
        return 0;
    };
    let w = usize::from(area.width).max(1);
    let wanted = format::width(&text).div_ceil(w);
    let cap = (usize::from(area.height) / 3).clamp(1, 4);
    rect_u16(wanted.min(cap))
}

pub(super) fn banner(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    if let Some(text) = banner_text(state) {
        f.render_widget(
            Paragraph::new(text)
                .style(theme::banner())
                .wrap(Wrap { trim: true }),
            area,
        );
    }
}

pub(super) fn header(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let mut spans = Vec::new();
    let mut room = usize::from(area.width);
    room = push_fit(&mut spans, " agyva ", theme::bold(), room);
    if area.width < 54 {
        push_fit(
            &mut spans,
            &format!("/ {}", state.view.title()),
            theme::tab_active(),
            room,
        );
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }
    for v in View::ALL {
        let label = format!(" {} ", v.title());
        let style = if v == state.view {
            theme::tab_active()
        } else {
            theme::plain()
        };
        room = push_fit(&mut spans, &label, style, room);
    }
    let busy = if state.switching {
        "switching..."
    } else if state.loading() && state.data.is_none() {
        "loading..."
    } else if state.loading() {
        "refreshing..."
    } else {
        ""
    };
    if !busy.is_empty() {
        let text = format!("  {busy}");
        push_fit(&mut spans, &text, theme::warn(), room);
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(super) fn footer(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let w = usize::from(area.width);
    let mut status = Vec::new();
    if state.filter_editing {
        push_fit(
            &mut status,
            &format!("/{}_  Enter keeps, Esc clears", state.filter),
            theme::bold(),
            w,
        );
    } else if let Some(e) = &state.error {
        push_fit(&mut status, e, theme::bad(), w);
    } else if let Some(m) = &state.message {
        push_fit(&mut status, m, theme::warn(), w);
    } else if !state.filter.is_empty() {
        push_fit(
            &mut status,
            &format!(
                "filter: {} ({} shown, Esc clears)",
                state.filter,
                state.visible().len()
            ),
            theme::bold(),
            w,
        );
    }
    let mut hints = Vec::new();
    let essential = if state.filter_editing {
        "? help  ^C quit"
    } else {
        "? help  q quit"
    };
    let room = push_fit(&mut hints, essential, theme::bold(), w);
    let actions = match state.view {
        View::Accounts => "  ↑↓ select  Enter switch  r refresh  d details  / filter  Tab views",
        View::Usage => "  ↑↓ select  PgUp/PgDn scroll  r refresh  d details  Tab views",
        View::Diagnostics => "  ↑↓ scroll  PgUp/PgDn page  r refresh  Tab views",
    };
    push_fit(&mut hints, actions, theme::dim(), room);
    let mut lines = Vec::new();
    if area.height > 1 {
        lines.push(Line::from(status));
    }
    lines.push(Line::from(hints));
    f.render_widget(Paragraph::new(lines), area);
}
