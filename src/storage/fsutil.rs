//! Filesystem helpers: private directories and crash-safe atomic writes.
//!
//! Atomic write protocol: write a temp file in the same directory, fsync the
//! file, rename over the target, fsync the directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub const PRIVATE_FILE_MODE: u32 = 0o600;
pub const PRIVATE_DIR_MODE: u32 = 0o700;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Create `path` (and parents) as a private directory (0700) that we own, and
/// tighten the leaf's permissions if it already existed with a wider mode.
pub fn create_private_dir(path: &Path) -> io::Result<()> {
    create_dir_with_mode(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(path)?;
        if meta.permissions().mode() & 0o777 != PRIVATE_DIR_MODE {
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIR_MODE))?;
        }
    }
    Ok(())
}

/// Create `path` and parents if missing, without touching an existing
/// directory's permissions (used for directories owned by `agy`).
pub fn create_dir_with_mode(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(PRIVATE_DIR_MODE);
    }
    builder.create(path)
}

/// Atomically replace `path` with `data`, mode 0600.
pub fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    atomic_write_mode(path, data, PRIVATE_FILE_MODE)
}

pub fn atomic_write_mode(path: &Path, data: &[u8], mode: u32) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = write_temp_and_rename(&tmp, path, data, mode, dir);
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn write_temp_and_rename(
    tmp: &Path,
    path: &Path,
    data: &[u8],
    mode: u32,
    dir: &Path,
) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let mut file = options.open(tmp)?;
    file.write_all(data)?;
    file.sync_all()?;
    drop(file);
    fs::rename(tmp, path)?;
    sync_dir(dir)
}

/// fsync a directory so a preceding rename/unlink is durable.
pub fn sync_dir(dir: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(dir)?.sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Read a file, mapping "not found" to `None`.
pub fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Remove a file if present and make the removal durable. Returns whether a
/// file was removed.
pub fn remove_durable(path: &Path) -> io::Result<bool> {
    match fs::remove_file(path) {
        Ok(()) => {
            if let Some(dir) = path.parent() {
                sync_dir(dir)?;
            }
            Ok(true)
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}
/// Create `path` with `data`, mode 0600, refusing to touch an existing
/// name. The bytes are written to a private temp file in the same
/// directory, fsynced, then hard-linked to `path` (which fails with
/// `AlreadyExists` when the name is taken) and the directory is fsynced.
/// Unlike `exists()`-then-`rename()` this cannot lose a file created in
/// between; on any failure the temp file is cleaned up and `path` is
/// never created or modified.
pub fn atomic_create(path: &Path, data: &[u8]) -> io::Result<()> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?
        .to_string_lossy();
    let tmp = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(PRIVATE_FILE_MODE);
        }
        let mut file = options.open(&tmp)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        fs::hard_link(&tmp, path)?;
        sync_dir(dir)
    })();
    // The temp name is always ours to remove: on failure so no debris is
    // left behind, on success because `path` now shares its inode.
    let removed = fs::remove_file(&tmp);
    match result {
        Ok(()) => {
            removed?;
            sync_dir(dir)
        }
        Err(e) => {
            let _ = removed;
            Err(e)
        }
    }
}

/// Permission bits (`mode & 0o777`) of `path`, or `None` where unsupported.
pub fn mode_of(path: &Path) -> Option<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .ok()
            .map(|m| m.permissions().mode() & 0o777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Files in `dir` with the given extension, sorted by file name.
pub fn list_with_extension(dir: &Path, ext: &str) -> io::Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(out),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().is_some_and(|e| e == ext)
            && !path
                .file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with('.'))
        {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "agyva-fsutil-{name}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).expect("dir");
        d
    }

    /// Success must not leave the temp hard-link name behind, and a taken
    /// destination name must be refused with the existing file untouched.
    #[test]
    fn atomic_create_leaves_no_temp_file() {
        let d = dir("create");
        let path = d.join("account.snap");
        atomic_create(&path, b"one").expect("create");
        assert_eq!(fs::read(&path).unwrap(), b"one");
        let names: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["account.snap".to_owned()], "{names:?}");

        let err = atomic_create(&path, b"two").expect_err("refuses an existing name");
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&path).unwrap(), b"one");
        let names: Vec<String> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["account.snap".to_owned()], "{names:?}");
        fs::remove_dir_all(&d).unwrap();
    }
}
