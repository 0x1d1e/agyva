//! Account management that does not touch agy's live state: list, current,
//! rename, remove.

use crate::agy::credentials::parse_credential;
use crate::core::app::App;
use crate::core::error::{Error, Result};
use crate::core::selector::{self, validate_alias};
use crate::core::view::{AccountView, CurrentView, ListView, VIEW_SCHEMA};
use crate::storage::fsutil;

impl App {
    pub fn list(&self) -> Result<ListView> {
        Ok(ListView::of(&self.load_registry()?))
    }

    /// What agy is logged in as right now (reads the native store only; no
    /// vault access, no network).
    pub fn current(&self) -> Result<CurrentView> {
        let reg = self.load_registry()?;
        let none = CurrentView {
            schema_version: VIEW_SCHEMA,
            logged_in: false,
            managed: false,
            id: None,
            email: None,
            auth_method: None,
            account: None,
        };
        let Some(raw) = self.native()?.read()? else {
            return Ok(none);
        };
        let info = parse_credential(raw.as_bytes())?;
        let id = info.account_id();
        let account = reg.find(&id).map(|r| AccountView::of(&reg, r));
        Ok(CurrentView {
            logged_in: true,
            managed: account.is_some(),
            email: account
                .as_ref()
                .and_then(|a| a.email.clone())
                .or(info.email),
            auth_method: Some(info.auth_method),
            id: Some(id),
            account,
            ..none
        })
    }

    /// Resolve a selector without changing anything.
    pub fn resolve(&self, selector: &str) -> Result<AccountView> {
        let reg = self.load_registry()?;
        let rec = selector::resolve(&reg, selector)?;
        Ok(AccountView::of(&reg, rec))
    }

    pub fn rename(&self, selector: &str, new_alias: &str) -> Result<AccountView> {
        validate_alias(new_alias)?;
        let _lock = self.lock()?;
        self.require_no_pending()?;
        let mut reg = self.load_registry()?;
        let id = selector::resolve(&reg, selector)?.id.clone();
        if reg.alias_taken(new_alias, Some(&id)) {
            return Err(
                Error::conflict(format!("the alias `{new_alias}` is already in use"))
                    .hint("choose another alias"),
            );
        }
        if let Some(rec) = reg.find_mut(&id) {
            rec.alias = new_alias.to_owned();
            rec.updated_at = chrono::Utc::now();
        }
        self.save_registry(&reg)?;
        let rec = reg
            .find(&id)
            .ok_or_else(|| Error::format("account vanished from the registry"))?;
        Ok(AccountView::of(&reg, rec))
    }

    /// Delete the saved copy of an account. agy's own login is never touched.
    pub fn remove(&self, selector: &str, force: bool) -> Result<AccountView> {
        let _lock = self.lock()?;
        self.require_no_pending()?;
        let mut reg = self.load_registry()?;
        let rec = selector::resolve(&reg, selector)?.clone();
        let view = AccountView::of(&reg, &rec);
        if view.active && !force {
            return Err(Error::refused(format!("`{}` is the active account", rec.alias))
                .hint("switch to another account first, or pass --force to forget it anyway (agy stays logged in)"));
        }
        reg.remove(&rec.id);
        self.save_registry(&reg)?;
        let path = self.dirs.snapshot_file(&rec.id);
        fsutil::remove_durable(&path)
            .map_err(|e| Error::io(format!("removing {}", path.display()), e))?;
        Ok(view)
    }
}
