//! Custom LLM endpoint registry, encrypted secret snapshots, model cache,
//! environment variable export, and `agy` settings synchronization.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use zeroize::{Zeroize, Zeroizing};

use crate::core::error::{Error, ErrorKind, Result};
use crate::core::ids;
use crate::core::protocol::{
    self, AuthScheme, EndpointProtocol, FetchedModel, HttpTransport, ProbeResult, RateLimitInfo,
};
use crate::core::selector::validate_alias;
use crate::paths::{AgyPaths, Dirs};
use crate::storage::fsutil;
use crate::storage::lock::StateLock;
use crate::storage::vault::Vault;

pub const ENDPOINT_REGISTRY_SCHEMA: u32 = 1;
pub const ENDPOINT_SECRET_SCHEMA: u32 = 1;
pub const ENDPOINT_CACHE_SCHEMA: u32 = 1;

const MAX_CACHE_BYTES: usize = 4 * 1024 * 1024;
const LOCK_TIMEOUT: Duration = Duration::from_secs(5);

pub fn registry_file(dirs: &Dirs) -> PathBuf {
    dirs.config.join("endpoints.json")
}

pub fn secrets_dir(dirs: &Dirs) -> PathBuf {
    dirs.config.join("endpoints")
}

pub fn secret_file(dirs: &Dirs, id: &str) -> PathBuf {
    secrets_dir(dirs).join(format!("{id}.ep.snap"))
}

pub fn cache_file(dirs: &Dirs) -> PathBuf {
    dirs.cache.join("endpoint-models.json")
}

fn cache_lock_file(dirs: &Dirs) -> PathBuf {
    dirs.cache.join("endpoint-models.lock")
}

/// AAD context binding an encrypted endpoint snapshot to its endpoint id.
pub fn endpoint_secret_context(id: &str) -> String {
    format!("endpoint:{id}")
}

/// Public endpoint metadata stored in `endpoints.json` (never contains secrets).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointRecord {
    pub id: String,
    pub alias: String,
    pub base_url: String,
    pub protocol: EndpointProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected_protocol: Option<EndpointProtocol>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth_scheme: Option<AuthScheme>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chat_path: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra_headers: BTreeMap<String, String>,
    pub enabled: bool,
    pub added_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
}

impl EndpointRecord {
    pub fn effective_protocol(&self) -> EndpointProtocol {
        if self.protocol == EndpointProtocol::Auto {
            self.detected_protocol.unwrap_or(EndpointProtocol::OpenAi)
        } else {
            self.protocol
        }
    }

    pub fn effective_auth_scheme(&self) -> AuthScheme {
        self.auth_scheme
            .clone()
            .unwrap_or_else(|| self.effective_protocol().default_auth_scheme())
    }
}

/// Public endpoint registry persisted at `<config>/endpoints.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EndpointRegistry {
    pub schema_version: u32,
    pub active_endpoint: Option<String>,
    pub endpoints: Vec<EndpointRecord>,
}

impl Default for EndpointRegistry {
    fn default() -> Self {
        Self {
            schema_version: ENDPOINT_REGISTRY_SCHEMA,
            active_endpoint: None,
            endpoints: Vec::new(),
        }
    }
}

