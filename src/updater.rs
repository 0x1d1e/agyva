//! Self-update from GitHub Releases.
//!
//! Startup check: one `GET <UPDATE_REPO_API>` per enabled invocation
//! (config `check_updates = true` by default, `AGYVA_UPDATE_CHECK=0`
//! opt-out), short bounded timeout, silent on any failure, so commands,
//! help and version output are never delayed or polluted. The notice goes
//! to stderr for human CLI runs and into the TUI dashboard when the
//! alternate screen owns the terminal.
//!
//! `agyva update` installs: fetch the latest release, require a stable
//! SemVer newer than this build, download the target's raw binary asset
//! (`agyva-<version>-<target>`, or `.exe` on Windows) plus `SHA256SUMS`,
//! verify the binary is non-empty and within `MAX_BINARY_BYTES`, verify its
//! SHA-256 against `SHA256SUMS` and the GitHub API asset digest when present,
//! stage it next to the running binary, fsync and replace it into place. Every
//! request and redirect is restricted to GitHub's release hosts.
//! `agyva update --check` performs the same metadata check and only reports.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::core::error::{Error, Result};

/// The one endpoint agyva ever contacts (GitHub API, HTTPS only).
pub const UPDATE_REPO_API: &str = "https://api.github.com/repos/0x1d1e/agyva/releases/latest";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const CACHE_SCHEMA_VERSION: u32 = 1;
const CACHE_OK_TTL_SECS: i64 = 86_400;
const CACHE_ERR_TTL_SECS: i64 = 3_600;

/// Release target triple supported by self-update on this build, if any.
pub const fn supported_target() -> Option<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
    {
        Some("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
    {
        Some("aarch64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        Some("x86_64-apple-darwin")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Some("aarch64-apple-darwin")
    }
    #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
    {
        Some("x86_64-pc-windows-msvc")
    }
    #[cfg(all(target_os = "windows", target_arch = "aarch64", target_env = "msvc"))]
    {
        Some("aarch64-pc-windows-msvc")
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
        all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64"),
        all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"),
        all(target_os = "windows", target_arch = "aarch64", target_env = "msvc"),
    )))]
    {
        None
    }
}

fn asset_name_for_target(version: &Version, target: &str) -> String {
    if target.contains("-windows-") {
        format!("agyva-{version}-{target}.exe")
    } else {
        format!("agyva-{version}-{target}")
    }
}

/// Passive startup checks must never hold a command back for long; the
/// explicit `update` gets more room.
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(10);
/// Metadata and download ceilings (a release binary is ~10 MiB).
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
pub const MAX_BINARY_BYTES: u64 = 64 * 1024 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = MAX_BINARY_BYTES;

/// What a check found: the installed version and a newer one, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCheck {
    pub current: String,
    pub latest: Option<String>,
}

impl UpdateCheck {
    fn now(latest: Option<Version>) -> Self {
        Self {
            current: CURRENT_VERSION.to_owned(),
            latest: latest.map(|v| v.to_string()),
        }
    }

    /// The human notice, or `None` when there is nothing to report.
    pub fn notice(&self) -> Option<String> {
        self.latest.as_ref().map(|v| {
            format!(
                "update available: agyva {v} (installed {}); run `agyva update` to install it",
                self.current
            )
        })
    }
}

// -------------------------------------------------------------- release API

#[derive(Debug, Deserialize)]
struct Release {
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    tag_name: String,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    /// SHA-256 digest GitHub computes for uploaded assets, when exposed.
    #[serde(default)]
    digest: Option<String>,
    #[serde(rename = "browser_download_url")]
    url: String,
}

/// A newer stable SemVer than `current`, else `None` (equal, older,
/// prerelease or unparseable tags are not updates).
fn newer_version(tag: &str, current: &str) -> Option<Version> {
    let parse = |t: &str| {
        let t = t.strip_prefix('v').unwrap_or(t);
        let v = t.parse::<Version>().ok()?;
        v.pre.is_empty().then_some(v)
    };
    let latest = parse(tag)?;
    let current = parse(current)?;
    (latest > current).then_some(latest)
}

// ---------------------------------------------------------------- transport

