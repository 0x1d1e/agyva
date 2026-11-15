//! `config.toml`: validated, user-editable settings.

use std::path::Path;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::core::error::{Error, ErrorKind, Result};
use crate::storage::fsutil;

pub const KEYS: &[(&str, &str)] = &[
    (
        "backend",
        "native credential store: auto | secret-service | file",
    ),
    (
        "vault_key",
        "where the vault key lives: auto | keyring | passphrase",
    ),
    ("color", "terminal colors: auto | always | never"),
    (
        "allow_untested_agy",
        "accept agy versions outside the known-good range: true | false",
    ),
    (
        "checkpoints_keep",
        "number of pre-switch checkpoints to keep (1-20)",
    ),
    (
        "lock_timeout_secs",
        "seconds to wait for another agyva (0-600)",
    ),
    (
        "policy_acknowledged",
        "whether the one-time policy notice was shown: true | false",
    ),
    (
        "quota_fresh_secs",
        "how long a quota reading counts as fresh (10-86400)",
    ),
    (
        "quota_min_refresh_secs",
        "minimum seconds between quota refreshes (10-3600)",
    ),
    (
        "watch_interval_secs",
        "seconds between `agyva watch` checks (60-86400)",
    ),
    (
        "check_updates",
        "check GitHub Releases for newer agyva at startup: true | false",
    ),
    (
        "desktop_notifications",
        "let `agyva watch` send desktop notifications: true | false",
    ),
];

macro_rules! string_enum {
    ($(#[$m:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name {
            $( #[serde(rename = $text)] $variant, )+
        }
        impl $name {
            pub fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $text, )+ }
            }
        }
        impl FromStr for $name {
            type Err = String;
            fn from_str(s: &str) -> std::result::Result<Self, String> {
                match s { $( $text => Ok(Self::$variant), )+
                    other => Err(format!("`{other}` is not one of: {}", [$($text),+].join(", "))) }
            }
        }
    };
}