impl EndpointRegistry {
    pub fn load(path: &Path) -> Result<Self> {
        let Some(bytes) = fsutil::read_optional(path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
        else {
            return Ok(Self::default());
        };
        let reg: Self = serde_json::from_slice(&bytes).map_err(|e| {
            Error::format(format!("{} is corrupt: {e}", path.display())).hint(
                "restore endpoints.json from a backup; encrypted snapshots in endpoints/ are intact",
            )
        })?;
        if reg.schema_version != ENDPOINT_REGISTRY_SCHEMA {
            return Err(Error::format(format!(
                "endpoint registry schema {} is not supported (expected {ENDPOINT_REGISTRY_SCHEMA})",
                reg.schema_version
            ))
            .hint("upgrade agyva"));
        }
        reg.validate()?;
        Ok(reg)
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        self.validate()?;
        if let Some(dir) = path.parent() {
            fsutil::create_private_dir(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        let mut bytes =
            serde_json::to_vec_pretty(self).map_err(|e| Error::format(e.to_string()))?;
        bytes.push(b'\n');
        fsutil::atomic_write(path, &bytes)
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))
    }

    fn validate(&self) -> Result<()> {
        for id in self
            .endpoints
            .iter()
            .map(|e| e.id.as_str())
            .chain(self.active_endpoint.as_deref())
        {
            if !ids::is_account_id(id) {
                return Err(Error::format(
                    "endpoints.json holds a malformed endpoint id (expected 10 lowercase hex characters)",
                ));
            }
        }
        for (i, ep) in self.endpoints.iter().enumerate() {
            let dup = self.endpoints[..i]
                .iter()
                .any(|other| other.id == ep.id || other.alias.eq_ignore_ascii_case(&ep.alias));
            if dup {
                return Err(Error::format(
                    "endpoint registry contains duplicate ids or aliases",
                ));
            }
        }
        if let Some(active) = self.active_endpoint.as_deref()
            && self.find(active).is_none()
        {
            return Err(Error::format(
                "endpoint registry refers to an active endpoint that does not exist",
            ));
        }
        Ok(())
    }

    pub fn find(&self, id: &str) -> Option<&EndpointRecord> {
        self.endpoints.iter().find(|e| e.id == id)
    }

    pub fn find_mut(&mut self, id: &str) -> Option<&mut EndpointRecord> {
        self.endpoints.iter_mut().find(|e| e.id == id)
    }

    pub fn alias_taken(&self, alias: &str, except_id: Option<&str>) -> bool {
        self.endpoints
            .iter()
            .any(|e| e.alias.eq_ignore_ascii_case(alias) && Some(e.id.as_str()) != except_id)
    }
}

/// Encrypted per-endpoint credentials held in memory and sealed via [`Vault`].
#[derive(Clone, Default)]
pub struct EndpointSecret {
    pub api_key: Zeroizing<String>,
    pub secret_headers: BTreeMap<String, String>,
}

impl EndpointSecret {
    pub fn new(api_key: impl Into<String>, secret_headers: BTreeMap<String, String>) -> Self {
        Self {
            api_key: Zeroizing::new(api_key.into()),
            secret_headers,
        }
    }
}

impl Drop for EndpointSecret {
    fn drop(&mut self) {
        self.api_key.zeroize();
        protocol::zeroize_string_map(&mut self.secret_headers);
    }
}

impl fmt::Debug for EndpointSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointSecret")
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("secret_header_count", &self.secret_headers.len())
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
struct StoredEndpointSecret {
    schema_version: u32,
    api_key: String,
    #[serde(default)]
    secret_headers: BTreeMap<String, String>,
}

impl Drop for StoredEndpointSecret {
    fn drop(&mut self) {
        self.api_key.zeroize();
        protocol::zeroize_string_map(&mut self.secret_headers);
    }
}

pub fn write_endpoint_secret(
    vault: &Vault,
    path: &Path,
    id: &str,
    secret: &EndpointSecret,
) -> Result<()> {
    ids::require_account_id(id)?;
    let stored = StoredEndpointSecret {
        schema_version: ENDPOINT_SECRET_SCHEMA,
        api_key: secret.api_key.as_str().to_owned(),
        secret_headers: secret.secret_headers.clone(),
    };
    let plain = Zeroizing::new(
        serde_json::to_vec(&stored)
            .map_err(|_| Error::format("cannot encode endpoint secret snapshot"))?,
    );
    vault.write_file(path, &endpoint_secret_context(id), &plain)
}

pub fn read_endpoint_secret(vault: &Vault, path: &Path, id: &str) -> Result<EndpointSecret> {
    ids::require_account_id(id)?;
    let plain = vault
        .read_file(path, &endpoint_secret_context(id))?
        .ok_or_else(|| {
            Error::format(format!("the saved secret for endpoint {id} is missing"))
                .hint("re-add or update the endpoint API key")
        })?;
    let mut stored: StoredEndpointSecret = serde_json::from_slice(&plain)
        .map_err(|_| Error::format(format!("the saved secret for endpoint {id} is malformed")))?;
    if stored.schema_version != ENDPOINT_SECRET_SCHEMA {
        return Err(Error::format(format!(
            "endpoint secret schema {} is not supported",
            stored.schema_version
        ))
        .hint("upgrade agyva"));
    }
    let api_key = Zeroizing::new(std::mem::take(&mut stored.api_key));
    let secret_headers = std::mem::take(&mut stored.secret_headers);
    Ok(EndpointSecret {
        api_key,
        secret_headers,
    })
}

/// Per-endpoint cached discovery and probe data (`<cache>/endpoint-models.json`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EndpointCacheEntry {
    pub models: Vec<FetchedModel>,
    pub fetched_at: Option<DateTime<Utc>>,
    pub latency_ms: Option<u64>,
    pub rate_limits: Option<RateLimitInfo>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EndpointCacheFile {
    pub schema_version: u32,
    pub endpoints: BTreeMap<String, EndpointCacheEntry>,
}

impl Default for EndpointCacheFile {
    fn default() -> Self {
        Self {
            schema_version: ENDPOINT_CACHE_SCHEMA,
            endpoints: BTreeMap::new(),
        }
    }
}

/// Lenient cache manager for endpoint model lists and probe state.
#[derive(Debug, Clone)]
pub struct EndpointCache {
    file: PathBuf,
    lock: PathBuf,
}

impl EndpointCache {
    pub fn new(dirs: &Dirs) -> Self {
        Self {
            file: cache_file(dirs),
            lock: cache_lock_file(dirs),
        }
    }

