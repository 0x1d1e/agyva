//! End-to-end updater tests: the whole `install_update` path against an
//! in-memory transport (never a loopback server, never the network), and a
//! temp-file target so the running test binary is never touched.

use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use agyva::updater::{
    Transport, UPDATE_REPO_API, install_update, run_update_for_target, startup_check_with,
    supported_target,
};

fn test_target() -> &'static str {
    supported_target().unwrap_or("x86_64-unknown-linux-gnu")
}

/// URL-keyed in-memory transport.
struct Mapping(Vec<(String, Vec<u8>)>);

impl Mapping {
    fn with(mut self, url: String, body: Vec<u8>) -> Self {
        self.0.push((url, body));
        self
    }
}

impl Transport for Mapping {
    fn get(&self, url: &str, _t: Duration) -> Result<Vec<u8>, agyva::core::error::Error> {
        self.0
            .iter()
            .find(|(u, _)| u == url)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| agyva::core::error::Error::backend(format!("no mapping for {url}")))
    }
}

struct Offline;
impl Transport for Offline {
    fn get(&self, _u: &str, _t: Duration) -> Result<Vec<u8>, agyva::core::error::Error> {
        Err(agyva::core::error::Error::backend("offline"))
    }
}

/// The release JSON agyva parses.
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

/// A valid release body for `version`: tarball asset + SHA256SUMS whose
/// entries match the real bytes, plus the GitHub-style `sha256:` digest.
fn release_with(version: &str, archive: &[u8]) -> Vec<u8> {
    let target = test_target();
    let archive_name = format!("agyva-{version}-{target}.tar.gz");
    let digest = format!("sha256:{}", hex_digest(archive));
    release_json(
        &format!("v{version}"),
        serde_json::json!([
            {
                "name": archive_name,
                "digest": digest,
                "browser_download_url": format!("https://example.invalid/{archive_name}")
            },
            {
                "name": "SHA256SUMS",
                "browser_download_url": "https://example.invalid/SHA256SUMS"
            }
        ]),
    )
}

fn sums_body(version: &str, archive: &[u8]) -> String {
    let target = test_target();
    let archive_name = format!("agyva-{version}-{target}.tar.gz");
    format!("{}  {archive_name}\n", hex_digest(archive))
}

fn hex_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
}

/// A tar.gz holding `<stem>/agyva` (mode 0755) plus an innocuous extra.
fn archive_with(
    version: &str,
    bytes: &[u8],
    extra: Option<(&str, tar::EntryType, u32)>,
) -> Vec<u8> {
    let target = test_target();
    let stem = format!("agyva-{version}-{target}");
    let exe_name = if target.contains("-windows-") {
        "agyva.exe"
    } else {
        "agyva"
    };
    let mut buf = Vec::new();
    let raw_member = extra
        .filter(|(n, _, _)| n.contains(".."))
        .map(|(n, ty, m)| {
            let mut h = [0u8; 512];
            let nb = n.as_bytes();
            h[..nb.len()].copy_from_slice(nb);
            let mode_oct = format!("{m:07o}\0");
            h[100..108].copy_from_slice(mode_oct.as_bytes());
            h[108..116].copy_from_slice(b"0000000\0");
            h[116..124].copy_from_slice(b"0000000\0");
            h[124..136].copy_from_slice(b"00000000000\0"); // size 0
            h[136..148].copy_from_slice(b"00000000000\0"); // mtime
            h[148..156].copy_from_slice(b"        "); // checksum blank
            h[156] = match ty {
                tar::EntryType::Symlink => b'2',
                _ => b'0',
            };
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
            let sum: u32 = h.iter().map(|b| *b as u32).sum();
            let chk = format!("{sum:06o}\0 ");
            h[148..156].copy_from_slice(chk.as_bytes());
            (h, ty)
        });
    if let Some((raw, _)) = &raw_member {
        buf.extend_from_slice(raw);
        buf.extend_from_slice(&[0u8; 512]); // empty data block
    }
    {
        let mut b = tar::Builder::new(&mut buf);
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o755);
        h.set_entry_type(tar::EntryType::Regular);
        b.append_data(&mut h, format!("{stem}/{exe_name}"), bytes)
            .unwrap();
        if raw_member.is_none()
            && let Some((name, ty, mode)) = extra
        {
            let mut h2 = tar::Header::new_gnu();
            h2.set_size(0);
            h2.set_mode(mode);
            h2.set_entry_type(ty);
            if ty == tar::EntryType::Symlink {
                h2.set_link_name("../outside").unwrap();
            }
            b.append_data(&mut h2, name, std::io::empty()).unwrap();
        }
        b.finish().unwrap();
    }
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(&buf).unwrap();
    e.finish().unwrap()
}

