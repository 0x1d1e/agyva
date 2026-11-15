//! `agyva use`: the journaled switch transaction.
//!
//! Steps (the lock is held throughout):
//!
//! 1. validate everything that can fail before touching agy's state
//! 2. write the encrypted checkpoint, then the journal (`Prepared`)
//! 3. re-read agy's login: if it changed since step 1, abort (nothing written)
//! 4. save the outgoing account's fresh tokens back to its snapshot
//! 5. re-read once more, then write the target credential + companion state
//!    (`CredentialWritten`)
//! 6. read back and verify (`Verified`)
//! 7. commit the registry (`Committed`), then best-effort cleanup (journal,
//!    checkpoint pruning): a cleanup failure is a warning, not an error
//!
//! A failure after step 2 rolls back from the checkpoint, except that:
//!
//! * a login that changed underneath us is never overwritten by a rollback
//!   (`ConcurrentChange`: the switch just stops);
//! * a keyring write that timed out has an unknown outcome, so no rollback is
//!   guessed: the journal is left `RecoveryRequired` and `agyva recover`
//!   decides from the actual native state.
//!
//! A crash leaves the journal behind and `agyva recover` finishes or undoes
//! the work.

use crate::agy::companion::{self, Mode};
use crate::agy::credentials::{CredentialInfo, RawCredential, parse_credential};
use crate::agy::sessions;
use crate::core::account::{Snapshot, read_snapshot, write_snapshot};
use crate::core::app::App;
use crate::core::checkpoint::Checkpoint;
use crate::core::error::{Error, ErrorKind, OUTCOME_UNKNOWN_HINT, Result};
use crate::core::policy::RESTART_NOTICE;
use crate::core::registry::{AccountRecord, Registry};
use crate::core::selector;
use crate::core::view::AccountView;
use crate::storage::journal::{Journal, Phase};
use crate::storage::vault::Vault;

#[derive(Debug)]
pub struct SwitchOutcome {
    pub account: AccountView,
    pub already_active: bool,
    pub warnings: Vec<String>,
    pub policy_notice: Option<&'static str>,
}

pub(crate) fn new_txn_id() -> Result<String> {
    let mut b = [0u8; 6];
    getrandom::fill(&mut b)
        .map_err(|e| Error::vault(format!("system randomness unavailable: {e}")))?;
    Ok(hex::encode(b))
}

/// The live login, classified.
struct Live {
    raw: Option<RawCredential>,
    info: Option<CredentialInfo>,
}

impl Live {
    fn id(&self) -> Option<String> {
        self.info.as_ref().map(CredentialInfo::account_id)
    }
}

impl App {
    pub fn switch(&self, selector: &str, force: bool) -> Result<SwitchOutcome> {
        let _lock = self.lock_login()?;
        self.require_no_pending()?;
        self.require_compat()?;

        let mut reg = self.load_registry()?;
        let target = selector::resolve(&reg, selector)?.clone();
        let vault = self.vault()?;
        let snap = read_snapshot(vault, &self.dirs.snapshot_file(&target.id), &target.id)?;
        companion::preflight(&self.agy)?;

        let raw = self.native()?.read()?;
        let info = raw
            .as_ref()
            .map(|r| parse_credential(r.as_bytes()))
            .transpose();
        let info = match info {
            Ok(i) => i,
            Err(e) if force => {
                tracing::warn!(
                    "live credential not recognized ({}); continuing because of --force",
                    e.message()
                );
                None
            }
            Err(e) => {
                return Err(Error::refused("the current agy login is not in a recognized format; switching would overwrite it")
                    .hint("pass --force to proceed (an encrypted checkpoint is kept), or investigate with `agyva doctor`")
                    .with_source(e));
            }
        };
        let live = Live { raw, info };
        let live_id = live.id();

        if let Some(id) = &live_id {
            if *id == target.id {
                return self.already_active(&mut reg, vault, &target, &snap, &live);
            }
            if reg.find(id).is_none() && !force {
                return Err(Error::refused("the current agy login is not saved in agyva; switching would replace it")
                    .hint("run `agyva add` first to save it, or pass --force (an encrypted checkpoint is kept)"));
            }
        }
        let from = live_id
            .clone()
            .filter(|id| reg.find(id).is_some())
            .or_else(|| reg.active.clone());

        // A live login agyva has no snapshot of (only reachable with --force)
        // would be lost for good once overwritten: keep its checkpoint.
        let unsaved_login =
            live.raw.is_some() && !live_id.as_ref().is_some_and(|id| reg.find(id).is_some());

        let txn_id = new_txn_id()?;
        let ckpt = Checkpoint::new(
            &txn_id,
            live.raw.clone(),
            companion::capture(&self.agy)?,
            companion::settings_exist(&self.agy),
            reg.active.clone(),
            reg.previous.clone(),
        );
        let outgoing_companion = ckpt.companion.clone();
        let name = if unsaved_login {
            self.checkpoints().write_pinned(vault, &ckpt)?
        } else {
            self.checkpoints().write(vault, &ckpt)?
        };
        self.failpoints.hit("after-checkpoint")?;
        let mut journal = Journal::new(txn_id, from, target.id.clone(), name);
        self.journals().save(&mut journal)?;

        let steps = self.switch_steps(
            vault,
            &mut journal,
            &mut reg,
            &snap,
            &live,
            outgoing_companion,
        );
        if let Err(e) = steps {
            return Err(self.abort_switch(vault, &journal, e));
        }

        let mut warnings = vec![RESTART_NOTICE.to_owned()];
        if unsaved_login {
            warnings.push(format!(
                "the previous agy login was not saved in agyva. An encrypted copy is kept in {} \
                 and is NOT pruned automatically; agyva cannot restore it for you yet, so delete \
                 it yourself once you no longer need it.",
                self.dirs
                    .checkpoint_dir()
                    .join(&journal.checkpoint)
                    .display()
            ));
        }
        if let Err(e) = self.post_commit_cleanup() {
            if e.kind() == ErrorKind::SimulatedCrash {
                return Err(e);
            }
            tracing::warn!(error = %e.message(), "post-commit cleanup failed");
            warnings.push(format!(
                "the switch completed, but cleanup failed ({}); it is harmless and the next command retries it.",
                e.message()
            ));
        }

        let rec = reg
            .find(&target.id)
            .ok_or_else(|| Error::format("account vanished from the registry"))?;
        warnings.extend(sessions::detect(&self.agy).warning());
        Ok(SwitchOutcome {
            account: AccountView::of(&reg, rec),
            already_active: false,
            warnings,
            policy_notice: self.take_policy_notice()?,
        })
    }

