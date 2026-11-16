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
//! 2. Cooldown: at least [`MIN_SWITCH_INTERVAL`] since the last automatic
//!    switch (any switch resets the cooldown — a manual `use` wins).
//! 3. Fresh active evidence: the decision runs only right after a
//!    Refreshed active reading; cached windows and reset times never
//!    trigger anything.
//! 4. Fresh eligible candidate: the sticky-free top row of the plan is
//!    eligible with fresh cache, and is not the active account.
//! 5. Session gate: no detected agy sessions and no detection failure —
//!    an unknown outcome defers, it never risks retargeting a running
//!    session.
//! 6. Health: the candidate is never TermsSuspended (or any blocked
//!    state); the active account being suspended simply blocks.
//! 7. Pending recovery: refused like any other mutation.
//!
//! Every outcome — applied, refused, deferred, failed — is recorded in the
//! route history. No hidden retry or rotation on refusal.

use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::error::{Error, Result};
use crate::core::route_apply::{HistoryEntry, HistoryOutcome, RouteHistory};
use crate::core::route_config::RoutingFile;
use crate::storage::fsutil;

/// Minimum time between automatic switches (also reset by any manual
/// switch through the same state file).
pub const MIN_SWITCH_INTERVAL: Duration = Duration::from_secs(900);
/// Bounded state file recording the last automatic switch.
pub const AUTO_STATE_FILE: &str = "auto-switch.json";
const AUTO_STATE_SCHEMA: u32 = 1;

/// The auto-switch policy acknowledgement in `config.toml`.
pub const AUTO_ACK_KEY: &str = "auto_switch_ack";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct AutoState {
    schema_version: u32,
    /// Last automatic switch (or refusal worth cooling down on).
    last_auto_switch: Option<DateTime<Utc>>,
    /// Last switch of ANY kind seen through this file; a manual `use`
    /// updates it too, so automation cannot chase a human.
    last_any_switch: Option<DateTime<Utc>>,
}

impl AutoState {
    fn load(dirs: &crate::paths::Dirs) -> Self {
        let path = dirs.state.join(AUTO_STATE_FILE);
        let Ok(Some(bytes)) = fsutil::read_optional(&path) else {
            return Self::default();
        };
        serde_json::from_slice(&bytes).unwrap_or_default()
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
    /// Nothing to do: automation off, unacknowledged, cooling down, or no
    /// better candidate.
    Hold(String),
    /// A gate (sessions, recovery, staleness) requires a human decision.
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
    let mut st = AutoState::load(dirs);
    st.schema_version = AUTO_STATE_SCHEMA;
    st.last_any_switch = Some(at);
    if automatic {
        st.last_auto_switch = Some(at);
    }
    st.save(dirs)
}

/// Record an automation outcome unless it repeats the previous entry
/// verbatim: the monitor evaluates every tick, and a steady hold (disabled,
/// cooling down) must not flush the bounded history of real decisions.
fn record_once(history: &RouteHistory, entry: HistoryEntry) -> Result<()> {
    let last = history.load().entries.last().cloned();
    if last.as_ref().is_some_and(|prev| {
        prev.group == entry.group
            && prev.target == entry.target
            && prev.outcome == entry.outcome
            && prev.reason == entry.reason
    }) {
        return Ok(());
    }
    history.record(entry)
}

/// A hold is a decision, and a contended history lock must not replace it
/// with "another agyva is running" — the same rule `route_apply` follows for
/// its refusals. One of these runs while `auto_switch_for` holds the login
/// lock (the under-lock re-checks), where the confusion is worst. Recording
/// is best effort; `record_once`'s dedupe still applies, and the
/// applied/failed path keeps propagating its errors.
fn record_hold_best_effort(history: &RouteHistory, entry: HistoryEntry) {
    if let Err(e) = record_once(history, entry) {
        tracing::warn!("could not record the auto-switch hold: {}", e.message());
    }
}

/// Evaluate the auto-switch decision for `group` against fresh evidence.
/// `plan` must come from a run whose active account was just refreshed
/// (`RefreshStatus::Refreshed`); the caller passes `refreshed` to prove it.
/// No mutation happens here beyond the cooldown bookkeeping.
pub fn decide(
    app: &crate::core::app::App,
    group: &str,
    plan: &crate::core::route::RoutePlan,
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
    if !refreshed {
        return AutoDecision::Defer(
            "decision needs a fresh live reading of the active account; cached reset times never trigger a switch".into(),
        );
    }
    // Cooldown: any recent switch (manual included) holds automation.
    let st = AutoState::load(&app.dirs);
    let min = chrono::Duration::from_std(MIN_SWITCH_INTERVAL).unwrap_or_default();
    if st.last_any_switch.is_some_and(|last| now - last < min) {
        // No elapsed seconds here: the reason is persisted, and a changing
        // number would defeat the consecutive-entry dedupe in the history.
        return AutoDecision::Hold(format!(
            "cooling down: minimum {}s between switches",
            MIN_SWITCH_INTERVAL.as_secs()
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
    // Candidate: the plan's top eligible row, which must be fresh and not
    // the active account. `rank()` already blocks TermsSuspended and other
    // blocked health states, exhausted quota, stale cache.
    let Some(target) = plan.accounts.iter().find(|r| r.eligible) else {
        return AutoDecision::Hold("no eligible candidate".into());
    };
    if target.active {
        return AutoDecision::Hold("the recommendation is already active".into());
    }
    AutoDecision::Switch {
        id: target.id.clone(),
        alias: target.alias.clone(),
    }
}

impl crate::core::app::App {
    /// The one journaled path: decide, revalidate under the login lock,
    /// then `route_apply` (which takes its own locks for the mutation and
    /// records history). The caller must have JUST refreshed the active
    /// account; that proof travels in `refreshed`.
    pub fn auto_switch_for(
        &self,
        group: &str,
        plan: &crate::core::route::RoutePlan,
        for_model: Option<&str>,
        refreshed: bool,
        history: &RouteHistory,
        now: DateTime<Utc>,
    ) -> Result<Option<crate::core::route_apply::ApplyOutcome>> {
        match decide(self, group, plan, refreshed, now) {
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
                // Revalidate under the lock: the group file may have changed
                // (auto_switch turned off), a manual switch may have landed,
                // recovery may be pending. Then release the lock and let
                // route_apply take it again for the journaled switch.
                {
                    let _lock = self.lock_login()?;
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
                    let st = AutoState::load(&self.dirs);
                    let min = chrono::Duration::from_std(MIN_SWITCH_INTERVAL).unwrap_or_default();
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
                    if let Some(p) = self.pending_journal()? {
                        return Err(Error::recovery_required(format!(
                            "an interrupted switch to account {} ({}) must be recovered first",
                            p.target,
                            p.phase.as_str()
                        )));
                    }
                }
                // The switch revalidates the SAME ranking the decision used:
                // without the caller's model filter, apply would fall back
                // to the whole-account order and activate a different
                // account than the one `decide` and the pane reported.
                let outcome = self.route_apply(group, for_model, false, true, history, now)?;
                note_switch(&self.dirs, now, true)?;
                Ok(Some(outcome))
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
        let st = AutoState::load(&dirs);
        assert!(st.last_auto_switch.is_some());
        assert!(st.last_any_switch.is_some());
        // Garbage in the file degrades to the default, never a crash.
        std::fs::write(dirs.state.join(AUTO_STATE_FILE), b"not json").unwrap();
        assert_eq!(AutoState::load(&dirs), AutoState::default());
    }
}
