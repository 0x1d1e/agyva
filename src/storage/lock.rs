//! Interprocess lock (`state/agyva.lock`, advisory flock via fs4).
//!
//! Held for the whole duration of every mutating operation. Acquisition polls
//! until `timeout`, then fails with a clear "another agyva is running" error.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::thread::sleep;
use std::time::{Duration, Instant};

use fs4::{FileExt, TryLockError};

use super::fsutil;
use crate::core::error::{Error, ErrorKind, Result};

#[derive(Debug)]
pub struct StateLock {
    file: File,
}

impl StateLock {
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        if let Some(dir) = path.parent() {
            fsutil::create_private_dir(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(fsutil::PRIVATE_FILE_MODE);
        }
        let mut file = options
            .open(path)
            .map_err(|e| Error::io(format!("opening {}", path.display()), e))?;
        let start = Instant::now();
        loop {
            match FileExt::try_lock(&file) {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => {
                    return Err(Error::io(format!("locking {}", path.display()), e));
                }
            }
            if start.elapsed() >= timeout {
                return Err(Self::busy_error(&mut file));
            }
            sleep(Duration::from_millis(50));
        }
        // Informational only: the holder's pid, for the error message above.
        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = write!(file, "{}", std::process::id());
        let _ = file.flush();
        Ok(Self { file })
    }

    fn busy_error(file: &mut File) -> Error {
        let mut text = String::new();
        let _ = file.seek(SeekFrom::Start(0));
        let _ = Read::by_ref(file).take(32).read_to_string(&mut text);
        let holder = text.trim().parse::<u32>().ok();
        let who = holder.map_or_else(String::new, |p| format!(" (pid {p})"));
        Error::new(ErrorKind::Locked, format!("another agyva is running{who}"))
            .hint("wait for it to finish, or raise `lock_timeout_secs` with `agyva config set`")
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_holder_times_out_with_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("agyva.lock");
        let first = StateLock::acquire(&path, Duration::from_millis(100)).unwrap();
        let err = StateLock::acquire(&path, Duration::from_millis(150)).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Locked);
        assert!(err.message().contains("another agyva is running"));
        drop(first);
        assert!(StateLock::acquire(&path, Duration::from_millis(100)).is_ok());
    }
}
