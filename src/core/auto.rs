//! Opt-in automatic activation. Disabled by default; enabling it requires an
//! explicit policy re-acknowledgment because it relaxes agyva's standing
//! "switching is always manual" promise in exactly one bounded way: when the
//! gates below all pass, the monitor's evaluation cycle may run the same
//! journaled switch a human would have typed. [`crate::core::monitor`]'s
//! probe is the only caller — automation therefore runs exactly as often as
//! the monitor is ticking, and never from a one-shot command.
//!
//! Gates, all mandatory, evaluated in order and re-validated under the
//! login lock:
//! 1. `routing.toml` declares `auto_switch = true` for the group AND
//!    `config.toml` carries the matching auto-switch policy
//!    acknowledgement (`auto_switch_ack = true`).
//! 2. Fresh active evidence: the decision runs only right after a
//!    `Refreshed` active reading; cached windows and reset times never
//!    trigger anything.
//! 3. Cooldown: at least `group.cooldown(&config)` (default 900s) since
//!    `last_any_switch` (any switch resets the cooldown — a manual `use`
//!    wins).
//! 4. Readable `AutoState`: corrupt or unsupported `auto-switch.json`
//!    fails closed (`Defer`) instead of defaulting to a cleared cooldown.
//! 5. Session gate: no detected agy sessions and no detection failure —
//!    an unknown outcome defers, it never risks retargeting a running
//!    session.
//! 6. Active account trigger: the active account is missing from the plan,
//!    ineligible (blocked health, `QuotaLimited`, model-ineligible, `<= 0%`),
//!    or at/below `five_hour_threshold` / `weekly_threshold`.
//! 7. Candidate hysteresis: the top eligible non-active candidate in the
//!    candidate-aware plan (which allows standby cache up to
//!    `auto_switch_candidate_max_age_secs` and post-reset windows) passes
//!    hysteresis over the active account when triggered by threshold.
//!
//! Every outcome — applied, refused, deferred, failed — is recorded in the
//! route history (with consecutive/streak deduplication for refusals and
//! failures).

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::error::{Error, Result};
use crate::core::route::{RoutePlan, resolve_mapped_group};
use crate::core::route_apply::{ApplyTarget, HistoryEntry, HistoryOutcome, RouteHistory};
use crate::core::route_config::{Ranking, RouteGroup, RoutingFile};
use crate::core::usage::{AccountUsage, QuotaView};
use crate::storage::fsutil;

/// Default minimum time between switches (also reset by any manual
/// switch through the same state file).
pub const MIN_SWITCH_INTERVAL: Duration = Duration::from_secs(900);
/// Bounded state file recording switch timestamps.
pub const AUTO_STATE_FILE: &str = "auto-switch.json";
const AUTO_STATE_LOCK: &str = "auto-switch.lock";
const AUTO_STATE_SCHEMA: u32 = 1;

/// The auto-switch policy acknowledgement in `config.toml`.
pub const AUTO_ACK_KEY: &str = "auto_switch_ack";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct AutoState {
    schema_version: u32,
    /// Last automatic switch.
    last_auto_switch: Option<DateTime<Utc>>,
    /// Last switch of ANY kind seen through this file; a manual `use`
    /// updates it too, so automation cannot chase a human.
    last_any_switch: Option<DateTime<Utc>>,
}

