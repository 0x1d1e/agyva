//! Locating `agy`, asking it for its version, and choosing the native
//! credential backend.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::compatibility::{AgyVersion, Compat, parse_version};
use super::credentials::{
    FileBackend, MemoryBackend, NativeAuth, NativeBackend, SecretServiceBackend,
};
use crate::config::BackendKind;
use crate::core::error::{Error, Result};
use crate::paths::{AgyPaths, ENV_AGY_HOME, ENV_CACHE_DIR, ENV_CONFIG_DIR, ENV_STATE_DIR, Env};
use crate::storage::keyring::{SecretStore, StoreErrorKind};

pub const ENV_AGY_BIN: &str = "AGYVA_AGY_BIN";
pub const ENV_BACKEND: &str = "AGYVA_BACKEND";
/// `AGYVA_BACKEND` swaps agy's credential store for a file or memory and
/// seals the OS keyring. It is a sandbox hook, so it is honored only when
/// every state location is redirected too; otherwise a stray variable could
/// make agyva act on a mix of real and fake state.
pub fn sandbox(env: &Env) -> Result<bool> {
    if env.get(ENV_BACKEND).is_none() {
        return Ok(false);
    }
    let missing: Vec<&str> = [ENV_CONFIG_DIR, ENV_STATE_DIR, ENV_CACHE_DIR, ENV_AGY_HOME]
        .into_iter()
        .filter(|v| env.get(v).is_none())
        .collect();
    if missing.is_empty() {
        return Ok(true);
    }
    Err(Error::usage(format!(
        "{ENV_BACKEND} is a sandbox hook and also needs {} to be set",
        missing.join(", ")
    ))
    .hint(format!(
        "unset {ENV_BACKEND} for normal use; for a sandbox, set {ENV_CONFIG_DIR}, {ENV_STATE_DIR}, {ENV_CACHE_DIR} and {ENV_AGY_HOME} to scratch directories"
    )))
}

const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// Find the `agy` executable: `AGYVA_AGY_BIN`, else the first match on `PATH`.
pub fn find_agy(env: &Env) -> Option<PathBuf> {
    if let Some(p) = env.get(ENV_AGY_BIN) {
        return Some(PathBuf::from(p));
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join("agy"))
        .find(|c| is_executable(c))
}

fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Run `<agy> --version` (bounded, no stdin) and parse the result.
pub fn query_version(agy: &Path) -> std::result::Result<AgyVersion, String> {
    let mut child = Command::new(agy)
        .env_remove(crate::storage::vault_key::ENV_PASSPHRASE)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", agy.display()))?;
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() < VERSION_TIMEOUT => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err("`agy --version` timed out".into());
            }
            Err(e) => return Err(format!("waiting for agy: {e}")),
        }
    }
    let mut out = String::new();
    if let Some(mut so) = child.stdout.take() {
        let _ = so.by_ref().take(4096).read_to_string(&mut out);
    }
    parse_version(&out).ok_or_else(|| "could not parse `agy --version` output".to_owned())
}

/// Detect agy and assess compatibility.
pub fn detect_compat(env: &Env) -> Compat {
    match find_agy(env) {
        None => Compat::assess(Err(
            "agy not found on PATH (set AGYVA_AGY_BIN to override)".into()
        )),
        Some(p) => Compat::assess(query_version(&p)),
    }
}

/// Choose the backend that holds agy's live credential.
///
/// * `memory`: in-process only (tests).
/// * `file`: the token file, as `AGY_AUTH_STORAGE=file` makes agy do.
/// * `secret-service`: the keyring item `gemini`/`antigravity`.
/// * `auto`: `file` if `AGY_AUTH_STORAGE=file`; else Secret Service, falling
///   back to the file only when the keyring is unreachable *and* the token
///   file exists.
pub fn select_backend(
    kind: BackendKind,
    env: &Env,
    paths: &AgyPaths,
    secrets: Arc<dyn SecretStore>,
    memory: Option<MemoryBackend>,
) -> Result<NativeAuth> {
    sandbox(env)?;
    let file = FileBackend::new(paths.token_file());
    let keyring =
        || -> Arc<dyn NativeBackend> { Arc::new(SecretServiceBackend::new(secrets.clone())) };
    let kind = match env.get(ENV_BACKEND) {
        Some(v) => v
            .parse::<BackendKind>()
            .map_err(|e| Error::usage(format!("invalid {ENV_BACKEND}: {e}")))?,
        None => kind,
    };
    Ok(match kind {
        BackendKind::Memory => {
            let m = memory.unwrap_or_default();
            NativeAuth::new(Arc::new(m), None)
        }
        BackendKind::File => NativeAuth::new(Arc::new(file), None),
        BackendKind::SecretService => NativeAuth::new(keyring(), Some(file)),
        BackendKind::Auto => {
            if env.get("AGY_AUTH_STORAGE") == Some("file") {
                NativeAuth::new(Arc::new(file), None)
            } else {
                match secrets.get(super::credentials::SERVICE, super::credentials::USER) {
                    Err(e) if e.kind == StoreErrorKind::Unavailable && file.exists() => {
                        tracing::info!("keyring unavailable; using agy's token file");
                        NativeAuth::new(Arc::new(file), None)
                    }
                    _ => NativeAuth::new(keyring(), Some(file)),
                }
            }
        }
    })
}
