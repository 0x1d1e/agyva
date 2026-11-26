//! `agyva doctor`: diagnostics (`doctor()` is read-only; `doctor_fix()` applies safe fixes).

use std::io::Read;
use std::path::Path;

use fs4::{FileExt, TryLockError};
use serde::Serialize;
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::agy::credentials::parse_credential;
use crate::agy::sessions;
use crate::core::account::read_snapshot;
use crate::core::app::App;
use crate::core::endpoints;
use crate::core::error::{Error, Result};
use crate::core::profile;
use crate::core::quota_history;
use crate::core::view::VIEW_SCHEMA;
use crate::storage::fsutil;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Level {
    Ok,
    Info,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
    pub hint: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    pub schema_version: u32,
    pub healthy: bool,
    pub recovery_pending: bool,
    pub checks: Vec<Check>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DoctorFixReport {
    pub schema_version: u32,
    pub actions: Vec<String>,
    pub report: DoctorReport,
}

pub(crate) struct Collector(Vec<Check>);

impl Collector {
    pub(crate) fn add(
        &mut self,
        name: &'static str,
        level: Level,
        detail: impl Into<String>,
        hint: Option<&str>,
    ) {
        self.0.push(Check {
            name,
            level,
            detail: detail.into(),
            hint: hint.map(str::to_owned),
        });
    }
    pub(crate) fn ok(&mut self, name: &'static str, detail: impl Into<String>) {
        self.add(name, Level::Ok, detail, None);
    }
}

pub(crate) fn permission_ok_detail(windows: bool) -> &'static str {
    if windows {
        "user-only ACLs on directories and files"
    } else {
        "private directories (0700) and files (0600)"
    }
}

#[cfg(windows)]
fn apply_user_acl(path: &Path, actions: &mut Vec<String>) -> Result<()> {
    match fsutil::private_acl_status(path) {
        fsutil::PrivateAcl::UserOnly => Ok(()),
        fsutil::PrivateAcl::Shared(_) | fsutil::PrivateAcl::Unchecked(_) => {
            fsutil::restrict_private_acl(path).map_err(|e| {
                Error::io(format!("setting a user-only ACL on {}", path.display()), e)
            })?;
            actions.push(format!("set user-only ACL on {}", path.display()));
            Ok(())
        }
    }
}

fn perm_problems(path: &Path, want: u32) -> Option<String> {
    let mode = fsutil::mode_of(path)?;
    (mode & 0o077 != 0 || mode != want).then(|| {
        format!(
            "{} has mode {mode:03o}, expected {want:03o}",
            path.display()
        )
    })
}

impl App {
    pub fn doctor(&self) -> DoctorReport {
        let mut c = Collector(Vec::new());
        let mut recovery_pending = false;

        match self.pending_journal() {
            Ok(None) => c.ok("journal", "no interrupted switch"),
            Ok(Some(j)) => {
                recovery_pending = true;
                c.add(
                    "journal",
                    Level::Fail,
                    format!(
                        "interrupted switch to account {} (phase {})",
                        j.target,
                        j.phase.as_str()
                    ),
                    Some("run `agyva recover`"),
                );
            }
            Err(e) => {
                recovery_pending = true;
                c.add(
                    "journal",
                    Level::Fail,
                    e.message().to_owned(),
                    e.hint_text(),
                );
            }
        }

        let reg = match self.load_registry() {
            Ok(r) => {
                c.ok("registry", format!("{} saved account(s)", r.accounts.len()));
                Some(r)
            }
            Err(e) => {
                c.add(
                    "registry",
                    Level::Fail,
                    e.message().to_owned(),
                    e.hint_text(),
                );
                None
            }
        };
        if let Err(e) = crate::config::Config::load(&self.dirs.config_file()) {
            c.add("config", Level::Fail, e.message().to_owned(), e.hint_text());
        } else {
            c.ok("config", "config.toml is valid");
        }

        self.check_permissions(&mut c);
        if let Some(reg) = &reg {
            self.check_snapshots(&mut c, reg);
            self.check_live(&mut c, reg);
        }
        self.check_endpoints(&mut c);
        self.check_profiles(&mut c);
        self.check_cache_dir(&mut c);

        let compat = self.compat();
        let level = if compat.is_supported() {
            Level::Ok
        } else {
            Level::Warn
        };
        c.add(
            "agy",
            level,
            compat.summary(),
            (!compat.is_supported())
                .then_some("add/use refuse untested agy versions unless --allow-untested is given"),
        );

        self.check_agy_extras(&mut c);

        let s = sessions::detect(&self.agy, self.session_scan);
        match (s.is_known(), s.warning()) {
            (false, w) => c.add(
                "sessions",
                Level::Warn,
                w.unwrap_or_default(),
                Some("check the AGY home's permissions"),
            ),
            (true, Some(w)) => c.add("sessions", Level::Info, w, None),
            (true, None) => c.ok("sessions", "no running agy detected"),
        }

        match self.checkpoints().list() {
            Ok(l) => c.ok(
                "checkpoints",
                format!("{} of max {} kept", l.len(), self.keep_checkpoints()),
            ),
            Err(e) => c.add("checkpoints", Level::Warn, e.message().to_owned(), None),
        }
        let leftovers = self.leftover_temp_files();
        if leftovers > 0 {
            c.add(
                "temp-files",
                Level::Warn,
                format!("{leftovers} leftover temporary file(s) from an interrupted write"),
                Some("they are safe to delete"),
            );
        }

        let healthy = !c.0.iter().any(|x| x.level == Level::Fail);
        DoctorReport {
            schema_version: VIEW_SCHEMA,
            healthy,
            recovery_pending,
            checks: c.0,
        }
    }

