//! OS keyring access behind a small trait.
//!
//! Both the native agy credential (`gemini`/`antigravity`) and agyva's own
//! vault key (`agyva`/`vault-key`) go through [`SecretStore`], so tests can
//! substitute [`MemoryKeyring`], including a failing one.
//!
//! The real implementation sits on `keyring-core` with the Secret Service
//! store. Errors from `keyring-core` can embed raw secret bytes
//! (`BadEncoding`, `BadDataFormat`), so they are mapped by variant and are
//! never formatted with `Debug`.

use std::collections::HashMap;
use std::fmt;
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use zeroize::Zeroizing;

use crate::core::error::{Error, ErrorKind};

pub const KEYRING_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreErrorKind {
    /// No keyring service reachable (no D-Bus, no daemon, unsupported OS).
    Unavailable,
    /// Reachable but locked or access denied.
    Locked,
    /// More than one matching item; refuse to guess.
    Ambiguous,
    /// Item exists but its content is not usable.
    BadData,
    Timeout,
    Other,
}

#[derive(Debug, Clone)]
pub struct StoreError {
    pub kind: StoreErrorKind,
    pub message: String,
}

impl StoreError {
    pub fn new(kind: StoreErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for StoreError {}

impl From<StoreError> for Error {
    fn from(e: StoreError) -> Self {
        let hint = match e.kind {
            StoreErrorKind::Unavailable => {
                "no OS keyring is reachable; is a Secret Service provider (gnome-keyring, KWallet) running?"
            }
            StoreErrorKind::Locked => "unlock your OS keyring and try again",
            StoreErrorKind::Ambiguous => {
                "several keyring items match; remove duplicates with your keyring manager"
            }
            StoreErrorKind::Timeout => {
                "the keyring did not answer; check for a pending unlock prompt"
            }
            StoreErrorKind::BadData | StoreErrorKind::Other => "see `agyva doctor`",
        };
        let kind = if e.kind == StoreErrorKind::Timeout {
            ErrorKind::Timeout
        } else {
            ErrorKind::Backend
        };
        Error::new(kind, format!("OS keyring: {}", e.message)).hint(hint)
    }
}

/// Options applied when a keyring item is *created* (never to existing items).
#[derive(Debug, Clone, Default)]
pub struct SetOptions {
    pub label: Option<String>,
    /// Value for the `xdg:schema` attribute on newly created items.
    pub xdg_schema: Option<String>,
}

pub trait SecretStore: Send + Sync + fmt::Debug {
    fn get(&self, service: &str, user: &str) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError>;
    fn set(
        &self,
        service: &str,
        user: &str,
        secret: &[u8],
        opts: &SetOptions,
    ) -> Result<(), StoreError>;
    /// Returns whether an item was removed.
    fn delete(&self, service: &str, user: &str) -> Result<bool, StoreError>;
}

// ---------------------------------------------------------------- system

/// The OS keyring (Secret Service on Linux).
#[derive(Debug, Default)]
pub struct SystemKeyring {
    _private: (),
}

static STORE_INIT: OnceLock<Result<(), StoreError>> = OnceLock::new();

fn init_default_store() -> Result<(), StoreError> {
    STORE_INIT
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            {
                let store =
                    zbus_secret_service_keyring_store::Store::new().map_err(|e| map_error(&e))?;
                keyring_core::set_default_store(store);
                Ok(())
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(StoreError::new(
                    StoreErrorKind::Unavailable,
                    "no keyring backend is built for this operating system yet",
                ))
            }
        })
        .clone()
}

fn map_error(e: &keyring_core::Error) -> StoreError {
    use keyring_core::Error as E;
    match e {
        E::NoStorageAccess(p) => StoreError::new(
            StoreErrorKind::Locked,
            format!("storage not accessible: {p}"),
        ),
        E::PlatformFailure(p) => StoreError::new(
            StoreErrorKind::Unavailable,
            format!("platform failure: {p}"),
        ),
        E::NoDefaultStore => {
            StoreError::new(StoreErrorKind::Unavailable, "no default keyring store")
        }
        E::Ambiguous(_) => StoreError::new(StoreErrorKind::Ambiguous, "multiple matching items"),
        E::BadEncoding(_) | E::BadDataFormat(..) | E::BadStoreFormat(_) => StoreError::new(
            StoreErrorKind::BadData,
            "stored item has an unexpected format",
        ),
        E::NoEntry => StoreError::new(StoreErrorKind::Other, "no such item"),
        E::TooLong(attr, n) => {
            StoreError::new(StoreErrorKind::Other, format!("`{attr}` exceeds {n} bytes"))
        }
        E::Invalid(attr, why) => {
            StoreError::new(StoreErrorKind::Other, format!("invalid `{attr}`: {why}"))
        }
        E::NotSupportedByStore(why) => {
            StoreError::new(StoreErrorKind::Other, format!("not supported: {why}"))
        }
        _ => StoreError::new(StoreErrorKind::Other, "unexpected keyring error"),
    }
}

