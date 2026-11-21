//! Rounded modal popups: help, context-sensitive details (Accounts/Usage,
//! Endpoints, or Routes/Profiles), and action confirmations.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use super::endpoints::protocol_badge;
use super::{line, rect_u16};
use crate::core::format::{self, percent_text};
use crate::core::profile::ProfileSource;
use crate::tui::app::{AppState, Overlay, RouteItemKind, View};
use crate::tui::theme;

const HELP: &[&str] = &[
    "Tab / Shift-Tab, F1-F5   switch view (5 tabs)",
    "Up / Down, j / k         select item; scroll in Diagnostics",
    "1-9                      quick-select item on list views",
    "Enter                    confirm switch / endpoint / route / profile",
    "s                        cycle sort order (Accounts)",
    "t / f                    test connection / fetch models (Endpoints)",
    "r                        refresh the active account",
    "d                        context-sensitive details",
    "/                        filter (Accounts, Endpoints, Routes; Esc clears)",
    "PageUp / PageDown        scroll Usage and Diagnostics",
    "Up / Down, PageUp/Down   scroll Help and Details",
    "Home / End               start / end of this window",
    "q / Esc                  close a window; quit from a view",
    "Ctrl+C                   quit, including while filtering",
    "",
    "Only the active account can refresh; others use the cache.",
    "Remaining: minimum reported window across enabled groups.",
    "Missing windows are not reported, never assumed to be full.",
    "Switching is manual. Restart agy after each switch.",
];

pub fn confirm_lines(state: &AppState, alias: &str) -> Vec<Line<'static>> {
    let mut out = vec![line(
        format!("Switch to {}?", format::sanitize(alias)),
        theme::bold(),
    )];
    match state.sessions_warning() {
        Some(w) => out.push(line(format!("Warning: {w}"), theme::warn())),
        None => out.push(line("Restart agy next.", theme::plain())),
    }
    out.push(line(
        "Running sessions keep their old login.",
        theme::plain(),
    ));
    out
}

fn confirm_endpoint_lines(state: &AppState, alias: &str) -> Vec<Line<'static>> {
    let mut out = vec![line(
        format!("Activate endpoint {}?", format::sanitize(alias)),
        theme::bold(),
    )];
    out.push(line(
        "Syncs agy custom provider settings to this endpoint.",
        theme::plain(),
    ));
    if let Some(w) = state.sessions_warning() {
        out.push(line(format!("Warning: {w}"), theme::warn()));
    } else {
        out.push(line("Restart agy next.", theme::plain()));
    }
    out
}

fn confirm_route_lines(state: &AppState, group: &str, target: Option<&str>) -> Vec<Line<'static>> {
    let mut out = vec![line(
        format!("Apply route group `{}`?", format::sanitize(group)),
        theme::bold(),
    )];
    if let Some(t) = target {
        out.push(line(
            format!("Recommended account: {}", format::sanitize(t)),
            theme::good(),
        ));
    }
    match state.sessions_warning() {
        Some(w) => out.push(line(format!("Warning: {w}"), theme::warn())),
        None => out.push(line("Restart agy next.", theme::plain())),
    }
    out
}

fn confirm_profile_lines(state: &AppState, name: &str) -> Vec<Line<'static>> {
    let mut out = vec![line(
        format!("Activate profile `{}`?", format::sanitize(name)),
        theme::bold(),
    )];
    if let Some(d) = &state.data
        && let Some(p) = d.profiles.iter().find(|p| p.name == name)
    {
        if let Some(a) = &p.account {
            out.push(line(format!("Account:  {a}"), theme::plain()));
        }
        if let Some(ep) = &p.endpoint {
            out.push(line(format!("Endpoint: {ep}"), theme::plain()));
        }
        if let Some(g) = &p.group {
            out.push(line(format!("Group:    {g}"), theme::plain()));
        }
    }
    if let Some(w) = state.sessions_warning() {
        out.push(line(format!("Warning: {w}"), theme::warn()));
    } else {
        out.push(line("Restart agy next.", theme::plain()));
    }
    out
}