    fn check_permissions(&self, c: &mut Collector) {
        #[cfg(unix)]
        self.check_unix_modes(c);
        #[cfg(windows)]
        self.check_windows_acls(c);
        #[cfg(not(any(unix, windows)))]
        c.add(
            "permissions",
            Level::Info,
            "not checked",
            Some("permission checks are not available on this platform"),
        );
    }

    #[cfg(unix)]
    fn check_unix_modes(&self, c: &mut Collector) {
        let mut problems = Vec::new();
        let endpoints_dir = endpoints::secrets_dir(&self.dirs);
        for d in [
            &self.dirs.config,
            &self.dirs.state,
            &self.dirs.cache,
            &self.dirs.accounts_dir(),
            &endpoints_dir,
        ] {
            if d.exists() {
                problems.extend(perm_problems(d, fsutil::PRIVATE_DIR_MODE));
            }
        }
        let mut files = vec![
            self.dirs.registry_file(),
            self.dirs.vault_meta_file(),
            endpoints::registry_file(&self.dirs),
            profile::profiles_file(&self.dirs),
        ];
        files.extend(
            fsutil::list_with_extension(&self.dirs.accounts_dir(), "snap").unwrap_or_default(),
        );
        files.extend(fsutil::list_with_extension(&endpoints_dir, "snap").unwrap_or_default());
        files.extend(
            fsutil::list_with_extension(&self.dirs.checkpoint_dir(), "ckpt").unwrap_or_default(),
        );
        for f in files.iter().filter(|f| f.exists()) {
            problems.extend(perm_problems(f, fsutil::PRIVATE_FILE_MODE));
        }
        if problems.is_empty() {
            c.ok("permissions", permission_ok_detail(false));
        } else {
            c.add(
                "permissions",
                Level::Warn,
                problems.join("; "),
                Some("run `agyva doctor --fix` (or chmod 700 on dirs and 600 on files)"),
            );
        }
    }

    #[cfg(windows)]
    fn check_windows_acls(&self, c: &mut Collector) {
        let mut shared = Vec::new();
        let mut unchecked = Vec::new();
        for path in self.permission_paths() {
            if !path.exists() {
                continue;
            }
            match fsutil::private_acl_status(&path) {
                fsutil::PrivateAcl::UserOnly => {}
                fsutil::PrivateAcl::Shared(detail) => shared.push(detail),
                fsutil::PrivateAcl::Unchecked(detail) => unchecked.push(detail),
            }
        }
        if !shared.is_empty() {
            c.add(
                "permissions",
                Level::Warn,
                shared.join("; "),
                Some("run `agyva doctor --fix` to set a user-only ACL"),
            );
        } else if !unchecked.is_empty() {
            c.add(
                "permissions",
                Level::Info,
                "not checked",
                Some("the Windows ACL could not be read"),
            );
        } else {
            c.ok("permissions", permission_ok_detail(true));
        }
    }

    #[cfg(windows)]
    fn permission_paths(&self) -> Vec<std::path::PathBuf> {
        let endpoints_dir = endpoints::secrets_dir(&self.dirs);
        let mut paths = vec![
            self.dirs.config.clone(),
            self.dirs.state.clone(),
            self.dirs.cache.clone(),
            self.dirs.accounts_dir(),
            endpoints_dir.clone(),
            self.dirs.registry_file(),
            self.dirs.vault_meta_file(),
            endpoints::registry_file(&self.dirs),
            profile::profiles_file(&self.dirs),
        ];
        paths.extend(
            fsutil::list_with_extension(&self.dirs.accounts_dir(), "snap").unwrap_or_default(),
        );
        paths.extend(fsutil::list_with_extension(&endpoints_dir, "snap").unwrap_or_default());
        paths.extend(
            fsutil::list_with_extension(&self.dirs.checkpoint_dir(), "ckpt").unwrap_or_default(),
        );
        paths
    }

