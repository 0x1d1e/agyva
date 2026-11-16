//! Shared harness: an `App` wired to in-memory fakes inside a tempdir.
//! Everything here is synthetic; nothing touches the real keyring or $HOME.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

pub mod fake_agy;

use agyva::agy::compatibility::{Compat, parse_version};
use agyva::agy::credentials::{MemoryBackend, NativeAuth, NativeBackend, parse_credential};
use agyva::config::Config;
use agyva::core::app::App;
use agyva::core::error::{Error, Result};
use agyva::core::failpoint::Failpoints;
use agyva::paths::{AgyPaths, Dirs, Env};
use agyva::storage::keyring::MemoryKeyring;
use agyva::storage::vault_key::{KdfParams, StaticPassphrase};
use agyva::testing::credential;
use tempfile::TempDir;

pub struct Rig {
    pub tmp: TempDir,
    pub native: MemoryBackend,
    pub keyring: MemoryKeyring,
}

pub const ALICE: (&str, &str, &str) = ("sub-alice", "alice@example.invalid", "alice");
pub const BOB: (&str, &str, &str) = ("sub-bob", "bob@example.invalid", "bob");
pub const CAROL: (&str, &str, &str) = ("sub-carol", "carol@example.invalid", "carol");

pub fn id_of(who: (&str, &str, &str)) -> String {
    parse_credential(&credential(who.0, who.1, who.2))
        .map(|i| i.account_id())
        .unwrap_or_default()
}

/// A `MemoryBackend` that can misbehave on schedule, to reproduce races and
/// keyring timeouts deterministically.
#[derive(Debug, Clone)]
pub struct FlakyNative {
    pub inner: MemoryBackend,
    plan: Arc<Mutex<Plan>>,
}

#[derive(Debug, Default)]
struct Plan {
    reads: usize,
    writes: usize,
    /// After the n-th read returns, replace the stored login with these bytes
    /// (another process changing it).
    replace_after_read: Option<(usize, Vec<u8>)>,
    /// The n-th write times out; `bool` says whether it was applied anyway.
    timeout_on_write: Option<(usize, bool)>,
}

impl FlakyNative {
    pub fn new(inner: MemoryBackend) -> Self {
        Self {
            inner,
            plan: Arc::default(),
        }
    }

    pub fn replace_after_read(&self, nth: usize, bytes: Vec<u8>) {
        self.plan.lock().expect("plan").replace_after_read = Some((nth, bytes));
    }

    pub fn timeout_on_write(&self, nth: usize, applied: bool) {
        self.plan.lock().expect("plan").timeout_on_write = Some((nth, applied));
    }

    /// Writes that reached the backend (including a timed-out, applied one).
    pub fn writes(&self) -> usize {
        self.plan.lock().expect("plan").writes
    }
}

impl NativeBackend for FlakyNative {
    fn name(&self) -> &'static str {
        "flaky-memory"
    }

    fn read(&self) -> Result<Option<agyva::agy::credentials::RawCredential>> {
        let got = self.inner.read()?;
        let mut plan = self.plan.lock().expect("plan");
        plan.reads += 1;
        if let Some((n, bytes)) = plan.replace_after_read.clone()
            && n == plan.reads
        {
            plan.replace_after_read = None;
            self.inner.write(&bytes)?;
        }
        Ok(got)
    }

    fn write(&self, raw: &[u8]) -> Result<()> {
        let timeout = {
            let mut plan = self.plan.lock().expect("plan");
            plan.writes += 1;
            match plan.timeout_on_write {
                Some((n, applied)) if n == plan.writes => {
                    plan.timeout_on_write = None;
                    Some(applied)
                }
                _ => None,
            }
        };
        match timeout {
            None => self.inner.write(raw),
            Some(applied) => {
                if applied {
                    self.inner.write(raw)?;
                }
                Err(Error::outcome_unknown("injected keyring timeout"))
            }
        }
    }

    fn delete(&self) -> Result<()> {
        self.inner.delete()
    }
}

fn build_app(
    root: &std::path::Path,
    native_backend: Arc<dyn NativeBackend>,
    keyring: &MemoryKeyring,
    failpoints: Failpoints,
) -> App {
    let kdf = KdfParams::with_cost(8, 1, 1).expect("kdf");
    let compat = Compat::Supported(parse_version("agy 1.5.9").expect("version"));
    let native = NativeAuth::new(native_backend, None);
    App::new(
        Dirs::under(root),
        AgyPaths {
            home: root.join("agyhome"),
        },
        Env::from_pairs([]),
        Config::default(),
    )
    .with_secrets(Arc::new(keyring.clone()))
    .with_passphrases(Arc::new(StaticPassphrase("synthetic-passphrase".into())))
    .with_native(native)
    .with_kdf(kdf)
    .with_compat(compat)
    .with_failpoints(failpoints)
    // The fake agy installed by tests is a real process named `agy`; a host
    // process scan would report it as a user session. The presence-lock
    // check (the in-sandbox signal) stays on.
    .with_session_scan(agyva::agy::sessions::SessionScan::PresenceOnly)
    .with_lock_timeout(Duration::from_millis(200))
}

