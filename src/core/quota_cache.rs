//! The quota cache: `<cache>/quota.json`.
//!
//! Non-sensitive by construction (account ids, percentages, timestamps and
//! short evidence strings; no tokens, no emails). Written atomically with
//! mode 0600. Reads are lenient: a missing, oversized, corrupt or
//! unknown-version file is treated as empty, never as an error, so a broken
//! cache cannot take the application down. Read-modify-write cycles hold a
//! small lock so `watch` and the TUI do not lose each other's updates.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::error::{Error, Result};
use crate::core::health::HealthRecord;
use crate::core::quota::{ModelQuota, Unavailable};
use crate::paths::Dirs;
use crate::storage::fsutil;
use crate::storage::lock::StateLock;

pub const CACHE_SCHEMA: u32 = 1;
const MAX_CACHE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedQuota {
    pub observed_at: DateTime<Utc>,
    pub groups: Vec<ModelQuota>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheEntry {
    pub quota: Option<CachedQuota>,
    pub last_successful_update: Option<DateTime<Utc>>,
    pub last_attempt: Option<DateTime<Utc>>,
    pub last_error: Option<Unavailable>,
    pub health: Option<HealthRecord>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CacheFile {
    pub schema_version: u32,
    pub accounts: BTreeMap<String, CacheEntry>,
}

impl Default for CacheFile {
    fn default() -> Self {
        Self {
            schema_version: CACHE_SCHEMA,
            accounts: BTreeMap::new(),
        }
    }
}

impl CacheFile {
    /// Drop values that cannot be real (so a damaged file cannot show them).
    fn sanitize(&mut self) {
        for entry in self.accounts.values_mut() {
            if let Some(q) = &mut entry.quota {
                for g in &mut q.groups {
                    g.group = crate::core::format::clean_name(&g.group);
                    g.model = g.model.as_deref().map(crate::core::format::clean_name);
                    for w in [&mut g.five_hour, &mut g.weekly] {
                        let bad = w.as_ref().is_some_and(|w| {
                            !w.remaining_percent.is_finite()
                                || !(0.0..=100.0).contains(&w.remaining_percent)
                        });
                        if bad {
                            *w = None;
                        }
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct QuotaCache {
    file: PathBuf,
    lock: PathBuf,
}

impl QuotaCache {
    pub fn new(dirs: &Dirs) -> Self {
        Self {
            file: dirs.quota_cache_file(),
            lock: dirs.cache.join("quota.lock"),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.file
    }

    pub fn load(&self) -> CacheFile {
        let bytes = match fsutil::read_optional(&self.file) {
            Ok(Some(b)) if b.len() <= MAX_CACHE_BYTES => b,
            Ok(Some(_)) => {
                tracing::warn!("quota cache is oversized; ignoring it");
                return CacheFile::default();
            }
            Ok(None) => return CacheFile::default(),
            Err(e) => {
                tracing::warn!("cannot read the quota cache: {e}");
                return CacheFile::default();
            }
        };
        match serde_json::from_slice::<CacheFile>(&bytes) {
            Ok(mut f) if f.schema_version == CACHE_SCHEMA => {
                f.sanitize();
                f
            }
            _ => {
                tracing::warn!("quota cache is unreadable or from another version; ignoring it");
                CacheFile::default()
            }
        }
    }

    /// Read-modify-write under the cache lock.
    pub fn update(&self, change: impl FnOnce(&mut CacheFile)) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            fsutil::create_private_dir(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        let _guard = StateLock::acquire(&self.lock, Duration::from_secs(3))?;
        let mut file = self.load();
        change(&mut file);
        let mut bytes =
            serde_json::to_vec_pretty(&file).map_err(|e| Error::format(e.to_string()))?;
        bytes.push(b'\n');
        fsutil::atomic_write(&self.file, &bytes)
            .map_err(|e| Error::io(format!("writing {}", self.file.display()), e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_or_foreign_files_read_as_empty_and_get_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let cache = QuotaCache::new(&Dirs::under(dir.path()));
        assert!(cache.load().accounts.is_empty());
        std::fs::create_dir_all(dir.path().join("cache")).unwrap();
        for junk in ["{{{", r#"{"schema_version":99,"accounts":{}}"#, ""] {
            std::fs::write(cache.path(), junk).unwrap();
            assert!(cache.load().accounts.is_empty());
        }
        cache
            .update(|f| {
                f.accounts.insert("abc".into(), CacheEntry::default());
            })
            .unwrap();
        assert!(cache.load().accounts.contains_key("abc"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(cache.path())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }
}
