//! Everything the interactive view shows, as one value built from the same
//! structs the CLI and `--json` use. No secrets.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::app::App;
use crate::core::doctor::DoctorReport;
use crate::core::error::Result;
use crate::core::status::StatusView;
use crate::core::usage::{RefreshMode, UsageOptions, UsageReport};
use crate::core::view::VIEW_SCHEMA;

#[derive(Debug, Clone, Serialize)]
pub struct Dashboard {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    pub usage: UsageReport,
    pub status: StatusView,
    pub doctor: DoctorReport,
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
        Ok(Dashboard {
            schema_version: VIEW_SCHEMA,
            generated_at: usage.generated_at,
            status: self.status()?,
            doctor: self.doctor(),
            quota_fresh_secs: self.config.quota_fresh_secs,
            update_notice: None,
            usage,
        })
    }
}
