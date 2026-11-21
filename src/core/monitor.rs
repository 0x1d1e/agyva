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

use crate::config::Config;
use crate::core::app::App;
use crate::core::endpoints;
use crate::core::profile;
use crate::core::quota_history::BurnRateView;
use crate::core::route::RoutePlan;
use crate::core::route_apply::RouteHistory;
use crate::core::usage::AccountUsage;
use crate::core::view::VIEW_SCHEMA;
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
    /// The pick that cycle settled on, so renderers can label it sticky
    /// without recomputing the selection.
    last_pick: Option<Picked>,
    /// What automation did on this cycle (an error, or the applied alias);
    /// `None` when automation is off or had nothing to say.
    last_auto: Option<String>,
    /// Enriched snapshot of the most recent cycle (with burn rates & context).
    last_snapshot: Option<MonitorSnapshot>,
    /// Exponential backoff deadline after an `auto_switch_for` error.
    auto_backoff_until: Option<DateTime<Utc>>,
    /// Consecutive `auto_switch_for` errors for backoff scaling.
    auto_failures: u32,
}

#[derive(Debug, Clone)]
pub struct MonitorConfig {
    pub eval_interval: Duration,
    pub refresh_interval: Duration,
    pub warn_percent: Option<f64>,
    pub critical_percent: Option<f64>,
    pub max_cycles: Option<u64>,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            eval_interval: DEFAULT_EVAL_INTERVAL,
            refresh_interval: DEFAULT_REFRESH_INTERVAL,
            warn_percent: None,
            critical_percent: None,
            max_cycles: None,
        }
    }
}

impl MonitorConfig {
    pub fn from_app(app: &App, eval_interval: Duration, refresh_interval: Duration) -> Self {
        Self {
            eval_interval,
            refresh_interval,
            warn_percent: Some(app.config.warn_percent),
            critical_percent: Some(app.config.critical_percent),
            max_cycles: None,
        }
    }