fn install_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("agyva-it-upd-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("install dir");
    dir
}

// ------------------------------------------------------------ gating

#[test]
fn semver_gating_across_tags() {
    // Newer stable tag reports an update; equal, older and prerelease do
    // not (check-only path never downloads).
    let cases = [
        ("v0.2.0", Some("0.2.0")),
        ("v0.1.0", None),
        ("v0.0.9", None),
        ("v0.2.0-rc.1", None),
        ("garbage", None),
    ];
    for (tag, expect) in cases {
        let t = Mapping(vec![]).with(
            UPDATE_REPO_API.into(),
            release_json(tag, serde_json::json!([])),
        );
        let check = install_update(true, Box::new(t), None).expect("check");
        assert_eq!(check.latest.as_deref(), expect, "tag {tag}");
    }
}

#[test]
fn offline_and_empty_release_fail_cleanly() {
    let err = install_update(true, Box::new(Offline), None).expect_err("offline");
    assert_eq!(err.message(), "offline");

    let t = Mapping(vec![]).with(
        UPDATE_REPO_API.into(),
        release_json("v9.9.9", serde_json::json!([])),
    );
    let err = install_update(false, Box::new(t), None).expect_err("no assets");
    assert!(err.message().contains("unambiguous"), "{}", err.message());
}

// ------------------------------------------------------------ install