    pub fn path(&self) -> &Path {
        &self.file
    }

    pub fn load(&self) -> EndpointCacheFile {
        let bytes = match fsutil::read_optional(&self.file) {
            Ok(Some(b)) if b.len() <= MAX_CACHE_BYTES => b,
            Ok(Some(_)) => {
                tracing::warn!("endpoint model cache is oversized; ignoring it");
                return EndpointCacheFile::default();
            }
            Ok(None) => return EndpointCacheFile::default(),
            Err(e) => {
                tracing::warn!("cannot read endpoint model cache: {e}");
                return EndpointCacheFile::default();
            }
        };
        match serde_json::from_slice::<EndpointCacheFile>(&bytes) {
            Ok(file) if file.schema_version == ENDPOINT_CACHE_SCHEMA => file,
            _ => {
                tracing::warn!("endpoint model cache is unreadable or foreign; ignoring it");
                EndpointCacheFile::default()
            }
        }
    }

    pub fn update(&self, change: impl FnOnce(&mut EndpointCacheFile)) -> Result<()> {
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

/// Specification for creating a new custom endpoint.
#[derive(Clone)]
pub struct NewEndpoint {
    pub alias: String,
    pub base_url: String,
    pub protocol: EndpointProtocol,
    pub auth_scheme: Option<AuthScheme>,
    pub api_key: Zeroizing<String>,
    pub default_model: Option<String>,
    pub api_version: Option<String>,
    pub models_path: Option<String>,
    pub chat_path: Option<String>,
    pub extra_headers: BTreeMap<String, String>,
    pub secret_headers: BTreeMap<String, String>,
    pub enabled: bool,
}

impl Default for NewEndpoint {
    fn default() -> Self {
        Self {
            alias: String::new(),
            base_url: String::new(),
            protocol: EndpointProtocol::Auto,
            auth_scheme: None,
            api_key: Zeroizing::new(String::new()),
            default_model: None,
            api_version: None,
            models_path: None,
            chat_path: None,
            extra_headers: BTreeMap::new(),
            secret_headers: BTreeMap::new(),
            enabled: true,
        }
    }
}

impl Drop for NewEndpoint {
    fn drop(&mut self) {
        self.api_key.zeroize();
        protocol::zeroize_string_map(&mut self.secret_headers);
    }
}

impl fmt::Debug for NewEndpoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NewEndpoint")
            .field("alias", &self.alias)
            .field("base_url", &self.base_url)
            .field("protocol", &self.protocol)
            .field("auth_scheme", &self.auth_scheme)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("secret_header_count", &self.secret_headers.len())
            .field("enabled", &self.enabled)
            .finish()
    }
}

/// Partial update patch for an existing endpoint.
#[derive(Clone, Default)]
pub struct EndpointPatch {
    pub base_url: Option<String>,
    pub protocol: Option<EndpointProtocol>,
    pub auth_scheme: Option<Option<AuthScheme>>,
    pub api_key: Option<Zeroizing<String>>,
    pub default_model: Option<Option<String>>,
    pub api_version: Option<Option<String>>,
    pub models_path: Option<Option<String>>,
    pub chat_path: Option<Option<String>>,
    pub extra_headers: Option<BTreeMap<String, String>>,
    pub secret_headers: Option<BTreeMap<String, String>>,
    pub enabled: Option<bool>,
}

impl Drop for EndpointPatch {
    fn drop(&mut self) {
        if let Some(k) = &mut self.api_key {
            k.zeroize();
        }
        if let Some(m) = &mut self.secret_headers {
            protocol::zeroize_string_map(m);
        }
    }
}

impl fmt::Debug for EndpointPatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EndpointPatch")
            .field("base_url", &self.base_url)
            .field("protocol", &self.protocol)
            .field("auth_scheme", &self.auth_scheme)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field(
                "secret_headers",
                &self.secret_headers.as_ref().map(|m| m.len()),
            )
            .field("enabled", &self.enabled)
            .finish()
    }
}

