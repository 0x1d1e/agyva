//! Routing preferences are separate from the credential registry and vault.
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::core::error::{Error, Result};
use crate::storage::fsutil;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Ranking {
    Priority,
    /// Like `smart`, but accounts with equal tier and freshness sort by the
    /// share already used (least first, from declared plans where given).
    /// Without declared plans it degenerates to `smart`'s share-remaining
    /// ordering — documented, never an imagined absolute capacity.
    LeastUsed,
    #[default]
    Smart,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteGroup {
    /// Listed order breaks ties; an empty list includes every saved account.
    pub accounts: Vec<String>,
    /// Larger explicit priority comes first with Priority ranking.
    pub priorities: BTreeMap<String, i32>,
    pub ranking: Ranking,
    /// User-declared plan per account (`alias = "free" | "pro" | "ultra"`).
    /// agy never reports a plan; this is the only source, and unstated
    /// accounts stay `unknown` — never inferred.
    pub plans: BTreeMap<String, DeclaredPlan>,
    /// Model-family → quota-group name mapping the user declares
    /// (`"gemini-4-carbon" = "Gemini Pro"`). Specific pools are matched
    /// before broad ones; unmapped families are not eligible for `--for`.
    pub models: BTreeMap<String, String>,
    /// Opt-in automation: when true AND `auto_switch_ack = true` in
    /// config.toml, `route` may run the journaled switch for this group's
    /// recommendation under the auto-switch gates (cooldown, fresh
    /// evidence, session gate). Default false.
    pub auto_switch: bool,
    /// Active five-hour quota threshold (%) at or below which auto-switch may trigger (default 10.0).
    pub five_hour_threshold_percent: Option<f64>,
    /// Active weekly quota threshold (%) at or below which auto-switch may trigger (default 10.0).
    pub weekly_threshold_percent: Option<f64>,
    /// Minimum margin (%) a healthy candidate must exceed the active account by (default 10.0).
    pub hysteresis_percent: Option<f64>,
    /// Optional per-group cooldown override in seconds (30-86400).
    pub cooldown_secs: Option<u64>,
}

impl RouteGroup {
    pub fn five_hour_threshold(&self) -> f64 {
        self.five_hour_threshold_percent.unwrap_or(10.0)
    }

    pub fn weekly_threshold(&self) -> f64 {
        self.weekly_threshold_percent.unwrap_or(10.0)
    }

    pub fn hysteresis(&self) -> f64 {
        self.hysteresis_percent.unwrap_or(10.0)
    }

    pub fn cooldown(&self, cfg: &Config) -> Duration {
        Duration::from_secs(self.cooldown_secs.unwrap_or(cfg.auto_switch_cooldown_secs))
    }
}

/// A plan label the user typed, with its provenance explicit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeclaredPlan {
    Free,
    Pro,
    Ultra,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingFile {
    pub schema_version: u32,
    pub groups: BTreeMap<String, RouteGroup>,
}

impl Default for RoutingFile {
    fn default() -> Self {
        Self {
            schema_version: 1,
            groups: BTreeMap::new(),
        }
    }
}

impl RoutingFile {
    pub fn load(path: &Path) -> Result<Self> {
        let Some(bytes) =
            fsutil::read_optional(path).map_err(|e| Error::io("reading routing.toml", e))?
        else {
            return Ok(Self::default());
        };
        if bytes.len() > 256 * 1024 {
            return Err(Error::usage("routing.toml exceeds 256 KiB"));
        }
        let text =
            std::str::from_utf8(&bytes).map_err(|_| Error::usage("routing.toml must be UTF-8"))?;
        let file: Self =
            toml::from_str(text).map_err(|e| Error::usage(format!("invalid routing.toml: {e}")))?;
        if file.schema_version != 1 {
            return Err(Error::usage("unsupported routing.toml schema_version"));
        }
        for (name, group) in &file.groups {
            for (field, value) in [
                (
                    "five_hour_threshold_percent",
                    group.five_hour_threshold_percent,
                ),
                ("weekly_threshold_percent", group.weekly_threshold_percent),
                ("hysteresis_percent", group.hysteresis_percent),
            ] {
                if let Some(v) = value
                    && (!v.is_finite() || !(0.0..=100.0).contains(&v))
                {
                    return Err(Error::usage(format!(
                        "group `{name}` {field} must be finite and between 0 and 100"
                    )));
                }
            }
            if let Some(v) = group.cooldown_secs
                && !(30..=86_400).contains(&v)
            {
                return Err(Error::usage(format!(
                    "group `{name}` cooldown_secs must be between 30 and 86400"
                )));
            }
        }
        Ok(file)
    }

    pub fn group(&self, name: Option<&str>) -> Result<RouteGroup> {
        match name {
            Some(name) => self.groups.get(name).cloned().ok_or_else(|| {
                Error::usage(format!("unknown routing group `{name}`"))
                    .hint("define the group in routing.toml next to config.toml")
            }),
            None => Ok(RouteGroup::default()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_defaults_and_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("routing.toml");
        std::fs::write(&path, "[groups.work]\nauto_switch = true\n").unwrap();
        let file = RoutingFile::load(&path).unwrap();
        let g = file.group(Some("work")).unwrap();
        assert_eq!(g.five_hour_threshold(), 10.0);
        assert_eq!(g.weekly_threshold(), 10.0);
        assert_eq!(g.hysteresis(), 10.0);
        assert_eq!(g.cooldown(&Config::default()), Duration::from_secs(900));

        std::fs::write(
            &path,
            "[groups.work]\nfive_hour_threshold_percent = 15.0\nweekly_threshold_percent = 20.0\nhysteresis_percent = 5.0\ncooldown_secs = 60\n",
        )
        .unwrap();
        let file = RoutingFile::load(&path).unwrap();
        let g = file.group(Some("work")).unwrap();
        assert_eq!(g.five_hour_threshold(), 15.0);
        assert_eq!(g.weekly_threshold(), 20.0);
        assert_eq!(g.hysteresis(), 5.0);
        assert_eq!(g.cooldown(&Config::default()), Duration::from_secs(60));

        for bad in [
            "[groups.work]\nfive_hour_threshold_percent = -1.0\n",
            "[groups.work]\nweekly_threshold_percent = 101.0\n",
            "[groups.work]\nhysteresis_percent = nan\n",
            "[groups.work]\ncooldown_secs = 29\n",
            "[groups.work]\ncooldown_secs = 86401\n",
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(RoutingFile::load(&path).is_err(), "expected err for {bad}");
        }
    }
}
