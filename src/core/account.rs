//! Encrypted per-account snapshot: the credential document plus the
//! companion state agy keeps next to it.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::agy::companion::Companion;
use crate::agy::credentials::{RawCredential, parse_credential};
use crate::core::error::{Error, Result};
use crate::core::ids;
use crate::storage::vault::Vault;

pub const SNAPSHOT_SCHEMA: u32 = 1;

#[derive(Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub schema_version: u32,
    pub credential: RawCredential,
    #[serde(default)]
    pub companion: Companion,
    pub captured_at: DateTime<Utc>,
}

impl Snapshot {
    pub fn new(credential: RawCredential, companion: Companion) -> Self {
        Self {
            schema_version: SNAPSHOT_SCHEMA,
            credential,
            companion,
            captured_at: Utc::now(),
        }
    }
}

/// AAD context binding a snapshot to its account id.
pub fn snapshot_context(id: &str) -> String {
    format!("snapshot:{id}")
}

pub fn write_snapshot(
    vault: &Vault,
    path: &std::path::Path,
    id: &str,
    snap: &Snapshot,
) -> Result<()> {
    ids::require_account_id(id)?;
    let plain = Zeroizing::new(
        serde_json::to_vec(snap).map_err(|_| Error::format("cannot encode snapshot"))?,
    );
    vault.write_file(path, &snapshot_context(id), &plain)
}

/// Read, decrypt and validate a snapshot: schema known, credential parses
/// and really belongs to `id`.
pub fn read_snapshot(vault: &Vault, path: &std::path::Path, id: &str) -> Result<Snapshot> {
    ids::require_account_id(id)?;
    let plain = vault
        .read_file(path, &snapshot_context(id))?
        .ok_or_else(|| {
            Error::format(format!("the saved credential for account {id} is missing")).hint(
                "remove the account with `agyva remove` and add it again from a fresh `agy` login",
            )
        })?;
    let snap: Snapshot = serde_json::from_slice(&plain).map_err(|_| {
        Error::format(format!(
            "the saved credential for account {id} is malformed"
        ))
    })?;
    if snap.schema_version != SNAPSHOT_SCHEMA {
        return Err(Error::format(format!(
            "snapshot schema {} is not supported",
            snap.schema_version
        ))
        .hint("upgrade agyva"));
    }
    let info = parse_credential(snap.credential.as_bytes())?;
    if info.account_id() != id {
        return Err(Error::format(format!(
            "snapshot for account {id} holds a different identity"
        )));
    }
    Ok(snap)
}