pub fn details_lines(state: &AppState) -> Vec<Line<'static>> {
    match state.view {
        View::Endpoints => endpoint_details_lines(state),
        View::Routes => route_details_lines(state),
        View::Accounts | View::Usage | View::Diagnostics => account_details_lines(state),
    }
}

fn account_details_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(a) = state.selected() else {
        return vec![line("No account selected.", theme::dim())];
    };
    let when = |t: Option<chrono::DateTime<chrono::Utc>>| match t {
        Some(t) => format::ago(state.now, t),
        None => "never".to_owned(),
    };
    let mut out = vec![
        line(format!("Alias:    {}", a.alias), theme::bold()),
        line(
            format!("Email:    {}", a.email.as_deref().unwrap_or("unknown")),
            theme::plain(),
        ),
        line(format!("Id:       {}", a.id), theme::plain()),
        line(
            format!("Active:   {}", if a.active { "yes" } else { "no" }),
            theme::plain(),
        ),
        line(
            format!(
                "Plan:     {}",
                a.plan.as_deref().unwrap_or("not reported by agy")
            ),
            theme::plain(),
        ),
        line(
            format!(
                "Health:   {} ({})",
                a.health.label,
                state.health_evidence(a)
            ),
            theme::health(a.health.state),
        ),
        line(
            format!(
                "Data:     {}; observed {}",
                state.freshness(a).label(),
                when(a.observed_at)
            ),
            theme::plain(),
        ),
        line(
            format!("Last successful update: {}", when(a.last_successful_update)),
            theme::plain(),
        ),
        line(
            format!("Last attempt:           {}", when(a.last_attempt)),
            theme::plain(),
        ),
    ];
    if let Some(u) = &a.unavailable {
        let (label, style) = if u.reason == crate::core::quota::UnavailableReason::TooSoon {
            ("Note", theme::dim())
        } else {
            ("Unavailable", theme::warn())
        };
        out.push(line(format!("{label}: {}", u.text()), style));
    }
    if let Some(w) = &a.health.warning {
        out.push(Line::default());
        out.push(line(w.clone(), theme::bad()));
    }
    out
}

fn endpoint_details_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(ep) = state.selected_endpoint() else {
        return vec![line("No endpoint selected.", theme::dim())];
    };
    let mut out = vec![
        line(
            format!("Alias:    {} {}", ep.alias, protocol_badge(ep)),
            theme::bold(),
        ),
        line(format!("Id:       {}", ep.id), theme::plain()),
        line(format!("URL:      {}", ep.base_url), theme::plain()),
        line(
            format!(
                "Protocol: {} (detected: {})",
                ep.protocol,
                ep.detected_protocol
                    .map_or_else(|| "none".to_owned(), |p| p.as_str().to_owned())
            ),
            theme::plain(),
        ),
        line(
            format!("Active:   {}", if ep.active { "yes" } else { "no" }),
            theme::plain(),
        ),
        line(
            format!("Enabled:  {}", if ep.enabled { "yes" } else { "no" }),
            theme::plain(),
        ),
        line(
            format!(
                "Default:  {}",
                ep.default_model.as_deref().unwrap_or("none")
            ),
            theme::plain(),
        ),
        line(
            format!(
                "Latency:  {}",
                ep.latency_ms
                    .map_or_else(|| "unknown".to_owned(), |ms| format!("{ms}ms"))
            ),
            theme::plain(),
        ),
        line(
            format!(
                "Fetched:  {}",
                ep.fetched_at
                    .map_or_else(|| "never".to_owned(), |t| format::ago(state.now, t))
            ),
            theme::dim(),
        ),
    ];
    if let Some(rl) = &ep.rate_limits
        && !rl.is_empty()
    {
        out.push(Line::default());
        out.push(line("Rate Limits & Credits", theme::card_title()));
        if let (Some(r), Some(l)) = (rl.requests_remaining, rl.requests_limit) {
            out.push(line(format!("  Requests: {r}/{l}"), theme::info()));
        }
        if let (Some(r), Some(l)) = (rl.tokens_remaining, rl.tokens_limit) {
            out.push(line(format!("  Tokens:   {r}/{l}"), theme::info()));
        }
        if let Some(c) = rl.credits_remaining {
            let lim = rl
                .credits_limit
                .map_or_else(String::new, |l| format!(" / ${l:.2}"));
            out.push(line(format!("  Credits:  ${c:.2}{lim}"), theme::info()));
        }
        if let Some(h) = &rl.reset_hint {
            out.push(line(format!("  Reset:    {h}"), theme::dim()));
        }
    }
    if let Some(err) = &ep.last_error {
        out.push(Line::default());
        out.push(line(format!("Last error: {err}"), theme::bad()));
    }
    out.push(Line::default());
    out.push(line(
        format!("Discovered Models ({})", ep.models.len()),
        theme::card_title(),
    ));
    if ep.models.is_empty() {
        out.push(line(
            "  No cached models. Press `f` on Endpoints to fetch.",
            theme::dim(),
        ));
    } else {
        for m in &ep.models {
            let mut desc = format!("  {}", m.id);
            if let Some(name) = &m.display_name
                && name != &m.id
            {
                desc.push_str(&format!(" ({name})"));
            }
            if let Some(ctx) = m.context_window {
                desc.push_str(&format!(" · ctx {ctx}"));
            }
            out.push(line(desc, theme::plain()));
        }
    }
    out
}