fn generate_endpoint_id(reg: &EndpointRegistry) -> Result<String> {
    for _ in 0..32 {
        let mut bytes = [0u8; 5];
        getrandom::fill(&mut bytes)
            .map_err(|e| Error::vault(format!("system randomness unavailable: {e}")))?;
        let id = hex::encode(bytes);
        if ids::is_account_id(&id) && reg.find(&id).is_none() {
            return Ok(id);
        }
    }
    Err(Error::backend("could not allocate a unique endpoint id"))
}

fn normalize_opt(s: Option<String>) -> Option<String> {
    s.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty())
}

pub fn load_registry(dirs: &Dirs) -> Result<EndpointRegistry> {
    EndpointRegistry::load(&registry_file(dirs))
}

pub fn list_endpoints(dirs: &Dirs) -> Result<Vec<EndpointRecord>> {
    Ok(load_registry(dirs)?.endpoints)
}

pub fn load_secret(dirs: &Dirs, vault: &Vault, id: &str) -> Result<EndpointSecret> {
    read_endpoint_secret(vault, &secret_file(dirs, id), id)
}

/// Resolve an endpoint by alias (case-insensitive), 1-based list index,
/// exact id, or unique id prefix.
pub fn resolve_endpoint<'a>(
    reg: &'a EndpointRegistry,
    selector: &str,
) -> Result<&'a EndpointRecord> {
    let input = selector.trim();
    if input.is_empty() {
        return Err(Error::usage("endpoint selector cannot be empty"));
    }

    let alias_hits: Vec<usize> = reg
        .endpoints
        .iter()
        .enumerate()
        .filter(|(_, e)| e.alias.eq_ignore_ascii_case(input))
        .map(|(i, _)| i)
        .collect();
    if let [idx] = alias_hits.as_slice() {
        return Ok(&reg.endpoints[*idx]);
    }

    let numeric = input.bytes().all(|b| b.is_ascii_digit());
    if numeric
        && let Some(ep) = input
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|i| reg.endpoints.get(i))
    {
        return Ok(ep);
    }

    let lower = input.to_ascii_lowercase();
    if let Some(ep) = reg.endpoints.iter().find(|e| e.id == lower) {
        return Ok(ep);
    }

    let prefix_hits: Vec<usize> = reg
        .endpoints
        .iter()
        .enumerate()
        .filter(|(_, e)| e.id.starts_with(&lower))
        .map(|(i, _)| i)
        .collect();
    match prefix_hits.as_slice() {
        [idx] => Ok(&reg.endpoints[*idx]),
        [] => {
            if numeric {
                Err(
                    Error::new(ErrorKind::NotFound, format!("no endpoint number {input}")).hint(
                        format!("there are {} saved endpoint(s)", reg.endpoints.len()),
                    ),
                )
            } else {
                Err(Error::new(
                    ErrorKind::NotFound,
                    format!("no saved endpoint matches `{input}`"),
                ))
            }
        }
        many => {
            let candidates: Vec<String> = many
                .iter()
                .map(|&i| {
                    format!(
                        "  {}. {} [{}]",
                        i + 1,
                        reg.endpoints[i].alias,
                        reg.endpoints[i].id
                    )
                })
                .collect();
            Err(Error::new(
                ErrorKind::Ambiguous,
                format!(
                    "`{input}` matches several endpoints:\n{}",
                    candidates.join("\n")
                ),
            )
            .hint("use the alias, list number, or a longer id"))
        }
    }
}

