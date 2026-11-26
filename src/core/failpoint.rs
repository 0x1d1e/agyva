//! Fault injection for crash-safety tests.
//!
//! `AGYVA_FAILPOINT=<name>[:crash|:error]` makes the named step fail:
//!
//! * `crash` (default): the process stops dead with exit code 99, with no
//!   cleanup and no automatic rollback, as if killed at that instant.
//! * `error`: the step returns an ordinary error, exercising automatic
//!   rollback.
//!
//! The hook is compiled into every build on purpose: the binary-level E2E
//! tests simulate an interrupted switch through it. It can only make agyva
//! fail itself; it grants no capability. To keep a stray variable from
//! breaking a real run it is honored only together with `AGYVA_TEST_MODE=1`;
//! otherwise it is ignored with one warning on stderr. Step names are listed
//! in [`NAMES`].

use crate::core::error::{Error, ErrorKind, Result};
use crate::paths::Env;

pub const ENV_FAILPOINT: &str = "AGYVA_FAILPOINT";
pub const ENV_TEST_MODE: &str = "AGYVA_TEST_MODE";

pub const NAMES: &[&str] = &[
    "after-checkpoint",
    "after-credential-written",
    "after-prepared",
    "after-native-write",
    "after-companion",
    "import-staged",
    "import-marker",
    "import-published",
    "import-registry",
    "verify",
    "after-verified",
    "after-registry",
    "post-commit-cleanup",
    "endpoint-clear",
    "rollback-after-native",
    "recover-after-native",
    "rekey-before-meta",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    Crash,
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct Failpoints {
    armed: Option<(String, Fault)>,
}

impl Failpoints {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn armed(name: &str, fault: Fault) -> Self {
        Self {
            armed: Some((name.to_owned(), fault)),
        }
    }

    pub fn from_env(env: &Env) -> Result<Self> {
        let Some(spec) = env.get(ENV_FAILPOINT) else {
            return Ok(Self::none());
        };
        if env.get(ENV_TEST_MODE) != Some("1") {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!(
                    "warning: ignoring {ENV_FAILPOINT}: fault injection needs {ENV_TEST_MODE}=1 as well"
                );
            });
            return Ok(Self::none());
        }
        let (name, fault) = match spec.split_once(':') {
            None => (spec, Fault::Crash),
            Some((n, "crash")) => (n, Fault::Crash),
            Some((n, "error")) => (n, Fault::Error),
            Some(_) => return Err(Error::usage(format!("invalid {ENV_FAILPOINT}"))),
        };
        if !NAMES.contains(&name) {
            return Err(Error::usage(format!("unknown failpoint `{name}`"))
                .hint(format!("known failpoints: {}", NAMES.join(", "))));
        }
        Ok(Self::armed(name, fault))
    }

    /// Called at each instrumented step.
    pub fn hit(&self, name: &str) -> Result<()> {
        match &self.armed {
            Some((n, Fault::Crash)) if n == name => Err(Error::new(
                ErrorKind::SimulatedCrash,
                format!("simulated crash at failpoint `{name}`"),
            )),
            Some((n, Fault::Error)) if n == name => Err(Error::backend(format!(
                "injected failure at failpoint `{name}`"
            ))),
            _ => Ok(()),
        }
    }
}
