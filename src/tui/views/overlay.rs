//! Modal popups: help, account details and the switch confirmation.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::{line, rect_u16};
use crate::core::format;
use crate::tui::app::{AppState, Overlay};
use crate::tui::theme;

const HELP: &[&str] = &[
    "Tab / Shift-Tab, F1-F3   switch view",
    "Up / Down, j / k         select; scroll in Diagnostics",
    "1-9                      select an account (Accounts / Usage)",
    "Enter                    confirm switch (Accounts / Usage)",
    "r                        refresh the active account",
    "d                        account details",
    "/                        filter (Esc clears)",
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

pub fn details_lines(state: &AppState) -> Vec<Line<'static>> {
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
        Some(Overlay::Details) => (" Details ", details_lines(state)),
        Some(Overlay::Confirm { alias, .. }) => (" Confirm ", confirm_lines(state, alias)),
        None => ("", Vec::new()),
    }
}

fn geometry(
    paragraph: &Paragraph<'_>,
    widest: usize,
    area: Rect,
) -> (Rect, Block<'static>, Rect, Rect, usize) {
    let width = rect_u16(widest.clamp(24, 76) + 2).min(area.width);
    let bordered = width >= 20 && area.height >= 5;
    let border = if bordered { 2 } else { 0 };
    let count = paragraph.line_count(width.saturating_sub(border));
    let rect = popup_rect(
        area,
        width,
        rect_u16(count.saturating_add(1 + usize::from(border))),
    );
    let block = if bordered {
        Block::default().borders(Borders::ALL)
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
    f.render_widget(block.title(title), rect);
    f.render_widget(paragraph.scroll((rect_u16(top), 0)), body);
    let hint = if matches!(overlay, Overlay::Confirm { .. }) {
        if controls.width >= 28 {
            "y switch  n / Esc cancel"
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
