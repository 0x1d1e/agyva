//! Quota data model: what was observed, where it came from and how old it is.
//!
//! Nothing here is ever invented. A number exists only if agy reported it;
//! otherwise the view says *why* it is unavailable.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// Where a number came from, so the UI never presents a guess as a fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaSource {
    /// Read from the running `agy -p "/usage"` just now.
    AgyUsageCommand,
    /// Loaded from agyva's non-sensitive cache (an earlier observation).
    Cache,
}

impl QuotaSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::AgyUsageCommand => "agy /usage",
            Self::Cache => "cache",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QuotaWindow {
    /// 0..=100, as reported (`remaining_fraction * 100`).
    pub remaining_percent: f64,
    pub reset_at: Option<DateTime<Utc>>,
    pub observed_at: DateTime<Utc>,
    pub source: QuotaSource,
}

/// One quota group (optionally one model inside it) with its two windows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ModelQuota {
    pub group: String,
    pub model: Option<String>,
    /// agy marks the group disabled (not entitled). Distinct from exhausted.
    #[serde(default)]
    pub disabled: bool,
    pub five_hour: Option<QuotaWindow>,
    pub weekly: Option<QuotaWindow>,
}

impl ModelQuota {
    pub fn windows(&self) -> impl Iterator<Item = &QuotaWindow> {
        self.five_hour.iter().chain(self.weekly.iter())
    }

    /// An enabled group with a window at exactly zero.
    pub fn exhausted(&self) -> bool {
        !self.disabled && self.windows().any(|w| w.remaining_percent <= 0.0)
    }

    pub fn label(&self) -> String {
        use crate::core::format::clean_name;
        match &self.model {
            Some(m) => format!("{} / {}", clean_name(&self.group), clean_name(m)),
            None => clean_name(&self.group),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Freshness {
    Fresh,
    Stale,
    /// Too old to be useful at all.
    Expired,
    /// Nothing was ever observed.
    Missing,
}

impl Freshness {
    pub fn label(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Stale => "stale",
            Self::Expired => "expired",
            Self::Missing => "no data",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FreshnessPolicy {
    pub fresh: Duration,
    pub expire: Duration,
}

impl FreshnessPolicy {
    pub fn new(fresh_secs: u64) -> Self {
        let fresh = i64::try_from(fresh_secs).unwrap_or(i64::MAX / 4);
        Self {
            fresh: Duration::seconds(fresh),
            expire: Duration::hours(24).max(Duration::seconds(fresh)),
        }
    }

    pub fn classify(&self, observed_at: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Freshness {
        let Some(at) = observed_at else {
            return Freshness::Missing;
        };
        let age = now - at;
        if age < -Duration::seconds(60) {
            // An observation from the future is not trustworthy.
            Freshness::Stale
        } else if age <= self.fresh {
            Freshness::Fresh
        } else if age <= self.expire {
            Freshness::Stale
        } else {
            Freshness::Expired
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnavailableReason {
    /// No way to read quota (agy missing, or a version that is not allowed).
    NoProvider,
    /// agy answered, but not in a shape this build understands.
    SchemaChanged,
    ActiveLoginDiffers,
    NoActiveAccount,
    NotLoggedIn,
    LoginUnreadable,
    SwitchPending,
    /// Another agyva is switching accounts or already refreshing.
    RefreshBusy,
    Suspended,
    TooSoon,
    TimedOut,
    Network,
    AuthRequired,
    RateLimited,
    AgyFailed,
    /// Only the active login can be read; this account is not it.
    NotRefreshable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Unavailable {
    pub reason: UnavailableReason,
    pub detail: String,
}

impl Unavailable {
    pub fn new(reason: UnavailableReason, detail: impl Into<String>) -> Self {
        Self {
            reason,
            detail: detail.into(),
        }
    }

    pub fn text(&self) -> String {
        self.detail.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_boundaries() {
        let p = FreshnessPolicy::new(300);
        let now = Utc::now();
        assert_eq!(p.classify(None, now), Freshness::Missing);
        assert_eq!(p.classify(Some(now), now), Freshness::Fresh);
        assert_eq!(
            p.classify(Some(now - Duration::seconds(300)), now),
            Freshness::Fresh
        );
        assert_eq!(
            p.classify(Some(now - Duration::seconds(301)), now),
            Freshness::Stale
        );
        assert_eq!(
            p.classify(Some(now - Duration::hours(25)), now),
            Freshness::Expired
        );
        assert_eq!(
            p.classify(Some(now + Duration::hours(2)), now),
            Freshness::Stale,
            "future timestamps are not trusted"
        );
    }
}
