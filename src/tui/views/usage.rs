//! Per-group quota for the selected account: framed rounded cards on wide
//! terminals, wide smooth block gauges, reset countdowns, burn-rate ETA, and
//! explicit disabled/unavailable reasons.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::{eta_short, line, rect_u16, rounded_card_lines, smooth_bar};
use crate::core::format::{self, percent_text};
use crate::core::usage::{AccountUsage, QuotaView, WindowView};
use crate::tui::app::AppState;
use crate::tui::theme;

fn window_lines(
    name: &str,
    w: &WindowView,
    a: &AccountUsage,
    state: &AppState,
    bar_cells: usize,
    indent: &str,
) -> Vec<Line<'static>> {
    let live = a.refreshed_now && state.freshness(a) == crate::core::quota::Freshness::Fresh;
    let reset = match w.reset_at {
        Some(t) => format!("resets {}", format::until(state.now, t)),
        None => "reset unknown".to_owned(),
    };
    let bar_str = smooth_bar(w.remaining_percent, bar_cells);
    let pct_str = percent_text(w.remaining_percent);
    let q_style = theme::quota(w.remaining_percent, live);

    let mut out = vec![Line::from(vec![
        Span::styled(format!("{indent}{name:<7}"), theme::bold()),
        Span::styled(format!("{bar_str} {pct_str:>4}"), q_style),
        Span::styled(format!("  {reset}"), theme::dim()),
    ])];

    if let Some(br) = &w.burn_rate
        && let Some(rate) = br.percent_per_hour
    {
        let mut burn_spans = vec![Span::styled(format!("{indent}       "), theme::plain())];
        if rate > 0.0 {
            let eta_text = br
                .eta_exhaustion_secs
                .map(|s| format!(" · ETA ~{}", eta_short(s)))
                .unwrap_or_default();
            burn_spans.push(Span::styled(
                format!("burn {rate:.1}%/h{eta_text}"),
                if br.exhausts_before_reset == Some(true) {
                    theme::warn()
                } else {
                    theme::dim()
                },
            ));
            if br.exhausts_before_reset == Some(true) {
                burn_spans.push(Span::styled(
                    " · ! exhausts before reset",
                    theme::bad().add_modifier(ratatui::style::Modifier::BOLD),
                ));
            }
        } else {
            burn_spans.push(Span::styled("burn 0.0%/h (steady)", theme::dim()));
        }
        out.push(Line::from(burn_spans));
    }
    out
}

fn group_card(
    g: &QuotaView,
    a: &AccountUsage,
    state: &AppState,
    width: usize,
    out: &mut Vec<Line<'static>>,
) {
    let framed = width >= 60;
    let bar_cells = if framed { 16 } else { 10 };
    let indent = if framed { "" } else { "  " };
    let mut body = Vec::new();
    if g.disabled {
        body.push(line(
            format!("{indent}not enabled for this account"),
            theme::dim(),
        ));
    } else {
        if g.exhausted {
            body.push(line(format!("{indent}[EXHAUSTED]"), theme::bad()));
        }
        match &g.five_hour {
            Some(w) => body.extend(window_lines("5H", w, a, state, bar_cells, indent)),
            None => body.push(line(format!("{indent}5H     not reported"), theme::dim())),
        }
        match &g.weekly {
            Some(w) => body.extend(window_lines("WEEKLY", w, a, state, bar_cells, indent)),
            None => body.push(line(format!("{indent}WEEKLY not reported"), theme::dim())),
        }
    }

    if framed {
        let title = if g.exhausted {
            format!("{} [EXHAUSTED]", g.label())
        } else if g.disabled {
            format!("{} [disabled]", g.label())
        } else {
            g.label()
        };
        let title_style = if g.exhausted {
            theme::bad().add_modifier(ratatui::style::Modifier::BOLD)
        } else if g.disabled {
            theme::dim()
        } else {
            theme::card_title()
        };
        out.extend(rounded_card_lines(&title, title_style, body, width.min(86)));
    } else {
        out.push(line(g.label(), theme::bold()));
        out.extend(body);
    }
}

/// All lines for the selected account at `width` columns.
pub fn lines_for_width(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let Some(a) = state.selected() else {
        let text = if state.data.is_none() {
            "Loading..."
        } else {
            "No account selected."
        };
        return vec![line(text, theme::dim())];
    };
    let mut out = Vec::new();
    let marker = if a.active { " ● (active)" } else { "" };
    out.push(Line::from(vec![
        Span::styled(format::sanitize(&a.alias), theme::bold()),
        Span::styled(marker, theme::active_marker()),
    ]));
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
        group_card(g, a, state, width, &mut out);
        out.push(Line::default());
    }
    out
}

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let paragraph =
        Paragraph::new(lines_for_width(state, usize::from(area.width))).wrap(Wrap { trim: false });
    let top = state.scroll.min(super::paragraph_max(&paragraph, area));
    f.render_widget(paragraph.scroll((rect_u16(top), 0)), area);
}
