//! The account table with a responsive Summary Metric Strip and rounded table
//! container on wide terminals. Columns drop progressively as the terminal
//! narrows: eta -> cache age -> WEEKLY -> STATUS -> 5H; the alias always stays.

use chrono::{DateTime, Utc};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Paragraph};

use super::{line, push_fit, smooth_bar, window_burn_eta};
use crate::core::format::{self, ago, fit, percent_text};
use crate::core::usage::{AccountUsage, WindowView};
use crate::tui::app::AppState;
use crate::tui::theme;

const MARK_W: usize = 2;
const NUM_W: usize = 3;
const CACHE_W: usize = 17;
const BAR_W: usize = 11;
const STATUS_W: usize = 15;
const ETA_W: usize = 8;
const ALIAS_MIN: usize = 6;

/// Which optional columns fit, and how wide the alias column becomes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ColumnPlan {
    pub five: bool,
    pub status: bool,
    pub weekly: bool,
    pub cache: bool,
    pub eta: bool,
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
    let eta = if cache && used + ETA_W + 9 < width {
        used += ETA_W + 1;
        true
    } else {
        false
    };
    ColumnPlan {
        five,
        status,
        weekly,
        cache,
        eta,
        alias: width.saturating_sub(used - ALIAS_MIN).max(1),
    }
}

fn bar_cell(w: Option<&WindowView>, live: bool) -> (String, Style) {
    match w {
        Some(w) => (
            format!(
                "{} {:>4}",
                smooth_bar(w.remaining_percent, 4),
                percent_text(w.remaining_percent)
            ),
            theme::quota(w.remaining_percent, live),
        ),
        None => (fit("\u{2014}", BAR_W), theme::dim()),
    }
}

