//! The TUI's only way to the outside world. Everything slow (running agy,
//! unlocking the vault, switching) happens on a worker thread behind this
//! trait, so the interface never blocks and tests can plug in a fake.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::core::app::App;
use crate::core::dashboard::Dashboard;
use crate::core::error::{Error, Result};
use crate::core::usage::RefreshMode;
use crate::paths::Env;

/// Errors cross the thread boundary as plain text for display.
pub type Reply<T> = std::result::Result<T, String>;

#[derive(Debug, Clone)]
pub struct SwitchSummary {
    pub alias: String,
    pub already_active: bool,
    pub warnings: Vec<String>,
}

pub trait Backend: Send + Sync {
    fn load(&self, mode: RefreshMode) -> Reply<Dashboard>;
    /// Switch exactly as `agyva use <id>` does (never with `--force`).
    fn switch(&self, id: &str) -> Reply<SwitchSummary>;
}

/// Work for the background thread; also what the state machine asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Load(RefreshMode),
    Switch { id: String },
}

#[derive(Debug)]
pub enum Msg {
    Loaded {
        mode: RefreshMode,
        result: Box<Reply<Dashboard>>,
    },
    Switched(Reply<SwitchSummary>),
}

pub fn describe(e: &Error) -> String {
    match e.hint_text() {
        Some(h) => format!("{} ({h})", e.message()),
        None => e.message().to_owned(),
    }
}

type Factory = dyn Fn() -> Result<App> + Send + Sync;

/// The real backend: a fresh `App` per job (like a fresh process), so no
/// non-thread-safe state is shared and file locks arbitrate with the CLI.
pub struct AppBackend {
    factory: Arc<Factory>,
}

impl AppBackend {
    pub fn new(factory: Arc<Factory>) -> Self {
        Self { factory }
    }

    pub fn from_env(env: Env, allow_untested: bool) -> Self {
        Self::new(Arc::new(move || App::from_env(env.clone(), allow_untested)))
    }
}

/// The same gate as `main`: config `check_updates` plus the
/// `AGYVA_UPDATE_CHECK=0` kill-switch; config problems keep the default.
fn update_check_enabled(env: &Env) -> bool {
    if env.get("AGYVA_UPDATE_CHECK").is_some_and(|v| v == "0") {
        return false;
    }
    crate::paths::Dirs::resolve(env)
        .ok()
        .and_then(|dirs| crate::config::Config::load(&dirs.config_file()).ok())
        .is_none_or(|c| c.check_updates)
}

/// The vault would ask for a passphrase on the terminal, which the TUI owns.
fn would_prompt(app: &App) -> bool {
    matches!(app.key_provider().load_meta(), Ok(Some(m)) if m.mode != "keyring")
        && !app.has_env_passphrase()
}

impl Backend for AppBackend {
    fn load(&self, mode: RefreshMode) -> Reply<Dashboard> {
        let app = (self.factory)().map_err(|e| describe(&e))?;
        let mut d = app.dashboard(mode).map_err(|e| describe(&e))?;
        // The dashboard is where a startup update notice belongs: stderr is
        // owned by the alternate screen. One passive check per process;
        // failures are silent.
        if d.update_notice.is_none() && update_check_enabled(&app.env) {
            d.update_notice = crate::updater::startup_check(true).and_then(|c| c.notice());
        }
        Ok(d)
    }

    fn switch(&self, id: &str) -> Reply<SwitchSummary> {
        let app = (self.factory)().map_err(|e| describe(&e))?;
        if would_prompt(&app) {
            return Err(
                "the vault is protected by a passphrase; run `agyva use` in a shell instead"
                    .to_owned(),
            );
        }
        let r = app.switch(id, false).map_err(|e| describe(&e))?;
        Ok(SwitchSummary {
            alias: r.account.alias,
            already_active: r.already_active,
            warnings: r.warnings,
        })
    }
}

pub struct Worker {
    pub jobs: Sender<Job>,
    pub results: Receiver<Msg>,
}

fn guarded<T>(job: impl FnOnce() -> Reply<T>) -> Reply<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(job))
        .unwrap_or_else(|_| Err("internal error: the background job crashed".to_owned()))
}

/// Run jobs one at a time on a background thread. The thread ends when the
/// job sender is dropped; it is never joined (a slow agy must not delay
/// quitting; a switch is protected by the quit deferral in the state machine).
pub fn spawn_worker(backend: Arc<dyn Backend>) -> Worker {
    let (jobs, job_rx) = mpsc::channel::<Job>();
    let (tx, results) = mpsc::channel::<Msg>();
    std::thread::spawn(move || {
        for job in job_rx {
            // A panic inside a job becomes an error result: the busy flags
            // in the interface always reset and quitting keeps working.
            let msg = match job {
                Job::Load(mode) => Msg::Loaded {
                    mode,
                    result: Box::new(guarded(|| backend.load(mode))),
                },
                Job::Switch { id } => Msg::Switched(guarded(|| backend.switch(&id))),
            };
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    Worker { jobs, results }
}
