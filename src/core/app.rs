//! Application context: directories, configuration and the injectable
//! boundaries (keyring, passphrases, native credential store, failpoints).
//! Operations are `impl App` blocks in sibling modules.

use std::cell::OnceCell;
use std::sync::Arc;
use std::time::Duration;

use crate::agy::compatibility::Compat;
use crate::agy::credentials::NativeAuth;
use crate::agy::discovery;
use crate::config::{Config, VaultKeyMode};
use crate::core::checkpoint::CheckpointStore;
use crate::core::error::{Error, Result};
use crate::core::failpoint::Failpoints;
use crate::core::provider::QuotaProvider;
use crate::core::registry::Registry;
use crate::paths::{AgyPaths, Dirs, Env};
use crate::storage::journal::{Journal, JournalStore};
use crate::storage::keyring::{MemoryKeyring, SecretStore, StoreErrorKind, SystemKeyring};
use crate::storage::lock::StateLock;
use crate::storage::vault::Vault;
use crate::storage::vault_key::{
    ENV_ARCHIVE_PASSPHRASE, ENV_PASSPHRASE, EnvOrTty, KdfParams, KeyProvider, PassphraseSource,
};
use secrecy::SecretString;

pub const ENV_VAULT_KEY: &str = "AGYVA_VAULT_KEY";

/// Both locks of [`App::lock_login`]; released together (state first).
pub(crate) struct LoginLock {
    _state: StateLock,
    _refresh: StateLock,
}

/// Extra patience for the refresh lock: a refresh in flight can take as long
/// as agy's own timeout (15s) plus the time to stop it.
const REFRESH_GRACE: Duration = Duration::from_secs(25);

pub struct App {
    pub dirs: Dirs,
    pub agy: AgyPaths,
    pub env: Env,
    pub config: Config,
    pub(crate) secrets: Arc<dyn SecretStore>,
    pub(crate) passphrases: Arc<dyn PassphraseSource>,
    pub(crate) failpoints: Failpoints,
    pub(crate) kdf: Option<KdfParams>,
    pub(crate) compat_override: Option<Compat>,
    pub(crate) allow_untested: bool,
    had_env_passphrase: bool,
    pub(crate) lock_timeout: Duration,
    /// How long a refresh waits for the refresh lock before skipping.
    pub(crate) refresh_wait: Duration,
    /// How long login-changing work waits for a refresh already running.
    pub(crate) refresh_grace: Duration,
    pub(crate) vault_mode: VaultKeyMode,
    /// Passphrase source for import/export archives; deliberately separate
    /// from the vault passphrase so an archive never unlocks the vault and
    /// vice versa.
    pub(crate) archive_passphrases: Arc<dyn PassphraseSource>,
    pub(crate) provider_override: Option<Arc<dyn QuotaProvider>>,
    native_override: Option<NativeAuth>,
    native: OnceCell<NativeAuth>,
    vault: OnceCell<Vault>,
}

impl App {
    /// Context with production defaults (system keyring, env/TTY passphrase).
    pub fn new(dirs: Dirs, agy: AgyPaths, mut env: Env, config: Config) -> Self {
        // The passphrase moves out of the general environment map into a
        // zeroizing holder, so it is not cloned around or passed to children.
        let env_passphrase = env.take(ENV_PASSPHRASE).map(SecretString::from);
        let had_env_passphrase = env_passphrase.is_some();
        // Likewise the archive passphrase for import/export: taken out of
        // the environment map immediately, separate from the vault one.
        let archive_passphrase = env.take(ENV_ARCHIVE_PASSPHRASE).map(SecretString::from);
        let passphrases: Arc<dyn PassphraseSource> = Arc::new(EnvOrTty::new(env_passphrase));
        let archive_passphrases: Arc<dyn PassphraseSource> =
            Arc::new(EnvOrTty::for_archives(archive_passphrase));
        Self {
            lock_timeout: Duration::from_secs(config.lock_timeout_secs),
            refresh_wait: Duration::from_secs(2),
            refresh_grace: REFRESH_GRACE,
            allow_untested: config.allow_untested_agy,
            vault_mode: config.vault_key,
            dirs,
            agy,
            env,
            config,
            secrets: Arc::new(SystemKeyring::new()),
            passphrases,
            archive_passphrases,
            failpoints: Failpoints::none(),
            had_env_passphrase,
            kdf: None,
            provider_override: None,
            compat_override: None,
            native_override: None,
            native: OnceCell::new(),
            vault: OnceCell::new(),
        }
    }

