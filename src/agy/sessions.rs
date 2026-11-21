//! Best-effort detection of running agy sessions. agyva only ever *warns*;
//! it never signals or kills a process.

use fs4::{FileExt, TryLockError};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

use crate::paths::AgyPaths;

/// How far detection may look for a running session.
///
/// The process table is the host's, not the caller's: a sandboxed harness
/// (or a test suite) must be able to ask only about the AGY home it owns,
/// otherwise any unrelated process named `agy` on the machine is reported
/// as the user's own session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionScan {
    /// Production default: the AGY home's presence locks plus the host
    /// process table, so `agyva status` sees session processes elsewhere.
    #[default]
    ProcessTable,
    /// Presence locks only. Used by the test harness, whose fake `agy`
    /// processes would otherwise be indistinguishable from real sessions.
    PresenceOnly,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SessionReport {
    /// Active `presence/*.lock` files (flock-held by a live session).
    pub presence_locks: usize,
    /// Stale `presence/*.lock` files left behind by exited sessions.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub stale_presence_locks: usize,
    /// PIDs of processes whose name is exactly `agy`.
    pub agy_pids: Vec<u32>,
    /// Why the check could not be completed (unreadable presence directory,
    /// an empty process table). `None` means the answer is known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncertain: Option<String>,
}

impl SessionReport {
    /// Positive evidence that agy is running.
    pub fn any(&self) -> bool {
        self.presence_locks > 0 || !self.agy_pids.is_empty()
    }

    /// Whether the check actually completed. Automation defers when it did
    /// not: "not detected" and "could not look" are different answers.
    pub fn is_known(&self) -> bool {
        self.uncertain.is_none()
    }

    pub fn warning(&self) -> Option<String> {
        if let Some(why) = &self.uncertain {
            return Some(format!(
                "could not check for running agy sessions ({why}); any session that IS running keeps its old login."
            ));
        }
        if !self.any() {
            return None;
        }
        let mut parts = Vec::new();
        if !self.agy_pids.is_empty() {
            parts.push(format!("{} agy process(es) running", self.agy_pids.len()));
        }
        if self.presence_locks > 0 {
            parts.push(format!("{} presence lock file(s)", self.presence_locks));
        }
        Some(format!(
            "agy appears to be running ({}); running sessions keep their old login. Restart agy to use the new account.",
            parts.join(", ")
        ))
    }
}

pub fn detect(paths: &AgyPaths, scan: SessionScan) -> SessionReport {
    // An AGY home that has never existed really has no sessions; one that
    // exists but cannot be read is an unknown answer, never "no sessions".
    let (presence_locks, stale_presence_locks, uncertain) =
        match std::fs::read_dir(paths.presence_dir()) {
            Ok(rd) => probe_presence_locks(rd),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => (0, 0, None),
            Err(e) => (
                0,
                0,
                Some(format!(
                    "presence directory {}: {e}",
                    paths.presence_dir().display()
                )),
            ),
        };
    // Skipping the scan is a deliberate "not looked at", not a failure to
    // look: it never sets `uncertain`, so the answer stays known.
    let (agy_pids, scan_uncertain) = match scan {
        SessionScan::ProcessTable => detect_processes(),
        SessionScan::PresenceOnly => (Vec::new(), None),
    };
    SessionReport {
        presence_locks,
        stale_presence_locks,
        agy_pids,
        uncertain: uncertain.or(scan_uncertain),
    }
}

fn is_lock_contended(err: &TryLockError) -> bool {
    match err {
        TryLockError::WouldBlock => true,
        TryLockError::Error(e) => {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                return true;
            }
            #[cfg(unix)]
            {
                e.raw_os_error() == Some(rustix::io::Errno::AGAIN.raw_os_error())
            }
            #[cfg(not(unix))]
            {
                false
            }
        }
    }
}

fn probe_presence_locks(rd: std::fs::ReadDir) -> (usize, usize, Option<String>) {
    let mut active = 0usize;
    let mut stale = 0usize;
    let mut uncertain: Option<String> = None;

    for entry in rd {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                uncertain.get_or_insert_with(|| format!("reading presence directory: {e}"));
                continue;
            }
        };
        let path = entry.path();
        if path.extension().is_none_or(|x| x != "lock") {
            continue;
        }
        let file = match std::fs::File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                uncertain.get_or_insert_with(|| format!("presence lock {}: {e}", path.display()));
                continue;
            }
        };
        match FileExt::try_lock_shared(&file) {
            Ok(()) => {
                let _ = FileExt::unlock(&file);
                stale += 1;
            }
            Err(ref e) if is_lock_contended(e) => {
                active += 1;
            }
            Err(TryLockError::Error(e)) => {
                uncertain.get_or_insert_with(|| {
                    format!("probing presence lock {}: {e}", path.display())
                });
            }
            Err(TryLockError::WouldBlock) => {
                active += 1;
            }
        }
    }

    (active, stale, uncertain)
}

