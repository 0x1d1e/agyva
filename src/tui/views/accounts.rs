//! The account table. Columns drop progressively as the terminal narrows:
//! cache age first, then WEEKLY, then STATUS, then 5H; the alias always stays.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use super::{line, push_fit};
use crate::core::format::{ago, bar, fit, percent_text};
use crate::core::usage::{AccountUsage, WindowView};
use crate::tui::app::AppState;
use crate::tui::theme;

const MARK_W: usize = 2;
const NUM_W: usize = 3;
const CACHE_W: usize = 17;
const BAR_W: usize = 11;
const STATUS_W: usize = 15;
const ALIAS_MIN: usize = 6;

/// Which optional columns fit, and how wide the alias column becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnPlan {
    pub five: bool,
    pub status: bool,
    pub weekly: bool,
    pub cache: bool,
    pub alias: usize,
}

/// Choose columns for `width` terminal columns (pure; tested directly).
pub fn column_plan(width: usize) -> ColumnPlan {
    let mut used = MARK_W + NUM_W + ALIAS_MIN;
    let mut take = |w: usize| {
        let ok = used + w < width;
        if ok {
            used += w + 1;
        }
        ok
    };
    let five = take(BAR_W);
    let status = five && take(STATUS_W);
    let weekly = status && take(BAR_W);
    let cache = weekly && take(CACHE_W);
    ColumnPlan {
        five,
        status,
        weekly,
        cache,
        alias: width.saturating_sub(used - ALIAS_MIN).max(1),
    }
}

fn bar_cell(w: Option<&WindowView>, live: bool) -> (String, ratatui::style::Style) {
    match w {
        Some(w) => (
            format!(
                "{} {:>4}",
                bar(w.remaining_percent, 4),
                percent_text(w.remaining_percent)
            ),
            theme::quota(w.remaining_percent, live),
        ),
        None => (fit("\u{2014}", BAR_W), theme::dim()),
    }
}

pub(super) fn row(
    a: &AccountUsage,
    plan: ColumnPlan,
    width: usize,
    state: &AppState,
) -> Line<'static> {
    let (five, weekly) = a.lowest();
    let live = a.refreshed_now && state.freshness(a) == crate::core::quota::Freshness::Fresh;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mark = if a.active { "\u{25cf}" } else { " " };
    let alarm = if a.health.state.is_alarming() {
        "!"
    } else {
        " "
    };
    let mut room = width;
    room = push_fit(&mut spans, mark, theme::active_marker(), room);
    room = push_fit(&mut spans, alarm, theme::health(a.health.state), room);
    room = push_fit(&mut spans, &format!("{:>2} ", a.number), theme::dim(), room);
    let alias_style = if a.active {
        theme::bold()
    } else {
        theme::plain()
    };
    room = push_fit(&mut spans, &fit(&a.alias, plan.alias), alias_style, room);
    if plan.five {
        let (t, st) = bar_cell(five, live);
        room = push_fit(&mut spans, &format!(" {}", fit(&t, BAR_W)), st, room);
    }
    if plan.status {
        let st = theme::health(a.health.state);
        room = push_fit(
            &mut spans,
            &format!(" {}", fit(a.health.label, STATUS_W)),
            st,
            room,
        );
    }
    if plan.weekly {
        let (t, st) = bar_cell(weekly, live);
        room = push_fit(&mut spans, &format!(" {}", fit(&t, BAR_W)), st, room);
    }
    if plan.cache {
        let age = a.observed_at.map_or_else(
            || "no reading".to_owned(),
            |at| format!("{} {}", state.freshness(a).label(), ago(state.now, at)),
        );
        push_fit(
            &mut spans,
            &format!(" {}", fit(&age, CACHE_W)),
            theme::dim(),
            room,
        );
    }
    Line::from(spans)
}

fn header_row(plan: ColumnPlan) -> Line<'static> {
    let mut t = format!("{}{}", fit("", MARK_W + NUM_W), fit("ACCOUNT", plan.alias));
    if plan.five {
        t.push_str(&format!(" {}", fit("5H LEFT", BAR_W)));
    }
    if plan.status {
        t.push_str(&format!(" {}", fit("STATUS", STATUS_W)));
    }
    if plan.weekly {
        t.push_str(&format!(" {}", fit("WEEKLY LEFT", BAR_W)));
    }
    if plan.cache {
        t.push_str(&format!(" {}", fit("CACHE / AGE", CACHE_W)));
    }
    line(t, theme::dim())
}

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let width = usize::from(area.width);
    let height = usize::from(area.height);
    let visible = state.visible();
    if visible.is_empty() {
        let text = if state.total() == 0 {
            if state.data.is_none() {
                "Loading accounts..."
            } else {
                "No saved accounts. Run `agyva add` while agy is logged in."
            }
        } else {
            "No account matches the filter."
        };
        f.render_widget(
            Paragraph::new(line(text, theme::dim())).wrap(ratatui::widgets::Wrap { trim: true }),
            area,
        );
        return;
    }
    let plan = column_plan(width);
    let with_header = height >= 4;
    let rows = height.saturating_sub(if with_header { 2 } else { 0 });
    let sel = state.selected_index().unwrap_or(0);
    // Keep the selection roughly centered once the list outgrows the area.
    let offset = sel
        .saturating_sub(rows / 2)
        .min(visible.len().saturating_sub(rows));
    let mut lines = Vec::new();
    if with_header {
        lines.push(header_row(plan));
        lines.push(line(
            "REMAINING: minimum per window across enabled groups; — not reported. Tab for groups.",
            theme::dim(),
        ));
    }
    for (i, a) in visible.iter().enumerate().skip(offset).take(rows) {
        let mut l = row(a, plan, width, state);
        if i == sel {
            l = l.style(theme::selected());
        }
        lines.push(l);
    }
    f.render_widget(Paragraph::new(lines), area);
}