string_enum!(
    /// Which native store holds agy's live credential.
    BackendKind { Auto => "auto", SecretService => "secret-service", File => "file", Memory => "memory" }
);
string_enum!(
    /// Where the vault encryption key is kept.
    VaultKeyMode { Auto => "auto", Keyring => "keyring", Passphrase => "passphrase" }
);
string_enum!(
    ColorChoice { Auto => "auto", Always => "always", Never => "never" }
);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub backend: BackendKind,
    pub vault_key: VaultKeyMode,
    pub color: ColorChoice,
    pub allow_untested_agy: bool,
    pub checkpoints_keep: u8,
    pub check_updates: bool,
    pub lock_timeout_secs: u64,
    pub policy_acknowledged: bool,
    pub quota_fresh_secs: u64,
    pub quota_min_refresh_secs: u64,
    pub watch_interval_secs: u64,
    pub desktop_notifications: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend: BackendKind::Auto,
            vault_key: VaultKeyMode::Auto,
            color: ColorChoice::Auto,
            allow_untested_agy: false,
            checkpoints_keep: 5,
            check_updates: true,
            lock_timeout_secs: 10,
            policy_acknowledged: false,
            quota_fresh_secs: 300,
            quota_min_refresh_secs: 60,
            watch_interval_secs: 300,
            desktop_notifications: false,
        }
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let Some(bytes) = fsutil::read_optional(path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
        else {
            return Ok(Self::default());
        };
        let text = String::from_utf8(bytes)
            .map_err(|_| Error::format(format!("{} is not valid UTF-8", path.display())))?;
        let cfg: Self = toml::from_str(&text).map_err(|e| {
            Error::format(format!("invalid config file {}: {e}", path.display()))
                .hint("fix the file or delete it to restore defaults")
        })?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            fsutil::create_private_dir(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::format(format!("cannot encode config: {e}")))?;
        fsutil::atomic_write(path, text.as_bytes())
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))
    }

    fn validate(&self) -> Result<()> {
        if self.backend == BackendKind::Memory {
            return Err(
                Error::new(ErrorKind::Usage, "backend `memory` is test-only")
                    .hint("set AGYVA_BACKEND=memory in the environment instead"),
            );
        }
        if !(1..=20).contains(&self.checkpoints_keep) {
            return Err(Error::usage("checkpoints_keep must be between 1 and 20"));
        }
        if self.lock_timeout_secs > 600 {
            return Err(Error::usage("lock_timeout_secs must be between 0 and 600"));
        }
        if !(10..=86_400).contains(&self.quota_fresh_secs) {
            return Err(Error::usage(
                "quota_fresh_secs must be between 10 and 86400",
            ));
        }
        if !(10..=3_600).contains(&self.quota_min_refresh_secs) {
            return Err(Error::usage(
                "quota_min_refresh_secs must be between 10 and 3600",
            ));
        }
        if !(60..=86_400).contains(&self.watch_interval_secs) {
            return Err(Error::usage(
                "watch_interval_secs must be between 60 and 86400",
            ));
        }
        Ok(())
    }

    /// Current value of `key` rendered as text.
    pub fn get(&self, key: &str) -> Option<String> {
        Some(match key {
            "backend" => self.backend.as_str().into(),
            "vault_key" => self.vault_key.as_str().into(),
            "color" => self.color.as_str().into(),
            "allow_untested_agy" => self.allow_untested_agy.to_string(),
            "check_updates" => self.check_updates.to_string(),
            "lock_timeout_secs" => self.lock_timeout_secs.to_string(),
            "policy_acknowledged" => self.policy_acknowledged.to_string(),
            "quota_fresh_secs" => self.quota_fresh_secs.to_string(),
            "quota_min_refresh_secs" => self.quota_min_refresh_secs.to_string(),
            "watch_interval_secs" => self.watch_interval_secs.to_string(),
            "desktop_notifications" => self.desktop_notifications.to_string(),
            _ => return None,
        })
    }

    /// Set `key` to `value`, validating both.
    pub fn set(&mut self, key: &str, value: &str) -> Result<()> {
        let bad = |what: String| {
            Error::usage(format!("invalid value for `{key}`: {what}"))
                .hint("run `agyva config show` to see valid keys and values")
        };
        let mut next = self.clone();
        match key {
            // `memory` parses, but saving it would be rejected by `validate`.
            // Refuse it here so `config set` and `config show` agree.
            "backend" if value == "memory" => {
                return Err(
                    Error::new(ErrorKind::Usage, "backend `memory` is test-only")
                        .hint("set AGYVA_BACKEND=memory in the environment instead"),
                );
            }
            "backend" => next.backend = value.parse().map_err(bad)?,
            "vault_key" => next.vault_key = value.parse().map_err(bad)?,
            "color" => next.color = value.parse().map_err(bad)?,
            "allow_untested_agy" => next.allow_untested_agy = parse_bool(value).map_err(bad)?,
            "check_updates" => next.check_updates = parse_bool(value).map_err(bad)?,
            "desktop_notifications" => {
                next.desktop_notifications = parse_bool(value).map_err(bad)?
            }
            "quota_fresh_secs" => {
                next.quota_fresh_secs =
                    value.parse().map_err(|_| bad("expected a number".into()))?;
            }
            "quota_min_refresh_secs" => {
                next.quota_min_refresh_secs =
                    value.parse().map_err(|_| bad("expected a number".into()))?;
            }
            "watch_interval_secs" => {
                next.watch_interval_secs =
                    value.parse().map_err(|_| bad("expected a number".into()))?;
            }
            "checkpoints_keep" => {
                next.checkpoints_keep =
                    value.parse().map_err(|_| bad("expected a number".into()))?;
            }
            "lock_timeout_secs" => {
                next.lock_timeout_secs =
                    value.parse().map_err(|_| bad("expected a number".into()))?;
            }
            _ => {
                let known: Vec<&str> = KEYS.iter().map(|(k, _)| *k).collect();
                return Err(Error::usage(format!("unknown config key `{key}`"))
                    .hint(format!("valid keys: {}", known.join(", "))));
            }
        }
        next.validate().map_err(|e| bad(e.message().to_owned()))?;
        *self = next;
        Ok(())
    }
}

fn parse_bool(s: &str) -> std::result::Result<bool, String> {
    match s {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        other => Err(format!("`{other}` is not true or false")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_validates_keys_and_values() {
        let mut c = Config::default();
        c.set("color", "never").unwrap();
        assert_eq!(c.color, ColorChoice::Never);
        assert!(c.set("color", "purple").is_err());
        assert!(c.set("nope", "1").is_err());
        assert!(c.set("checkpoints_keep", "0").is_err());
        assert!(c.set("checkpoints_keep", "21").is_err());
        assert!(c.set("backend", "memory").is_err());
        assert_eq!(c.checkpoints_keep, 5, "failed sets must not mutate");
    }

    #[test]
    fn roundtrip_and_unknown_fields_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c/config.toml");
        let mut c = Config::default();
        c.set("allow_untested_agy", "true").unwrap();
        c.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap(), c);
        std::fs::write(&path, "bogus = 1\n").unwrap();
        assert!(Config::load(&path).is_err());
    }
}
