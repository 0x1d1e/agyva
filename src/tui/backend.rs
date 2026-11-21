//! The TUI's only way to the outside world. Everything slow (running agy,
//! unlocking the vault, probing endpoints, switching) happens on a worker
//! thread behind this trait, so the interface never blocks and tests can plug
//! in a fake.

use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, Sender};

use crate::core::app::App;
use crate::core::dashboard::Dashboard;
use crate::core::error::{Error, Result};
use crate::core::protocol::ProbeResult;
use crate::core::route_apply::RouteHistory;
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

#[derive(Debug, Clone)]
pub struct EndpointSwitchSummary {
    pub alias: String,
    pub default_model: Option<String>,
}

#[derive(Debug, Clone)]
pub struct EndpointProbeSummary {
    pub id: String,
    pub alias: String,
    pub probe: Box<ProbeResult>,
}

#[derive(Debug, Clone)]
pub struct ProfileSwitchSummary {
    pub name: String,
    pub switched_account: Option<String>,
    pub active_endpoint: Option<String>,
    pub warnings: Vec<String>,
}

pub trait Backend: Send + Sync {
    fn load(&self, mode: RefreshMode) -> Reply<Dashboard>;
    /// Switch exactly as `agyva use <id>` does (never with `--force`).
    fn switch(&self, id: &str) -> Reply<SwitchSummary>;
    fn switch_endpoint(&self, id: &str) -> Reply<EndpointSwitchSummary>;
    fn test_endpoint(&self, id: &str) -> Reply<EndpointProbeSummary>;
    fn fetch_endpoint_models(&self, id: &str) -> Reply<EndpointProbeSummary>;
    fn apply_route(&self, group: &str) -> Reply<SwitchSummary>;
    fn activate_profile(&self, name: &str) -> Reply<ProfileSwitchSummary>;
}

/// Work for the background thread; also what the state machine asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Job {
    Load(RefreshMode),
    Switch { id: String },
    SwitchEndpoint { id: String },
    TestEndpoint { id: String },
    FetchEndpointModels { id: String },
    ApplyRoute { group: String },
    ActivateProfile { name: String },
}

#[derive(Debug)]
pub enum Msg {
    Loaded {
        mode: RefreshMode,
        result: Box<Reply<Dashboard>>,
    },
    Switched(Reply<SwitchSummary>),
    EndpointSwitched(Reply<EndpointSwitchSummary>),
    EndpointTested(Reply<EndpointProbeSummary>),
    EndpointModelsFetched(Reply<EndpointProbeSummary>),
    RouteApplied(Reply<SwitchSummary>),
    ProfileActivated(Reply<ProfileSwitchSummary>),
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
    /// One passive check per process (success or failure): the dashboard is
    /// reloaded on every refresh and must not re-hit the network. OnceLock,
    /// not LazyLock: the value comes from runtime env/config plus a network
    /// call, not a declaration-time initializer.
    update_check: std::sync::OnceLock<Option<String>>,
}