pub fn add_endpoint(dirs: &Dirs, vault: &Vault, spec: NewEndpoint) -> Result<EndpointRecord> {
    validate_alias(&spec.alias)?;
    protocol::validate_endpoint_url(&spec.base_url).map_err(Error::refused)?;

    let _guard = StateLock::acquire(&dirs.lock_file(), LOCK_TIMEOUT)?;
    let reg_path = registry_file(dirs);
    let mut reg = EndpointRegistry::load(&reg_path)?;
    if reg.alias_taken(&spec.alias, None) {
        return Err(Error::conflict(format!(
            "an endpoint named `{}` already exists",
            spec.alias
        )));
    }

    let id = generate_endpoint_id(&reg)?;
    let now = Utc::now();
    let record = EndpointRecord {
        id: id.clone(),
        alias: spec.alias.clone(),
        base_url: spec.base_url.trim().trim_end_matches('/').to_owned(),
        protocol: spec.protocol,
        detected_protocol: (spec.protocol != EndpointProtocol::Auto).then_some(spec.protocol),
        auth_scheme: spec.auth_scheme.clone(),
        default_model: normalize_opt(spec.default_model.clone()),
        api_version: normalize_opt(spec.api_version.clone()),
        models_path: normalize_opt(spec.models_path.clone()),
        chat_path: normalize_opt(spec.chat_path.clone()),
        extra_headers: spec.extra_headers.clone(),
        enabled: spec.enabled,
        added_at: now,
        updated_at: now,
        last_used_at: None,
    };

    let secret = EndpointSecret {
        api_key: spec.api_key.clone(),
        secret_headers: spec.secret_headers.clone(),
    };
    let snap_path = secret_file(dirs, &id);
    write_endpoint_secret(vault, &snap_path, &id, &secret)?;

    reg.endpoints.push(record.clone());
    if let Err(err) = reg.save(&reg_path) {
        let _ = fsutil::remove_durable(&snap_path);
        return Err(err);
    }
    Ok(record)
}

pub fn remove_endpoint(dirs: &Dirs, selector: &str) -> Result<EndpointRecord> {
    let _guard = StateLock::acquire(&dirs.lock_file(), LOCK_TIMEOUT)?;
    let reg_path = registry_file(dirs);
    let mut reg = EndpointRegistry::load(&reg_path)?;
    let target = resolve_endpoint(&reg, selector)?.clone();
    reg.endpoints.retain(|e| e.id != target.id);
    if reg.active_endpoint.as_deref() == Some(target.id.as_str()) {
        reg.active_endpoint = None;
    }
    reg.save(&reg_path)?;
    let _ = fsutil::remove_durable(&secret_file(dirs, &target.id));
    let _ = EndpointCache::new(dirs).update(|cache| {
        cache.endpoints.remove(&target.id);
    });
    Ok(target)
}

pub fn rename_endpoint(dirs: &Dirs, selector: &str, new_alias: &str) -> Result<EndpointRecord> {
    validate_alias(new_alias)?;
    let _guard = StateLock::acquire(&dirs.lock_file(), LOCK_TIMEOUT)?;
    let reg_path = registry_file(dirs);
    let mut reg = EndpointRegistry::load(&reg_path)?;
    let id = resolve_endpoint(&reg, selector)?.id.clone();
    if reg.alias_taken(new_alias, Some(&id)) {
        return Err(Error::conflict(format!(
            "an endpoint named `{new_alias}` already exists"
        )));
    }
    let rec = reg
        .find_mut(&id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "endpoint disappeared"))?;
    rec.alias = new_alias.to_owned();
    rec.updated_at = Utc::now();
    let updated = rec.clone();
    reg.save(&reg_path)?;
    Ok(updated)
}

pub fn update_endpoint(
    dirs: &Dirs,
    vault: &Vault,
    selector: &str,
    patch: EndpointPatch,
) -> Result<EndpointRecord> {
    if let Some(url) = patch.base_url.as_deref() {
        protocol::validate_endpoint_url(url).map_err(Error::refused)?;
    }
    let _guard = StateLock::acquire(&dirs.lock_file(), LOCK_TIMEOUT)?;
    let reg_path = registry_file(dirs);
    let mut reg = EndpointRegistry::load(&reg_path)?;
    let id = resolve_endpoint(&reg, selector)?.id.clone();

    if patch.api_key.is_some() || patch.secret_headers.is_some() {
        let snap_path = secret_file(dirs, &id);
        let mut secret = read_endpoint_secret(vault, &snap_path, &id).unwrap_or_default();
        if let Some(k) = patch.api_key.as_ref() {
            secret.api_key = k.clone();
        }
        if let Some(h) = patch.secret_headers.as_ref() {
            secret.secret_headers = h.clone();
        }
        write_endpoint_secret(vault, &snap_path, &id, &secret)?;
    }

    let rec = reg
        .find_mut(&id)
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "endpoint disappeared"))?;
    if let Some(url) = patch.base_url.as_deref() {
        rec.base_url = url.trim().trim_end_matches('/').to_owned();
    }
    if let Some(proto) = patch.protocol {
        rec.protocol = proto;
        if proto != EndpointProtocol::Auto {
            rec.detected_protocol = Some(proto);
        }
    }
    if let Some(scheme) = patch.auth_scheme.clone() {
        rec.auth_scheme = scheme;
    }
    if let Some(model) = patch.default_model.clone() {
        rec.default_model = normalize_opt(model);
    }
    if let Some(ver) = patch.api_version.clone() {
        rec.api_version = normalize_opt(ver);
    }
    if let Some(mp) = patch.models_path.clone() {
        rec.models_path = normalize_opt(mp);
    }
    if let Some(cp) = patch.chat_path.clone() {
        rec.chat_path = normalize_opt(cp);
    }
    if let Some(headers) = patch.extra_headers.clone() {
        rec.extra_headers = headers;
    }
    if let Some(enabled) = patch.enabled {
        rec.enabled = enabled;
    }
    rec.updated_at = Utc::now();
    let updated = rec.clone();
    reg.save(&reg_path)?;
    Ok(updated)
}

