//! End-to-end updater tests: the whole `install_update` path against an
//! in-memory transport (never a loopback server, never the network), and a
//! temp-file target so the running test binary is never touched.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use agyva::updater::{
    MAX_BINARY_BYTES, Transport, UPDATE_REPO_API, install_update, run_update_for_target,
    startup_check_with, supported_target,
};

fn test_target() -> &'static str {
    supported_target().unwrap_or("x86_64-unknown-linux-gnu")
}

fn asset_name_for(version: &str, target: &str) -> String {
    if target.contains("-windows-") {
        format!("agyva-{version}-{target}.exe")
    } else {
        format!("agyva-{version}-{target}")
    }
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

/// A valid release body for `version`: raw binary asset + SHA256SUMS whose
/// entries match the real bytes, plus the GitHub-style `sha256:` digest.
fn release_with_target(version: &str, target: &str, binary: &[u8]) -> Vec<u8> {
    let asset_name = asset_name_for(version, target);
    let digest = format!("sha256:{}", hex_digest(binary));
    release_json(
        &format!("v{version}"),
        serde_json::json!([
            {
                "name": asset_name,
                "digest": digest,
                "browser_download_url": format!("https://example.invalid/{asset_name}")
            },
            {
                "name": "SHA256SUMS",
                "browser_download_url": "https://example.invalid/SHA256SUMS"
            }
        ]),
    )
}

fn release_with(version: &str, binary: &[u8]) -> Vec<u8> {
    release_with_target(version, test_target(), binary)
}

fn sums_body_for(version: &str, target: &str, binary: &[u8]) -> String {
    let asset_name = asset_name_for(version, target);
    format!("{}  {asset_name}\n", hex_digest(binary))
}

fn sums_body(version: &str, binary: &[u8]) -> String {
    sums_body_for(version, test_target(), binary)
}

fn hex_digest(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(bytes))
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
    let sums = sums_body(version, payload);
    let target = test_target();
    let asset = asset_name_for(version, target);
    let t = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), release_with(version, payload))
        .with(format!("https://example.invalid/{asset}"), payload.to_vec())
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
    let binary = b"evil";
    let target = test_target();
    let asset = asset_name_for(version, target);
    let wrong_sums = format!("{}  {asset}\n", "0".repeat(64));
    let t = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), release_with(version, binary))
        .with(format!("https://example.invalid/{asset}"), binary.to_vec())
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
    let binary = b"bytes";
    let target = test_target();
    let asset = asset_name_for(version, target);
    // SUMS match the binary but the GitHub digest claims something else.
    let body = release_json(
        &format!("v{version}"),
        serde_json::json!([
            {
                "name": asset,
                "digest": format!("sha256:{}", "f".repeat(64)),
                "browser_download_url": format!("https://example.invalid/{asset}")
            },
            { "name": "SHA256SUMS", "browser_download_url": "https://example.invalid/SHA256SUMS" }
        ]),
    );
    let sums = sums_body(version, binary);
    let t = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), body)
        .with(format!("https://example.invalid/{asset}"), binary.to_vec())
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
fn empty_and_oversize_binaries_are_refused() {
    let version = "9.9.6";
    let target = test_target();
    let asset = asset_name_for(version, target);

    let dir = install_dir("bounds");
    let exe = dir.join("agyva");
    std::fs::write(&exe, b"keep-me").expect("old exe");

    // Empty binary is refused even with matching SHA256SUMS and API digest.
    let empty_sums = sums_body(version, b"");
    let t_empty = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), release_with(version, b""))
        .with(format!("https://example.invalid/{asset}"), Vec::new())
        .with(
            "https://example.invalid/SHA256SUMS".into(),
            empty_sums.into_bytes(),
        );
    let err = install_update(false, Box::new(t_empty), Some(&exe)).expect_err("empty");
    assert!(err.message().contains("empty"), "{}", err.message());
    assert_eq!(std::fs::read(&exe).unwrap(), b"keep-me");

    // Oversize binary (> MAX_BINARY_BYTES) is refused without touching exe.
    let oversize = vec![0u8; MAX_BINARY_BYTES as usize + 1];
    let oversize_sums = sums_body(version, &oversize);
    let t_oversize = Mapping(vec![])
        .with(UPDATE_REPO_API.into(), release_with(version, &oversize))
        .with(format!("https://example.invalid/{asset}"), oversize)
        .with(
            "https://example.invalid/SHA256SUMS".into(),
            oversize_sums.into_bytes(),
        );
    let err = install_update(false, Box::new(t_oversize), Some(&exe)).expect_err("oversize");
    assert!(err.message().contains("too large"), "{}", err.message());
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

#[test]
fn all_six_supported_targets_plan_and_windows_uses_exe_asset() {
    let version = "9.9.5";
    let payload = b"binary-payload";
    let dir = install_dir("six-targets");

    let all_targets = [
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "x86_64-apple-darwin",
        "aarch64-apple-darwin",
        "x86_64-pc-windows-msvc",
        "aarch64-pc-windows-msvc",
    ];
    for target in all_targets {
        let asset = asset_name_for(version, target);
        if target.contains("-windows-") {
            assert!(asset.ends_with(".exe"), "{asset}");
        } else {
            assert!(!asset.ends_with(".exe"), "{asset}");
        }
        let sums = sums_body_for(version, target, payload);
        let t = Mapping(vec![])
            .with(
                UPDATE_REPO_API.into(),
                release_with_target(version, target, payload),
            )
            .with(format!("https://example.invalid/{asset}"), payload.to_vec())
            .with(
                "https://example.invalid/SHA256SUMS".into(),
                sums.into_bytes(),
            );
        let exe = dir.join(format!("agyva-{target}"));
        std::fs::write(&exe, b"old").unwrap();
        let check = run_update_for_target(false, &t, Some(&exe), Some(target)).expect(target);
        assert_eq!(check.latest.as_deref(), Some(version));
        assert_eq!(std::fs::read(&exe).unwrap(), payload);
    }

    // Windows target refuses an asset missing the .exe suffix.
    for win_target in ["x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc"] {
        let bad_name = format!("agyva-{version}-{win_target}");
        let bad_release = release_json(
            &format!("v{version}"),
            serde_json::json!([
                {
                    "name": bad_name,
                    "browser_download_url": format!("https://example.invalid/{bad_name}")
                },
                {
                    "name": "SHA256SUMS",
                    "browser_download_url": "https://example.invalid/SHA256SUMS"
                }
            ]),
        );
        let t = Mapping(vec![]).with(UPDATE_REPO_API.into(), bad_release);
        let err =
            run_update_for_target(false, &t, None, Some(win_target)).expect_err("missing .exe");
        assert!(err.message().contains(".exe"), "{}", err.message());
    }

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