fn account_eta_cell(five: Option<&WindowView>, weekly: Option<&WindowView>) -> (String, Style) {
    let mut best: Option<(u64, String, bool)> = None;
    for w in [five, weekly].into_iter().flatten() {
        if let Some(br) = w.burn_rate.as_ref()
            && let Some(secs) = br.eta_exhaustion_secs
            && let Some((label, urgent)) = window_burn_eta(w)
            && best.as_ref().is_none_or(|(b, _, _)| secs < *b)
        {
            best = Some((secs, label, urgent));
        }
    }
    match best {
        Some((_, label, true)) => (format!("{label}!"), theme::bad()),
        Some((_, label, false)) => (label, theme::warn()),
        None => ("\u{2014}".to_owned(), theme::dim()),
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
        room = push_fit(
            &mut spans,
            &format!(" {}", fit(&age, CACHE_W)),
            theme::dim(),
            room,
        );
    }
    if plan.eta {
        let (t, st) = account_eta_cell(five, weekly);
        push_fit(&mut spans, &format!(" {}", fit(&t, ETA_W)), st, room);
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
    if plan.eta {
        t.push_str(&format!(" {}", fit("BURN ETA", ETA_W)));
    }
    line(t, theme::dim())
}

fn mini_card(f: &mut Frame<'_>, area: Rect, title: &str, value: String, value_style: Style) {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(theme::border())
        .title(Span::styled(format!(" {title} "), theme::card_title()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    if inner.width > 0 && inner.height > 0 {
        let mut spans = Vec::new();
        push_fit(&mut spans, &value, value_style, usize::from(inner.width));
        f.render_widget(Paragraph::new(Line::from(spans)), inner);
    }
}

fn lowest_window_summary(state: &AppState) -> (String, Style) {
    let target = state
        .accounts()
        .iter()
        .find(|a| a.active)
        .or_else(|| state.accounts().first());
    let Some(acct) = target else {
        return ("\u{2014}".to_owned(), theme::dim());
    };
    let (five, weekly) = acct.lowest();
    let best = match (five, weekly) {
        (Some(a), Some(b)) => {
            if a.remaining_percent <= b.remaining_percent {
                Some(a)
            } else {
                Some(b)
            }
        }
        (Some(a), None) | (None, Some(a)) => Some(a),
        (None, None) => None,
    };
    let Some(w) = best else {
        return ("\u{2014}".to_owned(), theme::dim());
    };
    let live = acct.refreshed_now && state.freshness(acct) == crate::core::quota::Freshness::Fresh;
    let pct = percent_text(w.remaining_percent);
    match window_burn_eta(w) {
        Some((eta, true)) => (format!("{pct} ({eta}!)"), theme::bad()),
        Some((eta, false)) => (
            format!("{pct} ({eta})"),
            theme::quota(w.remaining_percent, live),
        ),
        None => (pct, theme::quota(w.remaining_percent, live)),
    }
}

fn next_reset_summary(state: &AppState) -> (String, Style) {
    let active = state.accounts().iter().find(|a| a.active);
    let pool: Vec<&AccountUsage> = match active {
        Some(a) => vec![a],
        None => state.accounts().iter().collect(),
    };
    let soonest: Option<DateTime<Utc>> = pool
        .into_iter()
        .flat_map(|a| a.groups.iter().filter(|g| !g.disabled))
        .flat_map(|g| [g.five_hour.as_ref(), g.weekly.as_ref()])
        .flatten()
        .filter_map(|w| w.reset_at)
        .filter(|t| *t > state.now)
        .min();
    match soonest {
        Some(t) => (format::until(state.now, t), theme::info()),
        None => ("\u{2014}".to_owned(), theme::dim()),
    }
}

fn render_summary_strip(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let Some(d) = &state.data else {
        return;
    };
    let active_text = if let Some(ep) = &d.active_endpoint {
        (format!("ep:{ep}"), theme::badge_proto())
    } else if let Some(acct) = &d.status.active {
        (format!("\u{25cf} {acct}"), theme::active_marker())
    } else {
        ("none".to_owned(), theme::dim())
    };
    let (lowest_text, lowest_style) = lowest_window_summary(state);

    if area.width >= 96 {
        let cols = Layout::horizontal([
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
            Constraint::Ratio(1, 4),
        ])
        .areas::<4>(area);
        let ready = d
            .endpoints
            .iter()
            .filter(|e| e.enabled && e.last_error.is_none())
            .count();
        let ep_text = if d.endpoints.is_empty() {
            ("0 configured".to_owned(), theme::dim())
        } else {
            (
                format!("{ready}/{} ready", d.endpoints.len()),
                if ready > 0 {
                    theme::good()
                } else {
                    theme::warn()
                },
            )
        };
        let (reset_text, reset_style) = next_reset_summary(state);
        mini_card(f, cols[0], "Active Target", active_text.0, active_text.1);
        mini_card(f, cols[1], "Lowest & Burn", lowest_text, lowest_style);
        mini_card(f, cols[2], "Endpoints", ep_text.0, ep_text.1);
        mini_card(f, cols[3], "Next Reset", reset_text, reset_style);
    } else {
        let cols =
            Layout::horizontal([Constraint::Ratio(1, 2), Constraint::Ratio(1, 2)]).areas::<2>(area);
        mini_card(f, cols[0], "Active Target", active_text.0, active_text.1);
        mini_card(f, cols[1], "Lowest & Burn", lowest_text, lowest_style);
    }
}

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
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

    let table_area = if area.width >= 60 && area.height >= 12 && state.data.is_some() {
        let [strip, rest] =
            Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).areas(area);
        render_summary_strip(state, f, strip);
        rest
    } else {
        area
    };

    let bordered = table_area.width >= 60 && table_area.height >= 8;
    let inner = if bordered {
        let title = format!(
            " Accounts ({}/{}) · sort:{} ",
            visible.len(),
            state.total(),
            state.sort_order.label()
        );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border())
            .title(Span::styled(title, theme::card_title()));
        let inner = block.inner(table_area);
        f.render_widget(block, table_area);
        inner
    } else {
        table_area
    };

    let width = usize::from(inner.width);
    let height = usize::from(inner.height);
    if width == 0 || height == 0 {
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
    f.render_widget(Paragraph::new(lines), inner);
}