pub fn set_active_endpoint(dirs: &Dirs, selector: &str) -> Result<EndpointRecord> {
    let _guard = StateLock::acquire(&dirs.lock_file(), LOCK_TIMEOUT)?;
    let reg_path = registry_file(dirs);
    let mut reg = EndpointRegistry::load(&reg_path)?;
    let target = resolve_endpoint(&reg, selector)?.clone();
    if !target.enabled {
        return Err(Error::refused(format!(
            "endpoint `{}` is disabled",
            target.alias
        )));
    }
    let now = Utc::now();
    reg.active_endpoint = Some(target.id.clone());
    if let Some(rec) = reg.find_mut(&target.id) {
        rec.last_used_at = Some(now);
    }
    let updated = reg.find(&target.id).cloned().unwrap_or(target);
    reg.save(&reg_path)?;
    Ok(updated)
}

pub fn clear_active_endpoint(dirs: &Dirs) -> Result<()> {
    let _guard = StateLock::acquire(&dirs.lock_file(), LOCK_TIMEOUT)?;
    let reg_path = registry_file(dirs);
    let mut reg = EndpointRegistry::load(&reg_path)?;
    if reg.active_endpoint.is_some() {
        reg.active_endpoint = None;
        reg.save(&reg_path)?;
    }
    Ok(())
}

fn merged_headers(record: &EndpointRecord, secret: &EndpointSecret) -> BTreeMap<String, String> {
    let mut headers = record.extra_headers.clone();
    for (k, v) in &secret.secret_headers {
        headers.insert(k.clone(), v.clone());
    }
    headers
}

fn redact_probe_secrets(probe: &mut ProbeResult, secret: &EndpointSecret) {
    if let Some(err) = probe.error.as_mut() {
        let mut secrets: Vec<&str> = Vec::new();
        if !secret.api_key.is_empty() {
            secrets.push(secret.api_key.as_str());
        }
        for v in secret.secret_headers.values() {
            if !v.is_empty() {
                secrets.push(v.as_str());
            }
        }
        *err = protocol::sanitize_error_message(err, &secrets);
    }
}

/// Probe an endpoint's connectivity and models without mutating the registry or cache.
pub fn test_endpoint_connection(
    dirs: &Dirs,
    vault: &Vault,
    transport: &dyn HttpTransport,
    selector: &str,
) -> Result<ProbeResult> {
    let reg = load_registry(dirs)?;
    let record = resolve_endpoint(&reg, selector)?.clone();
    let secret = load_secret(dirs, vault, &record.id)?;
    let headers = merged_headers(&record, &secret);
    let mut probe = protocol::test_endpoint(
        transport,
        &record.base_url,
        record.protocol,
        record.auth_scheme.as_ref(),
        secret.api_key.as_str(),
        record.api_version.as_deref(),
        record.models_path.as_deref(),
        &headers,
    );
    redact_probe_secrets(&mut probe, &secret);
    Ok(probe)
}

