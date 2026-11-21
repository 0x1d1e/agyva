//! Encrypted pre-switch checkpoints.
//!
//! A checkpoint holds everything a switch overwrites (live credential,
//! companion state, registry pointers) so a failed or interrupted switch can
//! be undone. Written before the journal; the newest N are kept.

use std::path::PathBuf;

use chrono::{DateTime, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::agy::companion::Companion;
use crate::agy::credentials::RawCredential;
use crate::core::error::{Error, ErrorKind, Result};
use crate::core::ids;
use crate::storage::fsutil;
use crate::storage::vault::Vault;

pub const CHECKPOINT_SCHEMA: u32 = 1;
const PINNED_SUFFIX: &str = ".keep.ckpt";

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CheckpointMeta {
    pub name: String,
    pub txn_id: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub pinned: bool,
    pub size_bytes: u64,
    pub valid: bool,
}

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

    /// Parse metadata for all checkpoint files in `checkpoints/`, sorted newest first.
    /// Malformed filenames that fail `ids::is_checkpoint_name` are ignored safely;
    /// valid filenames whose timestamp/txn_id cannot be parsed are flagged `valid: false`.
    pub fn list_meta(&self) -> Result<Vec<CheckpointMeta>> {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(Error::io(format!("listing {}", self.dir.display()), e)),
        };
        let mut out = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| Error::io(format!("reading {}", self.dir.display()), e))?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "ckpt") {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if !ids::is_checkpoint_name(&name) {
                tracing::warn!(checkpoint = %name, "ignoring malformed checkpoint filename");
                continue;
            }
            let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
            out.push(parse_checkpoint_meta(&name, size_bytes));
        }
        out.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| b.name.cmp(&a.name))
        });
        Ok(out)
    }

    /// Decrypt a checkpoint by file name without requiring an external journal.
    pub fn read_by_name(&self, vault: &Vault, name: &str) -> Result<Checkpoint> {
        if !ids::is_checkpoint_name(name) {
            return Err(Error::usage(format!("invalid checkpoint name `{name}`")));
        }
        let path = self.dir.join(name);
        if !path.exists() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("checkpoint `{name}` not found"),
            )
            .hint("run `agyva checkpoints list` to see available checkpoints"));
        }
        let meta = parse_checkpoint_meta(name, 0);
        let txn_id = meta.txn_id.ok_or_else(|| {
            Error::format(format!(
                "checkpoint `{name}` has a malformed transaction id"
            ))
        })?;
        self.read(vault, name, &txn_id)
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
        Ok(self.prune_returning(keep, false, protect)?.len())
    }

    /// Like [`prune`](Self::prune), optionally including pinned checkpoints,
    /// and returning the names of removed checkpoint files.
    pub fn prune_returning(
        &self,
        keep: usize,
        include_pinned: bool,
        protect: Option<&str>,
    ) -> Result<Vec<String>> {
        let names = self.list()?;
        let ordinary: Vec<String> = names
            .iter()
            .filter(|n| !Self::is_pinned(n))
            .cloned()
            .collect();
        let excess = ordinary.len().saturating_sub(keep);
        let mut to_remove: Vec<String> = ordinary
            .into_iter()
            .take(excess)
            .filter(|n| Some(n.as_str()) != protect)
            .collect();
        if include_pinned {
            for n in names {
                if Self::is_pinned(&n) && Some(n.as_str()) != protect {
                    to_remove.push(n);
                }
            }
        }
        let mut removed = Vec::with_capacity(to_remove.len());
        for name in to_remove {
            let path = self.dir.join(&name);
            fsutil::remove_durable(&path)
                .map_err(|e| Error::io(format!("removing {}", path.display()), e))?;
            removed.push(name);
        }
        Ok(removed)
    }
}

pub fn parse_checkpoint_meta(name: &str, size_bytes: u64) -> CheckpointMeta {
    let pinned = CheckpointStore::is_pinned(name);
    let stem = name
        .strip_suffix(PINNED_SUFFIX)
        .or_else(|| name.strip_suffix(".ckpt"));
    let parsed = stem.and_then(|s| s.rsplit_once('-')).and_then(|(ts, txn)| {
        if !ids::is_txn_id(txn) {
            return None;
        }
        let dt = NaiveDateTime::parse_from_str(ts, "%Y%m%dT%H%M%S%.fZ")
            .ok()?
            .and_utc();
        Some((dt, txn.to_owned()))
    });
    match parsed {
        Some((dt, txn)) => CheckpointMeta {
            name: name.to_owned(),
            txn_id: Some(txn),
            created_at: Some(dt),
            pinned,
            size_bytes,
            valid: true,
        },
        None => CheckpointMeta {
            name: name.to_owned(),
            txn_id: None,
            created_at: None,
            pinned,
            size_bytes,
            valid: false,
        },
    }
}