impl AutoState {
    fn load(dirs: &crate::paths::Dirs) -> Result<Self> {
        let path = dirs.state.join(AUTO_STATE_FILE);
        let Some(bytes) = fsutil::read_optional(&path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
        else {
            return Ok(Self::default());
        };
        let st: Self = serde_json::from_slice(&bytes)
            .map_err(|e| Error::format(format!("invalid {}: {e}", path.display())))?;
        if st.schema_version != AUTO_STATE_SCHEMA {
            return Err(Error::format(format!(
                "unsupported {} schema_version {}",
                path.display(),
                st.schema_version
            )));
        }
        Ok(st)
    }

    fn save(&self, dirs: &crate::paths::Dirs) -> Result<()> {
        let path = dirs.state.join(AUTO_STATE_FILE);
        if let Some(dir) = path.parent() {
            fsutil::create_private_dir(dir).map_err(|e| Error::io("creating state dir", e))?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| Error::format(e.to_string()))?;
        fsutil::atomic_write(&path, &bytes).map_err(|e| Error::io("writing auto state", e))
    }
}

/// The decision `auto_switch_for` reached, before any mutation.
#[derive(Debug, Clone, PartialEq)]
pub enum AutoDecision {
    /// Nothing to do: automation off, unacknowledged, cooling down, active
    /// healthy above thresholds, or no qualifying candidate.
    Hold(String),
    /// A gate (sessions, recovery, staleness, corrupt state) requires human
    /// attention or a fresh active reading.
    Defer(String),
    /// Switch to this account id/alias.
    Switch { id: String, alias: String },
}

/// Whether automation is enabled and acknowledged for `group`.
pub fn enabled_for(app: &crate::core::app::App, group: &str, config: &RoutingFile) -> Result<bool> {
    let Some(g) = config.groups.get(group) else {
        return Ok(false);
    };
    if !g.auto_switch {
        return Ok(false);
    }
    // The explicit re-acknowledgment: enabling automation in the group
    // file is not enough; config.toml must carry the matching ack, set by
    // `agyva config set auto_switch_ack true` after reading the policy.
    Ok(app.config.auto_switch_ack)
}

/// Record that a switch of some kind just happened (manual or automatic).
pub fn note_switch(dirs: &crate::paths::Dirs, at: DateTime<Utc>, automatic: bool) -> Result<()> {
    let lock_path = dirs.state.join(AUTO_STATE_LOCK);
    if let Some(dir) = lock_path.parent() {
        fsutil::create_private_dir(dir).map_err(|e| Error::io("creating state dir", e))?;
    }
    let _guard = crate::storage::lock::StateLock::acquire(&lock_path, Duration::from_secs(3))?;
    let mut st = AutoState::load(dirs)?;
    st.schema_version = AUTO_STATE_SCHEMA;
    st.last_any_switch = Some(at);
    if automatic {
        st.last_auto_switch = Some(at);
    }
    st.save(dirs)
}

/// A hold is a decision, and a contended history lock must not replace it
/// with "another agyva is running" — the same rule `route_apply` follows for
/// its refusals. Recording is best effort; `record_once` deduplicates
/// within any trailing non-`Applied` streak.
fn record_hold_best_effort(history: &RouteHistory, entry: HistoryEntry) {
    if let Err(e) = history.record_once(entry) {
        tracing::warn!("could not record the auto-switch hold: {}", e.message());
    }
}

fn scoped_quotas<'a>(
    app: &crate::core::app::App,
    config: &RoutingFile,
    group_config: &RouteGroup,
    usage: &'a AccountUsage,
    for_model: Option<&str>,
    now: DateTime<Utc>,
) -> Option<Vec<&'a QuotaView>> {
    let Some(requested) = for_model.map(str::trim).filter(|w| !w.is_empty()) else {
        return Some(usage.groups.iter().filter(|g| !g.disabled).collect());
    };
    let mapped = resolve_mapped_group(
        usage,
        Some(group_config),
        config,
        requested,
        &app.models_cache(),
        app.config.models_cache_secs,
        now,
    )
    .ok()?;
    Some(
        usage
            .groups
            .iter()
            .filter(|g| {
                !g.disabled
                    && g.group == mapped.group_name
                    && g.model.as_deref() == mapped.model.as_deref()
            })
            .collect(),
    )
}

fn candidate_unreset_limiting(
    app: &crate::core::app::App,
    config: &RoutingFile,
    group_config: &RouteGroup,
    usages: &[AccountUsage],
    row: &crate::core::route::RouteRow,
    for_model: Option<&str>,
    now: DateTime<Utc>,
) -> Option<f64> {
    let Some(usage) = usages.iter().find(|u| u.id == row.id) else {
        return row.remaining_percent;
    };
    let groups = scoped_quotas(app, config, group_config, usage, for_model, now)?;
    let mut any_window = false;
    let mut unreset_min: Option<f64> = None;
    for q in groups {
        for w in q.five_hour.iter().chain(q.weekly.iter()) {
            any_window = true;
            if w.reset_at.is_some_and(|at| at > now) {
                unreset_min =
                    Some(unreset_min.map_or(w.remaining_percent, |v| v.min(w.remaining_percent)));
            }
        }
    }
    if !any_window {
        return None;
    }
    // If every observed window has already passed `reset_at`, the candidate
    // is post-reset and unconstrained by any active window.
    Some(unreset_min.unwrap_or(100.0))
}

enum ActiveTrigger {
    BlockedOrExhausted,
    Threshold { required: Vec<(f64, f64)> },
}