/// Fetch models for an endpoint, updating `endpoint-models.json` and storing
/// `detected_protocol` in `endpoints.json`.
pub fn refresh_endpoint_models(
    dirs: &Dirs,
    vault: &Vault,
    transport: &dyn HttpTransport,
    selector: &str,
) -> Result<ProbeResult> {
    let reg_snapshot = load_registry(dirs)?;
    let record = resolve_endpoint(&reg_snapshot, selector)?.clone();
    let secret = load_secret(dirs, vault, &record.id)?;
    let headers = merged_headers(&record, &secret);

    let mut probe = protocol::test_endpoint(
        transport,
        &record.base_url,
        record.protocol,
        record.auth_scheme.as_ref(),
        secret.api_key.as_str(),
        record.api_version.as_deref(),
        record.models_path.as_deref(),
        &headers,
    );
    redact_probe_secrets(&mut probe, &secret);

    let now = Utc::now();
    EndpointCache::new(dirs).update(|cache| {
        let entry = cache.endpoints.entry(record.id.clone()).or_default();
        if probe.reachable {
            entry.models = probe.models.clone();
            entry.fetched_at = Some(now);
            entry.latency_ms = probe.latency_ms;
            entry.rate_limits = probe.rate_limits.clone();
            entry.last_error = None;
        } else {
            entry.last_error = probe.error.clone();
        }
    })?;

    if let Some(detected) = probe.detected_protocol
        && record.detected_protocol != Some(detected)
    {
        let _guard = StateLock::acquire(&dirs.lock_file(), LOCK_TIMEOUT)?;
        let reg_path = registry_file(dirs);
        let mut reg = EndpointRegistry::load(&reg_path)?;
        if let Some(rec) = reg.find_mut(&record.id) {
            rec.detected_protocol = Some(detected);
            rec.updated_at = now;
            reg.save(&reg_path)?;
        }
    }

    Ok(probe)
}

/// Export protocol-appropriate environment variables for spawning `agy` or
/// third-party LLM CLI tools against an endpoint (or its local bridge).
pub fn export_env_vars(
    record: &EndpointRecord,
    secret: &EndpointSecret,
    override_model: Option<&str>,
    bridge_url: Option<&str>,
) -> Vec<(String, String)> {
    let proto = record.effective_protocol();
    let target_url = bridge_url.unwrap_or(&record.base_url).to_owned();
    let api_key = secret.api_key.as_str();
    let model = override_model
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(record.default_model.as_deref());

    let mut vars: Vec<(String, String)> = Vec::new();
    match proto {
        EndpointProtocol::OpenAi
        | EndpointProtocol::OpenAiResponses
        | EndpointProtocol::Custom
        | EndpointProtocol::Auto => {
            if !api_key.is_empty() {
                vars.push(("OPENAI_API_KEY".into(), api_key.to_owned()));
            }
            vars.push(("OPENAI_BASE_URL".into(), target_url.clone()));
        }
        EndpointProtocol::Anthropic => {
            if !api_key.is_empty() {
                vars.push(("ANTHROPIC_API_KEY".into(), api_key.to_owned()));
            }
            vars.push(("ANTHROPIC_BASE_URL".into(), target_url.clone()));
        }
        EndpointProtocol::Gemini => {
            if !api_key.is_empty() {
                vars.push(("GEMINI_API_KEY".into(), api_key.to_owned()));
            }
            vars.push(("GOOGLE_GEMINI_BASE_URL".into(), target_url.clone()));
        }
        EndpointProtocol::Ollama => {
            vars.push(("OLLAMA_HOST".into(), target_url.clone()));
        }
        EndpointProtocol::AzureOpenAi => {
            if !api_key.is_empty() {
                vars.push(("AZURE_OPENAI_API_KEY".into(), api_key.to_owned()));
            }
            vars.push(("AZURE_OPENAI_ENDPOINT".into(), target_url.clone()));
            if let Some(ver) = record.api_version.as_deref() {
                vars.push(("OPENAI_API_VERSION".into(), ver.to_owned()));
            }
        }
        EndpointProtocol::Cohere => {
            if !api_key.is_empty() {
                vars.push(("COHERE_API_KEY".into(), api_key.to_owned()));
            }
            vars.push(("COHERE_BASE_URL".into(), target_url.clone()));
        }
    }

    if let Some(bridge) = bridge_url {
        vars.push(("AGYVA_BRIDGE_URL".into(), bridge.to_owned()));
        if !vars.iter().any(|(k, _)| k == "GOOGLE_GEMINI_BASE_URL") {
            vars.push(("GOOGLE_GEMINI_BASE_URL".into(), bridge.to_owned()));
        }
        if !vars.iter().any(|(k, _)| k == "GEMINI_API_KEY") {
            vars.push(("GEMINI_API_KEY".into(), "agyva-bridge".into()));
        }
    }

    vars.push(("AGYVA_ENDPOINT".into(), record.alias.clone()));
    vars.push(("AGYVA_ENDPOINT_ID".into(), record.id.clone()));
    vars.push(("AGYVA_ENDPOINT_PROTOCOL".into(), proto.to_string()));
    if let Some(m) = model {
        vars.push(("AGYVA_MODEL".into(), m.to_owned()));
    }
    vars
}

