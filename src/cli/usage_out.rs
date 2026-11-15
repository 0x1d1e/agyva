//! Human rendering of usage, health and watch events.

use chrono::{DateTime, Utc};

use super::output::Style;
use crate::core::format::{ago, bar, fit, percent_text, truncate, until};
use crate::core::health::AccountHealth;
use crate::core::health_report::HealthReport;
use crate::core::usage::{AccountUsage, QuotaView, UsageReport, WindowView};
use crate::core::watch::{StopReason, WatchEvent};

const BAR_CELLS: usize = 10;

fn window(label: &str, w: Option<&WindowView>, now: DateTime<Utc>) -> String {
    match w {
        None => format!("{label} {:<width$}", "-", width = BAR_CELLS + 14),
        Some(w) => {
            let reset = w.reset_at.map_or_else(
                || "reset time unknown".to_owned(),
                |t| format!("resets {}", until(now, t)),
            );
            format!(
                "{label} {} {:>4}  {reset}",
                bar(w.remaining_percent, BAR_CELLS),
                percent_text(w.remaining_percent)
            )
        }
    }
}

fn group_line(g: &QuotaView, now: DateTime<Utc>) -> String {
    let name = fit(&g.label(), 22);
    if g.disabled {
        return format!(
            "  {name} disabled for this account (not entitled; not the same as exhausted)"
        );
    }
    let mut s = format!(
        "  {name} {}   {}",
        window("5H", g.five_hour.as_ref(), now),
        window("WEEKLY", g.weekly.as_ref(), now)
    );
    if g.exhausted {
        s.push_str("  EXHAUSTED");
    }
    s.trim_end().to_owned()
}

fn source_line(a: &AccountUsage, now: DateTime<Utc>) -> String {
    match a.observed_at {
        None => "no quota data".to_owned(),
        Some(t) => {
            let src = if a.refreshed_now {
                "agy /usage"
            } else {
                "cache"
            };
            format!("{src}, {} (observed {})", a.freshness.label(), ago(now, t))
        }
    }
}

pub fn render_usage(r: &UsageReport, st: Style) -> String {
    let mut out = String::new();
    if r.accounts.is_empty() {
        out.push_str(
            r.notice
                .as_deref()
                .unwrap_or("No accounts to show. Use --all to include every account."),
        );
        out.push('\n');
        return out;
    }
    for a in &r.accounts {
        let head = format!(
            "{} {} <{}>  [{}]",
            if a.active { "*" } else { " " },
            truncate(&a.alias, 40),
            a.email
                .as_deref()
                .map_or_else(|| "no email".to_owned(), |e| truncate(e, 60)),
            a.health.label
        );
        out.push_str(&if a.active { st.bold(&head) } else { head });
        out.push('\n');
        out.push_str(&format!("    source: {}\n", source_line(a, r.generated_at)));
        if let Some(w) = &a.health.warning {
            out.push_str(&format!("    WARNING: {w}\n"));
        }
        for g in &a.groups {
            out.push_str(&format!("  {}\n", group_line(g, r.generated_at)));
        }
        if let Some(u) = &a.unavailable {
            let label = if u.reason == crate::core::quota::UnavailableReason::TooSoon {
                "Note"
            } else {
                "Unavailable"
            };
            out.push_str(&format!("    {label}: {}\n", u.text()));
        }
    }
    out
}

pub fn render_health(r: &HealthReport, st: Style) -> String {
    if r.accounts.is_empty() {
        return "No saved accounts. Log in with `agy`, then run `agyva add`.\n".to_owned();
    }
    let mut out = st.dim(&format!(
        "{:<3} {:<18} {:<16} {:<14} EVIDENCE",
        "#", "ALIAS", "STATE", "CHECKED"
    ));
    out.push('\n');
    for a in &r.accounts {
        let checked = a
            .health
            .observed_at
            .map_or_else(|| "never".to_owned(), |t| ago(r.generated_at, t));
        let line = format!(
            "{}{:<2} {} {} {} {}{}",
            if a.active { "*" } else { " " },
            a.number,
            fit(&a.alias, 18),
            fit(a.health.label, 16),
            fit(&checked, 14),
            truncate(&a.health.evidence, 70),
            if a.health.verified_now || a.health.evidence.contains("not verified now") {
                ""
            } else {
                "  (not verified now)"
            }
        );
        out.push_str(&if a.active { st.bold(&line) } else { line });
        out.push('\n');
    }
    for a in r
        .accounts
        .iter()
        .filter(|a| a.health.state == AccountHealth::TermsSuspended)
    {
        out.push_str(&format!(
            "\nWARNING for {}: {}\n",
            a.alias,
            a.health.warning.as_deref().unwrap_or_default()
        ));
    }
    if let Some(n) = &r.notice {
        out.push_str(&format!("\n{n}\n"));
    }
    out
}

/// One line per watch event.
pub fn render_event(at: DateTime<Utc>, e: &WatchEvent) -> String {
    let ts = at.format("%H:%M:%S");
    let body = match e {
        WatchEvent::Started { interval_secs } => {
            format!("watching (every {interval_secs}s, read-only; Ctrl+C to stop)")
        }
        WatchEvent::CycleOk { cycle, summary } => format!("cycle {cycle}: {summary}"),
        WatchEvent::CycleFailed {
            cycle,
            reason,
            retry_in_secs,
        } => {
            format!("cycle {cycle} failed: {reason}; retrying in {retry_in_secs}s")
        }
        WatchEvent::Note { text } => text.clone(),
        WatchEvent::HealthChanged { alias, from, to } => format!(
            "HEALTH {alias}: {} -> {}",
            from.map_or("(first seen)", AccountHealth::label),
            to.label()
        ),
        WatchEvent::ResetApproaching {
            alias,
            window,
            group,
            reset_at,
        } => {
            format!(
                "{alias}: {group} {window} window resets at {}",
                reset_at.format("%H:%M UTC")
            )
        }
        WatchEvent::SuspendedSkipped { alias } => {
            format!(
                "{alias} is suspended (ToS); not checking it. See the Antigravity FAQ: https://antigravity.google/docs/faq"
            )
        }
        WatchEvent::Stopped { cycles, reason } => format!(
            "stopped after {cycles} cycle(s) ({})",
            match reason {
                StopReason::Interrupted => "interrupted",
                StopReason::CycleLimit => "cycle limit",
            }
        ),
    };
    format!("{ts} {body}")
}
