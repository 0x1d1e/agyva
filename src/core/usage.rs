//! Usage and health views: one set of structs behind `agyva usage`,
//! `agyva health`, `--json` and the TUI. No secrets ever appear here.
//!
//! Each account's numbers come from agyva's cache, except the active account
//! right after a successful refresh. Everything says where it came from and
//! how old it is, and anything that could not be read says why.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::app::App;
use crate::core::error::Result;
use crate::core::health::{APPEAL_NOTICE, AccountHealth, HealthRecord, HealthSource};
use crate::core::quota::{
    Freshness, FreshnessPolicy, ModelQuota, QuotaSource, QuotaWindow, Unavailable,
    UnavailableReason,
};
use crate::core::quota_cache::CacheEntry;
use crate::core::quota_history::{BurnRateView, GroupWindowHistory, compute_burn_rate};
use crate::core::refresh::{RefreshOutcome, RefreshStatus};
use crate::core::registry::Registry;
use crate::core::view::VIEW_SCHEMA;

pub const NOT_REFRESHED: &str =
    "not refreshed - only the active account can be read; switch with `agyva use` to refresh";

#[derive(Debug, Clone, Serialize)]
pub struct WindowView {
    pub remaining_percent: f64,
    pub reset_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub source: QuotaSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burn_rate: Option<BurnRateView>,
}

