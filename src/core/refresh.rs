//! Refreshing quota and health for the *active* account.
//!
//! Only the account agy is logged in as can be read. Rules enforced here:
//!
//! * the live login must match the registry's active account (checked before
//!   and after running agy), otherwise nothing is attributed to anyone;
//! * a pending interrupted switch blocks refreshing;
//! * a `TermsSuspended` account is never refreshed unless `force` is given;
//! * refreshes closer together than `quota_min_refresh_secs` are refused;
//! * a failed or throttled refresh never invents or upgrades anything.

use std::sync::Arc;

use chrono::{DateTime, Utc};

use crate::agy::credentials::parse_credential;
use crate::agy::discovery;
use crate::agy::usage_run::RunLimits;
use crate::core::app::App;
use crate::core::error::{ErrorKind, Result};
use crate::core::health::{AccountHealth, Classification, HealthRecord, HealthSource, classify};
use crate::core::provider::{AgyUsageProvider, NoProvider, QuotaProvider, Ran, Reading};
use crate::core::quota::{ModelQuota, Unavailable, UnavailableReason};
use crate::core::quota_cache::{CachedQuota, QuotaCache};
use crate::core::suspended::SuspendedMarkers;
use crate::storage::lock::StateLock;

/// What a refresh learned this run (as opposed to what the cache remembers).
#[derive(Debug, Clone)]
pub struct LiveReading {
    pub health: HealthRecord,
    pub groups: Option<Vec<ModelQuota>>,
}

#[derive(Debug, Clone)]
pub enum RefreshStatus {
    Refreshed,
    /// Nothing was run (throttled, suspended, no provider, ...).
    Skipped(Unavailable),
    /// agy ran but did not give usable quota.
    Failed(Unavailable),
}

#[derive(Debug, Clone)]
pub struct RefreshOutcome {
    pub account_id: Option<String>,
    pub status: RefreshStatus,
    pub live: Option<LiveReading>,
}

impl RefreshOutcome {
    fn skipped(id: Option<String>, u: Unavailable) -> Self {
        Self {
            account_id: id,
            status: RefreshStatus::Skipped(u),
            live: None,
        }
    }

    pub fn unavailable(&self) -> Option<&Unavailable> {
        match &self.status {
            RefreshStatus::Refreshed => None,
            RefreshStatus::Skipped(u) | RefreshStatus::Failed(u) => Some(u),
        }
    }

    /// Worth retrying later with backoff.
    pub fn is_transient_failure(&self) -> bool {
        matches!(
            &self.status,
            RefreshStatus::Failed(u) if matches!(
                u.reason,
                UnavailableReason::Network | UnavailableReason::TimedOut | UnavailableReason::RateLimited
            )
        )
    }
}

fn unavailable_for(class: &Classification, ran: &Ran, parse_note: &str) -> Unavailable {
    use UnavailableReason as R;
    if ran.signals.timed_out {
        return Unavailable::new(R::TimedOut, "agy did not answer in time");
    }
    match class.state {
        AccountHealth::TermsSuspended => Unavailable::new(
            R::Suspended,
            "agy reports this account is suspended (Terms of Service)",
        ),
        AccountHealth::NetworkUnavailable => {
            Unavailable::new(R::Network, "network error while agy asked for usage")
        }
        AccountHealth::AuthenticationRequired
        | AccountHealth::TokenExpired
        | AccountHealth::AccessRevoked => Unavailable::new(R::AuthRequired, &class.evidence),
        AccountHealth::QuotaLimited => Unavailable::new(R::RateLimited, &class.evidence),
        _ if ran.signals.exit_code != Some(0) => Unavailable::new(
            R::AgyFailed,
            format!(
                "agy exited with status {}",
                ran.signals
                    .exit_code
                    .map_or_else(|| "?".to_owned(), |c| c.to_string())
            ),
        ),
        _ => Unavailable::new(
            R::SchemaChanged,
            format!("agy's /usage output was not in a recognized format ({parse_note})"),
        ),
    }
}

