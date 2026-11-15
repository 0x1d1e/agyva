//! Transaction journal for multi-step switches.
//!
//! One pending journal at most (`state/journal/pending.json`), written
//! atomically at every phase change. It holds ids and phases only, never
//! credential material (that lives in the encrypted checkpoint it names).

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::fsutil;
use crate::core::error::{Error, Result};
use crate::core::ids;

pub const JOURNAL_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Checkpoint saved; the native store may or may not have been modified.
    Prepared,
    /// Native credential and companion state written; not yet verified.
    CredentialWritten,
    /// Read-back verification passed; registry commit pending.
    Verified,
    /// Fully applied. Only seen if a crash hit between commit and cleanup.
    Committed,
    /// Automatic rollback failed; operator action needed.
    RecoveryRequired,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Prepared => "prepared",
            Self::CredentialWritten => "credential_written",
            Self::Verified => "verified",
            Self::Committed => "committed",
            Self::RecoveryRequired => "recovery_required",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Journal {
    pub schema_version: u32,
    pub txn_id: String,
    pub operation: String,
    /// Account that was live before the switch (if a managed one).
    pub from: Option<String>,
    pub target: String,
    pub phase: Phase,
    /// File name of the encrypted checkpoint inside the checkpoint dir.
    pub checkpoint: String,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Journal {
    pub fn new(txn_id: String, from: Option<String>, target: String, checkpoint: String) -> Self {
        let now = Utc::now();
        Self {
            schema_version: JOURNAL_SCHEMA,
            txn_id,
            operation: "switch".into(),
            from,
            target,
            phase: Phase::Prepared,
            checkpoint,
            started_at: now,
            updated_at: now,
            detail: None,
        }
    }

    /// Ids and the checkpoint name end up in paths: refuse anything that is
    /// not the exact shape agyva writes.
    fn validate(&self) -> Result<()> {
        let bad = |what: &str| {
            Error::recovery_required(format!("the transaction journal holds a malformed {what}"))
                .hint("inspect the file and the checkpoints directory; remove the journal only after restoring your login with `agy`")
        };
        if !ids::is_txn_id(&self.txn_id) {
            return Err(bad("transaction id"));
        }
        if !ids::is_account_id(&self.target) {
            return Err(bad("target account id"));
        }
        if self.from.as_deref().is_some_and(|f| !ids::is_account_id(f)) {
            return Err(bad("source account id"));
        }
        if !ids::is_checkpoint_name(&self.checkpoint) {
            return Err(bad("checkpoint name"));
        }
        Ok(())
    }

    /// Whether the journal demands recovery (anything short of committed).
    pub fn is_incomplete(&self) -> bool {
        self.phase != Phase::Committed
    }
}

#[derive(Debug, Clone)]
pub struct JournalStore {
    dir: PathBuf,
}

impl JournalStore {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn path(&self) -> PathBuf {
        self.dir.join("pending.json")
    }

    /// The pending journal, if any. An unreadable journal is itself a
    /// recovery-required condition (it is never silently ignored).
    pub fn load(&self) -> Result<Option<Journal>> {
        let path = self.path();
        let Some(bytes) = fsutil::read_optional(&path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
        else {
            return Ok(None);
        };
        let journal: Journal = serde_json::from_slice(&bytes).map_err(|_| {
            Error::recovery_required(format!("the transaction journal {} is unreadable", path.display()))
                .hint("inspect the file and the checkpoints directory; remove the journal only after restoring your login with `agy`")
        })?;
        if journal.schema_version != JOURNAL_SCHEMA {
            return Err(Error::recovery_required(
                "the transaction journal has an unknown schema version",
            )
            .hint("upgrade agyva, then run `agyva recover`"));
        }
        journal.validate()?;
        Ok(Some(journal))
    }

    pub fn save(&self, journal: &mut Journal) -> Result<()> {
        journal.updated_at = Utc::now();
        fsutil::create_private_dir(&self.dir)
            .map_err(|e| Error::io(format!("creating {}", self.dir.display()), e))?;
        let bytes = serde_json::to_vec_pretty(journal).map_err(|e| Error::format(e.to_string()))?;
        let path = self.path();
        fsutil::atomic_write(&path, &bytes)
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))
    }

    pub fn set_phase(&self, journal: &mut Journal, phase: Phase) -> Result<()> {
        journal.phase = phase;
        self.save(journal)
    }

    pub fn clear(&self) -> Result<()> {
        let path = self.path();
        fsutil::remove_durable(&path)
            .map(|_| ())
            .map_err(|e| Error::io(format!("removing {}", path.display()), e))
    }
}
