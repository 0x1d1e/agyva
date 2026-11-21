//! Resolution of agyva's own directories and of agy's native file locations.
//!
//! Every location can be redirected with an `AGYVA_*` environment variable so
//! development runs and tests never touch real user state.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use directories::{BaseDirs, ProjectDirs};

use crate::core::error::{Error, Result};

pub const ENV_CONFIG_DIR: &str = "AGYVA_CONFIG_DIR";
pub const ENV_STATE_DIR: &str = "AGYVA_STATE_DIR";
pub const ENV_CACHE_DIR: &str = "AGYVA_CACHE_DIR";
pub const ENV_AGY_HOME: &str = "AGYVA_AGY_HOME";

/// Snapshot of the environment variables agyva cares about. Passing this
/// around (instead of reading `std::env` ad hoc) keeps behavior testable.
#[derive(Debug, Clone, Default)]
pub struct Env(HashMap<String, String>);

impl Env {
    pub fn from_process() -> Self {
        Self(
            std::env::vars()
                .filter(|(k, _)| {
                    k.starts_with("AGYVA_") || k == "AGY_AUTH_STORAGE" || k == "NO_COLOR"
                })
                .collect(),
        )
    }

    pub fn from_pairs<'a>(pairs: impl IntoIterator<Item = (&'a str, &'a str)>) -> Self {
        Self(
            pairs
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect(),
        )
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.0
            .get(key)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }

    /// Remove and return a value (secrets leave the map once consumed).
    pub fn take(&mut self, key: &str) -> Option<String> {
        self.0.remove(key).filter(|v| !v.is_empty())
    }

    pub fn set(&mut self, key: &str, value: &str) {
        self.0.insert(key.to_owned(), value.to_owned());
    }
}

/// agyva's own directories.
#[derive(Debug, Clone)]
pub struct Dirs {
    pub config: PathBuf,
    pub state: PathBuf,
    pub cache: PathBuf,
}

impl Dirs {
    pub fn resolve(env: &Env) -> Result<Self> {
        let project = ProjectDirs::from("", "", "agyva");
        let pick = |var: &str, default: Option<PathBuf>| -> Result<PathBuf> {
            if let Some(v) = env.get(var) {
                return Ok(PathBuf::from(v));
            }
            default.ok_or_else(|| {
                Error::usage("cannot determine a home directory").hint(format!(
                    "set {ENV_CONFIG_DIR}, {ENV_STATE_DIR} and {ENV_CACHE_DIR}"
                ))
            })
        };
        Ok(Self {
            config: pick(
                ENV_CONFIG_DIR,
                project.as_ref().map(|p| p.config_dir().to_path_buf()),
            )?,
            state: pick(
                ENV_STATE_DIR,
                project.as_ref().map(|p| {
                    p.state_dir()
                        .unwrap_or_else(|| p.data_local_dir())
                        .to_path_buf()
                }),
            )?,
            cache: pick(
                ENV_CACHE_DIR,
                project.as_ref().map(|p| p.cache_dir().to_path_buf()),
            )?,
        })
    }

    /// Layout rooted at one directory (tests).
    pub fn under(root: &Path) -> Self {
        Self {
            config: root.join("config"),
            state: root.join("state"),
            cache: root.join("cache"),
        }
    }

    pub fn config_file(&self) -> PathBuf {
        self.config.join("config.toml")
    }
    pub fn registry_file(&self) -> PathBuf {
        self.config.join("registry.json")
    }
    pub fn vault_meta_file(&self) -> PathBuf {
        self.config.join("vault.json")
    }
    pub fn accounts_dir(&self) -> PathBuf {
        self.config.join("accounts")
    }
    pub fn snapshot_file(&self, id: &str) -> PathBuf {
        self.accounts_dir().join(format!("{id}.snap"))
    }
    pub fn lock_file(&self) -> PathBuf {
        self.state.join("agyva.lock")
    }
    /// Held while agy's `/usage` runs, and for the whole of a switch,
    /// recovery or add, so none of them overlap (see `App::lock_login`).
    pub fn refresh_lock_file(&self) -> PathBuf {
        self.state.join("refresh.lock")
    }
    pub fn journal_dir(&self) -> PathBuf {
        self.state.join("journal")
    }
    pub fn checkpoint_dir(&self) -> PathBuf {
        self.state.join("checkpoints")
    }
    pub fn quota_cache_file(&self) -> PathBuf {
        self.cache.join("quota.json")
    }
    pub fn update_check_cache_file(&self) -> PathBuf {
        self.cache.join("update-check.json")
    }
}

/// Locations of agy's native companion files.
#[derive(Debug, Clone)]
pub struct AgyPaths {
    pub home: PathBuf,
}

impl AgyPaths {
    pub fn resolve(env: &Env) -> Result<Self> {
        if let Some(v) = env.get(ENV_AGY_HOME) {
            return Ok(Self {
                home: PathBuf::from(v),
            });
        }
        let base = BaseDirs::new().ok_or_else(|| {
            Error::usage("cannot determine the home directory")
                .hint(format!("set {ENV_AGY_HOME} to the agy data directory"))
        })?;
        Ok(Self {
            home: base.home_dir().join(".gemini").join("antigravity-cli"),
        })
    }

    pub fn token_file(&self) -> PathBuf {
        self.home.join("antigravity-oauth-token")
    }
    pub fn installation_id(&self) -> PathBuf {
        self.home.join("installation_id")
    }
    pub fn settings(&self) -> PathBuf {
        self.home.join("settings.json")
    }
    pub fn presence_dir(&self) -> PathBuf {
        self.home.join("presence")
    }
}