impl From<&QuotaWindow> for WindowView {
    fn from(w: &QuotaWindow) -> Self {
        Self {
            remaining_percent: w.remaining_percent,
            reset_at: w.reset_at,
            observed_at: w.observed_at,
            source: w.source,
            burn_rate: None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct QuotaView {
    pub group: String,
    pub model: Option<String>,
    /// agy reports the group as not enabled for this account (not "empty").
    pub disabled: bool,
    pub exhausted: bool,
    pub five_hour: Option<WindowView>,
    pub weekly: Option<WindowView>,
}

impl QuotaView {
    pub fn label(&self) -> String {
        match &self.model {
            Some(m) => format!("{} / {m}", self.group),
            None => self.group.clone(),
        }
    }

    fn of(q: &ModelQuota, source: QuotaSource) -> Self {
        let relabel = |w: &Option<QuotaWindow>| {
            w.as_ref().map(|w| WindowView {
                source,
                ..WindowView::from(w)
            })
        };
        Self {
            // These strings come from agy; clean them before anything shows them.
            group: crate::core::format::clean_name(&q.group),
            model: q.model.as_deref().map(crate::core::format::clean_name),
            disabled: q.disabled,
            exhausted: q.exhausted(),
            five_hour: relabel(&q.five_hour),
            weekly: relabel(&q.weekly),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthView {
    pub state: AccountHealth,
    pub label: &'static str,
    pub evidence: String,
    pub observed_at: Option<DateTime<Utc>>,
    pub source: HealthSource,
    /// True only when agy itself answered during this run.
    pub verified_now: bool,
    /// Present for a Terms-of-Service suspension.
    pub warning: Option<String>,
}

impl HealthView {
    pub fn of(r: &HealthRecord, verified_now: bool) -> Self {
        Self {
            state: r.state,
            label: r.state.label(),
            evidence: r.evidence.clone(),
            observed_at: r.observed_at,
            source: r.source,
            verified_now,
            warning: (r.state == AccountHealth::TermsSuspended).then(|| APPEAL_NOTICE.to_owned()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct AccountUsage {
    pub number: usize,
    pub id: String,
    pub alias: String,
    pub email: Option<String>,
    pub active: bool,
    /// Subscription plan; agy's `/usage` does not report one, so `None`.
    pub plan: Option<String>,
    pub freshness: Freshness,
    pub observed_at: Option<DateTime<Utc>>,
    pub last_successful_update: Option<DateTime<Utc>>,
    pub last_attempt: Option<DateTime<Utc>>,
    /// Read from agy during this run (as opposed to the cache).
    pub refreshed_now: bool,
    pub groups: Vec<QuotaView>,
    /// Why there is no (new) reading, when that is the case.
    pub unavailable: Option<Unavailable>,
    pub health: HealthView,
}

impl AccountUsage {
    /// The first two windows over all groups: lowest remaining `5h`, `weekly`.
    pub fn lowest(&self) -> (Option<&WindowView>, Option<&WindowView>) {
        let low = |pick: fn(&QuotaView) -> Option<&WindowView>| {
            self.groups
                .iter()
                .filter(|g| !g.disabled)
                .filter_map(pick)
                .min_by(|a, b| a.remaining_percent.total_cmp(&b.remaining_percent))
        };
        (low(|g| g.five_hour.as_ref()), low(|g| g.weekly.as_ref()))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct UsageReport {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    /// Why nothing could be refreshed, when no account is involved.
    pub notice: Option<String>,
    pub accounts: Vec<AccountUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefreshMode {
    /// Cache only.
    Skip,
    Normal,
    /// Also check a suspended account, and ignore the refresh interval.
    Force,
}

pub struct UsageOptions {
    pub selector: Option<String>,
    pub all: bool,
    pub refresh: RefreshMode,
}

pub(crate) fn account_usage(
    app: &App,
    reg: &Registry,
    id: &str,
    entry: Option<&CacheEntry>,
    history: Option<&std::collections::BTreeMap<String, GroupWindowHistory>>,
    outcome: Option<&RefreshOutcome>,
    now: DateTime<Utc>,
) -> Option<AccountUsage> {
    let rec = reg.find(id)?;
    let policy = FreshnessPolicy::new(app.config.quota_fresh_secs);
    let empty = CacheEntry::default();
    let entry = entry.unwrap_or(&empty);
    let active = reg.active.as_deref() == Some(id);
    let mine = outcome.filter(|o| o.account_id.as_deref() == Some(id));
    let refreshed_now = mine.is_some_and(|o| matches!(o.status, RefreshStatus::Refreshed));
    let source = if refreshed_now {
        QuotaSource::AgyUsageCommand
    } else {
        QuotaSource::Cache
    };
    let unavailable = match mine.and_then(RefreshOutcome::unavailable) {
        Some(u) => Some(u.clone()),
        None if mine.is_some() => None,
        None if !active => Some(Unavailable::new(
            UnavailableReason::NotRefreshable,
            NOT_REFRESHED,
        )),
        None => entry.last_error.clone(),
    };
    let mut record = match mine.and_then(|o| o.live.as_ref()) {
        Some(live) => HealthView::of(
            &live.health,
            live.health.source == HealthSource::AgyUsageCommand,
        ),
        None => HealthView::of(
            &entry
                .health
                .clone()
                .map_or_else(HealthRecord::never_observed, HealthRecord::from_cache),
            false,
        ),
    };
    // The suspension marker survives a deleted or corrupt cache, so it still
    // explains why this account is not being refreshed.
    if record.state != AccountHealth::TermsSuspended
        && let Some(since) = app.suspended_markers().get(id)
    {
        record = HealthView::of(
            &HealthRecord {
                state: AccountHealth::TermsSuspended,
                evidence: format!(
                    "recorded as suspended on {} (use `--force-refresh` to check again)",
                    since.format("%Y-%m-%d %H:%M UTC")
                ),
                observed_at: Some(since),
                source: HealthSource::Cache,
            },
            false,
        );
    }
    // What this run just read is the truth, even if writing it to the cache
    // failed: never fall back to stale (or empty) cached numbers and call
    // them fresh.
    let live_groups = mine
        .filter(|o| matches!(o.status, RefreshStatus::Refreshed))
        .and_then(|o| o.live.as_ref())
        .and_then(|l| l.groups.as_ref());
    let (mut groups, observed_at): (Vec<QuotaView>, _) = match live_groups {
        Some(groups) => (
            groups.iter().map(|g| QuotaView::of(g, source)).collect(),
            Some(now),
        ),
        None => (
            entry
                .quota
                .as_ref()
                .map(|q| q.groups.iter().map(|g| QuotaView::of(g, source)).collect())
                .unwrap_or_default(),
            entry.quota.as_ref().map(|q| q.observed_at),
        ),
    };
    if let Some(acct_hist) = history {
        for g in &mut groups {
            if let Some(gh) = acct_hist.get(&g.label()) {
                if let Some(w) = &mut g.five_hour {
                    w.burn_rate = compute_burn_rate(&gh.five_hour, w.reset_at, now);
                }
                if let Some(w) = &mut g.weekly {
                    w.burn_rate = compute_burn_rate(&gh.weekly, w.reset_at, now);
                }
            }
        }
    }
    Some(AccountUsage {
        number: reg
            .accounts
            .iter()
            .position(|a| a.id == id)
            .map_or(0, |i| i + 1),
        id: id.to_owned(),
        alias: rec.alias.clone(),
        email: rec.email.clone(),
        active,
        plan: None,
        freshness: policy.classify(observed_at, now),
        observed_at,
        last_successful_update: entry.last_successful_update,
        last_attempt: entry.last_attempt,
        refreshed_now,
        groups,
        unavailable,
        health: record,
    })
}

impl App {
    /// Every saved account, from the cache plus what `outcome` just learned.
    pub fn account_usages(
        &self,
        outcome: Option<&RefreshOutcome>,
        now: DateTime<Utc>,
    ) -> Result<Vec<AccountUsage>> {
        let reg = self.load_registry()?;
        let cache = self.quota_cache().load();
        let history = self.quota_history().load();
        Ok(reg
            .accounts
            .iter()
            .filter_map(|a| {
                account_usage(
                    self,
                    &reg,
                    &a.id,
                    cache.accounts.get(&a.id),
                    history.accounts.get(&a.id),
                    outcome,
                    now,
                )
            })
            .collect())
    }

    pub fn usage_report(&self, opts: &UsageOptions) -> Result<UsageReport> {
        let now = Utc::now();
        let wanted = match &opts.selector {
            Some(s) => Some(self.resolve(s)?.id),
            None => None,
        };
        let outcome = match opts.refresh {
            RefreshMode::Skip => None,
            m => Some(self.refresh_active_at(m == RefreshMode::Force, now)?),
        };
        let mut accounts = self.account_usages(outcome.as_ref(), now)?;
        match (&wanted, opts.all) {
            (Some(id), _) => accounts.retain(|a| &a.id == id),
            (None, true) => {}
            (None, false) => accounts.retain(|a| a.active),
        }
        let notice = outcome
            .as_ref()
            .filter(|o| o.account_id.is_none())
            .and_then(RefreshOutcome::unavailable)
            .map(Unavailable::text);
        Ok(UsageReport {
            schema_version: VIEW_SCHEMA,
            generated_at: now,
            notice,
            accounts,
        })
    }
}
