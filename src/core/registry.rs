//! The public registry: account metadata only, no secrets.
//!
//! `registry.json` carries a schema version and is replaced atomically.

use std::path::Path;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::error::{Error, Result};
use crate::core::ids;
use crate::storage::fsutil;

pub const REGISTRY_SCHEMA: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountRecord {
    pub id: String,
    pub alias: String,
    pub email: Option<String>,
    pub auth_method: String,
    pub added_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub schema_version: u32,
    /// Account agyva last made live.
    pub active: Option<String>,
    /// Account that was live before `active` (target of `use -`).
    pub previous: Option<String>,
    pub accounts: Vec<AccountRecord>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA,
            active: None,
            previous: None,
            accounts: Vec::new(),
        }
    }
}

impl Registry {
    pub fn load(path: &Path) -> Result<Self> {
        let Some(bytes) = fsutil::read_optional(path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
        else {
            return Ok(Self::default());
        };
        let reg: Self = serde_json::from_slice(&bytes).map_err(|e| {
            Error::format(format!("{} is corrupt: {e}", path.display()))
                .hint("restore registry.json from a backup; snapshots in accounts/ are intact")
        })?;
        if reg.schema_version != REGISTRY_SCHEMA {
            return Err(Error::format(format!(
                "registry schema {} is not supported (expected {REGISTRY_SCHEMA})",
                reg.schema_version
            ))
            .hint("upgrade agyva"));
        }
        reg.validate()?;
        Ok(reg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            fsutil::create_private_dir(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        let mut bytes =
            serde_json::to_vec_pretty(self).map_err(|e| Error::format(e.to_string()))?;
        bytes.push(b'\n');
        fsutil::atomic_write(path, &bytes)
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))
    }

    fn validate(&self) -> Result<()> {
        for id in self
            .accounts
            .iter()
            .map(|a| a.id.as_str())
            .chain(self.active.as_deref())
            .chain(self.previous.as_deref())
        {
            if !ids::is_account_id(id) {
                return Err(Error::format(
                    "registry.json holds a malformed account id (expected 10 lowercase hex characters)",
                )
                .hint("restore registry.json from a backup; snapshots in accounts/ are intact"));
            }
        }
        for (i, a) in self.accounts.iter().enumerate() {
            let dup = self.accounts[..i]
                .iter()
                .any(|b| b.id == a.id || b.alias.eq_ignore_ascii_case(&a.alias));
            if dup {
                return Err(Error::format("registry contains duplicate ids or aliases"));
            }
        }
        for r in [&self.active, &self.previous].into_iter().flatten() {
            if self.find(r).is_none() {
                return Err(Error::format(
                    "registry refers to an account that is not saved",
                ));
            }
        }
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&AccountRecord> {
        self.accounts.iter().find(|a| a.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut AccountRecord> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    pub fn alias_taken(&self, alias: &str, except_id: Option<&str>) -> bool {
        self.accounts
            .iter()
            .any(|a| a.alias.eq_ignore_ascii_case(alias) && Some(a.id.as_str()) != except_id)
    }

    /// Idempotently record that `target` became live, coming from `from`.
    pub fn mark_switched(&mut self, from: Option<&str>, target: &str) {
        let now = Utc::now();
        self.previous = from
            .filter(|f| *f != target && self.find(f).is_some())
            .map(str::to_owned)
            .or_else(|| {
                // Re-running with the target already active must not erase `previous`.
                if self.active.as_deref() == Some(target) {
                    self.previous.clone()
                } else {
                    None
                }
            });
        self.active = Some(target.to_owned());
        if let Some(rec) = self.find_mut(target) {
            rec.last_used_at = Some(now);
        }
    }

    /// Forget every reference to `id`, then drop the record.
    pub fn remove(&mut self, id: &str) {
        self.accounts.retain(|a| a.id != id);
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
        if self.previous.as_deref() == Some(id) {
            self.previous = None;
        }
    }
}