    fn switch_steps(
        &self,
        vault: &Vault,
        journal: &mut Journal,
        reg: &mut Registry,
        snap: &Snapshot,
        live: &Live,
        outgoing_companion: companion::Companion,
    ) -> Result<()> {
        let native = self.native()?;
        let journals = self.journals();

        self.failpoints.hit("after-prepared")?;
        // Another process (agy itself) may have refreshed or replaced the
        // login since it was read: stop before anything is written.
        self.ensure_native_unchanged(live)?;
        // Keep the outgoing account's refreshed tokens and companion state.
        if let (Some(raw), Some(id)) = (&live.raw, live.id().filter(|id| reg.find(id).is_some())) {
            let fresh = Snapshot::new(raw.clone(), outgoing_companion);
            write_snapshot(vault, &self.dirs.snapshot_file(&id), &id, &fresh)?;
        }

        self.ensure_native_unchanged(live)?;
        native.write(snap.credential.as_bytes())?;
        self.failpoints.hit("after-native-write")?;
        companion::apply(&self.agy, &snap.companion, Mode::Switch)?;
        self.failpoints.hit("after-companion")?;
        journals.set_phase(journal, Phase::CredentialWritten)?;
        self.failpoints.hit("after-credential-written")?;

        self.failpoints.hit("verify")?;
        self.verify_target(&journal.target, snap)?;
        journals.set_phase(journal, Phase::Verified)?;
        self.failpoints.hit("after-verified")?;

        self.commit_registry(reg, journal)?;
        self.failpoints.hit("after-registry")?;
        journals.set_phase(journal, Phase::Committed)
    }

    /// Abort if agy's login is no longer byte-identical to what this switch
    /// started from.
    fn ensure_native_unchanged(&self, live: &Live) -> Result<()> {
        let now = self.native()?.read()?;
        let same = now.as_ref().map(RawCredential::as_bytes)
            == live.raw.as_ref().map(RawCredential::as_bytes);
        if same {
            return Ok(());
        }
        Err(Error::new(
            ErrorKind::ConcurrentChange,
            "agy's login changed while the switch was being prepared; nothing was written",
        )
        .hint("agy may have refreshed its token or been logged in again; run the command again"))
    }

    /// Journal and checkpoint housekeeping after a successful commit.
    fn post_commit_cleanup(&self) -> Result<()> {
        self.failpoints.hit("post-commit-cleanup")?;
        self.journals().clear()?;
        self.checkpoints().prune(self.keep_checkpoints(), None)?;
        Ok(())
    }

    /// Read back (no network): the native store holds exactly the target's
    /// credential and its identity is the target.
    pub(crate) fn verify_target(&self, target_id: &str, snap: &Snapshot) -> Result<()> {
        let native = self.native()?;
        native.verify(snap.credential.as_bytes())?;
        let back = native
            .read()?
            .ok_or_else(|| Error::backend("verification failed: nothing readable after write"))?;
        if parse_credential(back.as_bytes())?.account_id() != target_id {
            return Err(Error::backend(
                "verification failed: the stored credential belongs to a different account",
            ));
        }
        Ok(())
    }