#[test]
fn a_full_install_replaces_a_temp_target() {
    let version = "9.9.9";
    let payload = b"new-binary-bytes";
    let archive = archive_with(version, payload, None);
    let sums = sums_body(version, &archive);
    let target = test_target();
    let t = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), release_with(version, &archive))
        .with(
            format!("https://example.invalid/agyva-{version}-{target}.tar.gz"),
            archive.clone(),
        )
        .with(
            "https://example.invalid/SHA256SUMS".into(),
            sums.into_bytes(),
        );

    let dir = install_dir("happy");
    let exe = dir.join("agyva");
    std::fs::write(&exe, b"old-binary").expect("old exe");

    let check = install_update(false, Box::new(t), Some(&exe)).expect("install");
    assert_eq!(check.latest.as_deref(), Some(version));
    assert_eq!(std::fs::read(&exe).unwrap(), payload, "binary replaced");
    #[cfg(unix)]
    {
        let mode = std::fs::metadata(&exe).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "installed executable");
    }
    // No staging leftover.
    let leftovers: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".agyva.update"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_checksum_mismatch_installs_nothing() {
    let version = "9.9.8";
    let archive = archive_with(version, b"evil", None);
    let target = test_target();
    let wrong_sums = format!("{}  agyva-{version}-{target}.tar.gz\n", "0".repeat(64));
    let t = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), release_with(version, &archive))
        .with(
            format!("https://example.invalid/agyva-{version}-{target}.tar.gz"),
            archive,
        )
        .with(
            "https://example.invalid/SHA256SUMS".into(),
            wrong_sums.into_bytes(),
        );

    let dir = install_dir("sums");
    let exe = dir.join("agyva");
    std::fs::write(&exe, b"keep-me").expect("old exe");

    let err = install_update(false, Box::new(t), Some(&exe)).expect_err("mismatch");
    assert!(
        err.message().contains("does not match SHA256SUMS"),
        "{}",
        err.message()
    );
    assert_eq!(std::fs::read(&exe).unwrap(), b"keep-me", "target untouched");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_api_digest_mismatch_installs_nothing() {
    let version = "9.9.7";
    let archive = archive_with(version, b"bytes", None);
    let target = test_target();
    // SUMS match the archive but the GitHub digest claims something else.
    let body = release_json(
        &format!("v{version}"),
        serde_json::json!([
            {
                "name": format!("agyva-{version}-{target}.tar.gz"),
                "digest": format!("sha256:{}", "f".repeat(64)),
                "browser_download_url":
                    format!("https://example.invalid/agyva-{version}-{target}.tar.gz")
            },
            { "name": "SHA256SUMS", "browser_download_url": "https://example.invalid/SHA256SUMS" }
        ]),
    );
    let sums = sums_body(version, &archive);
    let t = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), body)
        .with(
            format!("https://example.invalid/agyva-{version}-{target}.tar.gz"),
            archive,
        )
        .with(
            "https://example.invalid/SHA256SUMS".into(),
            sums.into_bytes(),
        );

    let dir = install_dir("api");
    let exe = dir.join("agyva");
    std::fs::write(&exe, b"keep-me").expect("old exe");
    let err = install_update(false, Box::new(t), Some(&exe)).expect_err("api digest");
    assert!(
        err.message().contains("does not match the release digest"),
        "{}",
        err.message()
    );
    assert_eq!(std::fs::read(&exe).unwrap(), b"keep-me");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn unsafe_archive_members_are_refused() {
    let version = "9.9.6";

    // A symlink member anywhere: refused.
    let symlinked = archive_with(
        version,
        b"bytes",
        Some(("extra", tar::EntryType::Symlink, 0o777)),
    );
    let sums = sums_body(version, &symlinked);
    let target = test_target();
    let t = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), release_with(version, &symlinked))
        .with(
            format!("https://example.invalid/agyva-{version}-{target}.tar.gz"),
            symlinked.clone(),
        )
        .with(
            "https://example.invalid/SHA256SUMS".into(),
            sums.into_bytes(),
        );
    let dir = install_dir("symlink");
    let exe = dir.join("agyva");
    std::fs::write(&exe, b"keep-me").expect("old exe");
    let err = install_update(false, Box::new(t), Some(&exe)).expect_err("symlink");
    assert!(
        err.message().contains("non-regular entry"),
        "{}",
        err.message()
    );
    assert_eq!(std::fs::read(&exe).unwrap(), b"keep-me");

    // A traversal member: refused.
    let traversal = archive_with(
        version,
        b"bytes",
        Some(("../escape", tar::EntryType::Regular, 0o755)),
    );
    let sums = sums_body(version, &traversal);
    let t = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), release_with(version, &traversal))
        .with(
            format!("https://example.invalid/agyva-{version}-{target}.tar.gz"),
            traversal.clone(),
        )
        .with(
            "https://example.invalid/SHA256SUMS".into(),
            sums.into_bytes(),
        );
    let err = install_update(false, Box::new(t), Some(&exe)).expect_err("traversal");
    assert!(err.message().contains("unsafe path"), "{}", err.message());
    assert_eq!(std::fs::read(&exe).unwrap(), b"keep-me");

    // Up-to-date version: refused without downloading.
    let up_to_date = Mapping(vec![]).with(
        UPDATE_REPO_API.into(),
        release_json(
            &format!("v{}", env!("CARGO_PKG_VERSION")),
            serde_json::json!([]),
        ),
    );
    let err = install_update(false, Box::new(up_to_date), Some(&exe)).expect_err("up to date");
    assert!(
        err.message().contains("already the newest stable release"),
        "{}",
        err.message()
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

struct CountingTransport {
    calls: std::sync::atomic::AtomicUsize,
    body: Option<Vec<u8>>,
}

impl CountingTransport {
    fn ok(body: Vec<u8>) -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            body: Some(body),
        }
    }

    fn offline() -> Self {
        Self {
            calls: std::sync::atomic::AtomicUsize::new(0),
            body: None,
        }
    }

    fn count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl Transport for CountingTransport {
    fn get(&self, _url: &str, _t: Duration) -> Result<Vec<u8>, agyva::core::error::Error> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.body
            .clone()
            .ok_or_else(|| agyva::core::error::Error::backend("offline"))
    }
}

