//! `agyva monitor`: a route-aware foreground loop over the existing watch
//! machinery. Every tick is CACHE-ONLY (15s default): ranking and
//! exclusions from `route plan`, never an agy spawn. The active account's
//! quota refresh runs on its own longer cadence (default 300s) through the
//! existing single-flight refresh guard — `refresh_active_at` already
//! skips when another agyva holds the lock or the min-interval has not
//! passed, so overlapping callers collapse to one run.
//!
//! Sticky selection: the recommended account only changes when it becomes
//! ineligible or a strictly better candidate appears (higher tier), so the
//! recommendation does not flap tick to tick on rounding noise.

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::app::App;
use crate::core::route::RoutePlan;
use crate::core::watch::{Clock, Jitter, Probe, Sink, WatchConfig, run_watch};

/// Evaluation (cache-only ranking) defaults to 15s; the user may raise or
/// lower it with `--interval`. Below 5s the loop would just burn CPU over
/// an unchanged cache, so the parser enforces that floor.
pub const MIN_EVAL_INTERVAL: Duration = Duration::from_secs(5);
pub const DEFAULT_EVAL_INTERVAL: Duration = Duration::from_secs(15);
/// How often the monitor triggers an active-account quota refresh; the
/// refresh's own guards (lock, min-interval) keep concurrent monitors
/// single-flight.
pub const DEFAULT_REFRESH_INTERVAL: Duration = Duration::from_secs(300);

/// A route-aware probe: cache-only ranking every cycle, periodic
/// single-flight active refresh.
pub struct MonitorProbe<'a> {
    app: &'a App,
    group: Option<String>,
    for_model: Option<String>,
    refresh_interval: Duration,
    last_refresh: Option<DateTime<Utc>>,
    /// The sticky recommendation (account id); kept across cycles unless
    /// it turns ineligible or a strictly better tier appears.
    sticky: Option<String>,
    /// The most recent cycle's plan, for the sink/renderers.
    last_plan: Option<RoutePlan>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Picked {
    pub id: String,
    pub alias: String,
    /// True when the pick differs from last cycle (a real change, not
    /// stable-sort noise).
    pub changed: bool,
}

impl<'a> MonitorProbe<'a> {
    pub fn new(
        app: &'a App,
        group: Option<String>,
        for_model: Option<String>,
        refresh_interval: Duration,
    ) -> Self {
        Self {
            app,
            group,
            for_model,
            refresh_interval,
            last_refresh: None,
            sticky: None,
            last_plan: None,
        }
    }

    /// The most recent cycle's plan (pick, rows, why), for renderers.
    pub fn last_plan(&self) -> Option<&RoutePlan> {
        self.last_plan.as_ref()
    }
}

impl Probe for MonitorProbe<'_> {
    fn cycle(&mut self, now: DateTime<Utc>) -> crate::core::watch::CycleReport {
        // Periodic single-flight refresh of the ACTIVE account only: the
        // refresh guard skips (TooSoon / RefreshBusy) instead of queueing,
        if self.last_refresh.is_none_or(|at| {
            now - at >= chrono::Duration::from_std(self.refresh_interval).unwrap_or_default()
        }) {
            let _ = self.app.refresh_active_at(false, now);
            self.last_refresh = Some(now);
        }
        let plan = self
            .app
            .route_group_for(self.group.as_deref(), self.for_model.as_deref(), now)
            .unwrap_or_else(|e| {
                tracing::warn!("ranking failed: {}", e.message());
                RoutePlan {
                    schema_version: 1,
                    generated_at: now,
                    recommendation: None,
                    accounts: Vec::new(),
                }
            });
        // Sticky selection among eligible accounts: keep the current pick
        // unless it is gone/ineligible; switch only for the top-ranked row.
        let top = plan.accounts.iter().find(|r| r.eligible);
        let picked = match (&self.sticky, top) {
            (Some(prev), Some(t)) if &t.id == prev => Some(Picked {
                id: t.id.clone(),
                alias: t.alias.clone(),
                changed: false,
            }),
            (Some(prev), _) if plan.accounts.iter().any(|r| &r.id == prev && r.eligible) => {
                // Previous pick still eligible but no longer top-ranked:
                // stickiness holds (no flapping), report it unchanged.
                let row = plan
                    .accounts
                    .iter()
                    .find(|r| &r.id == prev)
                    .expect("checked above");
                Some(Picked {
                    id: row.id.clone(),
                    alias: row.alias.clone(),
                    changed: false,
                })
            }
            (_, Some(t)) => {
                self.sticky = Some(t.id.clone());
                Some(Picked {
                    id: t.id.clone(),
                    alias: t.alias.clone(),
                    changed: true,
                })
            }
            (None, None) => None,
            (Some(_), None) => {
                self.sticky = None;
                None
            }
        };
        let mut report = crate::core::watch::CycleReport {
            accounts: self.app.account_usages(None, now).unwrap_or_default(),
            ..Default::default()
        };
        report.note = Some(match &picked {
            Some(p) => format!(
                "route pick: {}{}",
                p.alias,
                if p.changed { " (changed)" } else { "" }
            ),
            None => "route pick: none eligible".to_owned(),
        });
        self.last_plan = Some(plan);
        report
    }
}

