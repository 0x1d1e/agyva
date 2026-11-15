//! Environment and health diagnostics: agy version and compatibility, the
//! credential backend, doctor findings, pending recovery and suspensions.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Wrap};

use super::{line, rect_u16};
use crate::core::doctor::Level;
use crate::tui::app::AppState;
use crate::tui::theme;

pub fn lines(state: &AppState) -> Vec<Line<'static>> {
    let Some(d) = &state.data else {
        return vec![line("Loading...", theme::dim())];
    };
    let st = &d.status;
    let mut out = vec![
        line(format!("agyva {}", st.agyva_version), theme::bold()),
        line(format!("agy: {}", st.agy), theme::plain()),
    ];
    if let Some(p) = &st.agy_path {
        out.push(line(format!("agy path: {p}"), theme::dim()));
    }
    out.push(line(
        format!("quota: {}", st.quota_provider),
        theme::plain(),
    ));
    out.push(line(
        format!("credential backend: {}", st.backend),
        theme::plain(),
    ));
    out.push(line(format!("vault: {}", st.vault), theme::plain()));
    out.push(line(format!("accounts: {}", st.accounts), theme::plain()));
    if let Some(w) = state.sessions_warning() {
        out.push(line(format!("Running agy: {w}"), theme::warn()));
    }
    if let Some(p) = &st.recovery_pending {
        out.push(Line::default());
        out.push(line(
            format!(
                "RECOVERY REQUIRED: interrupted switch to {} (phase {}). Quit and run `agyva recover`.",
                p.target, p.phase
            ),
            theme::bad(),
        ));
    }
    for a in state.suspended() {
        out.push(Line::default());
        out.push(line(format!("SUSPENDED: {}", a.alias), theme::bad()));
        if let Some(w) = &a.health.warning {
            for l in w.lines() {
                out.push(line(format!("  {l}"), theme::warn()));
            }
        }
    }
    out.push(Line::default());
    out.push(line("Checks", theme::bold()));
    for c in &d.doctor.checks {
        let (tag, style) = match c.level {
            Level::Ok => ("ok  ", theme::good()),
            Level::Info => ("info", theme::dim()),
            Level::Warn => ("warn", theme::warn()),
            Level::Fail => ("FAIL", theme::bad()),
        };
        out.push(line(format!("{tag} {}: {}", c.name, c.detail), style));
        if let Some(h) = &c.hint {
            out.push(line(format!("     hint: {h}"), theme::dim()));
        }
    }
    out
}

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let all = lines(state);
    let max = all.len().saturating_sub(usize::from(area.height));
    let top = state.scroll.min(max);
    f.render_widget(
        Paragraph::new(all)
            .wrap(Wrap { trim: false })
            .scroll((rect_u16(top), 0)),
        area,
    );
}
