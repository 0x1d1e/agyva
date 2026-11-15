//! Cache-only recommendations. Ranking never refreshes quota or changes a login.
use std::cmp::Ordering;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::app::App;
use crate::core::error::Result;
use crate::core::health::AccountHealth;
use crate::core::quota::{Freshness, FreshnessPolicy, UnavailableReason};
use crate::core::usage::AccountUsage;

#[derive(Debug, Clone, Serialize)]
pub struct RouteRow {
    pub id: String,
    pub alias: String,
    pub active: bool,
    pub freshness: Freshness,
    pub eligible: bool,
    pub remaining_percent: Option<f64>,
    pub rate_remaining_percent: Option<f64>,
    pub renewals: [Option<DateTime<Utc>>; 2],
    pub why: String,
    pub command: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct RoutePlan {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub recommendation: Option<String>,
    pub accounts: Vec<RouteRow>,
}

fn known_time(a: Option<DateTime<Utc>>, b: Option<DateTime<Utc>>) -> Ordering {
    match (a, b) {
        (Some(a), Some(b)) => (a.timestamp() / 3600).cmp(&(b.timestamp() / 3600)),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        _ => Ordering::Equal,
    }
}

fn freshness_rank(f: Freshness) -> u8 {
    match f {
        Freshness::Fresh => 0,
        Freshness::Stale => 1,
        Freshness::Expired => 2,
        Freshness::Missing => 3,
    }
}

fn tier(r: &RouteRow) -> u8 {
    if !r.eligible {
        3
    } else if r.remaining_percent.is_some_and(|v| v <= 0.0) {
        2
    } else if r.rate_remaining_percent.is_some_and(|v| v <= 10.0) {
        1
    } else {
        0
    }
}

/// The fullest counting window sets the share; weekly budgets decide renewal
/// order before five-hour rate caps. Stable sorting preserves registry order.
pub fn rank(accounts: &[AccountUsage], now: DateTime<Utc>, policy: FreshnessPolicy) -> RoutePlan {
    let mut rows: Vec<_> = accounts
        .iter()
        .map(|a| {
            let freshness = policy.classify(a.observed_at, now);
            let mut remaining: Option<f64> = None;
            let mut rate: Option<f64> = None;
            let mut renewals = [None, None];
            let mut invalid_window = false;
            for g in a.groups.iter().filter(|g| !g.disabled) {
                invalid_window |= g.weekly.is_none() || g.five_hour.is_none();
                for (i, w) in [g.weekly.as_ref(), g.five_hour.as_ref()]
                    .into_iter()
                    .enumerate()
                {
                    if let Some(w) = w {
                        invalid_window |= !w.remaining_percent.is_finite()
                            || !(0.0..=100.0).contains(&w.remaining_percent)
                            || w.reset_at.is_none_or(|at| at <= now)
                            || policy.classify(Some(w.observed_at), now) != Freshness::Fresh;
                        remaining = Some(
                            remaining.map_or(w.remaining_percent, |v| v.min(w.remaining_percent)),
                        );
                        if i == 1 {
                            rate = Some(
                                rate.map_or(w.remaining_percent, |v| v.min(w.remaining_percent)),
                            );
                        }
                        if let Some(reset) = w.reset_at {
                            renewals[i] =
                                Some(renewals[i].map_or(reset, |v: DateTime<Utc>| v.min(reset)));
                        }
                    }
                }
            }
            let blocked_health = matches!(
                a.health.state,
                AccountHealth::TermsSuspended
                    | AccountHealth::AccessRevoked
                    | AccountHealth::AuthenticationRequired
                    | AccountHealth::TokenExpired
                    | AccountHealth::NetworkUnavailable
            );
            let blocked_error = a.unavailable.as_ref().is_some_and(|u| {
                !matches!(
                    u.reason,
                    UnavailableReason::NotRefreshable | UnavailableReason::TooSoon
                )
            });
            let eligible = !blocked_health
                && !blocked_error
                && freshness == Freshness::Fresh
                && !invalid_window
                && remaining.is_some_and(|v| v > 0.0);
            let why = if blocked_health {
                a.health.state.label().to_owned()
            } else if blocked_error {
                "unavailable reading".into()
            } else if remaining.is_none() {
                "quota unknown".into()
            } else if freshness != Freshness::Fresh || invalid_window {
                "quota stale, incomplete, expired, or reset passed; refresh required".into()
            } else if remaining.is_some_and(|v| v <= 0.0) {
                "quota exhausted".into()
            } else {
                format!(
                    "{} cache; {:.0}% remains in fullest window; weekly renewal before rate cap",
                    freshness.label(),
                    remaining.unwrap_or_default()
                )
            };
            RouteRow {
                id: a.id.clone(),
                alias: a.alias.clone(),
                active: a.active,
                freshness,
                eligible,
                remaining_percent: remaining,
                rate_remaining_percent: rate,
                renewals,
                why,
                command: format!("agyva use {}", a.alias),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        tier(a)
            .cmp(&tier(b))
            .then_with(|| freshness_rank(a.freshness).cmp(&freshness_rank(b.freshness)))
            .then_with(|| {
                if tier(a) == 0 {
                    known_time(a.renewals[0], b.renewals[0])
                        .then_with(|| known_time(a.renewals[1], b.renewals[1]))
                } else {
                    b.remaining_percent
                        .partial_cmp(&a.remaining_percent)
                        .unwrap_or(Ordering::Equal)
                }
            })
    });
    RoutePlan {
        schema_version: 1,
        generated_at: now,
        recommendation: rows.iter().find(|r| r.eligible).map(|r| r.id.clone()),
        accounts: rows,
    }
}

/// How a mapped quota pool is named in reasons: `group` or `group / model`.
fn group_label(group: &str, model: Option<&str>) -> String {
    match model {
        Some(m) if !m.is_empty() => format!("{group} / {m}"),
        _ => group.to_owned(),
    }
}

impl App {
    pub fn route_plan_at(&self, now: DateTime<Utc>) -> Result<RoutePlan> {
        Ok(rank(
            &self.account_usages(None, now)?,
            now,
            FreshnessPolicy::new(self.config.quota_fresh_secs),
        ))
    }

    pub fn route_group_at(&self, group: Option<&str>, now: DateTime<Utc>) -> Result<RoutePlan> {
        use crate::core::route_config::{DeclaredPlan, Ranking, RoutingFile};
        let config = RoutingFile::load(&self.dirs.config.join("routing.toml"))?;
        let group = config.group(group)?;
        let mut accounts = self.account_usages(None, now)?;
        if !group.accounts.is_empty() {
            let reg = self.load_registry()?;
            let ids = group
                .accounts
                .iter()
                .map(|s| crate::core::selector::resolve(&reg, s).map(|a| a.id.clone()))
                .collect::<Result<Vec<_>>>()?;
            accounts.retain(|a| ids.contains(&a.id));
            accounts.sort_by_key(|a| ids.iter().position(|id| id == &a.id));
        }
        if group.ranking == Ranking::Priority {
            accounts.sort_by_key(|a| {
                std::cmp::Reverse(
                    group
                        .priorities
                        .get(&a.alias)
                        .or_else(|| group.priorities.get(&a.id))
                        .copied()
                        .unwrap_or_default(),
                )
            });
        }
        let mut plan = rank(
            &accounts,
            now,
            FreshnessPolicy::new(self.config.quota_fresh_secs),
        );
        if group.ranking == Ranking::Priority {
            plan.accounts
                .sort_by_key(|row| (!row.eligible, accounts.iter().position(|a| a.id == row.id)));
            plan.recommendation = plan
                .accounts
                .iter()
                .find(|r| r.eligible)
                .map(|r| r.id.clone());
        }
        if group.ranking == Ranking::LeastUsed {
            // Share used, least first: declared plans rank free < pro <
            // ultra; unknown shares stay behind any declared plan. Equal
            // shares keep smart's ordering (stable sort).
            // A declared plan may be written against the alias or the
            // stable id; look up alias first, then id, exactly like
            // `priorities` — an id-only group must not silently degrade.
            let used_rank = |row: &RouteRow| -> u8 {
                match group
                    .plans
                    .get(&row.alias)
                    .or_else(|| group.plans.get(&row.id))
                {
                    Some(DeclaredPlan::Free) => 0,
                    Some(DeclaredPlan::Pro) => 1,
                    Some(DeclaredPlan::Ultra) => 2,
                    None => 3,
                }
            };
            plan.accounts
                .sort_by_key(|row| (!row.eligible, used_rank(row)));
            plan.recommendation = plan
                .accounts
                .iter()
                .find(|r| r.eligible)
                .map(|r| r.id.clone());
        }
        Ok(plan)
    }

    /// `route inspect|plan --for-model <model>`: the plain plan with a
    /// catalog-aware eligibility pass. The model never grants eligibility on
    /// its own: a missing or stale catalog, an absent mapping, exhausted or
    /// stale mapped windows, or bad health still excludes, with a reason.
    /// The declared mapping (routing.toml `models`) selects which quota
    /// group's windows apply when the catalog offers the family.
    ///
    /// When a mapping resolves, that group's windows — not the whole-account
    /// aggregate — decide both share and eligibility, so a family whose own
    /// quota is healthy is eligible even if another bucket on the same
    /// account is empty. Health and a missing reading still block. Because
    /// every row's evidence changed, the plan is then RE-RANKED on the
    /// re-scoped values (ineligible last, then the group's ranking, then its
    /// listed order, then the id), so ordering never contradicts the share.
    pub fn route_group_for(
        &self,
        group: Option<&str>,
        for_model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<RoutePlan> {
        let Some(requested) = for_model.map(str::trim).filter(|w| !w.is_empty()) else {
            return self.route_group_at(group, now);
        };
        let config =
            crate::core::route_config::RoutingFile::load(&self.dirs.config.join("routing.toml"))?;
        let group_config = match group {
            Some(name) => Some(config.group(Some(name))?),
            None => None,
        };
        let policy = FreshnessPolicy::new(self.config.quota_fresh_secs);
        let mut plan = self.route_group_at(group, now)?;
        let usages = self.account_usages(None, now)?;
        let cache = self.models_cache();
        for row in &mut plan.accounts {
            // The catalog is consulted first, whatever the whole-account
            // verdict was: an unrelated exhausted bucket must never answer
            // a question about this family.
            let state = cache.read_state(&row.id, self.config.models_cache_secs, now);
            let support = crate::core::catalog::offers_family(&state, requested);
            if !support.eligible() {
                row.eligible = false;
                let fix = match support {
                    crate::core::catalog::FamilySupport::UnknownCatalog => " (run `agyva models`)",
                    _ => "",
                };
                row.why = format!("{requested}: {}{fix}", support.reason());
                continue;
            }
            // An effort-suffixed id (`gemini-4-carbon-high`) is the same
            // family as its base for mapping purposes. The catalog decides
            // the base: `offers_family` only reports `Supported(family)`
            // when this account's observation lists that exact id, so a
            // standalone id that merely ends in an effort word resolves to
            // itself and is never silently re-based.
            let family = match &support {
                crate::core::catalog::FamilySupport::Supported(f)
                    if !f.is_empty() && f.eq_ignore_ascii_case(requested) =>
                {
                    requested
                }
                crate::core::catalog::FamilySupport::Supported(f) if !f.is_empty() => f.as_str(),
                _ => requested,
            };
            let declared = match &group_config {
                Some(g) => g
                    .models
                    .get(family)
                    .or_else(|| g.models.get(requested))
                    .cloned(),
                None => config
                    .groups
                    .values()
                    .find_map(|g| g.models.get(family).or_else(|| g.models.get(requested)))
                    .cloned(),
            };
            let Some(usage) = usages.iter().find(|a| a.id == row.id) else {
                continue;
            };
            let views = usage
                .groups
                .iter()
                .map(|g| crate::core::catalog::GroupView {
                    group: g.group.as_str(),
                    model: g.model.as_deref(),
                });
            let crate::core::catalog::FamilyGroups::Mapped((group_name, model)) =
                crate::core::catalog::map_family_to_groups(family, declared.as_deref(), views)
            else {
                // No group reports this family and none is declared: the
                // catalog listed it, but nothing local says which windows
                // would pay for it, so the whole-account share is NOT
                // evidence about this family. Excluded, with the fix spelled
                // out — never silently promoted on unrelated quota.
                row.eligible = false;
                row.why = format!(
                    "{requested}: no quota group reports this family; declare it in routing.toml [groups.<name>.models]"
                );
                continue;
            };
            let windows: Vec<&crate::core::usage::WindowView> = usage
                .groups
                .iter()
                .filter(|g| {
                    !g.disabled && g.group == group_name && g.model.as_deref() == model.as_deref()
                })
                .flat_map(|g| g.five_hour.iter().chain(g.weekly.iter()))
                .collect();
            if windows.is_empty() {
                row.eligible = false;
                row.why = format!(
                    "{requested}: mapped quota group {} reports no windows; refresh required",
                    group_label(&group_name, model.as_deref())
                );
                continue;
            }
            // Re-derive the headline numbers from the mapped group only.
            let remaining = windows.iter().map(|w| w.remaining_percent).reduce(f64::min);
            let rate = usage
                .groups
                .iter()
                .filter(|g| {
                    !g.disabled && g.group == group_name && g.model.as_deref() == model.as_deref()
                })
                .filter_map(|g| g.five_hour.as_ref())
                .map(|w| w.remaining_percent)
                .reduce(f64::min);
            let renewals = [
                usage
                    .groups
                    .iter()
                    .filter(|g| {
                        !g.disabled
                            && g.group == group_name
                            && g.model.as_deref() == model.as_deref()
                    })
                    .filter_map(|g| g.weekly.as_ref().and_then(|w| w.reset_at))
                    .min(),
                usage
                    .groups
                    .iter()
                    .filter(|g| {
                        !g.disabled
                            && g.group == group_name
                            && g.model.as_deref() == model.as_deref()
                    })
                    .filter_map(|g| g.five_hour.as_ref().and_then(|w| w.reset_at))
                    .min(),
            ];
            row.remaining_percent = remaining;
            row.rate_remaining_percent = rate;
            row.renewals = renewals;
            // Health and staleness still block; only quota eligibility is
            // re-scoped to the requested group.
            let window_ok = windows.iter().all(|w| {
                w.remaining_percent.is_finite()
                    && (0.0..=100.0).contains(&w.remaining_percent)
                    && w.reset_at.is_some_and(|at| at > now)
                    && policy.classify(Some(w.observed_at), now) == Freshness::Fresh
            }) && usage
                .groups
                .iter()
                .filter(|g| {
                    !g.disabled && g.group == group_name && g.model.as_deref() == model.as_deref()
                })
                .all(|g| g.five_hour.is_some() && g.weekly.is_some());
            let health_ok = !matches!(
                usage.health.state,
                AccountHealth::TermsSuspended
                    | AccountHealth::AccessRevoked
                    | AccountHealth::AuthenticationRequired
                    | AccountHealth::TokenExpired
                    | AccountHealth::NetworkUnavailable
            );
            let error_ok = usage.unavailable.as_ref().is_none_or(|u| {
                matches!(
                    u.reason,
                    UnavailableReason::NotRefreshable | UnavailableReason::TooSoon
                )
            });
            let enough = remaining.is_some_and(|v| v > 0.0);
            if !health_ok {
                row.eligible = false;
                row.why = usage.health.state.label().to_owned();
            } else if !error_ok {
                row.eligible = false;
                row.why = "unavailable reading".into();
            } else if row.freshness != Freshness::Fresh {
                row.eligible = false;
                row.why = format!("{requested}: quota stale; refresh required");
            } else if !window_ok {
                row.eligible = false;
                row.why =
                    format!("{requested}: mapped quota is stale, incomplete, or reset passed");
            } else if !enough {
                row.eligible = false;
                row.why = format!("{requested}: mapped quota exhausted");
            } else {
                row.eligible = true;
                row.why = format!(
                    "{requested}: {} cache; {:.0}% remains in {}",
                    row.freshness.label(),
                    remaining.unwrap_or_default(),
                    group_label(&group_name, model.as_deref())
                );
            }
        }
        // The re-scope above changes every row's evidence, so the order
        // `route_group_at` produced no longer reflects this question: a
        // 95%-overall account whose mapped pool sits at 5% must not rank
        // above a 60/60 one, and an ineligible row must never precede an
        // eligible one. Re-rank deterministically on the re-scoped values,
        // keeping the group's declared ordering where it is a tiebreak
        // (priority order, least-used plans) and ending on the id so the
        // output is stable.
        let declared_order: Vec<String> = plan.accounts.iter().map(|r| r.id.clone()).collect();
        let used_rank = |row: &RouteRow| -> u8 {
            let Some(g) = &group_config else {
                return 3;
            };
            match g.plans.get(&row.alias).or_else(|| g.plans.get(&row.id)) {
                Some(crate::core::route_config::DeclaredPlan::Free) => 0,
                Some(crate::core::route_config::DeclaredPlan::Pro) => 1,
                Some(crate::core::route_config::DeclaredPlan::Ultra) => 2,
                None => 3,
            }
        };
        let priority = |row: &RouteRow| -> i32 {
            let Some(g) = &group_config else {
                return 0;
            };
            g.priorities
                .get(&row.alias)
                .or_else(|| g.priorities.get(&row.id))
                .copied()
                .unwrap_or_default()
        };
        let ranking = group_config.as_ref().map(|g| g.ranking).unwrap_or_default();
        plan.accounts.sort_by(|a, b| {
            let by_tier = tier(a).cmp(&tier(b));
            if by_tier != Ordering::Equal {
                return by_tier;
            }
            let by_ranking = match ranking {
                crate::core::route_config::Ranking::Priority => {
                    b_priority_cmp(priority(a), priority(b))
                }
                crate::core::route_config::Ranking::LeastUsed => used_rank(a).cmp(&used_rank(b)),
                crate::core::route_config::Ranking::Smart => Ordering::Equal,
            };
            if by_ranking != Ordering::Equal {
                return by_ranking;
            }
            freshness_rank(a.freshness)
                .cmp(&freshness_rank(b.freshness))
                .then_with(|| {
                    if tier(a) == 0 {
                        known_time(a.renewals[0], b.renewals[0])
                            .then_with(|| known_time(a.renewals[1], b.renewals[1]))
                    } else {
                        b.remaining_percent
                            .partial_cmp(&a.remaining_percent)
                            .unwrap_or(Ordering::Equal)
                    }
                })
                .then_with(|| {
                    declared_order
                        .iter()
                        .position(|id| id == &a.id)
                        .cmp(&declared_order.iter().position(|id| id == &b.id))
                })
                .then_with(|| a.id.cmp(&b.id))
        });
        plan.recommendation = plan
            .accounts
            .iter()
            .find(|r| r.eligible)
            .map(|r| r.id.clone());
        Ok(plan)
    }
}

/// Priority ranking: the larger declared number wins.
fn b_priority_cmp(a: i32, b: i32) -> Ordering {
    b.cmp(&a)
}