    /// Build from the process environment. When `AGYVA_BACKEND` is set
    /// (sandbox mode for tests and scripted runs, which requires
    /// `AGYVA_CONFIG_DIR`, `AGYVA_STATE_DIR` and `AGYVA_AGY_HOME` too) the OS
    /// keyring is never touched: the vault key falls back to a passphrase
    /// vault.
    pub fn from_env(env: Env, allow_untested: bool) -> Result<Self> {
        // First: a half-configured sandbox must fail before anything is read.
        let sandbox = discovery::sandbox(&env)?;
        let dirs = Dirs::resolve(&env)?;
        let agy = AgyPaths::resolve(&env)?;
        let config = Config::load(&dirs.config_file())?;
        let failpoints = Failpoints::from_env(&env)?;
        let vault_mode = match env.get(ENV_VAULT_KEY) {
            Some(v) => v
                .parse::<VaultKeyMode>()
                .map_err(|e| Error::usage(format!("invalid {ENV_VAULT_KEY}: {e}")))?,
            None => config.vault_key,
        };
        let mut app = Self::new(dirs, agy, env, config);
        app.failpoints = failpoints;
        app.vault_mode = vault_mode;
        app.allow_untested |= allow_untested;
        if sandbox {
            let sealed = MemoryKeyring::new();
            sealed.set_failure(Some(StoreErrorKind::Unavailable));
            app.secrets = Arc::new(sealed);
        }
        Ok(app)
    }

    #[must_use]
    pub fn with_secrets(mut self, secrets: Arc<dyn SecretStore>) -> Self {
        self.secrets = secrets;
        self
    }

    #[must_use]
    pub fn with_passphrases(mut self, p: Arc<dyn PassphraseSource>) -> Self {
        self.passphrases = p;
        self
    }

    #[must_use]
    pub fn with_native(mut self, native: NativeAuth) -> Self {
        self.native_override = Some(native);
        self
    }

    #[must_use]
    pub fn with_failpoints(mut self, f: Failpoints) -> Self {
        self.failpoints = f;
        self
    }

    /// Replace the archive passphrase source (tests).
    #[must_use]
    pub fn with_archive_passphrases(mut self, p: Arc<dyn PassphraseSource>) -> Self {
        self.archive_passphrases = p;
        self
    }

    #[must_use]
    pub fn with_kdf(mut self, kdf: KdfParams) -> Self {
        self.kdf = Some(kdf);
        self
    }

    #[must_use]
    pub fn with_compat(mut self, compat: Compat) -> Self {
        self.compat_override = Some(compat);
        self
    }

    /// Replace the quota source (tests, or a future provider).
    #[must_use]
    pub fn with_provider(mut self, p: Arc<dyn QuotaProvider>) -> Self {
        self.provider_override = Some(p);
        self
    }

    #[must_use]
    pub fn with_vault_mode(mut self, mode: VaultKeyMode) -> Self {
        self.vault_mode = mode;
        self
    }

    #[must_use]
    pub fn with_refresh_wait(mut self, d: Duration) -> Self {
        self.refresh_wait = d;
        self
    }

    pub fn with_refresh_grace(mut self, d: Duration) -> Self {
        self.refresh_grace = d;
        self
    }

    pub fn with_lock_timeout(mut self, d: Duration) -> Self {
        self.lock_timeout = d;
        self
    }

    // ------------------------------------------------------------ accessors

    /// Whether a passphrase can be had without a terminal
    /// (`AGYVA_PASSPHRASE`, consumed when the context was built).
    pub fn has_env_passphrase(&self) -> bool {
        self.had_env_passphrase || self.env.get(ENV_PASSPHRASE).is_some()
    }

    pub fn compat(&self) -> Compat {
        self.compat_override
            .clone()
            .unwrap_or_else(|| discovery::detect_compat(&self.env))
    }

    pub(crate) fn require_compat(&self) -> Result<()> {
        self.compat().require(self.allow_untested)
    }