    /// Idempotent: safe to run again during roll-forward.
    pub(crate) fn commit_registry(&self, reg: &mut Registry, journal: &Journal) -> Result<()> {
        reg.mark_switched(journal.from.as_deref(), &journal.target);
        self.save_registry(reg)
    }

    fn already_active(
        &self,
        reg: &mut Registry,
        vault: &Vault,
        target: &AccountRecord,
        snap: &Snapshot,
        live: &Live,
    ) -> Result<SwitchOutcome> {
        // Same login: just keep the snapshot's tokens fresh and the registry honest.
        if let Some(raw) = &live.raw {
            let fresh = Snapshot::new(raw.clone(), companion::capture(&self.agy)?);
            if raw.as_bytes() != snap.credential.as_bytes() {
                write_snapshot(
                    vault,
                    &self.dirs.snapshot_file(&target.id),
                    &target.id,
                    &fresh,
                )?;
            }
        }
        if reg.active.as_deref() != Some(&target.id) {
            let from = reg.active.clone();
            reg.mark_switched(from.as_deref(), &target.id);
            self.save_registry(reg)?;
        }
        let rec = reg
            .find(&target.id)
            .ok_or_else(|| Error::format("account vanished from the registry"))?;
        Ok(SwitchOutcome {
            account: AccountView::of(reg, rec),
            already_active: true,
            warnings: Vec::new(),
            policy_notice: self.take_policy_notice()?,
        })
    }

    /// A step failed. A simulated crash propagates untouched (the process
    /// "dies"); a changed login just stops the switch; an unknown keyring
    /// outcome is left to `recover`; anything else is rolled back from the
    /// checkpoint.
    fn abort_switch(&self, vault: &Vault, journal: &Journal, cause: Error) -> Error {
        match cause.kind() {
            ErrorKind::SimulatedCrash => return cause,
            ErrorKind::ConcurrentChange => {
                // Nothing was written, and a rollback would overwrite the
                // newer login with the older checkpoint: just stand down.
                if let Err(e) = self.journals().clear() {
                    return e;
                }
                self.discard_checkpoint(&journal.checkpoint);
                return cause;
            }
            ErrorKind::OutcomeUnknown => return self.leave_for_recovery(journal, &cause),
            _ => {}
        }
        match self.rollback(vault, journal, false) {
            Ok(_) => match self.journals().clear() {
                Ok(()) => {
                    // The login is back as it was; a pinned copy is redundant.
                    if crate::core::checkpoint::CheckpointStore::is_pinned(&journal.checkpoint) {
                        self.discard_checkpoint(&journal.checkpoint);
                    }
                    Error::new(
                        cause.kind(),
                        format!("{}; the previous login was restored", cause.message()),
                    )
                    .hint(
                        cause
                            .hint_text()
                            .unwrap_or("nothing was changed")
                            .to_owned(),
                    )
                }
                Err(e) => e,
            },
            Err(rb) if rb.kind() == ErrorKind::OutcomeUnknown => {
                self.leave_for_recovery(journal, &rb)
            }
            Err(rb) => {
                let mut j = journal.clone();
                j.phase = Phase::RecoveryRequired;
                j.detail = Some(format!(
                    "switch failed ({}); rollback failed ({})",
                    cause.message(),
                    rb.message()
                ));
                let _ = self.journals().save(&mut j);
                Error::recovery_required(format!(
                    "the switch failed ({}) and the automatic rollback also failed ({})",
                    cause.message(),
                    rb.message()
                ))
            }
        }
    }

    /// A keyring write/delete timed out: it may or may not have happened.
    /// Do not roll back on a guess and do not clear the journal.
    fn leave_for_recovery(&self, journal: &Journal, cause: &Error) -> Error {
        let mut j = journal.clone();
        j.phase = Phase::RecoveryRequired;
        j.detail = Some(format!(
            "{}; outcome unknown, no rollback attempted",
            cause.message()
        ));
        let _ = self.journals().save(&mut j);
        Error::new(
            ErrorKind::RecoveryRequired,
            format!(
                "{}. agyva cannot tell whether agy's login was changed, so it left everything as it is",
                cause.message()
            ),
        )
        .hint(OUTCOME_UNKNOWN_HINT)
    }

    pub(crate) fn discard_checkpoint(&self, name: &str) {
        if let Err(e) = self.checkpoints().remove(name) {
            tracing::warn!(error = %e.message(), "could not remove checkpoint {name}");
        }
    }
}
