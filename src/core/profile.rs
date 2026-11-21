//! Project and global profiles (`.agyva.toml` and `<config>/profiles.toml`).
//!
//! Profiles carry non-sensitive workflow defaults only (account/endpoint
//! selectors, routing group, model, alert thresholds, monitor interval, and
//! non-secret environment variables). Secrets and command execution are
//! strictly rejected.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::error::{Error, ErrorKind, Result};
use crate::core::monitor::parse_interval;
use crate::core::selector::validate_alias;
use crate::paths::Dirs;
use crate::storage::fsutil;

pub const PROFILE_FILE_NAME: &str = ".agyva.toml";
pub const PROFILES_SCHEMA: u32 = 1;
pub const MAX_DISCOVERY_DEPTH: usize = 32;
const MAX_PROFILE_BYTES: usize = 256 * 1024;
const LOCAL_FILE_MODE: u32 = 0o644;

const FORBIDDEN_ENV_SUBSTRINGS: &[&str] = &["API_KEY", "SECRET", "TOKEN", "PASSWORD", "PASSPHRASE"];

pub fn profiles_file(dirs: &Dirs) -> PathBuf {
    dirs.config.join("profiles.toml")
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProfileSpec {
    pub name: Option<String>,
    pub description: Option<String>,
    pub account: Option<String>,
    pub endpoint: Option<String>,
    pub group: Option<String>,
    pub model: Option<String>,
    pub warn_percent: Option<f64>,
    pub critical_percent: Option<f64>,
    pub monitor_interval: Option<String>,
    pub env: BTreeMap<String, String>,
}

impl ProfileSpec {
    pub fn validate(&self) -> Result<()> {
        for (field, val) in [
            ("name", &self.name),
            ("description", &self.description),
            ("account", &self.account),
            ("endpoint", &self.endpoint),
            ("group", &self.group),
            ("model", &self.model),
            ("monitor_interval", &self.monitor_interval),
        ] {
            if let Some(s) = val {
                if s.trim().is_empty() {
                    return Err(Error::usage(format!("profile `{field}` cannot be empty")));
                }
                if s.contains('\0') {
                    return Err(Error::usage(format!(
                        "profile `{field}` contains invalid NUL byte"
                    )));
                }
            }
        }
        if let Some(interval) = self.monitor_interval.as_deref() {
            parse_interval(interval)?;
        }
        if let Some(w) = self.warn_percent
            && (!w.is_finite() || !(0.0..=100.0).contains(&w))
        {
            return Err(Error::usage(
                "profile `warn_percent` must be finite and between 0 and 100",
            ));
        }
        if let Some(c) = self.critical_percent
            && (!c.is_finite() || !(0.0..=100.0).contains(&c))
        {
            return Err(Error::usage(
                "profile `critical_percent` must be finite and between 0 and 100",
            ));
        }
        if let (Some(w), Some(c)) = (self.warn_percent, self.critical_percent)
            && c > w
        {
            return Err(Error::usage(
                "profile `critical_percent` must be less than or equal to `warn_percent`",
            ));
        }
        for (k, v) in &self.env {
            validate_env_entry(k, v)?;
        }
        Ok(())
    }
}

fn validate_env_entry(key: &str, value: &str) -> Result<()> {
    if key.is_empty()
        || key.contains('=')
        || key.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::usage(format!(
            "invalid profile env key `{key}`: must be non-empty without whitespace, `=`, or control characters"
        )));
    }
    let upper = key.to_ascii_uppercase();
    if FORBIDDEN_ENV_SUBSTRINGS.iter().any(|s| upper.contains(s)) {
        return Err(Error::refused(format!(
            "profile env key `{key}` looks like a secret; profiles must not store secrets"
        ))
        .hint("store credentials in encrypted storage via `agyva endpoint add` or the vault"));
    }
    if value.contains('\0') {
        return Err(Error::usage(format!(
            "profile env `{key}` value contains an invalid NUL byte"
        )));
    }
    Ok(())
}

