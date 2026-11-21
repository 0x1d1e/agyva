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
//! SemVer newer than this build, download the Linux x86_64 GNU `tar.gz`
//! asset plus `SHA256SUMS`, verify the archive against the file's digest
//! and the GitHub API asset digest when present, then extract exactly
//! `<archive-stem>/agyva` — one regular executable member, with plain
//! directory entries tolerated; symlinks, hard links, traversal names and
//! duplicates are refused — stage it next to the running binary, fsync and
//! atomically rename it into place. Every request and redirect is restricted
//! to GitHub's release hosts, and the whole decompressed stream is bounded.
//! `agyva update --check` performs the same checks and only reports.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, Duration as ChronoDuration, Utc};
use semver::Version;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::core::error::{Error, Result};

/// The one endpoint agyva ever contacts (GitHub API, HTTPS only).
pub const UPDATE_REPO_API: &str = "https://api.github.com/repos/0x1d1e/agyva/releases/latest";
const EXECUTABLE_NAME: &str = "agyva";
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
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64", target_env = "gnu"),
        all(target_os = "linux", target_arch = "aarch64", target_env = "gnu"),
    )))]
    {
        None
    }
}
/// Aggregate decompressed ceiling for the whole release archive.
const MAX_ARCHIVE_UNCOMPRESSED: u64 = 128 * 1024 * 1024;
/// Passive startup checks must never hold a command back for long; the
/// explicit `update` gets more room.
const CHECK_TIMEOUT: Duration = Duration::from_secs(2);
const UPDATE_TIMEOUT: Duration = Duration::from_secs(10);
/// Metadata and download ceilings (a release tarball is ~10 MiB).
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;

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
                .hint("download a release archive for your platform manually from GitHub Releases"),
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
    let archive_name = format!("agyva-{latest}-{target}.tar.gz");
    let archive = transport.get(&plan.archive_url, UPDATE_TIMEOUT)?;
    if archive.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err(Error::refused("the release archive is too large"));
    }
    let sums = transport.get(&plan.sums_url, UPDATE_TIMEOUT)?;
    if sums.len() as u64 > MAX_METADATA_BYTES {
        return Err(Error::refused("SHA256SUMS is too large"));
    }
    let actual = hex_digest(&archive);
    let expected = expected_sum(&sums, &archive_name)?;
    if actual != expected {
        return Err(Error::refused(
            "the downloaded archive does not match SHA256SUMS; nothing was installed",
        ));
    }
    if let Some(api) = &plan.api_digest
        && *api != actual
    {
        return Err(Error::refused(
            "the downloaded archive does not match the release digest; nothing was installed",
        ));
    }
    let bytes = extract_binary(&archive, &latest.to_string(), target)?;
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
    stage_and_replace(&bytes, &exe)?;
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
    archive_url: String,
    sums_url: String,
    /// The API digest when GitHub exposes one (verified too, when present).
    api_digest: Option<String>,
}

fn plan_from_release(release: &Release, latest: &Version, target: &str) -> Result<UpdatePlan> {
    // Exact names, no ambiguous duplicates.
    let archive_name = format!("agyva-{latest}-{target}.tar.gz");
    let pick = |name: &str| -> Result<&Asset> {
        let found: Vec<&Asset> = release.assets.iter().filter(|a| a.name == name).collect();
        let [only] = found.as_slice() else {
            return Err(Error::refused(format!(
                "release {latest} has no unambiguous `{name}` asset"
            )));
        };
        Ok(only)
    };
    let archive = pick(&archive_name)?;
    let sums = pick("SHA256SUMS")?;
    let api_digest = match archive.digest.as_deref() {
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
                .to_owned(),
        ),
    };
    Ok(UpdatePlan {
        archive_url: archive.url.clone(),
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
        Error::refused("SHA256SUMS does not name the release archive")
            .hint("download the release manually and verify it yourself")
    })
}

/// Refuse any archive path that could escape or alias the extraction root.
fn member_is_dangerous(path: &Path) -> bool {
    path.components().any(|c| {
        !matches!(
            c,
            std::path::Component::Normal(_) | std::path::Component::CurDir
        )
    })
}

/// Extract exactly `<stem>/agyva` from the gzipped tar. The whole archive is
/// decompressed under an aggregate bound and every member inspected: plain
/// directory entries are accepted, everything else must be a safely-named
/// regular file; the binary appears exactly once and is executable.
struct BoundedReader<R: std::io::Read> {
    inner: R,
    total: u64,
}

