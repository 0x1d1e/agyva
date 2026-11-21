//! Header (brand, pill tabs, active badges), suspension banner, and two-row
//! contextual footer (status/filter/context, keys).

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
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
    room = push_fit(&mut spans, " ◆ agyva ", theme::brand(), room);
    if area.width < 54 {
        room = push_fit(
            &mut spans,
            &format!("[{}]", state.view.title()),
            theme::tab_active(),
            room,
        );
        if state.switching || state.endpoint_busy || state.loading() {
            push_fit(&mut spans, " *", theme::warn(), room);
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
        return;
    }

    for v in View::ALL {
        let label = format!(" {} ", v.title());
        let style = if v == state.view {
            theme::tab_active()
        } else {
            theme::tab_inactive()
        };
        room = push_fit(&mut spans, &label, style, room);
        room = push_fit(&mut spans, " ", theme::plain(), room);
    }

    let mut badges: Vec<(String, ratatui::style::Style)> = Vec::new();
    let busy = if state.switching {
        Some("switching...")
    } else if state.endpoint_busy {
        Some("probing...")
    } else if state.loading() && state.data.is_none() {
        Some("loading...")
    } else if state.loading() {
        Some("refreshing...")
    } else {
        None
    };
    if let Some(b) = busy {
        badges.push((format!(" {b} "), theme::warn()));
    }
    if let Some(profile) = state.active_profile_name() {
        badges.push((format!(" ◈ {profile} "), theme::badge_profile()));
    }
    if let Some(d) = &state.data {
        if let Some(ep) = &d.active_endpoint {
            badges.push((format!(" ● ep:{ep} "), theme::badge_proto()));
        } else if let Some(acct) = &d.status.active {
            badges.push((format!(" ● {acct} "), theme::active_marker()));
        }
    }

    let badges_w: usize = badges.iter().map(|(t, _)| format::width(t)).sum();
    if room > badges_w {
        let pad = room - badges_w;
        spans.push(Span::raw(" ".repeat(pad)));
        room -= pad;
    }
    for (text, style) in badges {
        if room == 0 {
            break;
        }
        room = push_fit(&mut spans, &text, style, room);
    }

    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn idle_context_line(state: &AppState, width: usize) -> Line<'static> {
    let mut spans = Vec::new();
    let text = match (&state.data, state.view) {
        (None, _) => "Waiting for dashboard data...".to_owned(),
        (Some(d), View::Accounts) => format!(
            "Sort: {} [s]  ·  {}/{} account(s)",
            state.sort_order.label(),
            state.visible().len(),
            d.usage.accounts.len()
        ),
        (Some(_), View::Usage) => match state.selected() {
            Some(a) => format!("Usage: {}  ·  per-group quota & burn rate", a.alias),
            None => "Usage: no account selected".to_owned(),
        },
        (Some(d), View::Endpoints) => {
            let ready = d
                .endpoints
                .iter()
                .filter(|e| e.enabled && e.last_error.is_none())
                .count();
            format!(
                "Endpoints: {ready}/{} ready  ·  t test  f fetch models",
                d.endpoints.len()
            )
        }
        (Some(d), View::Routes) => {
            let prof = state
                .active_profile_name()
                .unwrap_or_else(|| "none".to_owned());
            format!(
                "Active profile: {prof}  ·  {} profile(s), {} route group(s)",
                d.profiles.len(),
                d.routing_groups.len()
            )
        }
        (Some(d), View::Diagnostics) => {
            let fails = d
                .doctor
                .checks
                .iter()
                .filter(|c| c.level == crate::core::doctor::Level::Fail)
                .count();
            let warns = d
                .doctor
                .checks
                .iter()
                .filter(|c| c.level == crate::core::doctor::Level::Warn)
                .count();
            if fails == 0 && warns == 0 {
                format!("Diagnostics: {} check(s) passing", d.doctor.checks.len())
            } else {
                format!(
                    "Diagnostics: {} check(s) ({fails} fail, {warns} warn)",
                    d.doctor.checks.len()
                )
            }
        }
    };
    push_fit(&mut spans, &text, theme::dim(), width);
    Line::from(spans)
}

pub(super) fn footer(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let w = usize::from(area.width);
    let mut lines = Vec::new();
    if area.height > 1 {
        let mut status = Vec::new();
        if state.filter_editing {
            push_fit(
                &mut status,
                &format!("/{}_  Enter keeps, Esc clears", state.filter),
                theme::accent_bold(),
                w,
            );
            lines.push(Line::from(status));
        } else if let Some(e) = &state.error {
            push_fit(&mut status, e, theme::bad(), w);
            lines.push(Line::from(status));
        } else if let Some(m) = &state.message {
            push_fit(&mut status, m, theme::warn(), w);
            lines.push(Line::from(status));
        } else if !state.filter.is_empty() {
            push_fit(
                &mut status,
                &format!(
                    "filter: {} ({} shown, Esc clears)",
                    state.filter,
                    state.visible_count()
                ),
                theme::accent_bold(),
                w,
            );
            lines.push(Line::from(status));
        } else {
            lines.push(idle_context_line(state, w));
        }
    }

    let mut hints = Vec::new();
    let essential = if state.filter_editing {
        "? help  ^C quit"
    } else {
        "? help  q quit"
    };
    let room = push_fit(&mut hints, essential, theme::bold(), w);
    let actions = match state.view {
        View::Accounts => {
            "  ↑↓ select  Enter switch  s sort  r refresh  d details  / filter  Tab views"
        }
        View::Usage => "  ↑↓ account  PgUp/PgDn scroll  r refresh  d details  Tab views",
        View::Endpoints => {
            "  ↑↓ select  Enter use  t test  f fetch  d details  / filter  Tab views"
        }
        View::Routes => "  ↑↓ select  Enter apply/use  d details  / filter  Tab views",
        View::Diagnostics => "  ↑↓ scroll  PgUp/PgDn page  r refresh  Tab views",
    };
    push_fit(&mut hints, actions, theme::dim(), room);
    lines.push(Line::from(hints));
    f.render_widget(Paragraph::new(lines), area);
}