    pub fn to_watch_config(&self) -> WatchConfig {
        let mut cfg = WatchConfig::new(self.eval_interval)
            .with_thresholds(self.warn_percent, self.critical_percent);
        cfg.max_cycles = self.max_cycles;
        cfg
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorRow {
    pub id: String,
    pub alias: String,
    pub active: bool,
    pub eligible: bool,
    pub remaining_percent: Option<f64>,
    pub rate_remaining_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burn_rate: Option<BurnRateView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_exhaustion_secs: Option<u64>,
    pub renewals: [Option<DateTime<Utc>>; 2],
    pub why: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MonitorSnapshot {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub for_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recommendation: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sticky_pick: Option<Picked>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_auto: Option<String>,
    pub rows: Vec<MonitorRow>,
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
            last_pick: None,
            last_auto: None,
            last_snapshot: None,
            auto_backoff_until: None,
            auto_failures: 0,
        }
    }

    /// The most recent cycle's plan (pick, rows, why), for renderers.
    pub fn last_plan(&self) -> Option<&RoutePlan> {
        self.last_plan.as_ref()
    }

    /// The account the last cycle picked, and whether it changed then.
    pub fn last_pick(&self) -> Option<&Picked> {
        self.last_pick.as_ref()
    }

    /// What opt-in automation did on the last cycle, if it ran and had
    /// anything to report (applied switch or an error).
    pub fn last_auto(&self) -> Option<&str> {
        self.last_auto.as_deref()
    }

    /// The enriched monitor snapshot from the most recent cycle.
    pub fn last_snapshot(&self) -> Option<&MonitorSnapshot> {
        self.last_snapshot.as_ref()
    }

    fn build_snapshot(
        &self,
        plan: &RoutePlan,
        usages: &[AccountUsage],
        now: DateTime<Utc>,
    ) -> MonitorSnapshot {
        let active_endpoint = endpoints::load_registry(&self.app.dirs)
            .ok()
            .and_then(|reg| {
                reg.active_endpoint
                    .as_deref()
                    .and_then(|id| reg.find(id).map(|ep| ep.alias.clone()))
            });
        let active_profile = std::env::current_dir()
            .ok()
            .and_then(|cwd| {
                profile::resolve_profile_with_default(
                    &self.app.dirs,
                    &cwd,
                    None,
                    self.app.config.default_profile.as_deref(),
                )
                .ok()
                .flatten()
            })
            .map(|p| p.name.unwrap_or_else(|| "local".to_owned()));

        let rows = plan
            .accounts
            .iter()
            .map(|row| {
                let burn_rate = usages.iter().find(|u| u.id == row.id).and_then(|u| {
                    let (five, week) = u.lowest();
                    [five, week]
                        .into_iter()
                        .flatten()
                        .filter_map(|w| w.burn_rate.clone())
                        .min_by_key(|b| b.eta_exhaustion_secs.unwrap_or(u64::MAX))
                });
                let eta_exhaustion_secs = burn_rate.as_ref().and_then(|b| b.eta_exhaustion_secs);
                MonitorRow {
                    id: row.id.clone(),
                    alias: row.alias.clone(),
                    active: row.active,
                    eligible: row.eligible,
                    remaining_percent: row.remaining_percent,
                    rate_remaining_percent: row.rate_remaining_percent,
                    burn_rate,
                    eta_exhaustion_secs,
                    renewals: row.renewals,
                    why: row.why.clone(),
                }
            })
            .collect();

        MonitorSnapshot {
            schema_version: VIEW_SCHEMA,
            generated_at: now,
            group: self.group.clone(),
            for_model: self.for_model.clone(),
            active_endpoint,
            active_profile,
            recommendation: plan.recommendation.clone(),
            sticky_pick: self.last_pick.clone(),
            last_auto: self.last_auto.clone(),
            rows,
        }
    }
}

impl Probe for MonitorProbe<'_> {
    fn cycle(&mut self, now: DateTime<Utc>) -> crate::core::watch::CycleReport {
        // Periodic single-flight refresh of the ACTIVE account only: the
        // refresh guard skips (TooSoon / RefreshBusy) instead of queueing.
        // `refreshed` is the freshness proof automation requires: only a run
        // that actually got a live reading this cycle may authorize a switch.
        let mut refreshed = false;
        let mut refresh_failure: Option<String> = None;
        if self.last_refresh.is_none_or(|at| {
            now - at >= chrono::Duration::from_std(self.refresh_interval).unwrap_or_default()
        }) {
            let mut transient_failure = false;
            match self.app.refresh_active_at(false, now) {
                Ok(outcome) => {
                    refreshed = matches!(
                        outcome.status,
                        crate::core::refresh::RefreshStatus::Refreshed
                    );
                    if outcome.is_transient_failure() {
                        transient_failure = true;
                        refresh_failure = outcome.unavailable().map(|u| u.detail.clone());
                    }
                }
                Err(e) => {
                    tracing::warn!("active refresh failed: {}", e.message());
                }
            }
            // Keep the active account's model catalog current on the same
            // tick so `--for-model` works without a manual `agyva models`.
            // The call self-throttles on `models_cache_secs` and shares the
            // refresh lock, so a fresh catalog means no spawn at all.
            if let Err(e) = self.app.refresh_models_at(false, now) {
                tracing::warn!("catalog refresh skipped: {}", e.message());
            }
            if !transient_failure {
                self.last_refresh = Some(now);
            }
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
            failure: refresh_failure,
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
        self.last_pick = picked;
        // Opt-in automation: only for a named group (there is no group to
        // declare `auto_switch` on otherwise), only with this cycle's live
        // reading as proof, and only through the journaled path. Every
        // hold/defer/applied outcome lands in the route history.
        self.last_auto = None;
        if let Some(group) = self.group.clone() {
            if self.auto_backoff_until.is_some_and(|until| now < until) {
                self.last_auto = Some("auto-switch backed off after prior failure".into());
            } else {
                let history = RouteHistory::new(&self.app.dirs);
                let result = self
                    .app
                    .auto_route_group_for(Some(&group), self.for_model.as_deref(), now)
                    .and_then(|auto_plan| {
                        self.app.auto_switch_for(
                            &group,
                            &auto_plan,
                            self.for_model.as_deref(),
                            refreshed,
                            &history,
                            now,
                        )
                    });
                match result {
                    Ok(Some(outcome)) => {
                        self.auto_failures = 0;
                        self.auto_backoff_until = None;
                        self.last_auto = Some(format!("auto-switch applied: {}", outcome.alias));
                        report.note = Some(format!(
                            "{}; {}",
                            report.note.unwrap_or_default(),
                            self.last_auto.clone().unwrap_or_default()
                        ));
                    }
                    Ok(None) => {
                        if refreshed {
                            self.auto_failures = 0;
                            self.auto_backoff_until = None;
                        }
                    }
                    Err(e) => {
                        let shift = self.auto_failures.min(5);
                        let secs = 60u64.saturating_mul(1u64 << shift).min(1800);
                        self.auto_failures = self.auto_failures.saturating_add(1);
                        self.auto_backoff_until =
                            Some(now + chrono::Duration::seconds(secs as i64));
                        self.last_auto = Some(format!("auto-switch error: {}", e.message()));
                    }
                }
            }
        }
        self.last_snapshot = Some(self.build_snapshot(&plan, &report.accounts, now));
        self.last_plan = Some(plan);
        report
    }
}

/// The monitor's own watch config: evaluation cadence, no reset-window
/// announcements beyond what watch already does.
pub fn monitor_config(interval: Duration) -> WatchConfig {
    WatchConfig::new(interval)
}

/// Monitor watch config with threshold alerts sourced from `Config`.
pub fn monitor_config_for_app(interval: Duration, cfg: &Config) -> WatchConfig {
    WatchConfig::from_config(interval, cfg)
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