fn read_agy_settings_map(paths: &AgyPaths) -> Result<Option<Map<String, Value>>> {
    let path = paths.settings();
    let Some(bytes) = fsutil::read_optional(&path)
        .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
    else {
        return Ok(None);
    };
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Some(Map::new()));
    }
    match serde_json::from_slice::<Value>(&bytes) {
        Ok(Value::Object(map)) => Ok(Some(map)),
        _ => Err(
            Error::format(format!("{} is not a JSON object", path.display()))
                .hint("agyva will not modify a settings file it cannot parse; fix or move it"),
        ),
    }
}

fn write_agy_settings_map(paths: &AgyPaths, map: &Map<String, Value>) -> Result<()> {
    let settings_path = paths.settings();
    let mut out = serde_json::to_vec_pretty(&Value::Object(map.clone()))
        .map_err(|e| Error::format(format!("cannot encode settings: {e}")))?;
    out.push(b'\n');
    let file_mode = fsutil::mode_of(&settings_path).unwrap_or(fsutil::PRIVATE_FILE_MODE);
    if let Some(dir) = settings_path.parent() {
        fsutil::create_dir_with_mode(dir)
            .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
    }
    fsutil::atomic_write_mode(&settings_path, &out, file_mode)
        .map_err(|e| Error::io(format!("writing {}", settings_path.display()), e))
}

/// Safely update `modelProvider` and `customModelsConfig` in `agy`'s
/// `settings.json`, preserving every other key and failing closed on corrupt JSON.
/// Never writes API keys into `settings.json`.
pub fn sync_agy_settings(
    agy_paths: &AgyPaths,
    record: &EndpointRecord,
    models: &[FetchedModel],
    bridge_url: Option<&str>,
) -> Result<()> {
    let mut map = read_agy_settings_map(agy_paths)?.unwrap_or_default();
    let effective_proto = record.effective_protocol();
    let base_url = bridge_url.unwrap_or(&record.base_url);

    let model_entries: Vec<Value> = models
        .iter()
        .map(|m| {
            let mut obj = Map::new();
            obj.insert("id".into(), Value::String(m.id.clone()));
            obj.insert(
                "displayName".into(),
                Value::String(m.display_name.as_deref().unwrap_or(&m.id).to_owned()),
            );
            if let Some(ctx) = m.context_window {
                obj.insert("contextWindow".into(), Value::from(ctx));
            }
            if let Some(max_out) = m.max_output_tokens {
                obj.insert("maxOutputTokens".into(), Value::from(max_out));
            }
            Value::Object(obj)
        })
        .collect();

    let mut config_obj = Map::new();
    config_obj.insert("endpointId".into(), Value::String(record.id.clone()));
    config_obj.insert("alias".into(), Value::String(record.alias.clone()));
    config_obj.insert(
        "protocol".into(),
        Value::String(effective_proto.as_str().to_owned()),
    );
    config_obj.insert("baseUrl".into(), Value::String(base_url.to_owned()));
    if let Some(default_model) = record.default_model.as_deref() {
        config_obj.insert(
            "defaultModel".into(),
            Value::String(default_model.to_owned()),
        );
    }
    config_obj.insert("models".into(), Value::Array(model_entries));

    map.insert("modelProvider".into(), Value::String("custom".into()));
    map.insert("customModelsConfig".into(), Value::Object(config_obj));

    write_agy_settings_map(agy_paths, &map)
}

/// Remove `modelProvider` and `customModelsConfig` from `agy`'s `settings.json`,
/// preserving all other keys and failing closed on corrupt JSON.
pub fn clear_agy_custom_provider(agy_paths: &AgyPaths) -> Result<()> {
    let Some(mut map) = read_agy_settings_map(agy_paths)? else {
        return Ok(());
    };
    let removed_provider = map.shift_remove("modelProvider").is_some();
    let removed_config = map.shift_remove("customModelsConfig").is_some();
    if !removed_provider && !removed_config {
        return Ok(());
    }
    write_agy_settings_map(agy_paths, &map)
}
