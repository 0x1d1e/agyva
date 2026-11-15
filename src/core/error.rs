//! Error type shared by the whole crate.
//!
//! Every error carries a [`ErrorKind`] (used for exit codes and test
//! assertions), a human message, and an optional hint line telling the user
//! what to do next. Errors must never embed credential material.

use std::fmt;

pub type Result<T> = std::result::Result<T, Error>;

/// Process exit codes (documented in `docs/architecture.md`).
pub mod exit {
    pub const OK: i32 = 0;
    pub const ERROR: i32 = 1;
    pub const USAGE: i32 = 2;
    pub const RECOVERY_REQUIRED: i32 = 3;
    /// Used only by fault-injection hooks to simulate a crash.
    pub const SIMULATED_CRASH: i32 = 99;
}

/// What to tell the user after a keyring write/delete whose outcome is unknown.
pub const OUTCOME_UNKNOWN_HINT: &str =
    "resolve any keyring unlock prompt, then run `agyva recover`";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    Usage,
    NotFound,
    Ambiguous,
    Conflict,
    Refused,
    Locked,
    RecoveryRequired,
    Backend,
    /// A keyring call did not answer in time. For a read this changes nothing.
    Timeout,
    /// A keyring write or delete timed out: it may or may not have been applied.
    OutcomeUnknown,
    /// agy's login changed underneath a running switch; nothing was written.
    ConcurrentChange,
    Vault,
    Io,
    Format,
    Incompatible,
    NotImplemented,
    SimulatedCrash,
}

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct Error {
    kind: ErrorKind,
    message: String,
    hint: Option<String>,
    #[source]
    source: Option<Box<dyn std::error::Error + Send + Sync>>,
}

impl Error {
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
            hint: None,
            source: None,
        }
    }

    #[must_use]
    pub fn hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    #[must_use]
    pub fn with_source(mut self, source: impl std::error::Error + Send + Sync + 'static) -> Self {
        self.source = Some(Box::new(source));
        self
    }

    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn hint_text(&self) -> Option<&str> {
        self.hint.as_deref()
    }

    /// The hint to show a user: the specific one if the error has it, else a
    /// general next step for its kind, so no error ends without advice.
    /// (`None` only for the simulated crash, which has no user.)
    pub fn user_hint(&self) -> Option<&str> {
        if let Some(h) = self.hint.as_deref() {
            return Some(h);
        }
        Some(match self.kind {
            ErrorKind::Usage => "see `agyva --help`",
            ErrorKind::NotFound => "run `agyva list` to see the saved accounts",
            ErrorKind::Ambiguous => "use the alias or a longer id prefix (see `agyva list`)",
            ErrorKind::Conflict => "see `agyva list` for what is already saved",
            ErrorKind::Refused => "run `agyva doctor` for details",
            ErrorKind::Locked => "wait for the other agyva to finish, then retry",
            ErrorKind::RecoveryRequired => "run `agyva recover` (or `agyva status` for details)",
            ErrorKind::Backend => "run `agyva doctor` to check access to agy's credential store",
            ErrorKind::Timeout => {
                "the OS keyring did not answer in time; resolve any unlock prompt and retry"
            }
            ErrorKind::OutcomeUnknown => OUTCOME_UNKNOWN_HINT,
            ErrorKind::ConcurrentChange => "run the command again",
            ErrorKind::Vault => {
                "check the passphrase or keyring access; `agyva doctor` shows the vault mode"
            }
            ErrorKind::Io => {
                "check permissions and free space of agyva's directories (`agyva doctor`)"
            }
            ErrorKind::Format => {
                "run `agyva doctor`; restore a corrupt file from a backup if you have one"
            }
            ErrorKind::Incompatible => {
                "run `agyva version` to see the agy version; `--allow-untested` tries it anyway"
            }
            ErrorKind::NotImplemented => "it is planned for a later release",
            ErrorKind::SimulatedCrash => return None,
        })
    }

    pub fn exit_code(&self) -> i32 {
        match self.kind {
            // A selector that matches nothing, or more than one account, is
            // a command-line usage error: the documented table (help, README)
            // says exit 2, and scripts can retry with a better selector.
            ErrorKind::Usage | ErrorKind::NotFound | ErrorKind::Ambiguous => exit::USAGE,
            ErrorKind::RecoveryRequired | ErrorKind::OutcomeUnknown => exit::RECOVERY_REQUIRED,
            ErrorKind::SimulatedCrash => exit::SIMULATED_CRASH,
            _ => exit::ERROR,
        }
    }

    pub fn usage(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Usage, message)
    }

    pub fn refused(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Refused, message)
    }

    pub fn conflict(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Conflict, message)
    }

    pub fn format(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Format, message)
    }

    pub fn backend(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Backend, message)
    }

    pub fn vault(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::Vault, message)
    }

    pub fn io(context: impl fmt::Display, source: std::io::Error) -> Self {
        let message = format!("{context}: {source}");
        Self::new(ErrorKind::Io, message).with_source(source)
    }

    pub fn recovery_required(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::RecoveryRequired, message)
            .hint("run `agyva recover` (or `agyva status` for details)")
    }

    /// A native write/delete whose result is unknown (timeout).
    pub fn outcome_unknown(message: impl Into<String>) -> Self {
        Self::new(ErrorKind::OutcomeUnknown, message).hint(OUTCOME_UNKNOWN_HINT)
    }

    pub fn not_implemented(what: &str) -> Self {
        Self::new(
            ErrorKind::NotImplemented,
            format!("`{what}` is not yet implemented in this build"),
        )
        .hint("it is planned for a later release")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: &[ErrorKind] = &[
        ErrorKind::Usage,
        ErrorKind::NotFound,
        ErrorKind::Ambiguous,
        ErrorKind::Conflict,
        ErrorKind::Refused,
        ErrorKind::Locked,
        ErrorKind::RecoveryRequired,
        ErrorKind::Backend,
        ErrorKind::Timeout,
        ErrorKind::OutcomeUnknown,
        ErrorKind::ConcurrentChange,
        ErrorKind::Vault,
        ErrorKind::Io,
        ErrorKind::Format,
        ErrorKind::Incompatible,
        ErrorKind::NotImplemented,
    ];

    #[test]
    fn every_user_facing_kind_has_a_hint_and_a_documented_exit_code() {
        for kind in ALL {
            let e = Error::new(*kind, "something went wrong");
            assert!(e.user_hint().is_some_and(|h| !h.is_empty()), "{kind:?}");
            assert!(
                [exit::ERROR, exit::USAGE, exit::RECOVERY_REQUIRED].contains(&e.exit_code()),
                "{kind:?}"
            );
        }
        assert_eq!(Error::new(ErrorKind::SimulatedCrash, "x").user_hint(), None);
        // Documented in `--help` and the README: bad selectors are usage errors.
        for kind in [ErrorKind::NotFound, ErrorKind::Ambiguous] {
            assert_eq!(Error::new(kind, "x").exit_code(), exit::USAGE, "{kind:?}");
        }
        // A specific hint wins over the general one.
        let e = Error::refused("no").hint("do this");
        assert_eq!(e.user_hint(), Some("do this"));
    }
}