#[test]
fn unsupported_target_is_refused_without_touching_transport_or_exe() {
    let t = CountingTransport::ok(release_json("v9.9.9", serde_json::json!([])));
    let dir = install_dir("unsupported");
    let exe = dir.join("agyva");
    std::fs::write(&exe, b"keep-me").expect("old exe");

    let err = run_update_for_target(false, &t, Some(&exe), None).expect_err("unsupported install");
    assert_eq!(err.kind(), agyva::core::error::ErrorKind::Refused);
    assert!(err.message().contains("not supported"), "{}", err.message());

    let err_check = run_update_for_target(true, &t, None, None).expect_err("unsupported check");
    assert_eq!(err_check.kind(), agyva::core::error::ErrorKind::Refused);
    assert_eq!(t.count(), 0, "transport must not be contacted");
    assert_eq!(std::fs::read(&exe).unwrap(), b"keep-me");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn startup_check_disk_cache_respects_ttls_and_version_invalidation() {
    if supported_target().is_none() {
        return;
    }
    let dir = install_dir("cache");
    let cache = dir.join("cache").join("update-check.json");
    let t0 = chrono::Utc::now();

    // Cache hit within 24h calls transport only once.
    let t = CountingTransport::ok(release_json("v9.9.9", serde_json::json!([])));
    let c1 = startup_check_with(true, Some(&cache), &t, t0).expect("first check");
    assert_eq!(c1.latest.as_deref(), Some("9.9.9"));
    let c2 = startup_check_with(true, Some(&cache), &t, t0 + chrono::Duration::hours(23))
        .expect("cached check");
    assert_eq!(c2.latest.as_deref(), Some("9.9.9"));
    assert_eq!(t.count(), 1);

    // After 24h, TTL expires and transport is called again.
    let c3 = startup_check_with(true, Some(&cache), &t, t0 + chrono::Duration::hours(24))
        .expect("expired check");
    assert_eq!(c3.latest.as_deref(), Some("9.9.9"));
    assert_eq!(t.count(), 2);

    // Cached version no longer newer than CURRENT_VERSION yields latest: None without hitting transport.
    let current_cached = serde_json::json!({
        "schema_version": 1,
        "checked_at": t0,
        "latest": env!("CARGO_PKG_VERSION"),
        "ok": true
    });
    std::fs::write(&cache, current_cached.to_string()).unwrap();
    let invalidated = startup_check_with(true, Some(&cache), &t, t0 + chrono::Duration::minutes(1))
        .expect("invalidated cache returns UpdateCheck with latest: None");
    assert_eq!(invalidated.latest, None);
    assert_eq!(t.count(), 2);

    // Offline failure caches for 1h.
    let err_cache = dir.join("cache").join("offline-check.json");
    let offline = CountingTransport::offline();
    assert!(startup_check_with(true, Some(&err_cache), &offline, t0).is_none());
    assert!(
        startup_check_with(
            true,
            Some(&err_cache),
            &offline,
            t0 + chrono::Duration::minutes(59)
        )
        .is_none()
    );
    assert_eq!(offline.count(), 1);
    assert!(
        startup_check_with(
            true,
            Some(&err_cache),
            &offline,
            t0 + chrono::Duration::hours(1)
        )
        .is_none()
    );
    assert_eq!(offline.count(), 2);

    std::fs::remove_dir_all(&dir).unwrap();
}