/// The HTTPS seam: production uses `ureq` (rustls, HTTPS-only, GitHub
/// redirects only); tests inject an in-memory transport — never a loopback
/// server.
pub trait Transport: Send + Sync {
    fn get(&self, url: &str, timeout: Duration) -> Result<Vec<u8>>;
}

pub struct HttpTransport;

impl Transport for HttpTransport {
    fn get(&self, url: &str, timeout: Duration) -> Result<Vec<u8>> {
        if !release_origin(url) {
            return Err(Error::refused(format!(
                "the release endpoint {url} is not on the GitHub release hosts"
            )));
        }
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .https_only(true)
            .max_redirects(5)
            .save_redirect_history(true)
            .build();
        let agent: ureq::Agent = config.into();
        let response = agent
            .get(url)
            .header("User-Agent", concat!("agyva/", env!("CARGO_PKG_VERSION")))
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(update_request_error)?;
        if let Some(history) = <dyn ureq::ResponseExt>::get_redirect_history(&response) {
            for hop in history.iter().skip(1) {
                let Some(host) = hop.host() else {
                    return Err(Error::refused(
                        "the release download was redirected to an unsafe host",
                    ));
                };
                if !host_allowed(host) {
                    return Err(Error::refused(format!(
                        "the release download was redirected to the unsafe host {host}"
                    )));
                }
            }
        }
        let mut body = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_DOWNLOAD_BYTES + 1)
            .read_to_end(&mut body)
            .map_err(|e| Error::backend(format!("update request failed: {e}")))?;
        if body.len() as u64 > MAX_DOWNLOAD_BYTES {
            return Err(Error::refused("the release download is too large"));
        }
        Ok(body)
    }
}

/// Hosts a GitHub release asset may live on, exactly.
/// GitHub redirects `/releases/download/...` to `objects.githubusercontent.com`
/// and `release-assets.githubusercontent.com`.
fn host_allowed(host: &str) -> bool {
    matches!(
        host,
        "api.github.com"
            | "github.com"
            | "objects.githubusercontent.com"
            | "release-assets.githubusercontent.com"
    )
}

/// Whether `url` is an HTTPS URL on one of the release hosts. A port (443)
/// is tolerated; userinfo, other schemes and lookalike hosts are not.
fn release_origin(url: &str) -> bool {
    let Ok(uri) = url.parse::<ureq::http::Uri>() else {
        return false;
    };
    uri.scheme_str() == Some("https")
        && !uri.authority().is_some_and(|a| a.as_str().contains('@'))
        && uri.host().is_some_and(host_allowed)
}

fn update_request_error(e: ureq::Error) -> Error {
    let message = format!("update request failed: {e}");
    let hint = match &e {
        ureq::Error::StatusCode(403) => {
            "GitHub rejected the request (HTTP 403), usually rate limiting; retry later, \
             or download the release manually"
        }
        ureq::Error::StatusCode(_) => {
            "GitHub did not serve the release; check your network and GitHub's status, \
             then retry with `agyva update --check`"
        }
        ureq::Error::Timeout(_) => {
            "the request timed out; check your network and retry with `agyva update --check`"
        }
        _ => {
            "check your network access to GitHub Releases, then retry with \
             `agyva update --check`"
        }
    };
    Error::backend(message).hint(hint)
}

// ------------------------------------------------------------- check + run

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCheckCache {
    schema_version: u32,
    checked_at: DateTime<Utc>,
    latest: Option<String>,
    ok: bool,
}

fn read_cached_check(path: &Path, now: DateTime<Utc>) -> Option<Option<UpdateCheck>> {
    let bytes = fs::read(path).ok()?;
    let entry: UpdateCheckCache = serde_json::from_slice(&bytes).ok()?;
    if entry.schema_version != CACHE_SCHEMA_VERSION {
        return None;
    }
    let ttl = if entry.ok {
        ChronoDuration::seconds(CACHE_OK_TTL_SECS)
    } else {
        ChronoDuration::seconds(CACHE_ERR_TTL_SECS)
    };
    let age = now.signed_duration_since(entry.checked_at);
    if age < ChronoDuration::zero() || age >= ttl {
        return None;
    }
    if !entry.ok {
        return Some(None);
    }
    let latest = entry
        .latest
        .as_deref()
        .and_then(|tag| newer_version(tag, CURRENT_VERSION));
    Some(Some(UpdateCheck::now(latest)))
}

