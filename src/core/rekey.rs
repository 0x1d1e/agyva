//! `agyva rekey`: rotate the vault key or passphrase, or migrate between
//! `keyring` and `passphrase` modes.
//!
//! All existing snapshots and checkpoints are decrypted into zeroizing memory
//! and re-encrypted under the new key before any keyring or disk mutation.
//! On any error before `vault.json` commits, replaced files and any changed
//! keyring key are restored.

use std::fs;
use std::path::PathBuf;

use serde::Serialize;
use zeroize::Zeroizing;

use crate::config::VaultKeyMode;
use crate::core::account::snapshot_context;
use crate::core::app::App;
use crate::core::error::{Error, ErrorKind, Result};
use crate::core::ids;
use crate::storage::fsutil;
use crate::storage::vault::{Vault, VaultKey};
use crate::storage::vault_key::{KeyProvider, VaultMeta};

/// Summary of a completed `rekey`. Counts and mode only; never secret data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RekeyOutcome {
    pub mode: String,
    pub snapshots: usize,
    pub checkpoints: usize,
    pub warnings: Vec<String>,
}

struct RekeyBlob {
    path: PathBuf,
    context: String,
    plain: Zeroizing<Vec<u8>>,
    old_sealed: Vec<u8>,
}

struct SealedBlob {
    path: PathBuf,
    old_sealed: Vec<u8>,
    new_sealed: Vec<u8>,
}

impl App {
    /// Rotate the vault key/passphrase, or migrate to `target_mode`.
    pub fn rekey(&self, target_mode: Option<VaultKeyMode>) -> Result<RekeyOutcome> {
        let _lock = self.lock()?;
        self.require_no_pending()?;

        let provider = self.key_provider();
        let old_meta = provider.load_meta()?.ok_or_else(|| {
            Error::refused("no vault exists yet").hint("run `agyva add` to save your first account")
        })?;
        let old_mode = parse_meta_mode(&old_meta.mode)?;

        // Unlock locally instead of populating `self.vault`, so `OnceCell`
        // cannot retain the retired key if this `App` is queried afterwards.
        let old_key = provider.unlock()?;
        let old_vault = Vault::new(old_key.clone());

        // Read and decrypt every snapshot and checkpoint before provisioning
        // or writing anything.
        let (snap_blobs, ckpt_blobs) = self.load_rekey_blobs(&old_vault)?;
        let snapshot_count = snap_blobs.len();
        let checkpoint_count = ckpt_blobs.len();

        let requested_mode = target_mode.unwrap_or(old_mode);
        let (mut new_key, mut new_meta) =
            provider.provision_new_key(requested_mode, &*self.new_passphrases)?;
        let mut sealed = seal_all(&new_key, &snap_blobs, &ckpt_blobs)?;

        if new_meta.mode == "keyring"
            && let Err(e) = provider.commit_keyring_key(&new_key)
        {
            if requested_mode == VaultKeyMode::Auto && e.kind() == ErrorKind::Backend {
                if old_mode == VaultKeyMode::Keyring {
                    let _ = provider.commit_keyring_key(&old_key);
                }
                tracing::info!(reason = %e.message(), "keyring write failed in auto mode, falling back to passphrase vault");
                eprintln!(
                    "note: OS keyring unavailable ({}); using a passphrase-protected vault",
                    e.message()
                );
                let fallback =
                    provider.provision_new_key(VaultKeyMode::Passphrase, &*self.new_passphrases)?;
                new_key = fallback.0;
                new_meta = fallback.1;
                sealed = seal_all(&new_key, &snap_blobs, &ckpt_blobs)?;
            } else {
                if old_mode == VaultKeyMode::Keyring && e.kind() != ErrorKind::Timeout {
                    let _ = provider.commit_keyring_key(&old_key);
                }
                return Err(e);
            }
        }

        let keyring_updated = new_meta.mode == "keyring";
        let restore_keyring = || {
            if !keyring_updated {
                return;
            }
            match old_mode {
                VaultKeyMode::Keyring => {
                    let _ = provider.commit_keyring_key(&old_key);
                }
                VaultKeyMode::Passphrase => {
                    let _ = provider.delete_keyring_key();
                }
                VaultKeyMode::Auto => {}
            }
        };

        for (idx, item) in sealed.iter().enumerate() {
            if let Err(e) = fsutil::atomic_write(&item.path, &item.new_sealed) {
                rollback_files(&sealed[..idx]);
                restore_keyring();
                return Err(Error::io(format!("writing {}", item.path.display()), e));
            }
        }

        if let Err(e) = self.failpoints.hit("rekey-before-meta") {
            rollback_files(&sealed);
            restore_keyring();
            return Err(e);
        }

        if let Err(e) = provider.write_meta(&new_meta) {
            rollback_files(&sealed);
            restore_keyring();
            return Err(e);
        }

        drop(snap_blobs);
        drop(ckpt_blobs);
        drop(old_vault);

        let mut warnings = Vec::new();
        if old_mode == VaultKeyMode::Keyring
            && new_meta.mode == "passphrase"
            && let Err(e) = clean_old_keyring(&provider)
        {
            warnings.push(e);
        }

        Ok(RekeyOutcome {
            mode: new_meta.mode,
            snapshots: snapshot_count,
            checkpoints: checkpoint_count,
            warnings,
        })
    }

