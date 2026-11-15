//! Best-effort detection of running agy sessions. agyva only ever *warns*;
//! it never signals or kills a process.

use sysinfo::{ProcessesToUpdate, System};

use crate::paths::AgyPaths;

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct SessionReport {
    /// `presence/*.lock` files (may include stale ones).
    pub presence_locks: usize,
    /// PIDs of processes whose name is exactly `agy`.
    pub agy_pids: Vec<u32>,
}

impl SessionReport {
    pub fn any(&self) -> bool {
        self.presence_locks > 0 || !self.agy_pids.is_empty()
    }

    pub fn warning(&self) -> Option<String> {
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

pub fn detect(paths: &AgyPaths) -> SessionReport {
    let presence_locks = std::fs::read_dir(paths.presence_dir())
        .map(|rd| {
            rd.filter_map(Result::ok)
                .filter(|e| e.path().extension().is_some_and(|x| x == "lock"))
                .count()
        })
        .unwrap_or(0);
    let mut sys = System::new();
    sys.refresh_processes(ProcessesToUpdate::All, true);
    let mut agy_pids: Vec<u32> = sys
        .processes()
        .iter()
        .filter(|(_, p)| p.name() == "agy")
        .map(|(pid, _)| pid.as_u32())
        .collect();
    agy_pids.sort_unstable();
    SessionReport {
        presence_locks,
        agy_pids,
    }
}