impl AppBackend {
    pub fn new(factory: Arc<Factory>) -> Self {
        Self {
            factory,
            update_check: std::sync::OnceLock::new(),
        }
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

fn endpoint_alias(app: &App, id: &str) -> String {
    app.list_endpoints()
        .ok()
        .and_then(|eps| eps.into_iter().find(|e| e.id == id).map(|e| e.alias))
        .unwrap_or_else(|| id.to_owned())
}

impl Backend for AppBackend {
    fn load(&self, mode: RefreshMode) -> Reply<Dashboard> {
        let app = (self.factory)().map_err(|e| describe(&e))?;
        let mut d = app.dashboard(mode).map_err(|e| describe(&e))?;
        // The dashboard is where a startup update notice belongs: stderr is
        // owned by the alternate screen. One passive check per process;
        // failures are silent.
        if update_check_enabled(&app.env) {
            let notice = self
                .update_check
                .get_or_init(|| {
                    crate::updater::startup_check_cached(
                        true,
                        crate::paths::Dirs::resolve(&app.env)
                            .ok()
                            .map(|d| d.update_check_cache_file())
                            .as_deref(),
                        chrono::Utc::now(),
                    )
                    .and_then(|c| c.notice())
                })
                .clone();
            d.update_notice = notice;
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

    fn switch_endpoint(&self, id: &str) -> Reply<EndpointSwitchSummary> {
        let app = (self.factory)().map_err(|e| describe(&e))?;
        let rec = app.use_endpoint(id).map_err(|e| describe(&e))?;
        Ok(EndpointSwitchSummary {
            alias: rec.alias,
            default_model: rec.default_model,
        })
    }

    fn test_endpoint(&self, id: &str) -> Reply<EndpointProbeSummary> {
        let app = (self.factory)().map_err(|e| describe(&e))?;
        if would_prompt(&app) {
            return Err(
                "the vault is protected by a passphrase; run `agyva endpoint test` in a shell instead"
                    .to_owned(),
            );
        }
        let alias = endpoint_alias(&app, id);
        let probe = app.test_endpoint(id).map_err(|e| describe(&e))?;
        Ok(EndpointProbeSummary {
            id: id.to_owned(),
            alias,
            probe: Box::new(probe),
        })
    }

    fn fetch_endpoint_models(&self, id: &str) -> Reply<EndpointProbeSummary> {
        let app = (self.factory)().map_err(|e| describe(&e))?;
        if would_prompt(&app) {
            return Err(
                "the vault is protected by a passphrase; run `agyva endpoint fetch` in a shell instead"
                    .to_owned(),
            );
        }
        let alias = endpoint_alias(&app, id);
        let probe = app.refresh_endpoint_models(id).map_err(|e| describe(&e))?;
        Ok(EndpointProbeSummary {
            id: id.to_owned(),
            alias,
            probe: Box::new(probe),
        })
    }

    fn apply_route(&self, group: &str) -> Reply<SwitchSummary> {
        let app = (self.factory)().map_err(|e| describe(&e))?;
        if would_prompt(&app) {
            return Err(
                "the vault is protected by a passphrase; run `agyva route apply` in a shell instead"
                    .to_owned(),
            );
        }
        let history = RouteHistory::new(&app.dirs);
        let r = app
            .route_apply(group, None, false, true, &history, chrono::Utc::now())
            .map_err(|e| describe(&e))?;
        Ok(SwitchSummary {
            alias: r.alias,
            already_active: r.already_active,
            warnings: r.warnings,
        })
    }

    fn activate_profile(&self, name: &str) -> Reply<ProfileSwitchSummary> {
        let app = (self.factory)().map_err(|e| describe(&e))?;
        if would_prompt(&app) {
            return Err(
                "the vault is protected by a passphrase; run `agyva profile use` in a shell instead"
                    .to_owned(),
            );
        }
        let r = app
            .use_profile(name, None, false)
            .map_err(|e| describe(&e))?;
        Ok(ProfileSwitchSummary {
            name: r.profile.name.unwrap_or_else(|| name.to_owned()),
            switched_account: r.switched_account.map(|a| a.alias),
            active_endpoint: r.active_endpoint.map(|e| e.alias),
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
                Job::SwitchEndpoint { id } => {
                    Msg::EndpointSwitched(guarded(|| backend.switch_endpoint(&id)))
                }
                Job::TestEndpoint { id } => {
                    Msg::EndpointTested(guarded(|| backend.test_endpoint(&id)))
                }
                Job::FetchEndpointModels { id } => {
                    Msg::EndpointModelsFetched(guarded(|| backend.fetch_endpoint_models(&id)))
                }
                Job::ApplyRoute { group } => {
                    Msg::RouteApplied(guarded(|| backend.apply_route(&group)))
                }
                Job::ActivateProfile { name } => {
                    Msg::ProfileActivated(guarded(|| backend.activate_profile(&name)))
                }
            };
            if tx.send(msg).is_err() {
                break;
            }
        }
    });
    Worker { jobs, results }
}