fn write_cached_check(path: &Path, entry: &UpdateCheckCache) {
    let Ok(bytes) = serde_json::to_vec(entry) else {
        return;
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && crate::storage::fsutil::create_private_dir(parent).is_err()
    {
        return;
    }
    let _ = crate::storage::fsutil::atomic_write(path, &bytes);
}

/// The startup check over the real network: one metadata request, every
/// failure silent. `enabled` is the resolved config/env gate.
pub fn startup_check(enabled: bool) -> Option<UpdateCheck> {
    startup_check_cached(enabled, None, Utc::now())
}

/// Cached startup check: reuses a fresh on-disk decision (24h on success,
/// 1h on failure) when `cache_file` is supplied.
pub fn startup_check_cached(
    enabled: bool,
    cache_file: Option<&Path>,
    now: DateTime<Utc>,
) -> Option<UpdateCheck> {
    startup_check_with(enabled, cache_file, &HttpTransport, now)
}

/// Testable startup check with an explicit cache path, transport and clock.
pub fn startup_check_with(
    enabled: bool,
    cache_file: Option<&Path>,
    transport: &dyn Transport,
    now: DateTime<Utc>,
) -> Option<UpdateCheck> {
    if !enabled || supported_target().is_none() {
        return None;
    }
    if let Some(path) = cache_file
        && let Some(cached) = read_cached_check(path, now)
    {
        return cached;
    }
    match fetch_release(transport, CHECK_TIMEOUT) {
        Ok(release) => {
            let check = UpdateCheck::now(newer_version(&release.tag_name, CURRENT_VERSION));
            if let Some(path) = cache_file {
                write_cached_check(
                    path,
                    &UpdateCheckCache {
                        schema_version: CACHE_SCHEMA_VERSION,
                        checked_at: now,
                        latest: check.latest.clone(),
                        ok: true,
                    },
                );
            }
            Some(check)
        }
        Err(_) => {
            if let Some(path) = cache_file {
                write_cached_check(
                    path,
                    &UpdateCheckCache {
                        schema_version: CACHE_SCHEMA_VERSION,
                        checked_at: now,
                        latest: None,
                        ok: false,
                    },
                );
            }
            None
        }
    }
}

/// Explicit update with an injectable transport (tests) and optional
/// redirect of the install target (tests never replace a running binary).
pub fn install_update(
    check_only: bool,
    transport: Box<dyn Transport>,
    target_exe: Option<&Path>,
) -> Result<UpdateCheck> {
    run_update_for_target(
        check_only,
        transport.as_ref(),
        target_exe,
        supported_target(),
    )
}

/// Shared engine so tests can redirect the install target away from the
/// running test binary and exercise target gating.
pub fn run_update_for_target(
    check_only: bool,
    transport: &dyn Transport,
    target_exe: Option<&Path>,
    target: Option<&str>,
) -> Result<UpdateCheck> {
    let Some(target) = target else {
        return Err(
            Error::refused("self-update is not supported on this platform/architecture")
                .hint("download a release binary for your platform manually from GitHub Releases"),
        );
    };
    let release = fetch_release(transport, UPDATE_TIMEOUT)?;
    let check = UpdateCheck::now(newer_version(&release.tag_name, CURRENT_VERSION));
    if check_only {
        return Ok(check);
    }
    let Some(latest) = check
        .latest
        .as_deref()
        .and_then(|v| v.parse::<Version>().ok())
    else {
        return Err(Error::refused(format!(
            "agyva {CURRENT_VERSION} is already the newest stable release"
        )));
    };
    let plan = plan_from_release(&release, &latest, target)?;
    let binary_name = asset_name_for_target(&latest, target);
    let binary = transport.get(&plan.binary_url, UPDATE_TIMEOUT)?;
    if binary.is_empty() {
        return Err(Error::refused(
            "the downloaded binary is empty; nothing was installed",
        ));
    }
    if binary.len() as u64 > MAX_BINARY_BYTES {
        return Err(Error::refused("the downloaded binary is too large"));
    }
    let sums = transport.get(&plan.sums_url, UPDATE_TIMEOUT)?;
    if sums.len() as u64 > MAX_METADATA_BYTES {
        return Err(Error::refused("SHA256SUMS is too large"));
    }
    let actual = hex_digest(&binary);
    let expected = expected_sum(&sums, &binary_name)?;
    if actual != expected {
        return Err(Error::refused(
            "the downloaded binary does not match SHA256SUMS; nothing was installed",
        ));
    }
    if let Some(api) = &plan.api_digest
        && *api != actual
    {
        return Err(Error::refused(
            "the downloaded binary does not match the release digest; nothing was installed",
        ));
    }
    let exe: PathBuf = match target_exe {
        Some(p) => p.to_path_buf(),
        None => std::env::current_exe()
            .map_err(|e| Error::backend("cannot locate the running binary").with_source(e))?,
    };
    let meta =
        fs::symlink_metadata(&exe).map_err(|e| Error::io("inspecting the running binary", e))?;
    if !meta.is_file() {
        return Err(Error::refused(
            "the running binary is not a regular file; refusing to replace it",
        ));
    }
    stage_and_replace(&binary, &exe)?;
    tracing::info!(version = %latest, "agyva updated itself");
    Ok(check)
}

/// Fetch and interpret the latest release; every failure maps to an error
/// the caller treats as "no update information".
fn fetch_release(transport: &dyn Transport, timeout: Duration) -> Result<Release> {
    let body = transport.get(UPDATE_REPO_API, timeout)?;
    if body.len() as u64 > MAX_METADATA_BYTES {
        return Err(Error::backend("release metadata is too large"));
    }
    let release: Release = serde_json::from_slice(&body)
        .map_err(|_| Error::backend("release metadata is not valid JSON"))?;
    if release.draft || release.prerelease {
        // `/releases/latest` never returns these; a proxy might.
        return Err(Error::backend(
            "the latest release is a draft or prerelease",
        ));
    }
    Ok(release)
}

// ----------------------------------------------------------------- install

/// Everything verified before the running binary is touched.
struct UpdatePlan {
    binary_url: String,
    sums_url: String,
    /// The API digest when GitHub exposes one (verified too, when present).
    api_digest: Option<String>,
}

fn plan_from_release(release: &Release, latest: &Version, target: &str) -> Result<UpdatePlan> {
    // Exact names, no ambiguous duplicates.
    let binary_name = asset_name_for_target(latest, target);
    let pick = |name: &str| -> Result<&Asset> {
        let found: Vec<&Asset> = release.assets.iter().filter(|a| a.name == name).collect();
        let [only] = found.as_slice() else {
            return Err(Error::refused(format!(
                "release {latest} has no unambiguous `{name}` asset"
            )));
        };
        Ok(only)
    };
    let binary = pick(&binary_name)?;
    let sums = pick("SHA256SUMS")?;
    let api_digest = match binary.digest.as_deref() {
        None => None,
        Some("") => None,
        Some(d) => Some(
            d.strip_prefix("sha256:")
                .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
                .ok_or_else(|| {
                    Error::refused(
                        "the release carries a malformed per-asset digest; nothing was installed",
                    )
                })?
                .to_ascii_lowercase(),
        ),
    };
    Ok(UpdatePlan {
        binary_url: binary.url.clone(),
        sums_url: sums.url.clone(),
        api_digest,
    })
}

/// Extract the SHA-256 line for exactly `name` from a SHA256SUMS body.
fn expected_sum(sums: &[u8], name: &str) -> Result<String> {
    let text =
        std::str::from_utf8(sums).map_err(|_| Error::refused("SHA256SUMS is not valid UTF-8"))?;
    let mut found: Option<String> = None;
    for line in text.lines() {
        let Some((hex, file)) = line.trim().split_once(char::is_whitespace) else {
            continue;
        };
        let file = file.trim().trim_start_matches('*');
        if file != name {
            continue;
        }
        let hex = hex.trim().to_lowercase();
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::refused("SHA256SUMS holds a malformed digest"));
        }
        if found.is_some() {
            return Err(Error::refused("SHA256SUMS is ambiguous for this asset"));
        }
        found = Some(hex);
    }
    found.ok_or_else(|| {
        Error::refused("SHA256SUMS does not name the release binary")
            .hint("download the release manually and verify it yourself")
    })
}