impl App {
    pub fn quota_cache(&self) -> QuotaCache {
        QuotaCache::new(&self.dirs)
    }

    pub fn suspended_markers(&self) -> SuspendedMarkers {
        SuspendedMarkers::new(&self.dirs)
    }

    /// The quota source for this environment.
    pub fn quota_provider(&self) -> Arc<dyn QuotaProvider> {
        if let Some(p) = &self.provider_override {
            return p.clone();
        }
        let none = |why: String| -> Arc<dyn QuotaProvider> { Arc::new(NoProvider { why }) };
        let Some(bin) = discovery::find_agy(&self.env) else {
            return none("agy was not found on PATH (set AGYVA_AGY_BIN to override)".into());
        };
        let compat = self.compat();
        if compat.require(self.allow_untested).is_err() {
            return none(format!("{}; usage reading is disabled", compat.summary()));
        }
        Arc::new(AgyUsageProvider {
            bin,
            limits: RunLimits::default(),
        })
    }

    /// Which saved-or-unsaved identity agy is logged in as.
    pub(crate) fn live_account_id(&self) -> std::result::Result<String, Unavailable> {
        let read = self.native().and_then(|n| n.read());
        let raw = match read {
            Ok(Some(r)) => r,
            Ok(None) => {
                return Err(Unavailable::new(
                    UnavailableReason::NotLoggedIn,
                    "agy is not logged in",
                ));
            }
            Err(e) => {
                return Err(Unavailable::new(
                    UnavailableReason::LoginUnreadable,
                    format!("cannot read agy's login: {}", e.message()),
                ));
            }
        };
        parse_credential(raw.as_bytes())
            .map(|i| i.account_id())
            .map_err(|e| {
                Unavailable::new(
                    UnavailableReason::LoginUnreadable,
                    format!("agy's login is not recognized: {}", e.message()),
                )
            })
    }

    pub fn refresh_active(&self, force: bool) -> Result<RefreshOutcome> {
        self.refresh_active_at(force, Utc::now())
    }

