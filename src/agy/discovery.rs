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
        .env_remove(crate::storage::vault_key::ENV_NEW_PASSPHRASE)
        .env_remove(crate::storage::vault_key::ENV_ARCHIVE_PASSPHRASE)
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
/// * `auto`: `file` if `AGY_AUTH_STORAGE=file`, or when running under SSH
///   (`SSH_TTY`, `SSH_CLIENT`, `SSH_CONNECTION`) with a non-empty token file;
///   else query Secret Service, falling back to the token file when Secret
///   Service has no credential (and the token file is non-empty), is
///   unavailable (and the token file exists), or is locked/timed out with a
///   non-empty token file.
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
        BackendKind::Auto => select_auto(env, file, &secrets, keyring),
    })
}

/// Antigravity CLI enters static file mode when any of these SSH variables are non-empty.
fn has_ssh_env(env: &Env) -> bool {
    ["SSH_TTY", "SSH_CLIENT", "SSH_CONNECTION"]
        .into_iter()
        .any(|k| env.get(k).is_some())
}

fn file_has_content(file: &FileBackend) -> bool {
    file.exists() && !matches!(file.read(), Ok(None))
}

fn select_auto(
    env: &Env,
    file: FileBackend,
    secrets: &Arc<dyn SecretStore>,
    keyring: impl FnOnce() -> Arc<dyn NativeBackend>,
) -> NativeAuth {
    let file_auth = |f: FileBackend| NativeAuth::new(Arc::new(f), None);
    if env.get("AGY_AUTH_STORAGE") == Some("file") {
        return file_auth(file);
    }
    if has_ssh_env(env) && file_has_content(&file) {
        tracing::info!("SSH session detected; using agy's token file");
        return file_auth(file);
    }
    match secrets.get(super::credentials::SERVICE, super::credentials::USER) {
        Ok(Some(_)) => NativeAuth::new(keyring(), Some(file)),
        Ok(None) if file_has_content(&file) => {
            tracing::info!("no credential in keyring; using agy's token file");
            file_auth(file)
        }
        Err(e) if e.kind == StoreErrorKind::Unavailable && file.exists() => {
            tracing::info!("keyring unavailable; using agy's token file");
            file_auth(file)
        }
        Err(e)
            if matches!(e.kind, StoreErrorKind::Locked | StoreErrorKind::Timeout)
                && matches!(file.read(), Ok(Some(_))) =>
        {
            tracing::info!(kind = ?e.kind, "keyring unusable; using agy's token file");
            file_auth(file)
        }
        _ => NativeAuth::new(keyring(), Some(file)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agy::credentials::{SERVICE, USER};
    use crate::core::error::ErrorKind;
    use crate::storage::keyring::{MemoryKeyring, SetOptions};

    fn temp_paths() -> (tempfile::TempDir, AgyPaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = AgyPaths {
            home: dir.path().join("agy"),
        };
        std::fs::create_dir_all(&paths.home).unwrap();
        (dir, paths)
    }

    fn write_token_file(paths: &AgyPaths, contents: &[u8]) {
        std::fs::write(paths.token_file(), contents).unwrap();
    }

    fn put_keyring(keyring: &MemoryKeyring, contents: &[u8]) {
        keyring
            .set(SERVICE, USER, contents, &SetOptions::default())
            .unwrap();
    }

    #[test]
    fn auto_uses_file_when_keyring_empty_and_token_file_non_empty() {
        let (_dir, paths) = temp_paths();
        write_token_file(&paths, b"file-cred");
        let keyring = Arc::new(MemoryKeyring::new());

        let auth =
            select_backend(BackendKind::Auto, &Env::default(), &paths, keyring, None).unwrap();
        assert_eq!(auth.backend_name(), "file");
        assert_eq!(auth.read().unwrap().unwrap().as_bytes(), b"file-cred");
    }

    #[test]
    fn auto_uses_secret_service_when_keyring_empty_and_token_file_missing_or_whitespace() {
        let (_dir, paths) = temp_paths();
        let keyring = Arc::new(MemoryKeyring::new());

        let auth = select_backend(
            BackendKind::Auto,
            &Env::default(),
            &paths,
            keyring.clone(),
            None,
        )
        .unwrap();
        assert_eq!(auth.backend_name(), "secret-service");
        assert!(auth.read().unwrap().is_none());

        write_token_file(&paths, b"   \n\t ");
        let auth =
            select_backend(BackendKind::Auto, &Env::default(), &paths, keyring, None).unwrap();
        assert_eq!(auth.backend_name(), "secret-service");
        assert!(auth.read().unwrap().is_none());
    }

    #[test]
    fn auto_uses_file_when_keyring_locked_or_timed_out_and_token_file_non_empty() {
        for kind in [StoreErrorKind::Timeout, StoreErrorKind::Locked] {
            let (_dir, paths) = temp_paths();
            write_token_file(&paths, b"file-cred");
            let keyring = Arc::new(MemoryKeyring::new());
            keyring.set_failure(Some(kind));

            let auth =
                select_backend(BackendKind::Auto, &Env::default(), &paths, keyring, None).unwrap();
            assert_eq!(auth.backend_name(), "file");
            assert_eq!(auth.read().unwrap().unwrap().as_bytes(), b"file-cred");
        }
    }

    #[test]
    fn auto_uses_secret_service_when_keyring_locked_and_token_file_missing() {
        let (_dir, paths) = temp_paths();
        let keyring = Arc::new(MemoryKeyring::new());
        keyring.set_failure(Some(StoreErrorKind::Locked));

        let auth =
            select_backend(BackendKind::Auto, &Env::default(), &paths, keyring, None).unwrap();
        assert_eq!(auth.backend_name(), "secret-service");
        let err = auth.read().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Backend);
    }

    #[test]
    fn auto_prefers_file_under_ssh_when_token_file_non_empty_even_if_keyring_populated() {
        for (var, val) in [
            ("SSH_CONNECTION", "10.0.0.1 54321 10.0.0.2 22"),
            ("SSH_CLIENT", "10.0.0.1 54321 22"),
            ("SSH_TTY", "/dev/pts/0"),
        ] {
            let (_dir, paths) = temp_paths();
            write_token_file(&paths, b"file-cred");
            let keyring = Arc::new(MemoryKeyring::new());
            put_keyring(&keyring, b"keyring-cred");
            let env = Env::from_pairs([(var, val)]);

            let auth = select_backend(BackendKind::Auto, &env, &paths, keyring, None).unwrap();
            assert_eq!(auth.backend_name(), "file", "failed for {var}");
            assert_eq!(auth.read().unwrap().unwrap().as_bytes(), b"file-cred");
        }
    }

    #[test]
    fn auto_uses_secret_service_under_ssh_when_token_file_missing() {
        let (_dir, paths) = temp_paths();
        let keyring = Arc::new(MemoryKeyring::new());
        put_keyring(&keyring, b"keyring-cred");
        let env = Env::from_pairs([("SSH_CONNECTION", "10.0.0.1 54321 10.0.0.2 22")]);

        let auth = select_backend(BackendKind::Auto, &env, &paths, keyring, None).unwrap();
        assert_eq!(auth.backend_name(), "secret-service");
        assert_eq!(auth.read().unwrap().unwrap().as_bytes(), b"keyring-cred");
    }

    #[test]
    fn auto_prefers_secret_service_without_ssh_when_both_keyring_and_file_exist() {
        let (_dir, paths) = temp_paths();
        write_token_file(&paths, b"file-cred");
        let keyring = Arc::new(MemoryKeyring::new());
        put_keyring(&keyring, b"keyring-cred");

        let auth =
            select_backend(BackendKind::Auto, &Env::default(), &paths, keyring, None).unwrap();
        assert_eq!(auth.backend_name(), "secret-service");
        assert_eq!(auth.read().unwrap().unwrap().as_bytes(), b"keyring-cred");
    }
}
