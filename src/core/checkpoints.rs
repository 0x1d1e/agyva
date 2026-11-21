//! Checkpoint listing, restoration, and pruning.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::agy::companion::{self, Mode};
use crate::agy::credentials::parse_credential;
use crate::core::app::App;
use crate::core::checkpoint::{Checkpoint, CheckpointMeta};
use crate::core::endpoints::{self, EndpointRegistry};
use crate::core::error::{Error, ErrorKind, Result};
use crate::core::ids;
use crate::core::policy::RESTART_NOTICE;
use crate::core::switch::new_txn_id;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckpointInfo {
    pub name: String,
    pub txn_id: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub pinned: bool,
    pub size_bytes: u64,
    pub valid: bool,
}

impl From<CheckpointMeta> for CheckpointInfo {
    fn from(m: CheckpointMeta) -> Self {
        Self {
            name: m.name,
            txn_id: m.txn_id,
            created_at: m.created_at,
            pinned: m.pinned,
            size_bytes: m.size_bytes,
            valid: m.valid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreCheckpointResult {
    pub restored: String,
    pub txn_id: String,
    pub target_account_id: Option<String>,
    pub active_account: Option<String>,
    pub kept_checkpoint: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PruneResult {
    pub removed_count: usize,
    pub removed: Vec<String>,
    pub remaining: usize,
    pub pinned_remaining: usize,
}

impl App {
    /// List saved checkpoints, newest first.
    pub fn list_checkpoints(&self) -> Result<Vec<CheckpointInfo>> {
        Ok(self
            .checkpoints()
            .list_meta()?
            .into_iter()
            .map(CheckpointInfo::from)
            .collect())
    }

    /// Restore a saved checkpoint (pinned or ordinary) by filename.
    ///
    /// Refuses if an interrupted switch journal is pending (`RecoveryRequired`,
    /// exit 3). If the live `agy` login differs from the checkpoint's saved
    /// login, refuses unless `force` is true (in which case the current live
    /// login is first saved in a pinned backup checkpoint).
    pub fn restore_checkpoint(&self, name: &str, force: bool) -> Result<RestoreCheckpointResult> {
        let name = name.trim();
        if !ids::is_checkpoint_name(name) {
            return Err(Error::usage(format!("invalid checkpoint name `{name}`"))
                .hint("run `agyva checkpoints list` to see available checkpoints"));
        }

        let _lock = self.lock_login()?;
        self.require_no_pending()?;

        let vault = self.vault()?;
        let ckpt = self.checkpoints().read_by_name(vault, name)?;
        let native = self.native()?;
        let current = native.read()?;

        let differs = match (&current, &ckpt.native) {
            (None, _) => false,
            (Some(cur), Some(saved)) => cur.as_bytes() != saved.as_bytes(),
            (Some(_), None) => true,
        };
        let mut kept_checkpoint = None;
        if differs {
            if !force {
                return Err(Error::refused(
                    "agy's current login differs from the checkpoint; restoring would overwrite it",
                )
                .hint(
                    "pass --force to keep an encrypted pinned backup of the current login first",
                ));
            }
            let reg = self.load_registry()?;
            let backup = Checkpoint::new(
                &new_txn_id()?,
                current,
                companion::capture(&self.agy)?,
                companion::settings_exist(&self.agy),
                reg.active.clone(),
                reg.previous.clone(),
            );
            kept_checkpoint = Some(self.checkpoints().write_pinned(vault, &backup)?);
        }

        match &ckpt.native {
            Some(raw) => {
                native.write(raw.as_bytes())?;
                native.verify(raw.as_bytes())?;
            }
            None => native.delete()?,
        }
        companion::apply(
            &self.agy,
            &ckpt.companion,
            Mode::Restore {
                settings_existed: ckpt.settings_existed,
            },
        )?;

        let mut reg = self.load_registry()?;
        let valid = |id: &Option<String>| {
            id.as_deref()
                .filter(|i| reg.find(i).is_some())
                .map(str::to_owned)
        };
        let (active, previous) = (valid(&ckpt.active), valid(&ckpt.previous));
        if reg.active != active || reg.previous != previous {
            reg.active = active.clone();
            reg.previous = previous;
            self.save_registry(&reg)?;
        }

        // Keep endpoints.json active_endpoint consistent with restored settings.
        let ep_path = endpoints::registry_file(&self.dirs);
        if ep_path.exists() {
            let mut ep_reg = EndpointRegistry::load(&ep_path)?;
            if ep_reg.active_endpoint.is_some() {
                ep_reg.active_endpoint = None;
                ep_reg.save(&ep_path)?;
            }
        }

        let target_account_id = ckpt
            .native
            .as_ref()
            .and_then(|r| parse_credential(r.as_bytes()).ok())
            .map(|c| c.account_id())
            .or_else(|| ckpt.active.clone());
        let active_account = active
            .as_deref()
            .and_then(|id| reg.find(id))
            .map(|r| r.alias.clone());

        let mut warnings = vec![RESTART_NOTICE.to_owned()];
        if let Some(ref kept) = kept_checkpoint {
            warnings.push(format!(
                "previous live login saved to pinned checkpoint `{kept}`"
            ));
        }

        Ok(RestoreCheckpointResult {
            restored: name.to_owned(),
            txn_id: ckpt.txn_id,
            target_account_id,
            active_account,
            kept_checkpoint,
            warnings,
        })
    }

    /// Prune checkpoints beyond `checkpoints_keep` (and pinned checkpoints only
    /// when `include_pinned == true`).
    pub fn prune_checkpoints(&self, include_pinned: bool) -> Result<PruneResult> {
        self.prune_checkpoints_with_keep(self.keep_checkpoints(), include_pinned)
    }

    /// Prune checkpoints keeping at most `keep` ordinary checkpoints (and pinned
    /// checkpoints only when `include_pinned == true`).
    pub fn prune_checkpoints_with_keep(
        &self,
        keep: usize,
        include_pinned: bool,
    ) -> Result<PruneResult> {
        let _lock = self.lock()?;
        self.require_no_pending()?;
        let store = self.checkpoints();
        let removed = store.prune_returning(keep, include_pinned, None)?;
        let after = store.list_meta()?;
        let pinned_remaining = after.iter().filter(|c| c.pinned).count();
        Ok(PruneResult {
            removed_count: removed.len(),
            removed,
            remaining: after.len(),
            pinned_remaining,
        })
    }

    /// Remove a single checkpoint by filename.
    pub fn delete_checkpoint(&self, name: &str) -> Result<()> {
        let name = name.trim();
        if !ids::is_checkpoint_name(name) {
            return Err(Error::usage(format!("invalid checkpoint name `{name}`")));
        }
        let _lock = self.lock()?;
        self.require_no_pending()?;
        let path = self.checkpoints().dir().join(name);
        if !path.exists() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("checkpoint `{name}` not found"),
            ));
        }
        self.checkpoints().remove(name)
    }
}