    fn check_endpoints(&self, c: &mut Collector) {
        let path = endpoints::registry_file(&self.dirs);
        if !path.exists() {
            c.ok("endpoints", "no custom endpoints configured");
            return;
        }
        match endpoints::EndpointRegistry::load(&path) {
            Err(e) => c.add(
                "endpoints",
                Level::Fail,
                e.message().to_owned(),
                e.hint_text(),
            ),
            Ok(reg) => {
                let missing: Vec<&str> = reg
                    .endpoints
                    .iter()
                    .filter(|ep| !endpoints::secret_file(&self.dirs, &ep.id).exists())
                    .map(|ep| ep.alias.as_str())
                    .collect();
                if missing.is_empty() {
                    c.ok(
                        "endpoints",
                        format!(
                            "{} saved endpoint(s), all secret snapshots present",
                            reg.endpoints.len()
                        ),
                    );
                } else {
                    c.add(
                        "endpoints",
                        Level::Fail,
                        format!(
                            "secret snapshot missing for endpoint(s): {}",
                            missing.join(", ")
                        ),
                        Some("update or re-add the affected endpoint API key"),
                    );
                }
            }
        }
    }

    fn check_profiles(&self, c: &mut Collector) {
        let path = profile::profiles_file(&self.dirs);
        if let Err(e) = profile::load_global_profiles(&path) {
            c.add(
                "profile",
                Level::Fail,
                e.message().to_owned(),
                e.hint_text(),
            );
            return;
        }
        let Ok(cwd) = std::env::current_dir() else {
            c.ok("profile", "global profiles valid");
            return;
        };
        match profile::resolve_profile_with_default(
            &self.dirs,
            &cwd,
            None,
            self.config.default_profile.as_deref(),
        ) {
            Ok(Some(active)) => {
                let label = active.name.as_deref().unwrap_or("local");
                c.ok("profile", format!("active profile `{label}` is valid"));
            }
            Ok(None) => c.ok("profile", "global profiles valid; no active profile"),
            Err(e) => c.add(
                "profile",
                Level::Fail,
                e.message().to_owned(),
                e.hint_text(),
            ),
        }
    }

    fn check_cache_dir(&self, c: &mut Collector) {
        let cache = &self.dirs.cache;
        if !cache.exists() {
            if self.dirs.config.exists() || self.dirs.state.exists() {
                c.add(
                    "cache",
                    Level::Info,
                    format!("cache directory {} not created yet", cache.display()),
                    Some("run `agyva doctor --fix` to pre-create it"),
                );
            } else {
                c.ok("cache", "cache directory will be created on demand");
            }
            return;
        }
        if !cache.is_dir() {
            c.add(
                "cache",
                Level::Fail,
                format!("{} exists but is not a directory", cache.display()),
                None,
            );
            return;
        }
        match std::fs::read_dir(cache) {
            Ok(_) => c.ok(
                "cache",
                format!("cache directory healthy ({})", cache.display()),
            ),
            Err(e) => c.add(
                "cache",
                Level::Fail,
                format!("cache directory {} is unreadable: {e}", cache.display()),
                Some("check directory permissions or run `agyva doctor --fix`"),
            ),
        }
    }