fn route_details_lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(item) = state.selected_route_item() else {
        return vec![line("No profile or route selected.", theme::dim())];
    };
    match item.kind {
        RouteItemKind::Profile(p) => {
            let src = match p.source {
                ProfileSource::Local => "local",
                ProfileSource::Global => "global",
                ProfileSource::Explicit => "explicit",
            };
            let mut out = vec![
                line(format!("Profile:  {}", p.name), theme::bold()),
                line(format!("Source:   {src}"), theme::plain()),
                line(
                    format!("Default:  {}", if p.is_default { "yes" } else { "no" }),
                    theme::plain(),
                ),
            ];
            if let Some(path) = &p.path {
                out.push(line(format!("Path:     {}", path.display()), theme::dim()));
            }
            if let Some(d) = &p.description {
                out.push(line(format!("Desc:     {d}"), theme::plain()));
            }
            out.push(line(
                format!("Account:  {}", p.account.as_deref().unwrap_or("—")),
                theme::plain(),
            ));
            out.push(line(
                format!("Endpoint: {}", p.endpoint.as_deref().unwrap_or("—")),
                theme::plain(),
            ));
            out.push(line(
                format!("Group:    {}", p.group.as_deref().unwrap_or("—")),
                theme::plain(),
            ));
            out.push(line(
                format!("Model:    {}", p.model.as_deref().unwrap_or("—")),
                theme::plain(),
            ));
            if let Some(w) = p.warn_percent {
                out.push(line(format!("Warn %:   {w:.0}%"), theme::dim()));
            }
            if let Some(c) = p.critical_percent {
                out.push(line(format!("Crit %:   {c:.0}%"), theme::dim()));
            }
            if let Some(iv) = &p.monitor_interval {
                out.push(line(format!("Monitor:  {iv}"), theme::dim()));
            }
            if !p.env.is_empty() {
                out.push(Line::default());
                out.push(line("Environment Overrides", theme::card_title()));
                for (k, v) in &p.env {
                    out.push(line(format!("  {k}={v}"), theme::dim()));
                }
            }
            out
        }
        RouteItemKind::Group(g) => {
            let mut out = vec![
                line(format!("Route Group: {}", g.name), theme::bold()),
                line(format!("Ranking:     {:?}", g.ranking), theme::plain()),
                line(
                    format!("Auto-switch: {}", if g.auto_switch { "yes" } else { "no" }),
                    theme::plain(),
                ),
            ];
            out.push(Line::default());
            out.push(line("Candidates", theme::card_title()));
            if g.plan.accounts.is_empty() {
                out.push(line("  No candidate accounts.", theme::dim()));
            }
            for (i, r) in g.plan.accounts.iter().enumerate() {
                let status = if r.eligible { "OK" } else { "EXCLUDED" };
                let share = r
                    .remaining_percent
                    .map_or_else(|| "—".to_owned(), percent_text);
                let style = if r.eligible {
                    theme::good()
                } else {
                    theme::warn()
                };
                out.push(line(
                    format!("  #{:<2} [{status}] {}  ({share})", i + 1, r.alias),
                    style,
                ));
                out.push(line(format!("      {}", r.why), theme::dim()));
            }
            out
        }
    }
}

