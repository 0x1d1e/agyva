//! `agyva status`: a quick, side-effect-free summary (no keyring access, no
//! prompts, no network).

use serde::Serialize;

use crate::agy::sessions::{self, SessionReport};
use crate::core::app::App;
use crate::core::error::Result;
use crate::core::view::VIEW_SCHEMA;

#[derive(Debug, Clone, Serialize)]
pub struct PendingView {
    pub txn_id: String,
    pub target: String,
    pub phase: String,
    pub started_at: chrono::DateTime<chrono::Utc>,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StatusView {
    pub schema_version: u32,
    pub agyva_version: &'static str,
    pub agy: String,
    pub agy_path: Option<String>,
    /// Whether quota can be read, and why not.
    pub quota_provider: String,
    pub accounts: usize,
    pub active: Option<String>,
    pub backend: String,
    pub vault: String,
    pub config_dir: String,
    pub state_dir: String,
    pub sessions: SessionReport,
    pub recovery_pending: Option<PendingView>,
}

impl App {
    pub fn status(&self) -> Result<StatusView> {
        let reg = self.load_registry()?;
        let vault = match self.key_provider().load_meta()? {
            Some(m) => m.mode,
            None => "not created yet".to_owned(),
        };
        let recovery_pending = self.pending_journal()?.map(|j| PendingView {
            txn_id: j.txn_id,
            target: j.target,
            phase: j.phase.as_str().to_owned(),
            started_at: j.started_at,
            detail: j.detail,
        });
        Ok(StatusView {
            schema_version: VIEW_SCHEMA,
            agyva_version: env!("CARGO_PKG_VERSION"),
            agy: self.compat().summary(),
            agy_path: crate::agy::discovery::find_agy(&self.env).map(|p| p.display().to_string()),
            quota_provider: self.quota_provider_status().detail,
            accounts: reg.accounts.len(),
            active: reg
                .active
                .as_deref()
                .and_then(|id| reg.find(id))
                .map(|r| r.alias.clone()),
            backend: self
                .env
                .get("AGYVA_BACKEND")
                .unwrap_or(self.config.backend.as_str())
                .to_owned(),
            vault,
            config_dir: self.dirs.config.display().to_string(),
            state_dir: self.dirs.state.display().to_string(),
            sessions: sessions::detect(&self.agy, self.session_scan),
            recovery_pending,
        })
    }
}