pub fn validate_profile_name(name: &str) -> Result<()> {
    validate_alias(name).map_err(|_| {
        Error::usage(format!("invalid profile name `{name}`")).hint(
            "use 1-32 lowercase letters, digits, `.`, `_` or `-`, starting with a letter or digit",
        )
    })
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GlobalProfiles {
    pub schema_version: u32,
    pub default: Option<String>,
    pub profiles: BTreeMap<String, ProfileSpec>,
}

impl Default for GlobalProfiles {
    fn default() -> Self {
        Self {
            schema_version: PROFILES_SCHEMA,
            default: None,
            profiles: BTreeMap::new(),
        }
    }
}

impl GlobalProfiles {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != PROFILES_SCHEMA {
            return Err(Error::usage(format!(
                "unsupported profiles.toml schema_version {} (expected {PROFILES_SCHEMA})",
                self.schema_version
            )));
        }
        for (name, spec) in &self.profiles {
            validate_profile_name(name)?;
            spec.validate()?;
        }
        if let Some(def) = self.default.as_deref() {
            if def.trim().is_empty() {
                return Err(Error::usage("profiles.toml `default` cannot be empty"));
            }
            if !self.profiles.contains_key(def) {
                return Err(Error::usage(format!(
                    "profiles.toml `default` refers to unknown profile `{def}`"
                )));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    Local,
    Global,
    Explicit,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalProfile {
    pub path: PathBuf,
    pub spec: ProfileSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResolvedProfile {
    pub name: Option<String>,
    pub source: ProfileSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    pub spec: ProfileSpec,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProfileSummary {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub source: ProfileSource,
    pub is_default: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub critical_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monitor_interval: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl ProfileSummary {
    pub fn from_spec(
        name: String,
        source: ProfileSource,
        is_default: bool,
        path: Option<PathBuf>,
        spec: &ProfileSpec,
    ) -> Self {
        Self {
            name,
            description: spec.description.clone(),
            source,
            is_default,
            path,
            account: spec.account.clone(),
            endpoint: spec.endpoint.clone(),
            group: spec.group.clone(),
            model: spec.model.clone(),
            warn_percent: spec.warn_percent,
            critical_percent: spec.critical_percent,
            monitor_interval: spec.monitor_interval.clone(),
            env: spec.env.clone(),
        }
    }
}

pub fn load_local_profile(path: &Path) -> Result<ProfileSpec> {
    let bytes =
        std::fs::read(path).map_err(|e| Error::io(format!("reading {}", path.display()), e))?;
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err(Error::usage(format!("{} exceeds 256 KiB", path.display())));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::format(format!("{} must be valid UTF-8", path.display())))?;
    let spec: ProfileSpec = toml::from_str(text)
        .map_err(|e| Error::format(format!("invalid profile {}: {e}", path.display())))?;
    spec.validate()?;
    Ok(spec)
}

/// Walk up from `cwd` looking for `.agyva.toml`, stopping after a directory
/// containing `.git`, at filesystem root, or after [`MAX_DISCOVERY_DEPTH`] levels.
pub fn discover_local_profile(cwd: &Path) -> Result<Option<LocalProfile>> {
    let mut cur = Some(cwd);
    for _ in 0..MAX_DISCOVERY_DEPTH {
        let Some(dir) = cur else {
            break;
        };
        let candidate = dir.join(PROFILE_FILE_NAME);
        if candidate.is_file() {
            let spec = load_local_profile(&candidate)?;
            return Ok(Some(LocalProfile {
                path: candidate,
                spec,
            }));
        }
        if dir.join(".git").exists() {
            break;
        }
        cur = dir.parent();
    }
    Ok(None)
}

pub fn load_global_profiles(path: &Path) -> Result<GlobalProfiles> {
    let Some(bytes) = fsutil::read_optional(path)
        .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
    else {
        return Ok(GlobalProfiles::default());
    };
    if bytes.len() > MAX_PROFILE_BYTES {
        return Err(Error::usage(format!("{} exceeds 256 KiB", path.display())));
    }
    let text = std::str::from_utf8(&bytes)
        .map_err(|_| Error::format(format!("{} must be valid UTF-8", path.display())))?;
    let file: GlobalProfiles = toml::from_str(text)
        .map_err(|e| Error::format(format!("invalid profiles file {}: {e}", path.display())))?;
    file.validate()?;
    Ok(file)
}

pub fn save_global_profiles(path: &Path, file: &GlobalProfiles) -> Result<()> {
    file.validate()?;
    if let Some(dir) = path.parent() {
        fsutil::create_private_dir(dir)
            .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
    }
    let text = toml::to_string_pretty(file)
        .map_err(|e| Error::format(format!("cannot encode profiles.toml: {e}")))?;
    fsutil::atomic_write(path, text.as_bytes())
        .map_err(|e| Error::io(format!("writing {}", path.display()), e))
}

pub fn write_local_profile(dir: &Path, spec: &ProfileSpec) -> Result<PathBuf> {
    spec.validate()?;
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
    let path = dir.join(PROFILE_FILE_NAME);
    let text = toml::to_string_pretty(spec)
        .map_err(|e| Error::format(format!("cannot encode {PROFILE_FILE_NAME}: {e}")))?;
    fsutil::atomic_write_mode(&path, text.as_bytes(), LOCAL_FILE_MODE)
        .map_err(|e| Error::io(format!("writing {}", path.display()), e))?;
    Ok(path)
}

pub fn resolve_profile(
    dirs: &Dirs,
    cwd: &Path,
    explicit_name: Option<&str>,
) -> Result<Option<ResolvedProfile>> {
    resolve_profile_with_default(dirs, cwd, explicit_name, None)
}

pub fn resolve_profile_with_default(
    dirs: &Dirs,
    cwd: &Path,
    explicit_name: Option<&str>,
    config_default: Option<&str>,
) -> Result<Option<ResolvedProfile>> {
    let global_path = profiles_file(dirs);
    let global = load_global_profiles(&global_path)?;
    let local = discover_local_profile(cwd)?;

    if let Some(name) = explicit_name {
        let name = name.trim();
        if let Some(spec) = global.profiles.get(name) {
            return Ok(Some(ResolvedProfile {
                name: Some(name.to_owned()),
                source: ProfileSource::Explicit,
                path: Some(global_path),
                spec: spec.clone(),
            }));
        }
        if let Some(loc) = local
            && loc.spec.name.as_deref() == Some(name)
        {
            return Ok(Some(ResolvedProfile {
                name: Some(name.to_owned()),
                source: ProfileSource::Local,
                path: Some(loc.path),
                spec: loc.spec,
            }));
        }
        return Err(
            Error::new(ErrorKind::NotFound, format!("no profile named `{name}`"))
                .hint("run `agyva profile list` to see available profiles"),
        );
    }

    if let Some(loc) = local {
        return Ok(Some(ResolvedProfile {
            name: loc.spec.name.clone(),
            source: ProfileSource::Local,
            path: Some(loc.path),
            spec: loc.spec,
        }));
    }

    if let Some(def_name) = config_default.or(global.default.as_deref()) {
        if let Some(spec) = global.profiles.get(def_name) {
            return Ok(Some(ResolvedProfile {
                name: Some(def_name.to_owned()),
                source: ProfileSource::Global,
                path: Some(global_path),
                spec: spec.clone(),
            }));
        }
        if config_default.is_some() {
            return Err(Error::new(
                ErrorKind::NotFound,
                format!("configured default_profile `{def_name}` does not exist"),
            )
            .hint("create it with `agyva profile init` or update `default_profile`"));
        }
    }

    Ok(None)
}

/// Set or clear a single field on a [`ProfileSpec`], validating before committing.
/// Supported fields: `name`, `description`, `account`, `endpoint`, `group`,
/// `model`, `warn_percent`, `critical_percent`, `monitor_interval`, `env.<KEY>`.
pub fn set_profile_field(spec: &mut ProfileSpec, field: &str, value: Option<&str>) -> Result<()> {
    let mut next = spec.clone();
    let opt = value.map(str::to_owned);
    if let Some(env_key) = field.strip_prefix("env.") {
        match value {
            Some(v) => {
                validate_env_entry(env_key, v)?;
                next.env.insert(env_key.to_owned(), v.to_owned());
            }
            None => {
                next.env.remove(env_key);
            }
        }
    } else {
        match field {
            "name" => next.name = opt,
            "description" => next.description = opt,
            "account" => next.account = opt,
            "endpoint" => next.endpoint = opt,
            "group" => next.group = opt,
            "model" => next.model = opt,
            "monitor_interval" => next.monitor_interval = opt,
            "warn_percent" => {
                next.warn_percent = value
                    .map(|v| {
                        v.parse::<f64>()
                            .map_err(|_| Error::usage(format!("invalid warn_percent `{v}`")))
                    })
                    .transpose()?;
            }
            "critical_percent" => {
                next.critical_percent = value
                    .map(|v| {
                        v.parse::<f64>()
                            .map_err(|_| Error::usage(format!("invalid critical_percent `{v}`")))
                    })
                    .transpose()?;
            }
            _ => {
                return Err(Error::usage(format!("unknown profile field `{field}`")).hint(
                    "valid fields: name, description, account, endpoint, group, model, warn_percent, critical_percent, monitor_interval, env.<KEY>",
                ));
            }
        }
    }
    next.validate()?;
    *spec = next;
    Ok(())
}

pub fn list_profile_summaries(
    dirs: &Dirs,
    cwd: Option<&Path>,
    config_default: Option<&str>,
) -> Result<Vec<ProfileSummary>> {
    let global_path = profiles_file(dirs);
    let global = load_global_profiles(&global_path)?;
    let default_name = config_default.or(global.default.as_deref());
    let mut out: Vec<ProfileSummary> = global
        .profiles
        .iter()
        .map(|(name, spec)| {
            ProfileSummary::from_spec(
                name.clone(),
                ProfileSource::Global,
                default_name == Some(name.as_str()),
                Some(global_path.clone()),
                spec,
            )
        })
        .collect();
    if let Some(cwd) = cwd
        && let Some(local) = discover_local_profile(cwd)?
    {
        let label = local
            .spec
            .name
            .clone()
            .unwrap_or_else(|| "local".to_owned());
        out.push(ProfileSummary::from_spec(
            label,
            ProfileSource::Local,
            false,
            Some(local.path),
            &local.spec,
        ));
    }
    Ok(out)
}
