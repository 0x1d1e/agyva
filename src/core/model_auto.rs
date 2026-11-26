//! Explicit one-family rotator for `agyva auto`.
//!
//! This is not the monitor engine. It does not read `routing.toml`, does not
//! require `auto_switch_ack`, and does not consult the session gate. The user
//! asked for the switch; a running agy still keeps the login it already
//! loaded, so the caller must print [`crate::core::policy::RESTART_NOTICE`].

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::app::App;
use crate::core::auto::last_any_switch;
use crate::core::error::{Error, Result};
use crate::core::health::AccountHealth;
use crate::core::policy::RESTART_NOTICE;
use crate::core::quota::ModelQuota;
use crate::core::refresh::RefreshStatus;
use crate::core::view::VIEW_SCHEMA;

/// Usage strictly over this percent switches. Quota stores remaining, so the
/// line is `remaining_percent < 10.0`. Exactly 10.0 does not switch.
pub const USAGE_OVER_PERCENT: f64 = 90.0;
pub const REMAINING_SWITCH_BELOW: f64 = 100.0 - USAGE_OVER_PERCENT;
/// Cooldown for this command only. The monitor's 900s default is unchanged.
pub const COOLDOWN: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    Gemini,
    Claude,
}

impl ModelFamily {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Gemini => "gemini",
            Self::Claude => "claude",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Gemini => "Gemini",
            Self::Claude => "Claude",
        }
    }

    fn needle(self) -> &'static str {
        self.as_str()
    }

    pub fn parse(raw: &str) -> Result<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "gemini" => Ok(Self::Gemini),
            "claude" => Ok(Self::Claude),
            other => Err(Error::usage(format!(
                "unknown model family `{other}` (expected gemini or claude)"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelAutoOnce {
    pub schema_version: u32,
    pub model: &'static str,
    pub switched: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_notice: Option<&'static str>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl ModelAutoOnce {
    fn hold(family: ModelFamily, message: String) -> Self {
        Self {
            schema_version: VIEW_SCHEMA,
            model: family.as_str(),
            switched: false,
            target: None,
            message,
            restart_notice: None,
            warnings: Vec::new(),
        }
    }
}

/// Lowest remaining percent among enabled groups whose name contains the
/// family, across the 5-hour and weekly windows. `None` means the family
/// was not reported — never a synthesized zero.
pub(crate) fn family_remaining(groups: &[ModelQuota], family: ModelFamily) -> Option<f64> {
    let needle = family.needle();
    let mut min: Option<f64> = None;
    for group in groups {
        if group.disabled || !group.group.to_ascii_lowercase().contains(needle) {
            continue;
        }
        for window in group.windows() {
            let pct = window.remaining_percent;
            if !pct.is_finite() || !(0.0..=100.0).contains(&pct) {
                continue;
            }
            min = Some(min.map_or(pct, |v| v.min(pct)));
        }
    }
    min
}

fn percent_text(pct: f64) -> String {
    format!("{pct:.1}%")
}

struct Candidate {
    alias: String,
    remaining: f64,
}

impl App {
    /// One evaluation. Live-reads the active account, then may switch through
    /// the journaled `use` path. Standby accounts are cache only.
    pub fn model_auto_once(
        &self,
        family: ModelFamily,
        now: DateTime<Utc>,
    ) -> Result<ModelAutoOnce> {
        self.require_no_pending()?;
        let refreshed = self.refresh_active(true)?;
        if !matches!(refreshed.status, RefreshStatus::Refreshed) {
            let detail = refreshed
                .unavailable()
                .map(|u| u.detail.clone())
                .unwrap_or_else(|| "live refresh did not run".into());
            return Err(Error::backend(format!(
                "could not live-read the active account: {detail}"
            )));
        }
        let groups = refreshed
            .live
            .as_ref()
            .and_then(|live| live.groups.as_deref())
            .unwrap_or(&[]);
        let Some(remaining) = family_remaining(groups, family) else {
            return Ok(ModelAutoOnce::hold(
                family,
                format!("{} was not reported; not switching", family.label()),
            ));
        };
        if remaining >= REMAINING_SWITCH_BELOW {
            return Ok(ModelAutoOnce::hold(
                family,
                format!(
                    "{} remaining is {}; not switching",
                    family.label(),
                    percent_text(remaining)
                ),
            ));
        }
        if let Some(wait) = cooldown_remaining(&self.dirs, now)? {
            return Ok(ModelAutoOnce::hold(
                family,
                format!(
                    "cooling down for {}s after a switch; not switching",
                    wait.as_secs().max(1)
                ),
            ));
        }
        let active = refreshed.account_id.as_deref();
        let Some(candidate) = self.best_candidate(family, active, now)? else {
            return Ok(ModelAutoOnce::hold(
                family,
                format!("no eligible account for {}; not switching", family.label()),
            ));
        };
        // Recheck under the login lock so a manual `use` that landed while
        // we were reading cache still wins.
        let lock = self.lock_login()?;
        if let Some(wait) = cooldown_remaining(&self.dirs, now)? {
            drop(lock);
            return Ok(ModelAutoOnce::hold(
                family,
                format!(
                    "cooling down for {}s after a switch; not switching",
                    wait.as_secs().max(1)
                ),
            ));
        }
        let outcome = self.switch_under_lock(&lock, &candidate.alias, false, now, true, true)?;
        drop(lock);
        if outcome.already_active {
            return Ok(ModelAutoOnce::hold(
                family,
                format!("{} is already active; not switching", outcome.account.alias),
            ));
        }
        let mut warnings = outcome.warnings;
        warnings.retain(|w| w != RESTART_NOTICE);
        if let Some(warning) = self.clear_endpoint_after_account_switch(&outcome.account.alias) {
            warnings.push(warning);
        }
        let switched_alias = outcome.account.alias;
        Ok(ModelAutoOnce {
            schema_version: VIEW_SCHEMA,
            model: family.as_str(),
            switched: true,
            target: Some(switched_alias.clone()),
            message: format!(
                "switched to {switched_alias} because {} remaining is {} (usage over 90%)",
                family.label(),
                percent_text(remaining)
            ),
            restart_notice: Some(RESTART_NOTICE),
            warnings,
        })
    }

    fn best_candidate(
        &self,
        family: ModelFamily,
        active_id: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<Candidate>> {
        let reg = self.load_registry()?;
        let cache = self.quota_cache().load();
        let markers = self.suspended_markers();
        let max_age = chrono::Duration::seconds(
            i64::try_from(self.config.auto_switch_candidate_max_age_secs).unwrap_or(21_600),
        );
        let mut best: Option<Candidate> = None;
        for account in &reg.accounts {
            if active_id == Some(account.id.as_str()) {
                continue;
            }
            let entry = cache.accounts.get(&account.id);
            let suspended = entry
                .and_then(|e| e.health.as_ref())
                .is_some_and(|h| h.state == AccountHealth::TermsSuspended)
                || markers.get(&account.id).is_some();
            if suspended {
                continue;
            }
            let Some(quota) = entry.and_then(|e| e.quota.as_ref()) else {
                continue;
            };
            let age = now.signed_duration_since(quota.observed_at);
            if age < chrono::Duration::zero() || age > max_age {
                continue;
            }
            let Some(remaining) = family_remaining(&quota.groups, family) else {
                continue;
            };
            if remaining < REMAINING_SWITCH_BELOW {
                continue;
            }
            let replace = match &best {
                None => true,
                Some(cur) => {
                    remaining > cur.remaining
                        || ((remaining - cur.remaining).abs() < f64::EPSILON
                            && account.alias < cur.alias)
                }
            };
            if replace {
                best = Some(Candidate {
                    alias: account.alias.clone(),
                    remaining,
                });
            }
        }
        Ok(best)
    }
}

fn cooldown_remaining(
    dirs: &crate::paths::Dirs,
    now: DateTime<Utc>,
) -> Result<Option<std::time::Duration>> {
    let Some(last) = last_any_switch(dirs)? else {
        return Ok(None);
    };
    let elapsed = now.signed_duration_since(last);
    if elapsed < chrono::Duration::zero() {
        return Ok(Some(COOLDOWN));
    }
    let elapsed = elapsed.to_std().unwrap_or(COOLDOWN);
    if elapsed < COOLDOWN {
        Ok(Some(COOLDOWN.saturating_sub(elapsed)))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::quota::{QuotaSource, QuotaWindow};

    fn window(pct: f64) -> QuotaWindow {
        QuotaWindow {
            remaining_percent: pct,
            reset_at: None,
            observed_at: Utc::now(),
            source: QuotaSource::Cache,
        }
    }

    fn group(name: &str, disabled: bool, five: Option<f64>, week: Option<f64>) -> ModelQuota {
        ModelQuota {
            group: name.into(),
            model: None,
            disabled,
            five_hour: five.map(window),
            weekly: week.map(window),
        }
    }

    #[test]
    fn family_remaining_uses_the_worse_matching_window_and_skips_disabled() {
        let groups = vec![
            group("Gemini Pro", false, Some(40.0), Some(12.0)),
            group("gemini flash", false, Some(8.0), Some(50.0)),
            group("Claude", true, Some(0.0), Some(0.0)),
            group("other", false, Some(1.0), None),
        ];
        assert_eq!(family_remaining(&groups, ModelFamily::Gemini), Some(8.0));
        assert_eq!(family_remaining(&groups, ModelFamily::Claude), None);
        assert_eq!(
            family_remaining(
                &[group("Claude Opus", false, Some(10.0), Some(4.0))],
                ModelFamily::Claude
            ),
            Some(4.0)
        );
    }

    #[test]
    fn exactly_ten_remaining_is_not_over_the_line() {
        let exact = 10.0_f64;
        let just_under = 9.9_f64;
        assert!(exact >= REMAINING_SWITCH_BELOW);
        assert!(just_under < REMAINING_SWITCH_BELOW);
    }
}