/// Evaluate the auto-switch decision for `group` against fresh evidence.
/// `plan` must come from a run whose active account was just refreshed
/// (`RefreshStatus::Refreshed`); the caller passes `refreshed` to prove it.
/// No mutation happens here.
pub fn decide(
    app: &crate::core::app::App,
    group: &str,
    plan: &RoutePlan,
    refreshed: bool,
    now: DateTime<Utc>,
) -> AutoDecision {
    decide_for(app, group, plan, None, refreshed, now)
}

/// Evaluate the auto-switch decision for `group`, optionally scoped to
/// `for_model`.
pub fn decide_for(
    app: &crate::core::app::App,
    group: &str,
    plan: &RoutePlan,
    for_model: Option<&str>,
    refreshed: bool,
    now: DateTime<Utc>,
) -> AutoDecision {
    let config = match RoutingFile::load(&app.dirs.config.join("routing.toml")) {
        Ok(c) => c,
        Err(e) => return AutoDecision::Defer(format!("cannot read routing.toml: {}", e.message())),
    };
    if !enabled_for(app, group, &config).unwrap_or(false) {
        return AutoDecision::Hold("auto_switch is not enabled and acknowledged".into());
    }
    let g = match config.group(Some(group)) {
        Ok(g) => g,
        Err(e) => {
            return AutoDecision::Defer(format!("cannot read routing group: {}", e.message()));
        }
    };
    if !refreshed {
        return AutoDecision::Defer(
            "decision needs a fresh live reading of the active account; cached reset times never trigger a switch".into(),
        );
    }
    // Load AutoState fail-closed and enforce cooldown (`last_any_switch`).
    let st = match AutoState::load(&app.dirs) {
        Ok(s) => s,
        Err(e) => {
            return AutoDecision::Defer(format!(
                "auto-switch state is unreadable or corrupt ({}); refusing to switch until fixed or removed",
                e.message()
            ));
        }
    };
    let cooldown = g.cooldown(&app.config);
    let min = chrono::Duration::from_std(cooldown).unwrap_or_default();
    if st.last_any_switch.is_some_and(|last| now - last < min) {
        // No elapsed seconds here: the reason is persisted, and a changing
        // number would defeat the streak dedupe in the history.
        return AutoDecision::Hold(format!(
            "cooling down: minimum {}s between switches",
            cooldown.as_secs()
        ));
    }
    // Session gate: detected sessions defer, and so does an UNKNOWN answer
    // (unreadable presence directory or process table): automation never
    // treats "could not look" as "nothing is running".
    let sessions = crate::agy::sessions::detect(&app.agy, app.session_scan);
    if !sessions.is_known() {
        return AutoDecision::Defer(format!(
            "could not determine whether agy sessions are running ({}); refusing to switch blind",
            sessions.uncertain.as_deref().unwrap_or("unknown cause")
        ));
    }
    if sessions.any() {
        return AutoDecision::Defer(
            "agy sessions detected; switching would not retarget them".to_owned(),
        );
    }

    let usages = match app.account_usages(None, now) {
        Ok(u) => u,
        Err(e) => {
            return AutoDecision::Defer(format!(
                "cannot read account usage cache: {}",
                e.message()
            ));
        }
    };

    // Active account trigger gate.
    let active_row = plan.accounts.iter().find(|r| r.active);
    let trigger = match active_row {
        None => ActiveTrigger::BlockedOrExhausted,
        Some(ar) if !ar.eligible => ActiveTrigger::BlockedOrExhausted,
        Some(ar) => {
            let (active_five, active_week) = usages
                .iter()
                .find(|u| u.id == ar.id)
                .and_then(|u| scoped_quotas(app, &config, &g, u, for_model, now))
                .map(|groups| {
                    let five = groups
                        .iter()
                        .filter_map(|q| q.five_hour.as_ref())
                        .map(|w| w.remaining_percent)
                        .reduce(f64::min);
                    let week = groups
                        .iter()
                        .filter_map(|q| q.weekly.as_ref())
                        .map(|w| w.remaining_percent)
                        .reduce(f64::min);
                    (five, week)
                })
                .unwrap_or((ar.rate_remaining_percent, ar.remaining_percent));
            if active_five.is_some_and(|v| v <= 0.0) || active_week.is_some_and(|v| v <= 0.0) {
                ActiveTrigger::BlockedOrExhausted
            } else {
                let mut required = Vec::new();
                if let Some(v) = active_five
                    && v <= g.five_hour_threshold()
                {
                    required.push((g.five_hour_threshold(), v));
                }
                if let Some(v) = active_week
                    && v <= g.weekly_threshold()
                {
                    required.push((g.weekly_threshold(), v));
                }
                if required.is_empty() {
                    return AutoDecision::Hold(
                        "active account is healthy and above switch thresholds".into(),
                    );
                }
                ActiveTrigger::Threshold { required }
            }
        }
    };

    // Eligible non-active candidates, checked against hysteresis when triggered
    // solely by threshold.
    let candidates: Vec<_> = plan
        .accounts
        .iter()
        .filter(|r| r.eligible && !r.active)
        .collect();
    if candidates.is_empty() {
        return AutoDecision::Hold("no eligible candidate".into());
    }

    let chosen = match trigger {
        ActiveTrigger::BlockedOrExhausted => candidates.into_iter().find(|c| {
            candidate_unreset_limiting(app, &config, &g, &usages, c, for_model, now)
                .is_some_and(|v| v > 0.0)
        }),
        ActiveTrigger::Threshold { ref required } => {
            let hyst = g.hysteresis();
            let hit = candidates.into_iter().find(|c| {
                let Some(cand_val) =
                    candidate_unreset_limiting(app, &config, &g, &usages, c, for_model, now)
                else {
                    return false;
                };
                required.iter().all(|(threshold, active_val)| {
                    cand_val > *threshold
                        && cand_val >= (active_val + hyst).min(100.0)
                        && cand_val > *active_val
                })
            });
            if hit.is_none() {
                return AutoDecision::Hold("no eligible candidate meets switch hysteresis".into());
            }
            hit
        }
    };

    let Some(target) = chosen else {
        return AutoDecision::Hold("no eligible candidate".into());
    };
    AutoDecision::Switch {
        id: target.id.clone(),
        alias: target.alias.clone(),
    }
}

