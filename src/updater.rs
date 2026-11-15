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
//! `<archive-stem>/agyva` — a single regular executable member; symlinks,
//! hard links, traversal names and duplicates are refused — stage it next
//! to the running binary, fsync and atomically rename it into place.
//! `agyva update --check` performs the same checks and only reports.

use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::core::error::{Error, Result};

/// The one endpoint agyva ever contacts (GitHub API, HTTPS only).
pub const UPDATE_REPO_API: &str = "https://api.github.com/repos/0x1d1e/agyva/releases/latest";
/// Release asset platform this build installs from (release scripts' name).
const ASSET_TARGET: &str = "x86_64-unknown-linux-gnu";
const EXECUTABLE_NAME: &str = "agyva";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
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
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .https_only(true)
            .max_redirects(5)
            .build();
        let agent: ureq::Agent = config.into();
        let response = agent
            .get(url)
            .header("User-Agent", concat!("agyva/", env!("CARGO_PKG_VERSION")))
            .header("Accept", "application/vnd.github+json")
            .call()
            .map_err(|e| Error::backend(format!("update request failed: {e}")))?;
        let mut body = Vec::new();
        response
            .into_body()
            .into_reader()
            .take(MAX_DOWNLOAD_BYTES)
            .read_to_end(&mut body)
            .map_err(|e| Error::backend(format!("update request failed: {e}")))?;
        Ok(body)
    }
}

// ------------------------------------------------------------- check + run

/// The startup check over the real network: one metadata request, every
/// failure silent. `enabled` is the resolved config/env gate.
pub fn startup_check(enabled: bool) -> Option<UpdateCheck> {
    if !enabled {
        return None;
    }
    let release = fetch_release(&HttpTransport, CHECK_TIMEOUT).ok()?;
    Some(UpdateCheck::now(newer_version(
        &release.tag_name,
        CURRENT_VERSION,
    )))
}

/// Explicit update with an injectable transport (tests) and optional
/// redirect of the install target (tests never replace a running binary).
pub fn install_update(
    check_only: bool,
    transport: Box<dyn Transport>,
    target_exe: Option<&Path>,
) -> Result<UpdateCheck> {
    run_update(check_only, transport.as_ref(), target_exe)
}

/// Shared engine so tests can redirect the install target away from the
/// running test binary.
fn run_update(
    check_only: bool,
    transport: &dyn Transport,
    target_exe: Option<&Path>,
) -> Result<UpdateCheck> {
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
    let plan = plan_from_release(&release, &latest)?;
    let archive_name = format!("agyva-{latest}-{ASSET_TARGET}.tar.gz");
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
    let bytes = extract_binary(&archive, &latest.to_string())?;
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

fn plan_from_release(release: &Release, latest: &Version) -> Result<UpdatePlan> {
    // Exact names, no ambiguous duplicates.
    let archive_name = format!("agyva-{latest}-{ASSET_TARGET}.tar.gz");
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
    let api_digest = archive.digest.as_deref().and_then(|d| {
        d.strip_prefix("sha256:")
            .filter(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .map(str::to_owned)
    });
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

/// Extract exactly `<stem>/agyva` from the gzipped tar. The whole archive
/// is bounded and inspected: every member must be a safely-named regular
/// file, the binary must appear exactly once and be executable.
fn extract_binary(archive: &[u8], version: &str) -> Result<Zeroizing<Vec<u8>>> {
    let stem = format!("agyva-{version}-{ASSET_TARGET}");
    let wanted = Path::new(&stem).join(EXECUTABLE_NAME);
    let gz = flate2::read::GzDecoder::new(archive);
    let mut tar = tar::Archive::new(gz);
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
        if !entry.header().entry_type().is_file() {
            // Symlinks and hard links have no place in a binary release.
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

    #[test]
    fn release_with_no_stable_assets_cannot_plan() {
        let body = release_json("v9.9.9", serde_json::json!([]));
        let transport = Mapping(vec![(UPDATE_REPO_API, body)]);
        let err = run_update(false, &transport, None).unwrap_err();
        assert!(err.message().contains("unambiguous"));
    }

    #[test]
    fn offline_update_fails_with_backend_error() {
        let err = run_update(true, &Offline, None).unwrap_err();
        assert_eq!(err.message(), "offline");
    }

    #[test]
    fn check_only_never_downloads() {
        let body = release_json("v9.9.9", serde_json::json!([]));
        let transport = Mapping(vec![(UPDATE_REPO_API, body)]);
        let check = run_update(true, &transport, None).unwrap();
        assert_eq!(check.latest.as_deref(), Some("9.9.9"));
        assert!(check.notice().is_some());
    }

    #[test]
    fn up_to_date_install_is_refused_not_forced() {
        let body = release_json("v0.0.1", serde_json::json!([]));
        let transport = Mapping(vec![(UPDATE_REPO_API, body)]);
        let err = run_update(false, &transport, None).unwrap_err();
        assert!(err.message().contains("newest stable release"));
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
        let got = extract_binary(&archive, "0.2.0").unwrap();
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
        assert!(extract_binary(&regular_archive(&[("other-stem/agyva", 0o755)]), "0.2.0").is_err());
        // Non-executable mode.
        assert!(
            extract_binary(
                &regular_archive(&[("agyva-0.2.0-x86_64-unknown-linux-gnu/agyva", 0o644)]),
                "0.2.0"
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
                "0.2.0"
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
        assert!(extract_binary(&gz(&buf), "0.2.0").is_err());
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
}