impl Rig {
    pub fn new() -> Self {
        Self {
            tmp: TempDir::new().expect("tempdir"),
            native: MemoryBackend::new(),
            keyring: MemoryKeyring::new(),
        }
    }

    pub fn dirs(&self) -> Dirs {
        Dirs::under(self.tmp.path())
    }

    pub fn agy_home(&self) -> std::path::PathBuf {
        self.tmp.path().join("agyhome")
    }

    /// A fresh context (like a new process) with the given failpoints.
    pub fn app_with(&self, failpoints: Failpoints) -> App {
        self.app_with_backend(Arc::new(self.native.clone()), failpoints)
    }

    /// A fresh context over a custom native backend (see [`FlakyNative`]).
    pub fn app_with_backend(&self, backend: Arc<dyn NativeBackend>, failpoints: Failpoints) -> App {
        build_app(self.tmp.path(), backend, &self.keyring, failpoints)
    }

    /// A [`FlakyNative`] over this rig's native store.
    pub fn flaky(&self) -> FlakyNative {
        FlakyNative::new(self.native.clone())
    }

    pub fn app(&self) -> App {
        self.app_with(Failpoints::none())
    }

    /// A context whose `agy` is the fake script (compat forced to supported).
    pub fn app_with_agy(&self, fake: &fake_agy::FakeAgy) -> App {
        let mut app = self.app();
        let bin = fake.bin().display().to_string();
        app.env = Env::from_pairs([("AGYVA_AGY_BIN", bin.as_str())]);
        app
    }

    /// What the interface's worker uses to get a fresh `App` per job, like a
    /// new process each time. `passphrase_env` mimics `AGYVA_PASSPHRASE`.
    pub fn factory(
        &self,
        agy_bin: Option<std::path::PathBuf>,
        passphrase_env: bool,
    ) -> Arc<dyn Fn() -> agyva::core::error::Result<App> + Send + Sync> {
        let root = self.tmp.path().to_path_buf();
        let native = self.native.clone();
        let keyring = self.keyring.clone();
        Arc::new(move || {
            let mut app = build_app(
                &root,
                Arc::new(native.clone()),
                &keyring,
                Failpoints::none(),
            );
            let mut pairs: Vec<(String, String)> = Vec::new();
            if let Some(b) = &agy_bin {
                pairs.push(("AGYVA_AGY_BIN".into(), b.display().to_string()));
            }
            if passphrase_env {
                pairs.push(("AGYVA_PASSPHRASE".into(), "synthetic-passphrase".into()));
            }
            app.env = Env::from_pairs(pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            Ok(app)
        })
    }

    /// Simulate `agy` logging in as this identity.
    pub fn login(&self, who: (&str, &str, &str)) {
        self.login_raw(credential(who.0, who.1, who.2));
    }

    /// Store an arbitrary synthetic credential as the live login.
    pub fn login_raw(&self, raw: Vec<u8>) {
        self.native.write(&raw).expect("login");
    }

    pub fn live_id(&self) -> Option<String> {
        let raw = self.native.read().expect("read")?;
        parse_credential(raw.as_bytes())
            .ok()
            .map(|i| i.account_id())
    }

    /// Log in as each identity and `add` it.
    pub fn add_all(&self, who: &[(&str, &str, &str)]) {
        let app = self.app();
        for w in who {
            self.login(*w);
            app.add(&Default::default()).expect("add");
        }
    }

    pub fn native_text(&self) -> String {
        self.native
            .read()
            .ok()
            .flatten()
            .map(|r| String::from_utf8_lossy(r.as_bytes()).into_owned())
            .unwrap_or_default()
    }

    /// Every file under the tempdir, for leak scans.
    pub fn all_files(&self) -> Vec<std::path::PathBuf> {
        fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            let Ok(rd) = std::fs::read_dir(p) else { return };
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    walk(&path, out)
                } else {
                    out.push(path)
                }
            }
        }
        let mut out = Vec::new();
        walk(self.tmp.path(), &mut out);
        out
    }
}
