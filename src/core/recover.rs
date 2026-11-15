//! Rollback and `agyva recover`.

use crate::agy::companion::{self, Mode};
use crate::agy::credentials::RawCredential;
use crate::core::app::App;
use crate::core::checkpoint::Checkpoint;
use crate::core::error::{Error, ErrorKind, Result};
use crate::core::switch::new_txn_id;
use crate::storage::journal::{Journal, Phase};
use crate::storage::vault::Vault;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoverMode {
    /// Roll forward a verified switch, otherwise roll back.
    Auto,
    /// Always restore the pre-switch state.
    Rollback,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoverOutcome {
    NothingToDo,
    RolledBack {
        target: String,
        /// Pinned checkpoint holding a login that was live but unrelated to
        /// the interrupted switch (only after `--force`).
        kept_checkpoint: Option<String>,
    },
    RolledForward {
        target: String,
    },
}

impl App {
    /// Restore the pre-switch state from the journal's checkpoint. Idempotent.
    ///
    /// agy's current login is read first. If it is neither the checkpointed
    /// one nor the switch target's saved one, something else changed it (the
    /// user logged in again, agy refreshed it): restoring would destroy that
    /// login, so without `force` the rollback is refused, and with `force`
    /// the current login is first kept in a pinned checkpoint, whose name is
    /// returned.
    pub(crate) fn rollback(
        &self,
        vault: &Vault,
        journal: &Journal,
        force: bool,
    ) -> Result<Option<String>> {
        let ckpt = self
            .checkpoints()
            .read(vault, &journal.checkpoint, &journal.txn_id)?;
        let native = self.native()?;
        let current = native.read()?;
        let mut kept = None;
        if !self.native_is_expected(vault, journal, &ckpt, current.as_ref()) {
            if !force {
                return Err(Error::refused(
                    "agy's current login is neither the one from before the interrupted switch nor the one it was switching to; restoring the old state would overwrite it",
                )
                .hint("run `agyva recover --force` to keep an encrypted copy of the current login and restore anyway"));
            }
            kept = Some(self.keep_current_login(vault, current)?);
        }
        match &ckpt.native {
            Some(raw) => {
                native.write(raw.as_bytes())?;
                native.verify(raw.as_bytes())?;
            }
            None => native.delete()?,
        }
        self.failpoints.hit("rollback-after-native")?;
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
            reg.active = active;
            reg.previous = previous;
            self.save_registry(&reg)?;
        }
        Ok(kept)
    }

    /// `None` (logged out), the checkpointed login or the target's saved
    /// login: safe to overwrite.
    fn native_is_expected(
        &self,
        vault: &Vault,
        journal: &Journal,
        ckpt: &Checkpoint,
        current: Option<&RawCredential>,
    ) -> bool {
        let Some(cur) = current else { return true };
        if ckpt
            .native
            .as_ref()
            .is_some_and(|c| c.as_bytes() == cur.as_bytes())
        {
            return true;
        }
        crate::core::account::read_snapshot(
            vault,
            &self.dirs.snapshot_file(&journal.target),
            &journal.target,
        )
        .is_ok_and(|s| s.credential.as_bytes() == cur.as_bytes())
    }

    /// Pin an encrypted copy of the current login before it is overwritten.
    fn keep_current_login(&self, vault: &Vault, current: Option<RawCredential>) -> Result<String> {
        let reg = self.load_registry()?;
        let ckpt = Checkpoint::new(
            &new_txn_id()?,
            current,
            companion::capture(&self.agy)?,
            companion::settings_exist(&self.agy),
            reg.active.clone(),
            reg.previous.clone(),
        );
        self.checkpoints().write_pinned(vault, &ckpt)
    }

    pub fn recover(&self, mode: RecoverMode) -> Result<RecoverOutcome> {
        self.recover_with(mode, false)
    }

    /// `force` allows restoring over a login that is not part of the
    /// interrupted switch (after keeping a copy of it).
    pub fn recover_with(&self, mode: RecoverMode, force: bool) -> Result<RecoverOutcome> {
        let _lock = self.lock_login()?;
        let Some(mut journal) = self.journals().load()? else {
            return Ok(RecoverOutcome::NothingToDo);
        };
        if journal.phase == Phase::Committed {
            self.journals().clear()?;
            return Ok(RecoverOutcome::NothingToDo);
        }
        let vault = self.vault()?;
        let target = journal.target.clone();

        if journal.phase == Phase::Verified && mode == RecoverMode::Auto {
            match self.roll_forward(vault, &mut journal) {
                Ok(()) => {
                    self.finish_recovery(&journal, false)?;
                    return Ok(RecoverOutcome::RolledForward { target });
                }
                Err(e) if e.kind() == ErrorKind::SimulatedCrash => return Err(e),
                Err(e) => tracing::warn!(
                    "roll-forward not possible ({}); rolling back instead",
                    e.message()
                ),
            }
        }
        let kept_checkpoint = self.rollback(vault, &journal, force)?;
        self.finish_recovery(&journal, true)?;
        Ok(RecoverOutcome::RolledBack {
            target,
            kept_checkpoint,
        })
    }

    fn roll_forward(&self, vault: &Vault, journal: &mut Journal) -> Result<()> {
        let snap = crate::core::account::read_snapshot(
            vault,
            &self.dirs.snapshot_file(&journal.target),
            &journal.target,
        )?;
        self.verify_target(&journal.target, &snap).map_err(|e| {
            Error::backend(format!(
                "native store no longer holds the target login: {}",
                e.message()
            ))
        })?;
        self.failpoints.hit("recover-after-native")?;
        let mut reg = self.load_registry()?;
        if reg.find(&journal.target).is_none() {
            return Err(Error::format("target account is no longer saved"));
        }
        self.commit_registry(&mut reg, journal)
    }

    /// `rolled_back` means the checkpoint's login is live again. A pinned
    /// checkpoint exists only for a login agyva has no snapshot of; after a
    /// rollback it is redundant (the same login is now live), so it goes,
    /// like `switch` does after its own rollback. A roll-forward did *not*
    /// restore it, so the pinned copy is kept.
    fn finish_recovery(&self, journal: &Journal, rolled_back: bool) -> Result<()> {
        self.journals().clear()?;
        if rolled_back && crate::core::checkpoint::CheckpointStore::is_pinned(&journal.checkpoint) {
            self.discard_checkpoint(&journal.checkpoint);
        }
        // The recovery itself is done; failing to tidy old checkpoints is not an error.
        if let Err(e) = self.checkpoints().prune(self.keep_checkpoints(), None) {
            tracing::warn!(error = %e.message(), "could not prune old checkpoints");
        }
        Ok(())
    }
}
