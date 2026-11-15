//! Encrypted pre-switch checkpoints.
//!
//! A checkpoint holds everything a switch overwrites (live credential,
//! companion state, registry pointers) so a failed or interrupted switch can
//! be undone. Written before the journal; the newest N are kept.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::agy::companion::Companion;
use crate::agy::credentials::RawCredential;
use crate::core::error::{Error, Result};
use crate::core::ids;
use crate::storage::fsutil;
use crate::storage::vault::Vault;

pub const CHECKPOINT_SCHEMA: u32 = 1;
const PINNED_SUFFIX: &str = ".keep.ckpt";

#[derive(Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub txn_id: String,
    pub created_at: DateTime<Utc>,
    /// The live credential before the switch (`None`: agy was logged out).
    pub native: Option<RawCredential>,
    pub companion: Companion,
    pub settings_existed: bool,
    pub active: Option<String>,
    pub previous: Option<String>,
}

impl Checkpoint {
    pub fn new(
        txn_id: &str,
        native: Option<RawCredential>,
        companion: Companion,
        settings_existed: bool,
        active: Option<String>,
        previous: Option<String>,
    ) -> Self {
        Self {
            schema_version: CHECKPOINT_SCHEMA,
            txn_id: txn_id.to_owned(),
            created_at: Utc::now(),
            native,
            companion,
            settings_existed,
            active,
            previous,
        }
    }
}

fn context(txn_id: &str) -> String {
    format!("checkpoint:{txn_id}")
}

#[derive(Debug, Clone)]
pub struct CheckpointStore {
    dir: PathBuf,
}

impl CheckpointStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    /// Encrypt and persist; returns the file name to record in the journal.
    pub fn write(&self, vault: &Vault, ckpt: &Checkpoint) -> Result<String> {
        self.write_named(vault, ckpt, false)
    }

    /// Like [`write`](Self::write), but the file holds a login agyva has no
    /// other copy of: it is never counted or deleted by [`prune`](Self::prune).
    pub fn write_pinned(&self, vault: &Vault, ckpt: &Checkpoint) -> Result<String> {
        self.write_named(vault, ckpt, true)
    }

    pub fn is_pinned(name: &str) -> bool {
        name.ends_with(PINNED_SUFFIX)
    }

    fn write_named(&self, vault: &Vault, ckpt: &Checkpoint, pinned: bool) -> Result<String> {
        ids::require_txn_id(&ckpt.txn_id)?;
        let name = format!(
            "{}-{}{}",
            ckpt.created_at.format("%Y%m%dT%H%M%S%.6fZ"),
            ckpt.txn_id,
            if pinned { PINNED_SUFFIX } else { ".ckpt" }
        );
        let plain = Zeroizing::new(
            serde_json::to_vec(ckpt).map_err(|_| Error::format("cannot encode checkpoint"))?,
        );
        vault.write_file(&self.dir.join(&name), &context(&ckpt.txn_id), &plain)?;
        Ok(name)
    }

    pub fn read(&self, vault: &Vault, name: &str, txn_id: &str) -> Result<Checkpoint> {
        if !ids::is_checkpoint_name(name) || !ids::is_txn_id(txn_id) {
            return Err(Error::format("journal names an invalid checkpoint file"));
        }
        let path = self.dir.join(name);
        let plain = vault.read_file(&path, &context(txn_id))?.ok_or_else(|| {
            Error::recovery_required(format!("checkpoint {name} is missing"))
                .hint("the previous login cannot be restored automatically; log in again with `agy`, then remove the journal")
        })?;
        let ckpt: Checkpoint = serde_json::from_slice(&plain)
            .map_err(|_| Error::format(format!("checkpoint {name} is malformed")))?;
        if ckpt.schema_version != CHECKPOINT_SCHEMA || ckpt.txn_id != txn_id {
            return Err(Error::format(format!(
                "checkpoint {name} does not match its journal"
            )));
        }
        Ok(ckpt)
    }

    /// Checkpoint file names, oldest first.
    pub fn list(&self) -> Result<Vec<String>> {
        let files = fsutil::list_with_extension(&self.dir, "ckpt")
            .map_err(|e| Error::io(format!("listing {}", self.dir.display()), e))?;
        Ok(files
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect())
    }

    /// Delete a checkpoint file (missing is fine).
    pub fn remove(&self, name: &str) -> Result<()> {
        if !ids::is_checkpoint_name(name) {
            return Err(Error::format("invalid checkpoint file name"));
        }
        let path = self.dir.join(name);
        fsutil::remove_durable(&path)
            .map(|_| ())
            .map_err(|e| Error::io(format!("removing {}", path.display()), e))
    }

    /// Keep the newest `keep` ordinary checkpoints (never `protect`), delete
    /// the rest. Pinned checkpoints are neither counted nor deleted.
    pub fn prune(&self, keep: usize, protect: Option<&str>) -> Result<usize> {
        let names: Vec<String> = self
            .list()?
            .into_iter()
            .filter(|n| !Self::is_pinned(n))
            .collect();
        let excess = names.len().saturating_sub(keep);
        let mut removed = 0;
        for name in names
            .iter()
            .take(excess)
            .filter(|n| Some(n.as_str()) != protect)
        {
            let path = self.dir.join(name);
            fsutil::remove_durable(&path)
                .map_err(|e| Error::io(format!("removing {}", path.display()), e))?;
            removed += 1;
        }
        Ok(removed)
    }
}