    /// agy's live credential store, chosen lazily (read-only commands that
    /// never need it never touch the keyring).
    pub fn native(&self) -> Result<&NativeAuth> {
        if let Some(n) = &self.native_override {
            return Ok(n);
        }
        if let Some(n) = self.native.get() {
            return Ok(n);
        }
        let n = discovery::select_backend(
            self.config.backend,
            &self.env,
            &self.agy,
            self.secrets.clone(),
            None,
        )?;
        Ok(self.native.get_or_init(|| n))
    }

    pub(crate) fn key_provider(&self) -> KeyProvider {
        let p = KeyProvider::new(
            self.dirs.clone(),
            self.secrets.clone(),
            self.passphrases.clone(),
            self.vault_mode,
        );
        match &self.kdf {
            Some(k) => p.with_kdf(k.clone()),
            None => p,
        }
    }

    /// Unlock the existing vault.
    pub fn vault(&self) -> Result<&Vault> {
        if let Some(v) = self.vault.get() {
            return Ok(v);
        }
        let v = Vault::new(self.key_provider().unlock()?);
        Ok(self.vault.get_or_init(|| v))
    }

    /// Unlock the vault, creating it on first use.
    pub(crate) fn vault_or_create(&self) -> Result<&Vault> {
        if let Some(v) = self.vault.get() {
            return Ok(v);
        }
        let v = Vault::new(self.key_provider().unlock_or_create()?);
        Ok(self.vault.get_or_init(|| v))
    }

    pub(crate) fn journals(&self) -> JournalStore {
        JournalStore::new(self.dirs.journal_dir())
    }

    pub(crate) fn checkpoints(&self) -> CheckpointStore {
        CheckpointStore::new(self.dirs.checkpoint_dir())
    }

    pub fn load_registry(&self) -> Result<Registry> {
        Registry::load(&self.dirs.registry_file())
    }

    pub(crate) fn save_registry(&self, reg: &Registry) -> Result<()> {
        reg.save(&self.dirs.registry_file())
    }

    /// The pending, incomplete journal if there is one.
    pub fn pending_journal(&self) -> Result<Option<Journal>> {
        Ok(self.journals().load()?.filter(Journal::is_incomplete))
    }

    // ------------------------------------------------------------- locking

    /// Lock for everything that changes saved state. An interrupted import
    /// is reconciled before the lock is returned, so no caller acts on a
    /// half-imported state (see `core::transfer::reconcile_import`).
    pub(crate) fn lock(&self) -> Result<StateLock> {
        let lock = StateLock::acquire(&self.dirs.lock_file(), self.lock_timeout)?;
        self.reconcile_import()?;
        Ok(lock)
    }

    /// Lock for everything that reads or replaces agy's login (switch,
    /// recover, add): first the refresh lock, so no `agy -p /usage` is
    /// running (agy may rewrite its own credential while it runs), then the
    /// state lock. The order is fixed (refresh, then state) so two holders can
    /// never wait on each other. A refresh in flight needs up to agy's
    /// timeout to finish, hence the longer wait for the first lock only.
    pub(crate) fn lock_login(&self) -> Result<LoginLock> {
        let wait = self.lock_timeout + self.refresh_grace;
        let refresh = StateLock::acquire(&self.dirs.refresh_lock_file(), wait)?;
        let state = self.lock()?;
        Ok(LoginLock {
            _state: state,
            _refresh: refresh,
        })
    }

    /// Mutating commands call this *under the lock*: refuse while an
    /// interrupted transaction awaits recovery; tidy a finished one.
    pub(crate) fn require_no_pending(&self) -> Result<()> {
        match self.journals().load()? {
            None => Ok(()),
            Some(j) if !j.is_incomplete() => self.journals().clear(),
            Some(j) => Err(Error::recovery_required(format!(
                "an interrupted switch to account {} ({}) must be recovered first",
                j.target,
                j.phase.as_str()
            ))),
        }
    }

    pub(crate) fn keep_checkpoints(&self) -> usize {
        usize::from(self.config.checkpoints_keep.max(1))
    }
}

impl App {
    /// Lock for `config set` (config edits race with the policy notice).
    pub fn lock_for_config(&self) -> Result<StateLock> {
        self.lock()
    }
}