    pub fn refresh_active_at(&self, force: bool, now: DateTime<Utc>) -> Result<RefreshOutcome> {
        use UnavailableReason as R;
        let reg = self.load_registry()?;
        let Some(active) = reg.active.clone() else {
            return Ok(RefreshOutcome::skipped(
                None,
                Unavailable::new(R::NoActiveAccount, "no account is active; run `agyva use`"),
            ));
        };
        let id = Some(active.clone());
        // One refresh at a time, and never alongside a switch, recovery or
        // add (they hold this lock for their whole run). Wait briefly, then
        // skip: background refreshes must not queue up behind each other.
        let _refreshing =
            match StateLock::acquire(&self.dirs.refresh_lock_file(), self.refresh_wait) {
                Ok(guard) => guard,
                Err(e) if e.kind() == ErrorKind::Locked => {
                    return Ok(RefreshOutcome::skipped(
                        id,
                        Unavailable::new(
                            R::RefreshBusy,
                            "another agyva is switching accounts or refreshing; try again shortly",
                        ),
                    ));
                }
                Err(e) => return Err(e),
            };
        if self.pending_journal().ok().flatten().is_some() {
            return Ok(RefreshOutcome::skipped(
                id,
                Unavailable::new(
                    R::SwitchPending,
                    "an interrupted switch awaits `agyva recover`",
                ),
            ));
        }
        let cache = self.quota_cache();
        let entry = cache.load().accounts.remove(&active).unwrap_or_default();
        // The persisted marker outlives the cache (which is disposable): a
        // suspended account stays unrefreshable even if quota.json is gone.
        let markers = self.suspended_markers();
        if !force {
            let flagged = entry
                .health
                .as_ref()
                .is_some_and(|h| h.state == AccountHealth::TermsSuspended)
                || markers.get(&active).is_some();
            if flagged {
                return Ok(RefreshOutcome::skipped(
                    id,
                    Unavailable::new(
                        R::Suspended,
                        "account is suspended; not refreshing (use --force-refresh to check once)",
                    ),
                ));
            }
            let min = chrono::Duration::seconds(
                i64::try_from(self.config.quota_min_refresh_secs).unwrap_or(60),
            );
            if let Some(last) = entry.last_attempt.filter(|l| now - *l < min && now >= *l) {
                let age = (now - last).num_seconds().max(0);
                return Ok(RefreshOutcome::skipped(
                    id,
                    Unavailable::new(
                        R::TooSoon,
                        format!("using cached reading (refreshed {age}s ago)"),
                    ),
                ));
            }
        }
        let provider = self.quota_provider();
        match self.live_account_id() {
            Err(u) => return Ok(RefreshOutcome::skipped(id, u)),
            Ok(live) if live != active => {
                return Ok(RefreshOutcome::skipped(id, differs()));
            }
            Ok(_) => {}
        }

        let reading = provider.fetch(now);
        let ran = match reading {
            Reading::NoProvider(u) => return Ok(RefreshOutcome::skipped(id, u)),
            Reading::Ran(r) => r,
        };
        if self.live_account_id().as_deref() != Ok(active.as_str()) {
            return Ok(RefreshOutcome::skipped(
                id,
                Unavailable::new(
                    R::ActiveLoginDiffers,
                    "the agy login changed during the refresh",
                ),
            ));
        }

        let class = classify(&ran.signals, ran.groups.as_deref().ok());
        let fresh = HealthRecord {
            state: class.state,
            evidence: class.evidence.clone(),
            observed_at: Some(now),
            source: HealthSource::AgyUsageCommand,
        };
        if class.state == AccountHealth::TermsSuspended {
            if let Err(e) = markers.mark(&active, now) {
                tracing::warn!("could not record the suspension: {}", e.message());
            }
        } else if force && ran.groups.is_ok() {
            // Only an explicit forced refresh that actually got quota may
            // clear the marker.
            if let Err(e) = markers.clear(&active) {
                tracing::warn!("could not clear the suspension marker: {}", e.message());
            }
        }
        let failure = ran
            .groups
            .as_ref()
            .err()
            .map(|note| unavailable_for(&class, &ran, note));
        let groups = ran.groups.clone().ok();
        let mut effective = fresh.clone();
        let ids: Vec<String> = reg.accounts.iter().map(|a| a.id.clone()).collect();
        // The catalog cache is pruned in the same breath as quota: a removed
        // account's observation is dead weight and must not linger forever
        // in cache/models.json. Best effort — a failure here must not turn a
        // successful quota refresh into an error.
        if let Err(e) = self.models_cache().retain(&ids) {
            tracing::warn!("could not prune the models cache: {}", e.message());
        }
        let write = cache.update(|f| {
            f.accounts.retain(|k, _| ids.contains(k));
            let e = f.accounts.entry(active.clone()).or_default();
            e.last_attempt = Some(now);
            match (&groups, &failure) {
                (Some(g), _) => {
                    e.quota = Some(CachedQuota {
                        observed_at: now,
                        groups: g.clone(),
                    });
                    e.last_successful_update = Some(now);
                    e.last_error = None;
                }
                (None, f) => e.last_error = f.clone(),
            }
            let transient = matches!(
                fresh.state,
                AccountHealth::Unknown | AccountHealth::NetworkUnavailable
            );
            match e.health.take() {
                Some(old) if old.state.is_sticky() && transient => {
                    effective = old.clone().from_cache();
                    e.health = Some(old);
                }
                _ => e.health = Some(fresh.clone()),
            }
        });
        if let Err(e) = write {
            tracing::warn!("could not update the quota cache: {}", e.message());
        }
        let live = Some(LiveReading {
            health: effective,
            groups: groups.clone(),
        });
        let status = match failure {
            None => RefreshStatus::Refreshed,
            Some(u) => RefreshStatus::Failed(u),
        };
        Ok(RefreshOutcome {
            account_id: id,
            status,
            live,
        })
    }
}

fn differs() -> Unavailable {
    Unavailable::new(
        UnavailableReason::ActiveLoginDiffers,
        "active login differs from registry; quota not attributed (run `agyva use` to correct)",
    )
}
