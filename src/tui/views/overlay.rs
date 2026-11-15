//! Modal popups: help, account details and the switch confirmation.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};

use super::line;
use crate::core::format;
use crate::tui::app::{AppState, Overlay};
use crate::tui::theme;

const HELP: &[&str] = &[
    "Tab / Shift-Tab, F1-F3   switch view",
    "Up / Down, j / k         move (wraps around)",
    "1-9                      select that account",
    "Enter                    switch to the selected account",
    "r                        refresh the active account",
    "d                        account details",
    "/                        filter (Esc clears)",
    "PageUp / PageDown        scroll Usage and Diagnostics",
    "q / Esc                  quit or close this window",
    "",
    "agyva never switches by itself and never rotates accounts.",
    "Quota can only be read for the active account; others show",
    "their last cached reading, labelled as such.",
];

pub fn confirm_lines(state: &AppState, alias: &str) -> Vec<Line<'static>> {
    let mut out = vec![
        line(
            format!("Switch agy to {}?", format::sanitize(alias)),
            theme::bold(),
        ),
        Line::default(),
    ];
    match state.sessions_warning() {
        Some(w) => out.push(line(format!("Warning: {w}"), theme::warn())),
        None => out.push(line(
            "Restart agy after switching; running sessions keep their old login.",
            theme::plain(),
        )),
    }
    out.push(Line::default());
    out.push(line("y = switch    n / Esc = cancel", theme::bold()));
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
            format!("Plan:     {}", a.plan.as_deref().unwrap_or("unknown")),
            theme::plain(),
        ),
        line(
            format!("Health:   {} ({})", a.health.label, a.health.evidence),
            theme::health(a.health.state),
        ),
        line(format!("Data:     {}", a.freshness.label()), theme::plain()),
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

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let Some(overlay) = &state.overlay else {
        return;
    };
    let (title, lines) = match overlay {
        Overlay::Help => (
            " Help ",
            HELP.iter().map(|l| line(*l, theme::plain())).collect(),
        ),
        Overlay::Details => (" Details ", details_lines(state)),
        Overlay::Confirm { alias, .. } => (" Confirm ", confirm_lines(state, alias)),
    };
    let want_w = u16::try_from(
        lines
            .iter()
            .map(Line::width)
            .max()
            .unwrap_or(10)
            .clamp(20, 76)
            + 4,
    )
    .unwrap_or(80);
    let inner_w = usize::from(want_w.min(area.width).saturating_sub(4)).max(1);
    let wrapped: usize = lines
        .iter()
        .map(|l| l.width().div_ceil(inner_w).max(1))
        .sum();
    let want_h = u16::try_from(wrapped + 2).unwrap_or(u16::MAX);
    let rect = popup_rect(area, want_w, want_h);
    f.render_widget(Clear, rect);
    let mut p = Paragraph::new(lines).wrap(Wrap { trim: false });
    if rect.width >= 8 && rect.height >= 4 {
        p = p.block(Block::default().borders(Borders::ALL).title(title));
    }
    f.render_widget(p, rect);
}