/// The monitor's own watch config: evaluation cadence, no reset-window
/// announcements beyond what watch already does.
pub fn monitor_config(interval: Duration) -> WatchConfig {
    WatchConfig::new(interval)
}

/// Run the monitor loop to completion (stop flag or max cycles).
pub fn run_monitor(
    cfg: &WatchConfig,
    clock: &dyn Clock,
    jitter: &mut dyn Jitter,
    probe: &mut MonitorProbe<'_>,
    sink: &mut dyn Sink,
    stop: &AtomicBool,
) -> crate::core::watch::WatchSummary {
    run_watch(cfg, clock, jitter, probe, sink, stop)
}

/// Parse the `--interval` duration grammar: `15s`, `30`, `1m`, `90s`.
/// Whole seconds minimum; `MIN_EVAL_INTERVAL` floor enforced.
pub fn parse_interval(text: &str) -> Result<Duration, crate::core::error::Error> {
    use crate::core::error::Error;
    let t = text.trim();
    let (num, unit) = match t.strip_suffix(['s', 'm', 'h']) {
        Some(num) if !num.is_empty() => (num, &t[num.len()..]),
        Some(_) | None => (t, ""),
    };
    let secs: u64 = num.parse().map_err(|_| {
        Error::usage(format!(
            "invalid --interval `{text}`: expected seconds like 15s, 2m or 1h"
        ))
    })?;
    let secs = match unit {
        "s" | "" => secs,
        "m" => secs
            .checked_mul(60)
            .ok_or_else(|| Error::usage("--interval too large"))?,
        "h" => secs
            .checked_mul(3600)
            .ok_or_else(|| Error::usage("--interval too large"))?,
        _ => unreachable!("suffix stripped above"),
    };
    if Duration::from_secs(secs) < MIN_EVAL_INTERVAL {
        return Err(Error::usage(format!(
            "--interval must be at least {}s",
            MIN_EVAL_INTERVAL.as_secs()
        ))
        .hint("the evaluation is cache-only; a faster loop changes nothing"));
    }
    Ok(Duration::from_secs(secs))
}

/// One line describing the pick for non-TUI output.
pub fn pick_line(cycle_note: Option<&str>, plan: &RoutePlan, now: DateTime<Utc>) -> String {
    use crate::core::format::until;
    let mut out = cycle_note.unwrap_or("route pick: none eligible").to_owned();
    if let Some(reset) = plan
        .recommendation
        .as_ref()
        .and_then(|rec| plan.accounts.iter().find(|r| &r.id == rec))
        .and_then(|row| row.renewals.iter().flatten().min())
    {
        out.push_str(&format!("; weekly resets {}", until(now, *reset)));
    }
    out
}

/// UTC timestamp for TUI/JSON consumers.
pub fn now_utc() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interval_grammar_accepts_s_m_h_and_bare() {
        assert_eq!(parse_interval("15s").unwrap(), Duration::from_secs(15));
        assert_eq!(parse_interval("15").unwrap(), Duration::from_secs(15));
        assert_eq!(parse_interval("2m").unwrap(), Duration::from_secs(120));
        assert_eq!(parse_interval("1h").unwrap(), Duration::from_secs(3600));
        assert!(parse_interval("1s").is_err(), "below the 5s floor");
        assert!(parse_interval("x").is_err());
        assert!(parse_interval("1d").is_err());
    }
}
