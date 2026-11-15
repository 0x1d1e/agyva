//! `agyva doctor`: diagnostics. Read-only; never modifies state.

use std::path::Path;

use serde::Serialize;

use crate::agy::credentials::parse_credential;
use crate::agy::sessions;
use crate::core::account::read_snapshot;
use crate::core::app::App;
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

        let s = sessions::detect(&self.agy);
        match s.warning() {
            Some(w) => c.add("sessions", Level::Info, w, None),
            None => c.ok("sessions", "no running agy detected"),
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
        let mut problems = Vec::new();
        for d in [
            &self.dirs.config,
            &self.dirs.state,
            &self.dirs.accounts_dir(),
        ] {
            if d.exists() {
                problems.extend(perm_problems(d, fsutil::PRIVATE_DIR_MODE));
            }
        }
        let mut files = vec![self.dirs.registry_file(), self.dirs.vault_meta_file()];
        files.extend(
            fsutil::list_with_extension(&self.dirs.accounts_dir(), "snap").unwrap_or_default(),
        );
        files.extend(
            fsutil::list_with_extension(&self.dirs.checkpoint_dir(), "ckpt").unwrap_or_default(),
        );
        for f in files.iter().filter(|f| f.exists()) {
            problems.extend(perm_problems(f, fsutil::PRIVATE_FILE_MODE));
        }
        if problems.is_empty() {
            c.ok("permissions", "private directories (0700) and files (0600)");
        } else {
            c.add(
                "permissions",
                Level::Warn,
                problems.join("; "),
                Some("run `chmod 700` on directories and `chmod 600` on files"),
            );
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
        .flat_map(|rd| rd.filter_map(Result::ok))
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
        .count()
    }
}