impl crate::core::app::App {
    /// The one journaled path: decide, revalidate under the login lock, and
    /// switch through `route_apply_target_under_lock` while continuously
    /// holding that lock. The caller must have JUST refreshed the active
    /// account; that proof travels in `refreshed`.
    pub fn auto_switch_for(
        &self,
        group: &str,
        plan: &RoutePlan,
        for_model: Option<&str>,
        refreshed: bool,
        history: &RouteHistory,
        now: DateTime<Utc>,
    ) -> Result<Option<crate::core::route_apply::ApplyOutcome>> {
        match decide_for(self, group, plan, for_model, refreshed, now) {
            AutoDecision::Hold(reason) => {
                record_hold_best_effort(
                    history,
                    HistoryEntry {
                        at: now,
                        group: Some(group.to_owned()),
                        target: plan.recommendation.clone().unwrap_or_else(|| "-".into()),
                        outcome: HistoryOutcome::Refused,
                        reason: format!("auto-switch held: {reason}"),
                    },
                );
                Ok(None)
            }
            AutoDecision::Defer(reason) => {
                record_hold_best_effort(
                    history,
                    HistoryEntry {
                        at: now,
                        group: Some(group.to_owned()),
                        target: plan.recommendation.clone().unwrap_or_else(|| "-".into()),
                        outcome: HistoryOutcome::Refused,
                        reason: format!("auto-switch deferred: {reason}"),
                    },
                );
                Ok(None)
            }
            AutoDecision::Switch { id, .. } => {
                // Revalidate under the login lock and keep it held through
                // the switch + cooldown update.
                let lock = self.lock_login()?;
                let config = RoutingFile::load(&self.dirs.config.join("routing.toml"))?;
                if !enabled_for(self, group, &config)? {
                    record_hold_best_effort(
                        history,
                        HistoryEntry {
                            at: now,
                            group: Some(group.to_owned()),
                            target: id,
                            outcome: HistoryOutcome::Refused,
                            reason: "auto-switch held: disabled while preparing".into(),
                        },
                    );
                    return Ok(None);
                }
                let st = match AutoState::load(&self.dirs) {
                    Ok(s) => s,
                    Err(e) => {
                        record_hold_best_effort(
                            history,
                            HistoryEntry {
                                at: now,
                                group: Some(group.to_owned()),
                                target: id,
                                outcome: HistoryOutcome::Refused,
                                reason: format!(
                                    "auto-switch deferred: auto-switch state is unreadable or corrupt ({}); refusing to switch until fixed or removed",
                                    e.message()
                                ),
                            },
                        );
                        return Ok(None);
                    }
                };
                let g = config.group(Some(group))?;
                let min = chrono::Duration::from_std(g.cooldown(&self.config)).unwrap_or_default();
                if st.last_any_switch.is_some_and(|last| now - last < min) {
                    record_hold_best_effort(
                        history,
                        HistoryEntry {
                            at: now,
                            group: Some(group.to_owned()),
                            target: id,
                            outcome: HistoryOutcome::Refused,
                            reason: "auto-switch held: cooldown re-check failed".into(),
                        },
                    );
                    return Ok(None);
                }
                let sessions = crate::agy::sessions::detect(&self.agy, self.session_scan);
                if !sessions.is_known() {
                    record_hold_best_effort(
                        history,
                        HistoryEntry {
                            at: now,
                            group: Some(group.to_owned()),
                            target: id,
                            outcome: HistoryOutcome::Refused,
                            reason: format!(
                                "auto-switch deferred: could not determine whether agy sessions are running ({}); refusing to switch blind",
                                sessions.uncertain.as_deref().unwrap_or("unknown cause")
                            ),
                        },
                    );
                    return Ok(None);
                }
                if sessions.any() {
                    record_hold_best_effort(
                        history,
                        HistoryEntry {
                            at: now,
                            group: Some(group.to_owned()),
                            target: id,
                            outcome: HistoryOutcome::Refused,
                            reason: "auto-switch deferred: agy sessions detected; switching would not retarget them".into(),
                        },
                    );
                    return Ok(None);
                }
                if let Some(p) = self.pending_journal()? {
                    return Err(Error::recovery_required(format!(
                        "an interrupted switch to account {} ({}) must be recovered first",
                        p.target,
                        p.phase.as_str()
                    )));
                }
                let fresh_plan = self.auto_route_group_for(Some(group), for_model, now)?;
                match decide_for(self, group, &fresh_plan, for_model, refreshed, now) {
                    AutoDecision::Switch {
                        id: fresh_id,
                        alias: _,
                    } if fresh_id == id => {
                        let ranking = match g.ranking {
                            Ranking::Priority => "priority order",
                            Ranking::LeastUsed => "least-used ranking",
                            Ranking::Smart => "smart ranking",
                        };
                        let why = fresh_plan
                            .accounts
                            .iter()
                            .find(|r| r.id == fresh_id)
                            .map(|r| format!("{ranking}: {}", r.why))
                            .unwrap_or_else(|| ranking.to_owned());
                        let outcome = self.route_apply_target_under_lock(
                            &lock,
                            ApplyTarget {
                                group,
                                target_id: &fresh_id,
                                reason: &why,
                                automatic: true,
                                record_cooldown: true,
                            },
                            history,
                            now,
                        )?;
                        Ok(Some(outcome))
                    }
                    AutoDecision::Switch { .. } => {
                        record_hold_best_effort(
                            history,
                            HistoryEntry {
                                at: now,
                                group: Some(group.to_owned()),
                                target: id,
                                outcome: HistoryOutcome::Refused,
                                reason: "auto-switch held: recommendation changed under lock"
                                    .into(),
                            },
                        );
                        Ok(None)
                    }
                    AutoDecision::Hold(reason) => {
                        record_hold_best_effort(
                            history,
                            HistoryEntry {
                                at: now,
                                group: Some(group.to_owned()),
                                target: id,
                                outcome: HistoryOutcome::Refused,
                                reason: format!("auto-switch held: {reason}"),
                            },
                        );
                        Ok(None)
                    }
                    AutoDecision::Defer(reason) => {
                        record_hold_best_effort(
                            history,
                            HistoryEntry {
                                at: now,
                                group: Some(group.to_owned()),
                                target: id,
                                outcome: HistoryOutcome::Refused,
                                reason: format!("auto-switch deferred: {reason}"),
                            },
                        );
                        Ok(None)
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_interval_is_nine_hundred_seconds() {
        assert_eq!(MIN_SWITCH_INTERVAL, Duration::from_secs(900));
    }

    #[test]
    fn state_survives_roundtrip_and_garbage() {
        use crate::paths::Dirs;
        let tmp = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(tmp.path());
        note_switch(&dirs, Utc::now(), true).unwrap();
        let st = AutoState::load(&dirs).unwrap();
        assert!(st.last_auto_switch.is_some());
        assert!(st.last_any_switch.is_some());
        // Corrupt or unsupported state fails closed (returns Err, never default).
        std::fs::write(dirs.state.join(AUTO_STATE_FILE), b"not json").unwrap();
        assert!(AutoState::load(&dirs).is_err());
        std::fs::write(
            dirs.state.join(AUTO_STATE_FILE),
            br#"{"schema_version":99}"#,
        )
        .unwrap();
        assert!(AutoState::load(&dirs).is_err());
    }
}