/// Centered popup inside `area`; no border when the area is too small for one.
fn popup_rect(area: Rect, want_w: u16, want_h: u16) -> Rect {
    let w = want_w.min(area.width);
    let h = want_h.min(area.height);
    Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    }
}

fn content(state: &AppState) -> (&'static str, Vec<Line<'static>>) {
    match &state.overlay {
        Some(Overlay::Help) => (
            " Help ",
            HELP.iter().map(|l| line(*l, theme::plain())).collect(),
        ),
        Some(Overlay::Details) => {
            let title = match state.view {
                View::Endpoints => " Endpoint Details ",
                View::Routes => " Route / Profile Details ",
                _ => " Account Details ",
            };
            (title, details_lines(state))
        }
        Some(Overlay::Confirm { alias, .. }) => (" Confirm ", confirm_lines(state, alias)),
        Some(Overlay::ConfirmEndpoint { alias, .. }) => {
            (" Confirm Endpoint ", confirm_endpoint_lines(state, alias))
        }
        Some(Overlay::ConfirmRoute { group, target }) => (
            " Confirm Route ",
            confirm_route_lines(state, group, target.as_deref()),
        ),
        Some(Overlay::ConfirmProfile { name }) => {
            (" Confirm Profile ", confirm_profile_lines(state, name))
        }
        None => ("", Vec::new()),
    }
}

fn geometry(
    paragraph: &Paragraph<'_>,
    widest: usize,
    area: Rect,
) -> (Rect, Block<'static>, Rect, Rect, usize) {
    let width = rect_u16(widest.clamp(24, 78) + 2).min(area.width);
    let bordered = width >= 20 && area.height >= 5;
    let border = if bordered { 2 } else { 0 };
    let count = paragraph.line_count(width.saturating_sub(border));
    let rect = popup_rect(
        area,
        width,
        rect_u16(count.saturating_add(1 + usize::from(border))),
    );
    let block = if bordered {
        Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(theme::border_focus())
    } else {
        Block::default()
    };
    let [body, controls] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(block.inner(rect));
    let max = count.saturating_sub(usize::from(body.height));
    (rect, block, body, controls, max)
}

pub(super) fn max_scroll(state: &AppState, area: Rect) -> usize {
    let (_, lines) = content(state);
    let widest = lines.iter().map(Line::width).max().unwrap_or(20);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    geometry(&paragraph, widest, area).4
}

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let Some(overlay) = &state.overlay else {
        return;
    };
    let (title, lines) = content(state);
    let widest = lines.iter().map(Line::width).max().unwrap_or(20);
    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let (rect, block, body, controls, max) = geometry(&paragraph, widest, area);
    let top = state.overlay_scroll.min(max);
    f.render_widget(Clear, rect);
    f.render_widget(block.title(Span::styled(title, theme::card_title())), rect);
    f.render_widget(paragraph.scroll((rect_u16(top), 0)), body);
    let is_confirm = matches!(
        overlay,
        Overlay::Confirm { .. }
            | Overlay::ConfirmEndpoint { .. }
            | Overlay::ConfirmRoute { .. }
            | Overlay::ConfirmProfile { .. }
    );
    let hint = if is_confirm {
        if controls.width >= 28 {
            "y confirm  n / Esc cancel"
        } else if controls.width >= 13 {
            "↑↓ y / Esc"
        } else {
            "y / Esc"
        }
    } else if controls.width >= 29 {
        "↑↓ PgUp/PgDn scroll  Esc close"
    } else if controls.width >= 13 {
        "↑↓  Esc close"
    } else {
        "Esc close"
    };
    f.render_widget(Paragraph::new(line(hint, theme::bold())), controls);
}
