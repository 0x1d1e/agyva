//! Styles. ANSI named colors only (so the terminal's own palette applies),
//! no gradients, no animation. Meaning never depends on color alone: every
//! colored thing also carries text.

use ratatui::style::{Color, Modifier, Style};

use crate::core::health::AccountHealth;

pub fn plain() -> Style {
    Style::default()
}

pub fn bold() -> Style {
    Style::default().add_modifier(Modifier::BOLD)
}

pub fn dim() -> Style {
    Style::default().add_modifier(Modifier::DIM)
}

pub fn selected() -> Style {
    Style::default().add_modifier(Modifier::REVERSED)
}

pub fn tab_active() -> Style {
    Style::default().add_modifier(Modifier::REVERSED | Modifier::BOLD)
}

pub fn good() -> Style {
    Style::default().fg(Color::Green)
}

pub fn warn() -> Style {
    Style::default().fg(Color::Yellow)
}

pub fn bad() -> Style {
    Style::default().fg(Color::Red)
}

pub fn active_marker() -> Style {
    Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD)
}

/// White on red, bold: the suspension banner.
pub fn banner() -> Style {
    Style::default()
        .fg(Color::White)
        .bg(Color::Red)
        .add_modifier(Modifier::BOLD)
}

/// Color quota only while a reading from this session is still fresh.
/// Cached and older readings retain their explicit age in the view.
pub fn quota(percent: f64, live: bool) -> Style {
    if !live {
        return dim();
    }
    if percent < 20.0 {
        bad()
    } else if percent < 50.0 {
        warn()
    } else {
        good()
    }
}

pub fn health(state: AccountHealth) -> Style {
    match state {
        AccountHealth::Healthy => good(),
        AccountHealth::TermsSuspended => bad().add_modifier(Modifier::BOLD),
        s if s.is_alarming() => bad(),
        AccountHealth::QuotaLimited | AccountHealth::NetworkUnavailable => warn(),
        _ => dim(),
    }
}
