//! `App` integration methods for custom endpoints, project/global profiles,
//! unified `use`, and fast cache-only statuslines.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::app::App;
use crate::core::bridge::BridgeTarget;
use crate::core::endpoints::{
    self, EndpointCache, EndpointPatch, EndpointRecord, EndpointRegistry, NewEndpoint,
};
use crate::core::error::{Error, ErrorKind, Result};
use crate::core::format;
use crate::core::health::AccountHealth;
use crate::core::profile::{self, ProfileSource, ProfileSpec, ProfileSummary, ResolvedProfile};
use crate::core::protocol::{EndpointProtocol, FetchedModel, ProbeResult, RateLimitInfo};
use crate::core::quota_history::compute_burn_rate;
use crate::core::view::{AccountView, VIEW_SCHEMA};

/// Non-secret summary of a configured custom endpoint plus cached probe/model state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EndpointView {
    pub id: String,
    pub alias: String,
    pub base_url: String,
    pub protocol: EndpointProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_protocol: Option<EndpointProtocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    pub active: bool,
    pub enabled: bool,
    pub model_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<FetchedModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limits: Option<RateLimitInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UnifiedUseOutcome {
    Account {
        account: AccountView,
        already_active: bool,
        warnings: Vec<String>,
        policy_notice: Option<&'static str>,
    },
    Endpoint {
        endpoint: EndpointRecord,
    },
}

#[derive(Debug, Serialize)]
pub struct ProfileUseOutcome {
    pub profile: ResolvedProfile,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub switched_account: Option<AccountView>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_endpoint: Option<EndpointRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatuslineData {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_endpoint_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_profile: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lowest_five_hour_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lowest_weekly_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub burn_rate_percent_per_hour: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eta_exhaustion_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<AccountHealth>,
    pub suspended: bool,
    pub recovery_pending: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold_alert: Option<String>,
}

impl StatuslineData {
    /// Format the statusline according to `compact`, `full`, or `json`.
    pub fn render(&self, fmt_name: &str) -> Result<String> {
        match fmt_name {
            "json" => serde_json::to_string(self)
                .map_err(|e| Error::format(format!("encoding statusline json: {e}"))),
            "full" => {
                let mut parts = Vec::new();
                if let Some(ep) = &self.active_endpoint {
                    let model = self
                        .active_endpoint_model
                        .as_deref()
                        .map(|m| format!(" ({m})"))
                        .unwrap_or_default();
                    parts.push(format!("ep:{ep}{model}"));
                } else if let Some(acct) = &self.active_account {
                    parts.push(acct.clone());
                } else {
                    parts.push("no-active".to_owned());
                }
                if let Some(p) = &self.active_profile {
                    parts.push(format!("profile:{p}"));
                }
                if let Some(five) = self.lowest_five_hour_percent {
                    parts.push(format!("5h:{}", format::percent_text(five)));
                }
                if let Some(week) = self.lowest_weekly_percent {
                    parts.push(format!("wk:{}", format::percent_text(week)));
                }
                if let Some(rate) = self.burn_rate_percent_per_hour
                    && rate > 0.0
                {
                    let eta = self
                        .eta_exhaustion_secs
                        .map(|s| format!(" eta {}", format_duration_short(s)))
                        .unwrap_or_default();
                    parts.push(format!("burn:{rate:.1}%/h{eta}"));
                }
                if let Some(h) = self.health
                    && h != AccountHealth::Healthy
                {
                    parts.push(format!("health:{}", h.label()));
                }
                if self.suspended {
                    parts.push("SUSPENDED".to_owned());
                }
                if self.recovery_pending {
                    parts.push("RECOVERY_PENDING".to_owned());
                }
                if let Some(alert) = &self.threshold_alert {
                    parts.push(alert.to_ascii_uppercase());
                }
                Ok(parts.join(" | "))
            }
            _ => {
                let head = if let Some(ep) = &self.active_endpoint {
                    format!("ep:{ep}")
                } else {
                    self.active_account
                        .clone()
                        .unwrap_or_else(|| "-".to_owned())
                };
                let q5 = self
                    .lowest_five_hour_percent
                    .map_or_else(|| "-".to_owned(), format::percent_text);
                let qw = self
                    .lowest_weekly_percent
                    .map_or_else(|| "-".to_owned(), format::percent_text);
                let mut out = format!("{head} 5h:{q5} wk:{qw}");
                if let Some(p) = &self.active_profile {
                    out.push_str(&format!(" [{p}]"));
                }
                if self.recovery_pending {
                    out.push_str(" !recover");
                } else if self.suspended {
                    out.push_str(" !suspended");
                } else if let Some(alert) = &self.threshold_alert {
                    out.push_str(&format!(" !{alert}"));
                }
                Ok(out)
            }
        }
    }
}

fn format_duration_short(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else {
        let h = secs / 3600;
        let m = (secs % 3600) / 60;
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h{m}m")
        }
    }
}