impl<R: std::io::Read> std::io::Read for BoundedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.total += n as u64;
        if self.total > MAX_ARCHIVE_UNCOMPRESSED {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "release archive decompresses beyond its bound",
            ));
        }
        Ok(n)
    }
}
fn extract_binary(archive: &[u8], version: &str, target: &str) -> Result<Zeroizing<Vec<u8>>> {
    let stem = format!("agyva-{version}-{target}");
    let wanted = Path::new(&stem).join(EXECUTABLE_NAME);
    let gz = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(BoundedReader {
        inner: gz,
        total: 0,
    });
    let mut found: Option<Zeroizing<Vec<u8>>> = None;
    let entries = tar
        .entries()
        .map_err(|e| Error::refused(format!("the release archive is unreadable: {e}")))?;
    for entry in entries {
        let entry =
            entry.map_err(|e| Error::refused(format!("the release archive is unreadable: {e}")))?;
        let path = entry
            .path()
            .map_err(|e| Error::refused(format!("the release archive is unreadable: {e}")))?
            .into_owned();
        if member_is_dangerous(&path) {
            return Err(Error::refused(format!(
                "the release archive holds an unsafe path: {}",
                path.display()
            )));
        }
        // Plain directories are neutral: they hold no bytes to install.
        if entry.header().entry_type().is_dir() || path.to_string_lossy().ends_with('/') {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(Error::refused(format!(
                "the release archive holds a non-regular entry: {}",
                path.display()
            )));
        }
        if path != wanted {
            continue;
        }
        if entry.header().mode().unwrap_or(0) & 0o111 == 0 {
            return Err(Error::refused("the archived agyva is not executable"));
        }
        if found.is_some() {
            return Err(Error::refused(
                "the release archive holds duplicate agyva members",
            ));
        }
        let mut bytes = Vec::new();
        entry
            .take(MAX_BINARY_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| Error::refused(format!("the archived binary is unreadable: {e}")))?;
        if bytes.len() as u64 > MAX_BINARY_BYTES {
            return Err(Error::refused("the archived binary is too large"));
        }
        found = Some(Zeroizing::new(bytes));
    }
    found.ok_or_else(|| {
        Error::refused(format!(
            "the release archive does not hold {stem}/{EXECUTABLE_NAME}"
        ))
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

/// Stage `bytes` next to `exe` (fresh 0755 temp file, fsync) and atomically
/// rename it over the binary. The temp file is removed on any failure.
fn stage_and_replace(bytes: &[u8], exe: &Path) -> Result<()> {
    let dir = exe
        .parent()
        .ok_or_else(|| Error::refused("cannot determine the directory of the running binary"))?;
    let stage = dir.join(format!(".agyva.update.{}.tmp", std::process::id()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        options.mode(0o755);
        let mut file = options
            .open(&stage)
            .map_err(|e| Error::io("staging the new binary", e))?;
        let written = file
            .write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|e| Error::io("staging the new binary", e));
        drop(file);
        written?;
        fs::rename(&stage, exe).map_err(|e| Error::io("replacing the binary", e))?;
        crate::storage::fsutil::sync_dir(dir)
            .map_err(|e| Error::io("syncing the install directory", e))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result
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

    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
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
        let name = "agyva-0.2.0-x86_64-unknown-linux-gnu.tar.gz";
        let sums = format!(
            "{}  {name}\n{}  other.tar.gz\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert_eq!(expected_sum(sums.as_bytes(), name).unwrap(), "a".repeat(64));
        assert!(expected_sum(sums.as_bytes(), "missing.tar.gz").is_err());
        let dup = format!("{}  x\n{}  x\n", "a".repeat(64), "b".repeat(64));
        assert!(expected_sum(dup.as_bytes(), "x").is_err());
        let malformed = format!("zz  {name}\n");
        assert!(expected_sum(malformed.as_bytes(), name).is_err());
    }

    #[test]
    fn member_paths_are_screened() {
        assert!(!member_is_dangerous(Path::new(
            "agyva-0.2.0-x86_64-unknown-linux-gnu/agyva"
        )));
        assert!(member_is_dangerous(Path::new("../agyva")));
        assert!(member_is_dangerous(Path::new("/etc/agyva")));
        assert!(member_is_dangerous(Path::new("stem/../../agyva")));
    }

    #[test]
    fn extraction_accepts_only_the_exact_executable() {
        use tar::Builder;

        let mut buf = Vec::new();
        {
            let mut b = Builder::new(&mut buf);
            let mut h = tar::Header::new_gnu();
            h.set_size(5);
            h.set_mode(0o755);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(
                &mut h,
                "agyva-0.2.0-x86_64-unknown-linux-gnu/agyva",
                &b"hello"[..],
            )
            .unwrap();
            b.finish().unwrap();
        }
        let archive = gz(&buf);
        let got = extract_binary(&archive, "0.2.0", TEST_TARGET).unwrap();
        assert_eq!(&**got, b"hello");

        // Build a one- or two-member archive of regular files.
        fn regular_archive(members: &[(&str, u32)]) -> Vec<u8> {
            let mut buf = Vec::new();
            {
                let mut b = Builder::new(&mut buf);
                for (name, mode) in members {
                    let mut h = tar::Header::new_gnu();
                    h.set_size(5);
                    h.set_mode(*mode);
                    h.set_entry_type(tar::EntryType::Regular);
                    b.append_data(&mut h, *name, &b"hello"[..]).unwrap();
                }
                b.finish().unwrap();
            }
            gz(&buf)
        }

        // Wrong stem: the wanted member is absent.
        assert!(
            extract_binary(
                &regular_archive(&[("other-stem/agyva", 0o755)]),
                "0.2.0",
                TEST_TARGET
            )
            .is_err()
        );
        // Non-executable mode.
        assert!(
            extract_binary(
                &regular_archive(&[("agyva-0.2.0-x86_64-unknown-linux-gnu/agyva", 0o644)]),
                "0.2.0",
                TEST_TARGET
            )
            .is_err()
        );
        // Duplicate members.
        assert!(
            extract_binary(
                &regular_archive(&[
                    ("agyva-0.2.0-x86_64-unknown-linux-gnu/agyva", 0o755),
                    ("agyva-0.2.0-x86_64-unknown-linux-gnu/agyva", 0o755),
                ]),
                "0.2.0",
                TEST_TARGET
            )
            .is_err()
        );

        // A traversal member anywhere is refused even when the good member
        // is present.
        let mut buf = Vec::new();
        {
            let mut b = Builder::new(&mut buf);
            let mut h = tar::Header::new_gnu();
            h.set_size(5);
            h.set_mode(0o755);
            h.set_entry_type(tar::EntryType::Regular);
            b.append_data(
                &mut h,
                "agyva-0.2.0-x86_64-unknown-linux-gnu/agyva",
                &b"hello"[..],
            )
            .unwrap();
            let mut h2 = tar::Header::new_gnu();
            h2.set_size(5);
            h2.set_mode(0o755);
            h2.set_entry_type(tar::EntryType::Symlink);
            h2.set_link_name("../target/agyva").unwrap();
            b.append_data(
                &mut h2,
                "agyva-0.2.0-x86_64-unknown-linux-gnu/link",
                &b""[..],
            )
            .unwrap();
            b.finish().unwrap();
        }
        assert!(extract_binary(&gz(&buf), "0.2.0", TEST_TARGET).is_err());
    }

    #[test]
    fn stage_and_replace_swaps_a_temp_executable() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("agyva-upd-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("agyva");
        fs::write(&exe, b"old").unwrap();
        stage_and_replace(b"new", &exe).unwrap();
        assert_eq!(fs::read(&exe).unwrap(), b"new");
        let mode = fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0o111);
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
    use tar::Builder;

    /// `scripts/release.sh` tars a staged tree, so the archive holds the
    /// root directory entry, a nested `docs/` entry and regular files.
    #[test]
    fn release_script_shaped_archive_extracts() {
        let stem = "agyva-0.2.0-x86_64-unknown-linux-gnu";
        let mut buf = Vec::new();
        {
            let mut b = Builder::new(&mut buf);
            for dir in [format!("{stem}/"), format!("{stem}/docs/")] {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Directory);
                h.set_size(0);
                h.set_mode(0o755);
                b.append_data(&mut h, dir, &[][..]).unwrap();
            }
            for (name, mode, body) in [
                ("README.md", 0o644, &b"readme"[..]),
                ("docs/security.md", 0o644, &b"doc"[..]),
                ("agyva", 0o755, &b"binary"[..]),
            ] {
                let mut h = tar::Header::new_gnu();
                h.set_entry_type(tar::EntryType::Regular);
                h.set_size(body.len() as u64);
                h.set_mode(mode);
                b.append_data(&mut h, format!("{stem}/{name}"), body)
                    .unwrap();
            }
            b.finish().unwrap();
        }
        let got =
            extract_binary(&gz(&buf), "0.2.0", TEST_TARGET).expect("directory entries tolerated");
        assert_eq!(&**got, b"binary");
    }

    #[test]
    fn a_malformed_api_digest_refuses_instead_of_being_ignored() {
        let name = "agyva-9.9.9-x86_64-unknown-linux-gnu.tar.gz";
        let release = |digest: Option<serde_json::Value>| -> Release {
            let mut archive = serde_json::json!([
                {"name": name, "browser_download_url": "https://objects.githubusercontent.com/a"},
                {"name": "SHA256SUMS", "browser_download_url": "https://objects.githubusercontent.com/b"},
            ]);
            if let Some(d) = digest {
                archive[0]["digest"] = d;
            }
            serde_json::from_slice(&release_json("v9.9.9", archive)).unwrap()
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
    fn the_whole_decompressed_stream_is_bounded() {
        // A gzip bomb: highly compressible zeros far beyond the ceiling.
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        let chunk = vec![0u8; 1 << 20];
        for _ in 0..129 {
            e.write_all(&chunk).unwrap();
        }
        let archive = e.finish().unwrap();
        assert!(archive.len() < 32 * 1024 * 1024, "the bomb stays small");
        assert!(extract_binary(&archive, "0.2.0", TEST_TARGET).is_err());
    }
}
