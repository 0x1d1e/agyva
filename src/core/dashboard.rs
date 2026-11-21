//! Everything the interactive view shows, as one value built from the same
//! structs the CLI and `--json` use. No secrets.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::app::App;
use crate::core::app_integrations::EndpointView;
use crate::core::checkpoints::CheckpointInfo;
use crate::core::doctor::DoctorReport;
use crate::core::error::Result;
use crate::core::profile::{ProfileSummary, ResolvedProfile};
use crate::core::route::RoutePlan;
use crate::core::route_config::{Ranking, RoutingFile};
use crate::core::status::StatusView;
use crate::core::usage::{RefreshMode, UsageOptions, UsageReport};
use crate::core::view::VIEW_SCHEMA;

#[derive(Debug, Clone, Serialize)]
pub struct RouteGroupSnapshot {
    pub name: String,
    pub ranking: Ranking,
    pub auto_switch: bool,
    pub plan: RoutePlan,
}

#[derive(Debug, Clone, Serialize)]
pub struct Dashboard {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub usage: UsageReport,
    pub status: StatusView,
    pub doctor: DoctorReport,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub endpoints: Vec<EndpointView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profiles: Vec<ProfileSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<ResolvedProfile>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub routing_groups: Vec<RouteGroupSnapshot>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checkpoints: Vec<CheckpointInfo>,
    /// Presentation policy for the open TUI; not part of the JSON schema.
    #[serde(skip)]
    pub quota_fresh_secs: u64,
    /// A newer agyva release found by the startup check, shown once on the
    /// dashboard. Not part of the JSON schema.
    #[serde(skip)]
    pub update_notice: Option<String>,
}

impl App {
    /// Accounts with quota/health plus diagnostics. `mode` decides whether
    /// agy is asked (the same rules as `agyva usage`: only the active account,
    /// never a suspended one unless forced, never more often than allowed).
    pub fn dashboard(&self, mode: RefreshMode) -> Result<Dashboard> {
        let usage = self.usage_report(&UsageOptions {
            selector: None,
            all: true,
            refresh: mode,
        })?;
        let endpoints = self.list_endpoints().unwrap_or_default();
        let active_endpoint = endpoints.iter().find(|e| e.active).map(|e| e.alias.clone());
        let profiles = self.list_profiles(None).unwrap_or_default();
        let active_profile = self.resolve_active_profile(None, None).ok().flatten();
        let routing_groups = RoutingFile::load(&self.dirs.config.join("routing.toml"))
            .map(|rf| {
                rf.groups
                    .into_iter()
                    .filter_map(|(name, group)| {
                        self.route_group_for(Some(&name), None, usage.generated_at)
                            .ok()
                            .map(|plan| RouteGroupSnapshot {
                                name,
                                ranking: group.ranking,
                                auto_switch: group.auto_switch,
                                plan,
                            })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let checkpoints = self.list_checkpoints().unwrap_or_default();

        Ok(Dashboard {
            schema_version: VIEW_SCHEMA,
            generated_at: usage.generated_at,
            status: self.status()?,
            doctor: self.doctor(),
            endpoints,
            active_endpoint,
            profiles,
            active_profile,
            routing_groups,
            checkpoints,
            quota_fresh_secs: self.config.quota_fresh_secs,
            update_notice: None,
            usage,
        })
    }
}