impl App {
    // ------------------------------------------------------------ endpoints

    pub fn list_endpoints(&self) -> Result<Vec<EndpointView>> {
        let reg = endpoints::load_registry(&self.dirs)?;
        let cache = EndpointCache::new(&self.dirs).load();
        Ok(reg
            .endpoints
            .iter()
            .map(|ep| {
                let c = cache.endpoints.get(&ep.id);
                let models = c.map(|e| e.models.clone()).unwrap_or_default();
                EndpointView {
                    id: ep.id.clone(),
                    alias: ep.alias.clone(),
                    base_url: ep.base_url.clone(),
                    protocol: ep.protocol,
                    detected_protocol: ep.detected_protocol,
                    default_model: ep.default_model.clone(),
                    active: reg.active_endpoint.as_deref() == Some(ep.id.as_str()),
                    enabled: ep.enabled,
                    model_count: models.len(),
                    models,
                    fetched_at: c.and_then(|e| e.fetched_at),
                    latency_ms: c.and_then(|e| e.latency_ms),
                    rate_limits: c.and_then(|e| e.rate_limits.clone()),
                    last_error: c.and_then(|e| e.last_error.clone()),
                }
            })
            .collect())
    }

    pub fn active_endpoint_record(&self) -> Result<Option<EndpointRecord>> {
        let reg = endpoints::load_registry(&self.dirs)?;
        Ok(reg
            .active_endpoint
            .as_deref()
            .and_then(|id| reg.find(id).cloned()))
    }

    pub fn add_endpoint(&self, spec: NewEndpoint) -> Result<EndpointRecord> {
        let vault = self.vault_or_create()?;
        endpoints::add_endpoint(&self.dirs, vault, spec)
    }

    pub fn remove_endpoint(&self, selector: &str) -> Result<EndpointRecord> {
        let reg = endpoints::load_registry(&self.dirs)?;
        let target_id = endpoints::resolve_endpoint(&reg, selector)?.id.clone();
        let was_active = reg.active_endpoint.as_deref() == Some(target_id.as_str());
        let removed = endpoints::remove_endpoint(&self.dirs, selector)?;
        if was_active {
            endpoints::clear_agy_custom_provider(&self.agy)?;
        }
        Ok(removed)
    }

    pub fn rename_endpoint(&self, selector: &str, new_alias: &str) -> Result<EndpointRecord> {
        endpoints::rename_endpoint(&self.dirs, selector, new_alias)
    }

    pub fn update_endpoint(&self, selector: &str, patch: EndpointPatch) -> Result<EndpointRecord> {
        let vault = self.vault_or_create()?;
        endpoints::update_endpoint(&self.dirs, vault, selector, patch)
    }

    pub fn use_endpoint(&self, selector: &str) -> Result<EndpointRecord> {
        let reg = endpoints::load_registry(&self.dirs)?;
        let prev_active = reg.active_endpoint.clone();
        let rec = endpoints::set_active_endpoint(&self.dirs, selector)?;
        let cache = EndpointCache::new(&self.dirs).load();
        let models = cache
            .endpoints
            .get(&rec.id)
            .map(|e| e.models.as_slice())
            .unwrap_or(&[]);
        if let Err(err) = endpoints::sync_agy_settings(&self.agy, &rec, models, None) {
            // Roll back active_endpoint so endpoints.json and agy settings stay consistent.
            if let Ok(mut latest) = endpoints::load_registry(&self.dirs) {
                latest.active_endpoint = prev_active;
                let _ = latest.save(&endpoints::registry_file(&self.dirs));
            }
            return Err(err);
        }
        Ok(rec)
    }

    pub fn clear_active_endpoint(&self) -> Result<()> {
        endpoints::clear_agy_custom_provider(&self.agy)?;
        endpoints::clear_active_endpoint(&self.dirs)
    }