fn detect_processes() -> (Vec<u32>, Option<String>) {
    let mut sys = System::new();
    let seen = sys.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing().with_user(UpdateKind::OnlyIfNotSet),
    );
    let current_pid = sysinfo::get_current_pid().ok();
    #[cfg(unix)]
    let euid = rustix::process::geteuid().as_raw();
    let mut agy_pids: Vec<u32> = sys
        .processes()
        .iter()
        .filter(|(pid, p)| {
            if Some(**pid) == current_pid {
                return false;
            }
            if p.name() != "agy" {
                return false;
            }
            #[cfg(unix)]
            {
                p.user_id().is_some_and(|u| **u == euid)
            }
            #[cfg(not(unix))]
            {
                let _ = p;
                true
            }
        })
        .map(|(pid, _)| pid.as_u32())
        .collect();
    agy_pids.sort_unstable();
    // Every Unix host has at least an init process; zero means the
    // process table could not be read, not that agy is absent.
    let uncertain = (seen == 0 && agy_pids.is_empty())
        .then(|| "the process table could not be read".to_owned());
    (agy_pids, uncertain)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(tmp: &std::path::Path) -> AgyPaths {
        AgyPaths {
            home: tmp.join("agyhome"),
        }
    }

    /// Production must keep looking: the scan is the default, and building a
    /// context picks it up unchanged.
    #[test]
    fn default_scan_is_the_process_table() {
        assert_eq!(SessionScan::default(), SessionScan::ProcessTable);
        let tmp = tempfile::tempdir().unwrap();
        let app = crate::core::app::App::new(
            crate::paths::Dirs::under(tmp.path()),
            paths(tmp.path()),
            crate::paths::Env::from_pairs([]),
            crate::config::Config::default(),
        );
        assert_eq!(app.session_scan, SessionScan::ProcessTable);
        // And narrowing it is an explicit, opt-in choice.
        let narrowed = app.with_session_scan(SessionScan::PresenceOnly);
        assert_eq!(narrowed.session_scan, SessionScan::PresenceOnly);
    }

    /// Skipping the scan is "did not look", never "could not look": the
    /// answer stays known so automation keeps working.
    #[test]
    fn skipped_scan_is_a_known_answer() {
        let tmp = tempfile::tempdir().unwrap();
        let report = detect(&paths(tmp.path()), SessionScan::PresenceOnly);
        assert!(report.is_known(), "{report:?}");
        assert!(report.uncertain.is_none(), "{report:?}");
        assert!(!report.any());
        let real = detect(&paths(tmp.path()), SessionScan::ProcessTable);
        assert!(real.is_known(), "{real:?}");
    }

    #[test]
    fn unlocked_stale_presence_lock_is_ignored_and_counted() {
        let tmp = tempfile::tempdir().unwrap();
        let agy_paths = paths(tmp.path());
        std::fs::create_dir_all(agy_paths.presence_dir()).unwrap();
        std::fs::write(agy_paths.presence_dir().join("stale.lock"), "").unwrap();

        let report = detect(&agy_paths, SessionScan::PresenceOnly);
        assert!(report.is_known(), "{report:?}");
        assert!(!report.any(), "{report:?}");
        assert_eq!(report.presence_locks, 0);
        assert_eq!(report.stale_presence_locks, 1);
    }

    #[test]
    fn exclusively_locked_presence_lock_reports_active_session() {
        let tmp = tempfile::tempdir().unwrap();
        let agy_paths = paths(tmp.path());
        std::fs::create_dir_all(agy_paths.presence_dir()).unwrap();
        let lock_path = agy_paths.presence_dir().join("active.lock");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .unwrap();
        FileExt::lock(&file).unwrap();

        let report = detect(&agy_paths, SessionScan::PresenceOnly);
        assert!(report.is_known(), "{report:?}");
        assert!(report.any(), "{report:?}");
        assert_eq!(report.presence_locks, 1);
        assert_eq!(report.stale_presence_locks, 0);

        drop(file);
        let after = detect(&agy_paths, SessionScan::PresenceOnly);
        assert!(!after.any(), "{after:?}");
        assert_eq!(after.presence_locks, 0);
        assert_eq!(after.stale_presence_locks, 1);
    }
}
