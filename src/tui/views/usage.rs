//! Per-group quota for the selected account: bars, reset times, freshness
//! and, when there is no reading, the reason why.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use super::{line, rect_u16};
use crate::core::format::{self, bar, percent_text};
use crate::core::usage::{AccountUsage, QuotaView, WindowView};
use crate::tui::app::AppState;
use crate::tui::theme;

fn window_line(name: &str, w: &WindowView, a: &AccountUsage, state: &AppState) -> Line<'static> {
    let reset = match w.reset_at {
        Some(t) => format!("resets {}", format::until(state.now, t)),
        None => "reset unknown".to_owned(),
    };
    line(
        format!(
            "  {name:<7}{} {:>4}  {reset}",
            bar(w.remaining_percent, 10),
            percent_text(w.remaining_percent)
        ),
        theme::quota(
            w.remaining_percent,
            a.refreshed_now && state.freshness(a) == crate::core::quota::Freshness::Fresh,
        ),
    )
}

fn group_lines(g: &QuotaView, a: &AccountUsage, state: &AppState, out: &mut Vec<Line<'static>>) {
    out.push(line(g.label(), theme::bold()));
    if g.disabled {
        out.push(line("  not enabled for this account", theme::dim()));
        return;
    }
    if g.exhausted {
        out.push(line("  exhausted", theme::bad()));
    }
    match &g.five_hour {
        Some(w) => out.push(window_line("5H", w, a, state)),
        None => out.push(line("  5H     not reported", theme::dim())),
    }
    match &g.weekly {
        Some(w) => out.push(window_line("WEEKLY", w, a, state)),
        None => out.push(line("  WEEKLY not reported", theme::dim())),
    }
}

/// All lines for the selected account (public for tests).
pub fn lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(a) = state.selected() else {
        let text = if state.data.is_none() {
            "Loading..."
        } else {
            "No account selected."
        };
        return vec![line(text, theme::dim())];
    };
    let mut out = Vec::new();
    let marker = if a.active { " (active)" } else { "" };
    out.push(line(format!("{}{marker}", a.alias), theme::bold()));
    let seen = match a.observed_at {
        Some(t) => format!(
            "{}, {}",
            state.freshness(a).label(),
            format::ago(state.now, t)
        ),
        None => state.freshness(a).label().to_owned(),
    };
    let source = if a.refreshed_now {
        "read from agy /usage during this session"
    } else {
        "from agyva's cache"
    };
    out.push(line(format!("Data: {seen}; {source}"), theme::dim()));
    out.push(line(
        format!("Health: {} - {}", a.health.label, state.health_evidence(a)),
        theme::health(a.health.state),
    ));
    if let Some(u) = &a.unavailable {
        let (label, style) = if u.reason == crate::core::quota::UnavailableReason::TooSoon {
            ("Note", theme::dim())
        } else {
            ("Unavailable", theme::warn())
        };
        out.push(line(format!("{label}: {}", u.text()), style));
    }
    out.push(Line::default());
    out.push(line(
        "REMAINING by group; missing windows are not reported.",
        theme::dim(),
    ));
    if a.groups.is_empty() {
        out.push(line("No quota reading available.", theme::dim()));
    }
    for g in &a.groups {
        group_lines(g, a, state, &mut out);
        out.push(Line::default());
    }
    out
}

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let paragraph = Paragraph::new(lines(state)).wrap(Wrap { trim: false });
    let top = state.scroll.min(super::paragraph_max(&paragraph, area));
    f.render_widget(paragraph.scroll((rect_u16(top), 0)), area);
}