    fn load_rekey_blobs(&self, old_vault: &Vault) -> Result<(Vec<RekeyBlob>, Vec<RekeyBlob>)> {
        let reg = self.load_registry()?;
        let mut snaps = Vec::with_capacity(reg.accounts.len());
        for rec in &reg.accounts {
            ids::require_account_id(&rec.id)?;
            let path = self.dirs.snapshot_file(&rec.id);
            let old_sealed = fsutil::read_optional(&path)
                .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
                .ok_or_else(|| {
                    Error::format(format!("the saved credential for account {} is missing", rec.id))
                        .hint("remove the account with `agyva remove` and add it again from a fresh `agy` login")
                })?;
            let context = snapshot_context(&rec.id);
            let plain = old_vault.open(&context, &old_sealed)?;
            snaps.push(RekeyBlob {
                path,
                context,
                plain,
                old_sealed,
            });
        }

        let ckpts = self.checkpoints();
        let ckpt_names = ckpts.list()?;
        let mut ckpt_blobs = Vec::with_capacity(ckpt_names.len());
        for name in ckpt_names {
            let txn_id = parse_checkpoint_txn_id(&name)?;
            let path = ckpts.dir().join(&name);
            let old_sealed =
                fs::read(&path).map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
            let context = format!("checkpoint:{txn_id}");
            let plain = old_vault.open(&context, &old_sealed)?;
            ckpt_blobs.push(RekeyBlob {
                path,
                context,
                plain,
                old_sealed,
            });
        }

        Ok((snaps, ckpt_blobs))
    }
}

fn parse_meta_mode(mode: &str) -> Result<VaultKeyMode> {
    match mode {
        "keyring" => Ok(VaultKeyMode::Keyring),
        "passphrase" => Ok(VaultKeyMode::Passphrase),
        other => Err(Error::format(format!(
            "vault.json: unknown key mode `{other}`"
        ))),
    }
}

fn parse_checkpoint_txn_id(name: &str) -> Result<String> {
    if !ids::is_checkpoint_name(name) {
        return Err(Error::format(format!(
            "invalid checkpoint file name `{name}`"
        )));
    }
    let stem = name
        .strip_suffix(".keep.ckpt")
        .or_else(|| name.strip_suffix(".ckpt"))
        .ok_or_else(|| Error::format(format!("invalid checkpoint file name `{name}`")))?;
    let (_, txn_id) = stem
        .rsplit_once('-')
        .ok_or_else(|| Error::format(format!("checkpoint `{name}` has no transaction id")))?;
    ids::require_txn_id(txn_id)?;
    Ok(txn_id.to_owned())
}

fn seal_all(
    new_key: &VaultKey,
    snaps: &[RekeyBlob],
    ckpts: &[RekeyBlob],
) -> Result<Vec<SealedBlob>> {
    let new_vault = Vault::new(new_key.clone());
    let mut out = Vec::with_capacity(snaps.len() + ckpts.len());
    for blob in snaps.iter().chain(ckpts.iter()) {
        let new_sealed = new_vault.seal(&blob.context, &blob.plain)?;
        out.push(SealedBlob {
            path: blob.path.clone(),
            old_sealed: blob.old_sealed.clone(),
            new_sealed,
        });
    }
    Ok(out)
}

fn rollback_files(written: &[SealedBlob]) {
    for item in written {
        let _ = fsutil::atomic_write(&item.path, &item.old_sealed);
    }
}

fn clean_old_keyring(provider: &KeyProvider) -> std::result::Result<(), String> {
    match provider.delete_keyring_key() {
        Ok(_) => Ok(()),
        Err(e) => Err(format!(
            "rekey completed, but removing the old keyring entry failed ({})",
            e.message()
        )),
    }
}

// Keep VaultMeta referenced for doc/type clarity if needed; also verified in unit tests.
#[allow(dead_code)]
fn _assert_meta_type(_: &VaultMeta) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_regular_and_pinned_checkpoint_txn_ids() {
        assert_eq!(
            parse_checkpoint_txn_id("20260101T000000.000001Z-0123456789ab.ckpt").unwrap(),
            "0123456789ab"
        );
        assert_eq!(
            parse_checkpoint_txn_id("20260101T000000.000001Z-abcdef012345.keep.ckpt").unwrap(),
            "abcdef012345"
        );
        assert!(parse_checkpoint_txn_id("../escape.ckpt").is_err());
        assert!(parse_checkpoint_txn_id("no-txn.ckpt").is_err());
    }
}
