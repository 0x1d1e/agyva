//! `agyva add`: capture the live agy login into the encrypted vault.

use chrono::Utc;

use crate::agy::companion;
use crate::agy::credentials::parse_credential;
use crate::config::Config;
use crate::core::account::{Snapshot, write_snapshot};
use crate::core::app::App;
use crate::core::error::{Error, Result};
use crate::core::policy::POLICY_NOTICE;
use crate::core::registry::AccountRecord;
use crate::core::selector::{derive_alias, validate_alias};
use crate::core::view::AccountView;

#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    pub alias: Option<String>,
    /// Update the saved credential if this account is already saved.
    pub refresh: bool,
}

#[derive(Debug)]
pub struct AddOutcome {
    pub account: AccountView,
    pub refreshed: bool,
    pub policy_notice: Option<&'static str>,
}

impl App {
    /// Show the policy notice once: returns it and records the
    /// acknowledgement in `config.toml`.
    pub(crate) fn take_policy_notice(&self) -> Result<Option<&'static str>> {
        let path = self.dirs.config_file();
        let mut cfg = Config::load(&path)?;
        if cfg.policy_acknowledged {
            return Ok(None);
        }
        cfg.policy_acknowledged = true;
        cfg.save(&path)?;
        Ok(Some(POLICY_NOTICE))
    }

    pub fn add(&self, opts: &AddOptions) -> Result<AddOutcome> {
        if let Some(a) = &opts.alias {
            validate_alias(a)?;
        }
        let _lock = self.lock_login()?;
        self.require_no_pending()?;
        self.require_compat()?;

        let live = self.native()?.read()?.ok_or_else(|| {
            Error::refused("agy is not logged in, so there is nothing to save")
                .hint("run `agy`, log in, then run `agyva add` again")
        })?;
        let info = parse_credential(live.as_bytes())?;
        let id = info.account_id();
        let companion = companion::capture(&self.agy)?;

        let mut reg = self.load_registry()?;
        let existing = reg.find(&id).cloned();
        if existing.is_some() && !opts.refresh {
            let alias = existing.map(|r| r.alias).unwrap_or_default();
            return Err(
                Error::conflict(format!("this login is already saved as `{alias}`"))
                    .hint("use `agyva add --refresh` to update the saved credential"),
            );
        }
        if let Some(a) = opts
            .alias
            .as_ref()
            .filter(|a| reg.alias_taken(a, Some(&id)))
        {
            return Err(
                Error::conflict(format!("the alias `{a}` is already in use"))
                    .hint("choose another alias or rename the other account"),
            );
        }

        let vault = self.vault_or_create()?;
        let snapshot = Snapshot::new(live, companion);
        write_snapshot(vault, &self.dirs.snapshot_file(&id), &id, &snapshot)?;

        let now = Utc::now();
        let refreshed = existing.is_some();
        if let Some(rec) = reg.find_mut(&id) {
            rec.email = info.email.clone().or_else(|| rec.email.take());
            rec.auth_method = info.auth_method.clone();
            rec.updated_at = now;
            if let Some(a) = &opts.alias {
                rec.alias = a.clone();
            }
        } else {
            let alias = opts
                .alias
                .clone()
                .unwrap_or_else(|| derive_alias(info.email.as_deref(), &id, &reg));
            reg.accounts.push(AccountRecord {
                id: id.clone(),
                alias,
                email: info.email.clone(),
                auth_method: info.auth_method.clone(),
                added_at: now,
                updated_at: now,
                last_used_at: None,
            });
        }
        // The login we just captured is the live one.
        if reg.active.as_deref() != Some(&id) {
            reg.previous = reg.active.take();
            reg.active = Some(id.clone());
        }
        self.save_registry(&reg)?;

        let rec = reg
            .find(&id)
            .ok_or_else(|| Error::format("account vanished from the registry"))?;
        Ok(AddOutcome {
            account: AccountView::of(&reg, rec),
            refreshed,
            policy_notice: self.take_policy_notice()?,
        })
    }
}