    fn check_snapshots(&self, c: &mut Collector, reg: &crate::core::registry::Registry) {
        let on_disk =
            fsutil::list_with_extension(&self.dirs.accounts_dir(), "snap").unwrap_or_default();
        let stems: Vec<String> = on_disk
            .iter()
            .filter_map(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
            .collect();
        let missing: Vec<&str> = reg
            .accounts
            .iter()
            .filter(|a| !stems.contains(&a.id))
            .map(|a| a.alias.as_str())
            .collect();
        let orphans: Vec<&String> = stems.iter().filter(|s| reg.find(s).is_none()).collect();
        if missing.is_empty() && orphans.is_empty() {
            c.ok("snapshots", "every account has a saved credential");
        }
        if !missing.is_empty() {
            c.add(
                "snapshots",
                Level::Fail,
                format!("saved credential missing for: {}", missing.join(", ")),
                Some("remove and re-add the account"),
            );
        }
        if !orphans.is_empty() {
            c.add(
                "snapshots",
                Level::Warn,
                format!("{} credential file(s) not in the registry", orphans.len()),
                Some("harmless leftovers; delete them from the accounts directory"),
            );
        }

        let can_unlock = match self.key_provider().load_meta() {
            Ok(Some(m)) => m.mode == "keyring" || self.has_env_passphrase(),
            _ => false,
        };
        if reg.accounts.is_empty() {
            return;
        }
        if !can_unlock {
            c.add("vault", Level::Info, "decryption check skipped (needs the passphrase; set it in the environment to include it)", None);
            return;
        }
        match self.vault() {
            Err(e) => c.add("vault", Level::Fail, e.message().to_owned(), e.hint_text()),
            Ok(v) => {
                let bad: Vec<&str> = reg
                    .accounts
                    .iter()
                    .filter(|a| stems.contains(&a.id))
                    .filter(|a| read_snapshot(v, &self.dirs.snapshot_file(&a.id), &a.id).is_err())
                    .map(|a| a.alias.as_str())
                    .collect();
                if bad.is_empty() {
                    c.ok("vault", "all saved credentials decrypt and validate");
                } else {
                    c.add(
                        "vault",
                        Level::Fail,
                        format!("cannot decrypt or validate: {}", bad.join(", ")),
                        Some("restore from a backup or re-add the account"),
                    );
                }
            }
        }
    }

    fn check_live(&self, c: &mut Collector, reg: &crate::core::registry::Registry) {
        let live = match self
            .native()
            .and_then(|n| n.read().map(|r| (n.backend_name(), r)))
        {
            Ok(x) => x,
            Err(e) => {
                return c.add(
                    "agy-login",
                    Level::Warn,
                    e.message().to_owned(),
                    e.hint_text(),
                );
            }
        };
        let (backend, Some(raw)) = live else {
            return c.add(
                "agy-login",
                Level::Info,
                format!("agy is not logged in (store: {})", live.0),
                None,
            );
        };
        match parse_credential(raw.as_bytes()) {
            Err(e) => c.add(
                "agy-login",
                Level::Warn,
                e.message().to_owned(),
                e.hint_text(),
            ),
            Ok(info) => {
                let id = info.account_id();
                match reg.find(&id) {
                    Some(rec) if reg.active.as_deref() == Some(&id) => {
                        c.ok(
                            "agy-login",
                            format!("live login is `{}` (store: {backend})", rec.alias),
                        );
                    }
                    Some(rec) => c.add(
                        "agy-login",
                        Level::Warn,
                        format!(
                            "live login is `{}` but agyva last activated another account",
                            rec.alias
                        ),
                        Some("harmless; `agyva use` will correct it"),
                    ),
                    None => c.add(
                        "agy-login",
                        Level::Info,
                        format!("live login ({id}) is not saved in agyva"),
                        Some("run `agyva add` to save it"),
                    ),
                }
            }
        }
    }

    fn leftover_temp_files(&self) -> usize {
        [
            self.dirs.config.clone(),
            self.dirs.accounts_dir(),
            self.dirs.state.clone(),
            self.dirs.journal_dir(),
            self.dirs.checkpoint_dir(),
        ]
        .iter()
        .filter_map(|d| std::fs::read_dir(d).ok())
        .flat_map(|rd| rd.filter_map(std::result::Result::ok))
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .count()
    }

    /// Apply non-destructive safe fixes:
    /// - ensure 0700 permissions on existing private directories (and create
    ///   missing directories if already initialized),
    /// - ensure 0600 permissions on sensitive files under Unix,
    /// - remove uncontended stale lockfiles whose recorded PID is dead,
    /// - re-run `doctor()` and return the updated diagnostics.
    pub fn doctor_fix(&self) -> Result<DoctorFixReport> {
        let mut actions = Vec::new();
        let initialized = self.dirs.config.exists() || self.dirs.state.exists();
        let endpoints_dir = endpoints::secrets_dir(&self.dirs);

        let mut dirs_to_check = vec![
            self.dirs.config.clone(),
            self.dirs.state.clone(),
            self.dirs.cache.clone(),
            self.dirs.accounts_dir(),
            self.dirs.journal_dir(),
            self.dirs.checkpoint_dir(),
        ];
        if endpoints::registry_file(&self.dirs).exists() || endpoints_dir.exists() {
            dirs_to_check.push(endpoints_dir.clone());
        }

        for dir in &dirs_to_check {
            if !dir.exists() {
                if initialized {
                    fsutil::create_private_dir(dir)
                        .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
                    actions.push(format!("created directory {}", dir.display()));
                }
            } else if dir.is_dir() {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    if fsutil::mode_of(dir).is_some_and(|m| m != fsutil::PRIVATE_DIR_MODE) {
                        std::fs::set_permissions(
                            dir,
                            std::fs::Permissions::from_mode(fsutil::PRIVATE_DIR_MODE),
                        )
                        .map_err(|e| Error::io(format!("chmod 0700 {}", dir.display()), e))?;
                        actions.push(format!("set mode 0700 on {}", dir.display()));
                    }
                }
                #[cfg(windows)]
                {
                    apply_user_acl(dir, &mut actions)?;
                }
            }
        }

        let mut files = vec![
            self.dirs.config_file(),
            self.dirs.registry_file(),
            self.dirs.vault_meta_file(),
            endpoints::registry_file(&self.dirs),
            profile::profiles_file(&self.dirs),
            self.dirs.quota_cache_file(),
            quota_history::history_file(&self.dirs),
            endpoints::cache_file(&self.dirs),
        ];
        files.extend(
            fsutil::list_with_extension(&self.dirs.accounts_dir(), "snap").unwrap_or_default(),
        );
        files.extend(fsutil::list_with_extension(&endpoints_dir, "snap").unwrap_or_default());
        files.extend(
            fsutil::list_with_extension(&self.dirs.checkpoint_dir(), "ckpt").unwrap_or_default(),
        );
        for file in files.into_iter().filter(|f| f.is_file()) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if fsutil::mode_of(&file).is_some_and(|m| m != fsutil::PRIVATE_FILE_MODE) {
                    std::fs::set_permissions(
                        &file,
                        std::fs::Permissions::from_mode(fsutil::PRIVATE_FILE_MODE),
                    )
                    .map_err(|e| Error::io(format!("chmod 0600 {}", file.display()), e))?;
                    actions.push(format!("set mode 0600 on {}", file.display()));
                }
            }
            #[cfg(windows)]
            {
                apply_user_acl(&file, &mut actions)?;
            }
            #[cfg(not(any(unix, windows)))]
            let _ = file;
        }

        let lock_paths = [
            self.dirs.lock_file(),
            self.dirs.refresh_lock_file(),
            self.dirs.cache.join("quota.lock"),
            self.dirs.cache.join("quota-history.lock"),
            self.dirs.cache.join("endpoint-models.lock"),
        ];
        let mut sys = System::new();
        for lock_path in lock_paths {
            // `is_file` is false for a missing path and for access denied.
            // A missing lock is nothing to remove. Access denied is a failed
            // fix: the stale lock would otherwise survive a successful report.
            match std::fs::metadata(&lock_path) {
                Ok(meta) if meta.is_file() => {}
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(Error::io(
                        format!("checking lockfile {}", lock_path.display()),
                        e,
                    ));
                }
            }
            let mut file = match std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&lock_path)
            {
                Ok(file) => file,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    return Err(Error::io(
                        format!("opening lockfile {}", lock_path.display()),
                        e,
                    ));
                }
            };
            match FileExt::try_lock(&file) {
                Ok(()) => {
                    let mut pid_text = String::new();
                    let _ = Read::by_ref(&mut file)
                        .take(32)
                        .read_to_string(&mut pid_text);
                    let pid = pid_text.trim().parse::<u32>().ok();
                    let pid_alive = pid.is_some_and(|p| {
                        let spid = Pid::from_u32(p);
                        sys.refresh_processes_specifics(
                            ProcessesToUpdate::Some(&[spid]),
                            true,
                            ProcessRefreshKind::nothing(),
                        );
                        sys.process(spid).is_some()
                    });
                    let _ = FileExt::unlock(&file);
                    drop(file);
                    if pid_alive {
                        continue;
                    }
                    match std::fs::remove_file(&lock_path) {
                        Ok(()) => {
                            actions.push(format!("removed stale lockfile {}", lock_path.display()));
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => {
                            return Err(Error::io(
                                format!("removing stale lockfile {}", lock_path.display()),
                                e,
                            ));
                        }
                    }
                }
                Err(TryLockError::WouldBlock) => {}
                Err(TryLockError::Error(e)) => {
                    return Err(Error::io(format!("locking {}", lock_path.display()), e));
                }
            }
        }

        let report = self.doctor();
        Ok(DoctorFixReport {
            schema_version: VIEW_SCHEMA,
            actions,
            report,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::permission_ok_detail;

    #[test]
    fn permission_ok_does_not_claim_unix_modes_on_windows() {
        assert_eq!(
            permission_ok_detail(true),
            "user-only ACLs on directories and files"
        );
        assert!(!permission_ok_detail(true).contains("0600"));
        assert!(!permission_ok_detail(true).contains("0700"));
        assert_eq!(
            permission_ok_detail(false),
            "private directories (0700) and files (0600)"
        );
    }
}
