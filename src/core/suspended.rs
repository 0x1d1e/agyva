//! A tiny note that an account is suspended: `<state>/suspended.json`.
//!
//! The quota cache is disposable by design (deleting or corrupting it is
//! always safe), but that must not make a suspended account eligible for
//! refreshes again: agy would be run against it, and the suspension is not
//! something to retry. So the fact is persisted separately, outside the
//! cache, and only an explicit `--force-refresh` that succeeds clears it.
//!
//! Contents are account ids and a timestamp: no tokens, no emails, no
//! evidence text.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::error::{Error, Result};
use crate::paths::Dirs;
use crate::storage::fsutil;

const MAX_BYTES: usize = 64 * 1024;
const SCHEMA: u32 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct File {
    schema: u32,
    accounts: BTreeMap<String, DateTime<Utc>>,
}

pub struct SuspendedMarkers {
    path: PathBuf,
}

impl SuspendedMarkers {
    pub fn new(dirs: &Dirs) -> Self {
        Self {
            path: dirs.state.join("suspended.json"),
        }
    }

    /// Missing, oversized or corrupt reads as empty: the marker is a hint
    /// that costs one extra (checked) refresh if lost, never an error.
    fn load(&self) -> File {
        let Ok(meta) = std::fs::metadata(&self.path) else {
            return File::default();
        };
        if meta.len() > MAX_BYTES as u64 {
            tracing::warn!("suspension marker is implausibly large; ignoring it");
            return File::default();
        }
        match std::fs::read(&self.path)
            .ok()
            .and_then(|b| serde_json::from_slice::<File>(&b).ok())
        {
            Some(f) if f.schema == SCHEMA => f,
            Some(_) => {
                tracing::warn!("suspension marker has another schema; ignoring it");
                File::default()
            }
            None => File::default(),
        }
    }

    fn save(&self, file: &File) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fsutil::create_private_dir(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        let mut bytes = serde_json::to_vec_pretty(file)
            .map_err(|e| Error::format(format!("cannot encode the suspension marker: {e}")))?;
        bytes.push(b'\n');
        fsutil::atomic_write(&self.path, &bytes)
            .map_err(|e| Error::io(format!("writing {}", self.path.display()), e))
    }

    /// When this account was first seen suspended, if it is still marked.
    pub fn get(&self, id: &str) -> Option<DateTime<Utc>> {
        self.load().accounts.get(id).copied()
    }

    /// Record the suspension (keeps the first time it was seen).
    pub fn mark(&self, id: &str, at: DateTime<Utc>) -> Result<()> {
        let mut file = self.load();
        file.schema = SCHEMA;
        file.accounts.entry(id.to_owned()).or_insert(at);
        self.save(&file)
    }

    /// Clear it. Callers do this only after an explicit forced refresh that
    /// did not come back suspended.
    pub fn clear(&self, id: &str) -> Result<()> {
        let mut file = self.load();
        if file.accounts.remove(id).is_none() {
            return Ok(());
        }
        file.schema = SCHEMA;
        if file.accounts.is_empty() {
            match std::fs::remove_file(&self.path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(Error::io(format!("removing {}", self.path.display()), e)),
            }
        } else {
            self.save(&file)
        }
    }
}