    pub fn test_endpoint(&self, selector: &str) -> Result<ProbeResult> {
        let vault = self.vault()?;
        let transport = self.http_transport();
        endpoints::test_endpoint_connection(&self.dirs, vault, transport.as_ref(), selector)
    }

    pub fn refresh_endpoint_models(&self, selector: &str) -> Result<ProbeResult> {
        let vault = self.vault()?;
        let transport = self.http_transport();
        endpoints::refresh_endpoint_models(&self.dirs, vault, transport.as_ref(), selector)
    }

    pub fn endpoint_env_vars(
        &self,
        selector: &str,
        override_model: Option<&str>,
        bridge_url: Option<&str>,
    ) -> Result<Vec<(String, String)>> {
        let reg = endpoints::load_registry(&self.dirs)?;
        let rec = endpoints::resolve_endpoint(&reg, selector)?.clone();
        let secret = endpoints::load_secret(&self.dirs, self.vault()?, &rec.id)?;
        Ok(endpoints::export_env_vars(
            &rec,
            &secret,
            override_model,
            bridge_url,
        ))
    }

    pub fn bridge_target_for_endpoint(&self, selector: &str) -> Result<BridgeTarget> {
        let reg = endpoints::load_registry(&self.dirs)?;
        let rec = endpoints::resolve_endpoint(&reg, selector)?.clone();
        let secret = endpoints::load_secret(&self.dirs, self.vault()?, &rec.id)?;
        let cache = EndpointCache::new(&self.dirs).load();
        let models = cache
            .endpoints
            .get(&rec.id)
            .map(|e| e.models.clone())
            .unwrap_or_default();
        Ok(BridgeTarget::from_endpoint(&rec, &secret, models))
    }

    /// Unified `use` accepting either `ep:<selector>` for custom endpoints or
    /// an account selector (including `-` for previous OAuth account).
    pub fn unified_use(&self, target: &str, force: bool) -> Result<UnifiedUseOutcome> {
        let trimmed = target.trim();
        if let Some(ep_sel) = trimmed.strip_prefix("ep:") {
            let endpoint = self.use_endpoint(ep_sel.trim())?;
            return Ok(UnifiedUseOutcome::Endpoint { endpoint });
        }
        let outcome = self.switch(trimmed, force)?;
        let ep_path = endpoints::registry_file(&self.dirs);
        if ep_path.exists() {
            let ep_reg = EndpointRegistry::load(&ep_path)?;
            if ep_reg.active_endpoint.is_some() {
                endpoints::clear_agy_custom_provider(&self.agy)?;
                endpoints::clear_active_endpoint(&self.dirs)?;
            }
        }
        Ok(UnifiedUseOutcome::Account {
            account: outcome.account,
            already_active: outcome.already_active,
            warnings: outcome.warnings,
            policy_notice: outcome.policy_notice,
        })
    }

    // ------------------------------------------------------------- profiles

    pub fn list_profiles(&self, cwd: Option<&Path>) -> Result<Vec<ProfileSummary>> {
        let detected = match cwd {
            Some(d) => Some(d.to_path_buf()),
            None => std::env::current_dir().ok(),
        };
        profile::list_profile_summaries(
            &self.dirs,
            detected.as_deref(),
            self.config.default_profile.as_deref(),
        )
    }