/// Run `f` on a helper thread so a hung D-Bus call cannot hang the CLI.
fn with_timeout<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, StoreError> + Send + 'static,
) -> Result<T, StoreError> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    match rx.recv_timeout(KEYRING_TIMEOUT) {
        Ok(r) => r,
        Err(_) => Err(StoreError::new(
            StoreErrorKind::Timeout,
            "keyring operation timed out",
        )),
    }
}

impl SystemKeyring {
    pub fn new() -> Self {
        Self::default()
    }

    /// Probe that the keyring is reachable at all (store construction).
    pub fn probe(&self) -> Result<(), StoreError> {
        with_timeout(init_default_store)
    }
}

impl SecretStore for SystemKeyring {
    fn get(&self, service: &str, user: &str) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        let (service, user) = (service.to_owned(), user.to_owned());
        with_timeout(move || {
            init_default_store()?;
            let entry = keyring_core::Entry::new(&service, &user).map_err(|e| map_error(&e))?;
            match entry.get_secret() {
                Ok(bytes) => Ok(Some(Zeroizing::new(bytes))),
                Err(keyring_core::Error::NoEntry) => Ok(None),
                Err(e) => Err(map_error(&e)),
            }
        })
    }

    fn set(
        &self,
        service: &str,
        user: &str,
        secret: &[u8],
        opts: &SetOptions,
    ) -> Result<(), StoreError> {
        let (service, user) = (service.to_owned(), user.to_owned());
        let secret = Zeroizing::new(secret.to_vec());
        let opts = opts.clone();
        with_timeout(move || {
            init_default_store()?;
            let mut mods: HashMap<&str, &str> = HashMap::new();
            if let Some(label) = opts.label.as_deref() {
                mods.insert("label", label);
            }
            let entry = keyring_core::Entry::new_with_modifiers(&service, &user, &mods)
                .map_err(|e| map_error(&e))?;
            let existed = match entry.get_secret() {
                Ok(_) => true,
                Err(keyring_core::Error::NoEntry) => false,
                Err(e) => return Err(map_error(&e)),
            };
            entry.set_secret(&secret).map_err(|e| map_error(&e))?;
            if !existed && let Some(schema) = opts.xdg_schema.as_deref() {
                // Best effort: only for items we created ourselves.
                let attrs = HashMap::from([("xdg:schema", schema)]);
                if let Err(e) = entry.update_attributes(&attrs) {
                    tracing::debug!(error = %map_error(&e), "could not set xdg:schema on new keyring item");
                }
            }
            Ok(())
        })
    }

    fn delete(&self, service: &str, user: &str) -> Result<bool, StoreError> {
        let (service, user) = (service.to_owned(), user.to_owned());
        with_timeout(move || {
            init_default_store()?;
            let entry = keyring_core::Entry::new(&service, &user).map_err(|e| map_error(&e))?;
            match entry.delete_credential() {
                Ok(()) => Ok(true),
                Err(keyring_core::Error::NoEntry) => Ok(false),
                Err(e) => Err(map_error(&e)),
            }
        })
    }
}

// ---------------------------------------------------------------- memory

/// In-memory keyring for tests, with switchable failure injection.
#[derive(Debug, Default, Clone)]
pub struct MemoryKeyring {
    inner: Arc<Mutex<MemoryState>>,
}

#[derive(Debug, Default)]
struct MemoryState {
    items: HashMap<(String, String), Vec<u8>>,
    failure: Option<StoreErrorKind>,
}

impl MemoryKeyring {
    pub fn new() -> Self {
        Self::default()
    }

    /// Make every subsequent operation fail with `kind` (or recover with `None`).
    pub fn set_failure(&self, kind: Option<StoreErrorKind>) {
        if let Ok(mut s) = self.inner.lock() {
            s.failure = kind;
        }
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>, StoreError> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| StoreError::new(StoreErrorKind::Other, "memory keyring poisoned"))?;
        if let Some(kind) = guard.failure {
            return Err(StoreError::new(kind, "injected keyring failure"));
        }
        Ok(guard)
    }
}

impl SecretStore for MemoryKeyring {
    fn get(&self, service: &str, user: &str) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        Ok(self
            .state()?
            .items
            .get(&(service.to_owned(), user.to_owned()))
            .map(|v| Zeroizing::new(v.clone())))
    }

    fn set(
        &self,
        service: &str,
        user: &str,
        secret: &[u8],
        _opts: &SetOptions,
    ) -> Result<(), StoreError> {
        self.state()?
            .items
            .insert((service.to_owned(), user.to_owned()), secret.to_vec());
        Ok(())
    }

    fn delete(&self, service: &str, user: &str) -> Result<bool, StoreError> {
        Ok(self
            .state()?
            .items
            .remove(&(service.to_owned(), user.to_owned()))
            .is_some())
    }
}
