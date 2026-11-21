//! Environment and health diagnostics: framed environment cards (`agyva`, `agy`,
//! `backend`, `vault`, `endpoints`, `profiles`, `checkpoints`), pending
//! recovery, suspensions, and `[ OK ]`/`[INFO]`/`[WARN]`/`[FAIL]` check rows.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use super::{line, rect_u16, rounded_card_lines};
use crate::core::doctor::Level;
use crate::tui::app::AppState;
use crate::tui::theme;

pub fn lines_for_width(state: &AppState, width: usize) -> Vec<Line<'static>> {
    let Some(d) = &state.data else {
        return vec![line("Loading...", theme::dim())];
    };
    let st = &d.status;
    let framed = width >= 60;
    let card_w = width.min(86);

    let mut runtime = vec![
        line(format!("agyva: {}", st.agyva_version), theme::bold()),
        line(format!("agy: {}", st.agy), theme::plain()),
    ];
    if let Some(p) = &st.agy_path {
        runtime.push(line(format!("agy path: {p}"), theme::dim()));
    }
    runtime.push(line(
        format!("quota: {}", st.quota_provider),
        theme::plain(),
    ));

    let pinned_ckpts = d.checkpoints.iter().filter(|c| c.pinned).count();
    let security = vec![
        line(
            format!("credential backend: {}", st.backend),
            theme::plain(),
        ),
        line(format!("vault: {}", st.vault), theme::plain()),
        line(format!("accounts: {}", st.accounts), theme::plain()),
        line(
            format!(
                "checkpoints: {} saved ({pinned_ckpts} pinned)",
                d.checkpoints.len()
            ),
            theme::plain(),
        ),
    ];

    let ep_active = d.active_endpoint.as_deref().unwrap_or("none");
    let prof_active = state.active_profile_name();
    let integrations = vec![
        line(
            format!(
                "endpoints: {} configured (active: {ep_active})",
                d.endpoints.len()
            ),
            theme::plain(),
        ),
        line(
            format!(
                "profiles: {} saved (active: {})",
                d.profiles.len(),
                prof_active.as_deref().unwrap_or("none")
            ),
            theme::plain(),
        ),
        line(
            format!("routes: {} group(s)", d.routing_groups.len()),
            theme::plain(),
        ),
    ];

    let mut out = Vec::new();
    if framed {
        out.extend(rounded_card_lines(
            "Runtime (agyva & agy)",
            theme::card_title(),
            runtime,
            card_w,
        ));
        out.extend(rounded_card_lines(
            "Security & Checkpoints",
            theme::card_title(),
            security,
            card_w,
        ));
        out.extend(rounded_card_lines(
            "Endpoints & Profiles",
            theme::card_title(),
            integrations,
            card_w,
        ));
    } else {
        out.extend(runtime);
        out.extend(security);
        out.extend(integrations);
    }

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
    out.push(line("Checks", theme::card_title()));
    for c in &d.doctor.checks {
        let tag = match c.level {
            Level::Ok => "[ OK ]",
            Level::Info => "[INFO]",
            Level::Warn => "[WARN]",
            Level::Fail => "[FAIL]",
        };
        let style = theme::doctor_level(c.level);
        out.push(Line::from(vec![
            Span::styled(tag, style),
            Span::styled(
                format!(" {}: {}", c.name, crate::core::format::sanitize(&c.detail)),
                if c.level == Level::Fail {
                    style
                } else {
                    theme::plain()
                },
            ),
        ]));
        if let Some(h) = &c.hint {
            out.push(line(format!("       hint: {h}"), theme::dim()));
        }
    }
    out
}

pub(super) fn render(state: &AppState, f: &mut Frame<'_>, area: Rect) {
    let paragraph =
        Paragraph::new(lines_for_width(state, usize::from(area.width))).wrap(Wrap { trim: false });
    let top = state.scroll.min(super::paragraph_max(&paragraph, area));
    f.render_widget(paragraph.scroll((rect_u16(top), 0)), area);
}