    pub fn resolve_active_profile(
        &self,
        explicit_name: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<Option<ResolvedProfile>> {
        let dir = match cwd {
            Some(d) => d.to_path_buf(),
            None => std::env::current_dir()
                .map_err(|e| Error::io("determining current working directory", e))?,
        };
        profile::resolve_profile_with_default(
            &self.dirs,
            &dir,
            explicit_name,
            self.config.default_profile.as_deref(),
        )
    }

    pub fn use_profile(
        &self,
        name: &str,
        cwd: Option<&Path>,
        force: bool,
    ) -> Result<ProfileUseOutcome> {
        let resolved = self
            .resolve_active_profile(Some(name), cwd)?
            .ok_or_else(|| Error::new(ErrorKind::NotFound, format!("no profile named `{name}`")))?;

        let mut switched_account = None;
        let mut active_endpoint = None;
        let mut warnings = Vec::new();

        if let Some(acct) = resolved.spec.account.as_deref() {
            match self.unified_use(acct, force)? {
                UnifiedUseOutcome::Account {
                    account,
                    warnings: w,
                    ..
                } => {
                    switched_account = Some(account);
                    warnings.extend(w);
                }
                UnifiedUseOutcome::Endpoint { endpoint } => {
                    active_endpoint = Some(endpoint);
                }
            }
        }
        if let Some(ep) = resolved.spec.endpoint.as_deref() {
            active_endpoint = Some(self.use_endpoint(ep)?);
        }

        Ok(ProfileUseOutcome {
            profile: resolved,
            switched_account,
            active_endpoint,
            warnings,
        })
    }

    pub fn init_profile(
        &self,
        name: Option<&str>,
        dir: Option<&Path>,
        spec: ProfileSpec,
        set_default: bool,
    ) -> Result<PathBuf> {
        spec.validate()?;
        if let Some(name) = name {
            profile::validate_profile_name(name)?;
            let path = profile::profiles_file(&self.dirs);
            let mut file = profile::load_global_profiles(&path)?;
            if file.profiles.contains_key(name) {
                return Err(Error::conflict(format!("profile `{name}` already exists"))
                    .hint("use `agyva profile set` to update it"));
            }
            file.profiles.insert(name.to_owned(), spec);
            if set_default {
                file.default = Some(name.to_owned());
            }
            profile::save_global_profiles(&path, &file)?;
            Ok(path)
        } else {
            let target_dir = match dir {
                Some(d) => d.to_path_buf(),
                None => std::env::current_dir()
                    .map_err(|e| Error::io("determining current working directory", e))?,
            };
            let local_path = target_dir.join(profile::PROFILE_FILE_NAME);
            if local_path.exists() {
                return Err(
                    Error::conflict(format!("{} already exists", local_path.display()))
                        .hint("edit it or use `agyva profile set` to modify fields"),
                );
            }
            profile::write_local_profile(&target_dir, &spec)
        }
    }

    pub fn set_profile_field(
        &self,
        target: Option<&str>,
        dir: Option<&Path>,
        field: &str,
        value: Option<&str>,
    ) -> Result<ProfileSpec> {
        if let Some(name) = target {
            let path = profile::profiles_file(&self.dirs);
            let mut file = profile::load_global_profiles(&path)?;
            let spec = file.profiles.get_mut(name).ok_or_else(|| {
                Error::new(ErrorKind::NotFound, format!("no global profile `{name}`"))
            })?;
            profile::set_profile_field(spec, field, value)?;
            let updated = spec.clone();
            profile::save_global_profiles(&path, &file)?;
            Ok(updated)
        } else {
            let cwd = match dir {
                Some(d) => d.to_path_buf(),
                None => std::env::current_dir()
                    .map_err(|e| Error::io("determining current working directory", e))?,
            };
            let local = profile::discover_local_profile(&cwd)?.ok_or_else(|| {
                Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "no {} found from {}",
                        profile::PROFILE_FILE_NAME,
                        cwd.display()
                    ),
                )
                .hint("run `agyva profile init` first")
            })?;
            let mut spec = local.spec;
            profile::set_profile_field(&mut spec, field, value)?;
            let parent = local.path.parent().unwrap_or(&cwd);
            profile::write_local_profile(parent, &spec)?;
            Ok(spec)
        }
    }

    pub fn delete_profile(&self, name: &str) -> Result<()> {
        let path = profile::profiles_file(&self.dirs);
        let mut file = profile::load_global_profiles(&path)?;
        if file.profiles.remove(name).is_none() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("no global profile `{name}`"),
            ));
        }
        if file.default.as_deref() == Some(name) {
            file.default = None;
        }
        profile::save_global_profiles(&path, &file)
    }

    // ----------------------------------------------------------- statusline

    /// Fast, cache-only summary for shell prompts / editor statuslines.
    pub fn statusline(
        &self,
        profile_name: Option<&str>,
        cwd: Option<&Path>,
    ) -> Result<StatuslineData> {
        let now = Utc::now();
        let reg = self.load_registry()?;
        let cache = self.quota_cache().load();
        let history = self.quota_history().load();

        let active_id = reg.active.clone();
        let active_rec = active_id.as_deref().and_then(|id| reg.find(id));
        let active_account = active_rec.map(|r| r.alias.clone());

        let ep_reg = endpoints::load_registry(&self.dirs).unwrap_or_default();
        let active_ep = ep_reg
            .active_endpoint
            .as_deref()
            .and_then(|id| ep_reg.find(id));
        let active_endpoint = active_ep.map(|e| e.alias.clone());
        let active_endpoint_model = active_ep.and_then(|e| e.default_model.clone());

        let resolved_profile = match profile_name {
            Some(name) => self.resolve_active_profile(Some(name), cwd)?,
            None => self.resolve_active_profile(None, cwd).ok().flatten(),
        };
        let active_profile = resolved_profile.as_ref().map(|p| {
            p.name.clone().unwrap_or_else(|| match p.source {
                ProfileSource::Local => "local".to_owned(),
                ProfileSource::Global | ProfileSource::Explicit => "default".to_owned(),
            })
        });

        let warn_pct = resolved_profile
            .as_ref()
            .and_then(|p| p.spec.warn_percent)
            .unwrap_or(self.config.warn_percent);
        let crit_pct = resolved_profile
            .as_ref()
            .and_then(|p| p.spec.critical_percent)
            .unwrap_or(self.config.critical_percent);

        let mut lowest_five_hour_percent: Option<f64> = None;
        let mut lowest_weekly_percent: Option<f64> = None;
        let mut burn_rate_percent_per_hour: Option<f64> = None;
        let mut eta_exhaustion_secs: Option<u64> = None;
        let mut health: Option<AccountHealth> = None;
        let mut suspended = false;

        if let Some(id) = active_id.as_deref() {
            suspended = self.suspended_markers().get(id).is_some();
            if let Some(entry) = cache.accounts.get(id) {
                health = entry.health.as_ref().map(|h| h.state);
                if health == Some(AccountHealth::TermsSuspended) {
                    suspended = true;
                }
                if let Some(q) = &entry.quota {
                    let acct_hist = history.accounts.get(id);
                    for g in q.groups.iter().filter(|g| !g.disabled) {
                        let label = g.label();
                        let gh = acct_hist.and_then(|m| m.get(&label));
                        if let Some(w) = &g.five_hour {
                            lowest_five_hour_percent = Some(match lowest_five_hour_percent {
                                Some(cur) => cur.min(w.remaining_percent),
                                None => w.remaining_percent,
                            });
                            if let Some(br) =
                                gh.and_then(|h| compute_burn_rate(&h.five_hour, w.reset_at, now))
                            {
                                update_worst_burn(
                                    &mut burn_rate_percent_per_hour,
                                    &mut eta_exhaustion_secs,
                                    br.percent_per_hour,
                                    br.eta_exhaustion_secs,
                                );
                            }
                        }
                        if let Some(w) = &g.weekly {
                            lowest_weekly_percent = Some(match lowest_weekly_percent {
                                Some(cur) => cur.min(w.remaining_percent),
                                None => w.remaining_percent,
                            });
                            if let Some(br) =
                                gh.and_then(|h| compute_burn_rate(&h.weekly, w.reset_at, now))
                            {
                                update_worst_burn(
                                    &mut burn_rate_percent_per_hour,
                                    &mut eta_exhaustion_secs,
                                    br.percent_per_hour,
                                    br.eta_exhaustion_secs,
                                );
                            }
                        }
                    }
                }
            }
        }

        let min_pct = match (lowest_five_hour_percent, lowest_weekly_percent) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), None) | (None, Some(a)) => Some(a),
            (None, None) => None,
        };
        let threshold_alert = min_pct.and_then(|p| {
            if p <= crit_pct {
                Some("critical".to_owned())
            } else if p <= warn_pct {
                Some("warn".to_owned())
            } else {
                None
            }
        });

        let recovery_pending = self.pending_journal().ok().flatten().is_some();

        Ok(StatuslineData {
            schema_version: VIEW_SCHEMA,
            active_account,
            active_account_id: active_id,
            active_endpoint,
            active_endpoint_model,
            active_profile,
            lowest_five_hour_percent,
            lowest_weekly_percent,
            burn_rate_percent_per_hour,
            eta_exhaustion_secs,
            health,
            suspended,
            recovery_pending,
            threshold_alert,
        })
    }
}

fn update_worst_burn(
    best_rate: &mut Option<f64>,
    best_eta: &mut Option<u64>,
    rate: Option<f64>,
    eta: Option<u64>,
) {
    if let Some(r) = rate {
        *best_rate = Some(match *best_rate {
            Some(cur) => cur.max(r),
            None => r,
        });
    }
    if let Some(e) = eta {
        *best_eta = Some(match *best_eta {
            Some(cur) => cur.min(e),
            None => e,
        });
    }
}
