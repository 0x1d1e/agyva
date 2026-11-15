//! The per-account health table behind `agyva health` and `--json`.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::app::App;
use crate::core::error::Result;
use crate::core::usage::{AccountUsage, HealthView, UsageOptions};
use crate::core::view::VIEW_SCHEMA;

#[derive(Debug, Clone, Serialize)]
pub struct HealthRow {
    pub number: usize,
    pub id: String,
    pub alias: String,
    pub email: Option<String>,
    pub active: bool,
    pub health: HealthView,
    pub freshness: crate::core::quota::Freshness,
    pub unavailable: Option<crate::core::quota::Unavailable>,
}

impl From<&AccountUsage> for HealthRow {
    fn from(a: &AccountUsage) -> Self {
        Self {
            number: a.number,
            id: a.id.clone(),
            alias: a.alias.clone(),
            email: a.email.clone(),
            active: a.active,
            health: a.health.clone(),
            freshness: a.freshness,
            unavailable: a.unavailable.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub notice: Option<String>,
    pub accounts: Vec<HealthRow>,
}

impl App {
    /// Health for every account (or the selected one); refreshes the active
    /// account unless told not to.
    pub fn health_report(&self, opts: &UsageOptions) -> Result<HealthReport> {
        let all = UsageOptions {
            selector: opts.selector.clone(),
            all: true,
            refresh: opts.refresh,
        };
        let u = self.usage_report(&all)?;
        Ok(HealthReport {
            schema_version: VIEW_SCHEMA,
            generated_at: u.generated_at,
            notice: u.notice,
            accounts: u.accounts.iter().map(HealthRow::from).collect(),
        })
    }
}