#[cfg(test)]
mod origin_tests {
    use super::*;
    #[test]
    fn release_origins_are_exact() {
        assert!(release_origin(
            "https://api.github.com/repos/0x1d1e/agyva/releases/latest"
        ));
        assert!(release_origin(
            "https://objects.githubusercontent.com/x?y=1"
        ));
        assert!(release_origin(
            "https://release-assets.githubusercontent.com/x?y=1"
        ));
        assert!(!release_origin("https://evil.example/objects"));
        assert!(!release_origin("http://github.com/x"));
        assert!(!host_allowed("not-github.com"));
        assert!(!host_allowed("github.com.evil.example"));
        assert!(!host_allowed(
            "release-assets.githubusercontent.com.evil.example"
        ));
        assert!(host_allowed("github.com"));
    }
}

/// Stage `bytes` next to `exe` (fresh 0755 temp file on Unix, fsync) and
/// replace the binary. The temp file is removed on any failure.
fn stage_and_replace(bytes: &[u8], exe: &Path) -> Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| Error::refused("cannot determine the directory of the running binary"))?;
    let stage = dir.join(format!(".agyva.update.{}.tmp", std::process::id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o755);
        }
        let mut file = options
            .open(&stage)
            .map_err(|e| Error::io("staging the new binary", e))?;
        let written = file
            .write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| Error::io("staging the new binary", e));
        drop(file);
        written?;
        crate::storage::fsutil::make_executable(&stage)
            .map_err(|e| Error::io("marking the staged binary executable", e))?;
        replace_executable(&stage, exe)?;
        crate::storage::fsutil::sync_dir(dir)
            .map_err(|e| Error::io("syncing the install directory", e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result
}

#[cfg(not(windows))]
fn replace_executable(stage: &Path, exe: &Path) -> Result<()> {
    fs::rename(stage, exe).map_err(|e| Error::io("replacing the binary", e))
}

#[cfg(windows)]
fn replace_executable(stage: &Path, exe: &Path) -> Result<()> {
    // Windows refuses to overwrite or unlink a running .exe, but allows
    // renaming it aside within the same directory.
    let mut old_name = exe.as_os_str().to_os_string();
    old_name.push(".old");
    let old = PathBuf::from(old_name);
    let _ = fs::remove_file(&old);
    fs::rename(exe, &old).map_err(|e| Error::io("moving the running binary aside", e))?;
    if let Err(e) = fs::rename(stage, exe) {
        let _ = fs::rename(&old, exe);
        return Err(Error::io("replacing the binary", e));
    }
    let _ = fs::remove_file(&old);
    Ok(())
}

fn hex_digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Mapping(Vec<(&'static str, Vec<u8>)>);
    impl Transport for Mapping {
        fn get(&self, url: &str, _t: Duration) -> Result<Vec<u8>> {
            self.0
                .iter()
                .find(|(u, _)| *u == url)
                .map(|(_, b)| b.clone())
                .ok_or_else(|| Error::backend(format!("no mapping for {url}")))
        }
    }

    struct Offline;
    impl Transport for Offline {
        fn get(&self, _u: &str, _t: Duration) -> Result<Vec<u8>> {
            Err(Error::backend("offline"))
        }
    }

    fn release_json(tag: &str, assets: serde_json::Value) -> Vec<u8> {
        serde_json::json!({
            "tag_name": tag,
            "draft": false,
            "prerelease": false,
            "assets": assets
        })
        .to_string()
        .into_bytes()
    }

    #[test]
    fn version_gating_is_stable_only() {
        assert!(newer_version("v0.1.0", "0.1.0").is_none());
        assert!(newer_version("v0.0.9", "0.1.0").is_none());
        assert!(newer_version("garbage", "0.1.0").is_none());
        assert!(newer_version("v0.2.0-rc.1", "0.1.0").is_none());
        assert_eq!(
            newer_version("v0.2.0", "0.1.0").map(|v| v.to_string()),
            Some("0.2.0".to_owned())
        );
    }

    const TEST_TARGET: &str = "x86_64-unknown-linux-gnu";

    struct CountingTransport {
        calls: std::sync::atomic::AtomicUsize,
        result: Result<Vec<u8>>,
    }

    impl CountingTransport {
        fn ok(body: Vec<u8>) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                result: Ok(body),
            }
        }
        fn err(msg: &str) -> Self {
            Self {
                calls: std::sync::atomic::AtomicUsize::new(0),
                result: Err(Error::backend(msg)),
            }
        }
        fn count(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    impl Transport for CountingTransport {
        fn get(&self, _url: &str, _t: Duration) -> Result<Vec<u8>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            match &self.result {
                Ok(b) => Ok(b.clone()),
                Err(e) => Err(Error::backend(e.message())),
            }
        }
    }

    #[test]
    fn release_with_no_stable_assets_cannot_plan() {
        let body = release_json("v9.9.9", serde_json::json!([]));
        let transport = Mapping(vec![(UPDATE_REPO_API, body)]);
        let err = run_update_for_target(false, &transport, None, Some(TEST_TARGET)).unwrap_err();
        assert!(err.message().contains("unambiguous"));
    }

    #[test]
    fn offline_update_fails_with_backend_error() {
        let err = run_update_for_target(true, &Offline, None, Some(TEST_TARGET)).unwrap_err();
        assert_eq!(err.message(), "offline");
    }

    #[test]
    fn unsupported_target_is_refused_without_touching_transport_or_exe() {
        let transport = CountingTransport::ok(release_json("v9.9.9", serde_json::json!([])));
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("agyva");
        fs::write(&exe, b"keep").unwrap();

        let err_install = run_update_for_target(false, &transport, Some(&exe), None).unwrap_err();
        assert!(err_install.message().contains("not supported"));

        let err_check = run_update_for_target(true, &transport, None, None).unwrap_err();
        assert!(err_check.message().contains("not supported"));

        assert_eq!(transport.count(), 0);
        assert_eq!(fs::read(&exe).unwrap(), b"keep");
    }

    #[test]
    fn check_only_never_downloads() {
        let body = release_json("v9.9.9", serde_json::json!([]));
        let transport = Mapping(vec![(UPDATE_REPO_API, body)]);
        let check = run_update_for_target(true, &transport, None, Some(TEST_TARGET)).unwrap();
        assert_eq!(check.latest.as_deref(), Some("9.9.9"));
        assert!(check.notice().is_some());
    }

    #[test]
    fn up_to_date_install_is_refused_not_forced() {
        let body = release_json("v0.0.1", serde_json::json!([]));
        let transport = Mapping(vec![(UPDATE_REPO_API, body)]);
        let err = run_update_for_target(false, &transport, None, Some(TEST_TARGET)).unwrap_err();
        assert!(err.message().contains("newest stable release"));
    }

    #[test]
    fn startup_check_cache_hits_expires_and_invalidates_older_versions() {
        if supported_target().is_none() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let cache = tmp.path().join("cache").join("update-check.json");
        let t0 = Utc::now();

        // 1. First call fetches and populates cache; second call within 24h hits cache.
        let transport = CountingTransport::ok(release_json("v9.9.9", serde_json::json!([])));
        let first = startup_check_with(true, Some(&cache), &transport, t0).unwrap();
        assert_eq!(first.latest.as_deref(), Some("9.9.9"));
        let second = startup_check_with(
            true,
            Some(&cache),
            &transport,
            t0 + ChronoDuration::hours(23),
        )
        .unwrap();
        assert_eq!(second.latest.as_deref(), Some("9.9.9"));
        assert_eq!(transport.count(), 1);

        // 2. Expired after 24h: hits transport again.
        let third = startup_check_with(
            true,
            Some(&cache),
            &transport,
            t0 + ChronoDuration::hours(24),
        )
        .unwrap();
        assert_eq!(third.latest.as_deref(), Some("9.9.9"));
        assert_eq!(transport.count(), 2);

        // 3. Cached version no longer newer than CURRENT_VERSION yields latest: None.
        write_cached_check(
            &cache,
            &UpdateCheckCache {
                schema_version: CACHE_SCHEMA_VERSION,
                checked_at: t0,
                latest: Some(CURRENT_VERSION.to_owned()),
                ok: true,
            },
        );
        let stale_notice = startup_check_with(
            true,
            Some(&cache),
            &transport,
            t0 + ChronoDuration::minutes(5),
        )
        .unwrap();
        assert_eq!(stale_notice.latest, None);
        assert_eq!(transport.count(), 2);

        // 4. Offline failure is cached for 1h.
        let err_cache = tmp.path().join("cache").join("err-check.json");
        let offline = CountingTransport::err("offline");
        assert!(startup_check_with(true, Some(&err_cache), &offline, t0).is_none());
        assert!(
            startup_check_with(
                true,
                Some(&err_cache),
                &offline,
                t0 + ChronoDuration::minutes(59),
            )
            .is_none()
        );
        assert_eq!(offline.count(), 1);
        assert!(
            startup_check_with(
                true,
                Some(&err_cache),
                &offline,
                t0 + ChronoDuration::hours(1),
            )
            .is_none()
        );
        assert_eq!(offline.count(), 2);
    }

    #[test]
    fn sums_parsing_is_exact_and_strict() {
        let name = "agyva-0.2.0-x86_64-unknown-linux-gnu";
        let win_name = "agyva-0.2.0-x86_64-pc-windows-msvc.exe";
        let sums = format!(
            "{}  {name}\n{} *{win_name}\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert_eq!(expected_sum(sums.as_bytes(), name).unwrap(), "a".repeat(64));
        assert_eq!(
            expected_sum(sums.as_bytes(), win_name).unwrap(),
            "b".repeat(64)
        );
        assert!(expected_sum(sums.as_bytes(), "missing").is_err());
        let dup = format!("{}  x\n{}  x\n", "a".repeat(64), "b".repeat(64));
        assert!(expected_sum(dup.as_bytes(), "x").is_err());
        let malformed = format!("zz  {name}\n");
        assert!(expected_sum(malformed.as_bytes(), name).is_err());
    }

    #[test]
    fn stage_and_replace_swaps_a_temp_executable() {
        let dir = std::env::temp_dir().join(format!("agyva-upd-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("agyva");
        fs::write(&exe, b"old").unwrap();
        stage_and_replace(b"new", &exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&exe).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111);
        }
        assert!(
            !dir.join(format!(".agyva.update.{}.tmp", std::process::id()))
                .exists()
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn digest_is_hex_sha256() {
        assert_eq!(
            hex_digest(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_malformed_api_digest_refuses_instead_of_being_ignored() {
        let name = "agyva-9.9.9-x86_64-unknown-linux-gnu";
        let release = |digest: Option<serde_json::Value>| -> Release {
            let mut assets = serde_json::json!([
                {"name": name, "browser_download_url": "https://objects.githubusercontent.com/a"},
                {"name": "SHA256SUMS", "browser_download_url": "https://objects.githubusercontent.com/b"},
            ]);
            if let Some(d) = digest {
                assets[0]["digest"] = d;
            }
            serde_json::from_slice(&release_json("v9.9.9", assets)).unwrap()
        };
        let version: Version = "9.9.9".parse().unwrap();
        for digest in ["md5:", "sha256:abc", "sha512:0000"] {
            let r = release(Some(serde_json::json!(digest)));
            assert!(
                plan_from_release(&r, &version, TEST_TARGET).is_err(),
                "digest {digest} must not be silently ignored"
            );
        }
        // Absent stays absent (GitHub does not always expose it yet).
        assert!(
            plan_from_release(&release(None), &version, TEST_TARGET)
                .unwrap()
                .api_digest
                .is_none()
        );
    }

    #[test]
    fn supported_target_and_asset_names_cover_release_platforms() {
        let version: Version = "0.2.0".parse().unwrap();
        let unix_targets = [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "x86_64-apple-darwin",
            "aarch64-apple-darwin",
        ];
        for t in unix_targets {
            assert_eq!(
                asset_name_for_target(&version, t),
                format!("agyva-0.2.0-{t}")
            );
        }
        let windows_targets = ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"];
        for t in windows_targets {
            assert_eq!(
                asset_name_for_target(&version, t),
                format!("agyva-0.2.0-{t}.exe")
            );
        }

        let current = supported_target();
        #[cfg(all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"))]
        assert_eq!(current, Some("x86_64-unknown-linux-gnu"));
        #[cfg(all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"))]
        assert_eq!(current, Some("aarch64-unknown-linux-gnu"));
        #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
        assert_eq!(current, Some("x86_64-apple-darwin"));
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(current, Some("aarch64-apple-darwin"));
        #[cfg(all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"))]
        assert_eq!(current, Some("x86_64-pc-windows-msvc"));
        #[cfg(all(target_os = "windows", target_arch = "aarch64", target_env = "msvc"))]
        assert_eq!(current, Some("aarch64-pc-windows-msvc"));
        #[cfg(not(any(
            all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
            all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"),
            all(target_os = "macos", target_arch = "x86_64"),
            all(target_os = "macos", target_arch = "aarch64"),
            all(target_os = "windows", target_arch = "x86_64", target_env = "msvc"),
            all(target_os = "windows", target_arch = "aarch64", target_env = "msvc"),
        )))]
        assert_eq!(current, None);
    }

    #[test]
    fn windows_targets_require_exe_asset_name_in_release_plan() {
        let version: Version = "0.2.0".parse().unwrap();
        for target in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
            let exe_name = format!("agyva-0.2.0-{target}.exe");
            let good_release: Release = serde_json::from_slice(&release_json(
                "v0.2.0",
                serde_json::json!([
                    {"name": exe_name, "browser_download_url": "https://objects.githubusercontent.com/win"},
                    {"name": "SHA256SUMS", "browser_download_url": "https://objects.githubusercontent.com/sums"},
                ]),
            ))
            .unwrap();
            let plan = plan_from_release(&good_release, &version, target).expect("finds .exe");
            assert_eq!(plan.binary_url, "https://objects.githubusercontent.com/win");

            let unix_only_release: Release = serde_json::from_slice(&release_json(
                "v0.2.0",
                serde_json::json!([
                    {"name": format!("agyva-0.2.0-{target}"), "browser_download_url": "https://objects.githubusercontent.com/unix"},
                    {"name": "SHA256SUMS", "browser_download_url": "https://objects.githubusercontent.com/sums"},
                ]),
            ))
            .unwrap();
            assert!(plan_from_release(&unix_only_release, &version, target).is_err());
        }
    }

    #[test]
    fn empty_and_oversize_binary_are_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let exe = tmp.path().join("agyva");
        fs::write(&exe, b"keep").unwrap();

        let asset = format!("agyva-9.9.9-{TEST_TARGET}");
        let release = |digest: &str| {
            release_json(
                "v9.9.9",
                serde_json::json!([
                    {"name": asset, "digest": format!("sha256:{digest}"), "browser_download_url": "https://example.invalid/bin"},
                    {"name": "SHA256SUMS", "browser_download_url": "https://example.invalid/sums"},
                ]),
            )
        };

        // Empty binary is refused even if SHA256SUMS and api_digest match empty bytes.
        let empty_sum = hex_digest(b"");
        let empty_transport = Mapping(vec![
            (UPDATE_REPO_API, release(&empty_sum)),
            ("https://example.invalid/bin", Vec::new()),
            (
                "https://example.invalid/sums",
                format!("{empty_sum}  {asset}\n").into_bytes(),
            ),
        ]);
        let err = run_update_for_target(false, &empty_transport, Some(&exe), Some(TEST_TARGET))
            .unwrap_err();
        assert!(err.message().contains("empty"), "{}", err.message());
        assert_eq!(fs::read(&exe).unwrap(), b"keep");

        // Oversize binary is refused before touching exe.
        let oversize = vec![0u8; MAX_BINARY_BYTES as usize + 1];
        let oversize_transport = Mapping(vec![
            (UPDATE_REPO_API, release(&hex_digest(&oversize))),
            ("https://example.invalid/bin", oversize),
            (
                "https://example.invalid/sums",
                format!("{}  {asset}\n", "a".repeat(64)).into_bytes(),
            ),
        ]);
        let err = run_update_for_target(false, &oversize_transport, Some(&exe), Some(TEST_TARGET))
            .unwrap_err();
        assert!(err.message().contains("too large"), "{}", err.message());
        assert_eq!(fs::read(&exe).unwrap(), b"keep");
    }
}
