//! Account health, classified from *explicit* evidence only.
//!
//! The classifier reads what agy printed (stderr, exit status, and the
//! non-quota parts of stdout) and looks for explicit signals. It is
//! deliberately conservative:
//!
//! * `TermsSuspended` needs an explicit Terms-of-Service violation message.
//!   A bare HTTP 403 / `PERMISSION_DENIED` is **not** enough: it is reported
//!   as `Unknown` ("access denied, reason not stated").
//! * `Healthy` needs quota data that agy actually returned just now (exit 0).
//!   Cached data never produces `Healthy`.
//! * Stored evidence is a short description of the matching rule, never raw
//!   agy output, so it cannot contain tokens.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::quota::ModelQuota;

pub const APPEAL_NOTICE: &str = "Google reported that this account is suspended for a Terms of Service violation. \
Agyva will not retry it automatically and will not work around the suspension. \
See the Antigravity FAQ (https://antigravity.google/docs/faq) and contact Google support via the account's suspension notice.";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountHealth {
    Healthy,
    QuotaLimited,
    TokenExpired,
    AuthenticationRequired,
    AccessRevoked,
    TermsSuspended,
    NetworkUnavailable,
    Unknown,
}

impl AccountHealth {
    pub fn label(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::QuotaLimited => "quota limited",
            Self::TokenExpired => "token expired",
            Self::AuthenticationRequired => "login required",
            Self::AccessRevoked => "access revoked",
            Self::TermsSuspended => "SUSPENDED (ToS)",
            Self::NetworkUnavailable => "offline",
            Self::Unknown => "unknown",
        }
    }

    /// States that stay true until something explicitly contradicts them; a
    /// transient failure (offline, unknown) must not overwrite them.
    pub fn is_sticky(self) -> bool {
        matches!(
            self,
            Self::TermsSuspended
                | Self::AccessRevoked
                | Self::AuthenticationRequired
                | Self::TokenExpired
        )
    }

    /// Needs the user's attention.
    pub fn is_alarming(self) -> bool {
        matches!(
            self,
            Self::TermsSuspended
                | Self::AccessRevoked
                | Self::AuthenticationRequired
                | Self::TokenExpired
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthSource {
    /// Classified from agy's output during this run.
    AgyUsageCommand,
    /// Read back from the cache; not verified now.
    Cache,
    /// Never observed.
    None,
}

impl HealthSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::AgyUsageCommand => "agy /usage",
            Self::Cache => "cache",
            Self::None => "-",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthRecord {
    pub state: AccountHealth,
    /// Which rule matched. Never raw agy output.
    pub evidence: String,
    pub observed_at: Option<DateTime<Utc>>,
    pub source: HealthSource,
}

impl HealthRecord {
    pub fn never_observed() -> Self {
        Self {
            state: AccountHealth::Unknown,
            evidence: "never checked".into(),
            observed_at: None,
            source: HealthSource::None,
        }
    }

    /// How a stored record is presented: cached data never claims `Healthy`
    /// (it cannot be verified now), and transient offline notes expire.
    pub fn from_cache(mut self) -> Self {
        let when = self
            .observed_at
            .map(|t| t.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_default();
        if matches!(
            self.state,
            AccountHealth::Healthy | AccountHealth::NetworkUnavailable
        ) {
            self.evidence = format!(
                "last seen {} at {when} (cached; not verified now)",
                self.state.label()
            );
            self.state = AccountHealth::Unknown;
        }
        self.source = HealthSource::Cache;
        self
    }
}

/// Raw facts about one agy run.
#[derive(Debug, Clone, Default)]
pub struct Signals {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    /// stderr plus stdout text that was not quota data.
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Classification {
    pub state: AccountHealth,
    pub evidence: String,
    /// Worth retrying later with backoff (as opposed to needing the user).
    pub transient: bool,
}

fn norm(s: &str) -> String {
    s.to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// A character that is part of a word: letters, digits and `_`. Matching
/// only at word edges keeps ids (`acc_403x`), timestamps (`...403Z`) and
/// field names (`revoked_at`) from looking like status signals.
fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// `needle` appears in `text` as a whole word or phrase, not inside a longer
/// identifier or number. Both sides must be free of [`word_char`].
fn phrase(text: &str, needle: &str) -> bool {
    let mut from = 0;
    while let Some(found) = text[from..].find(needle) {
        let start = from + found;
        let end = start + needle.len();
        let before_ok = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| !word_char(c));
        let after_ok = text[end..].chars().next().is_none_or(|c| !word_char(c));
        if before_ok && after_ok {
            return true;
        }
        from = start + needle.chars().next().map_or(1, char::len_utf8);
    }
    false
}

fn any_phrase(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| phrase(text, n))
}

/// A 403 needs HTTP/status context; a bare number is often an id, a byte
/// count or a timestamp.
const HTTP_403: &[&str] = &[
    "http 403",
    "http/1.0 403",
    "http/1.1 403",
    "http/2 403",
    "http/3 403",
    "status 403",
    "status code 403",
    "status: 403",
    "code 403",
    "code: 403",
    "error 403",
    "error: 403",
    "403 forbidden",
    "403 unauthorized",
    "403 permission",
    "403 access denied",
    "403: ",
    "=> 403",
];

/// Access-denied wording, as whole words or phrases.
const DENIED: &[&str] = &[
    "permission_denied",
    "permission denied",
    "forbidden",
    "access denied",
];

/// A revocation must say what was revoked. `revoked_at` and friends do not
/// count (the `_` keeps them from matching).
const REVOKED: &[&str] = &[
    "access revoked",
    "token revoked",
    "credential revoked",
    "credentials revoked",
    "revoked access",
    "revoked token",
    "revoked credentials",
    "permissions revoked",
    "grant revoked",
    "was revoked",
    "has been revoked",
    "have been revoked",
];

fn verdict(state: AccountHealth, evidence: &str, transient: bool) -> Classification {
    Classification {
        state,
        evidence: evidence.to_owned(),
        transient,
    }
}

/// Classify one run. `quota` is `Some` only when agy returned parseable data.
pub fn classify(sig: &Signals, quota: Option<&[ModelQuota]>) -> Classification {
    // Exit 0 with parseable quota is the strongest evidence there is. Text
    // that happens to sit on stderr (banners, retries, other accounts) must
    // not override it: without this, one stray "403" would turn a healthy
    // reading into "unknown".
    if sig.exit_code == Some(0)
        && let Some(groups) = quota
    {
        if !groups.is_empty() && groups.iter().all(|g| g.disabled) {
            return verdict(
                AccountHealth::Unknown,
                "agy lists quota groups but none is enabled for this account (plan or entitlement); no usable quota to report",
                false,
            );
        }
        return if groups.iter().any(ModelQuota::exhausted) {
            verdict(
                AccountHealth::QuotaLimited,
                "an enabled quota group reports 0% remaining",
                false,
            )
        } else {
            verdict(
                AccountHealth::Healthy,
                "agy returned quota data just now",
                false,
            )
        };
    }

    let t = norm(&sig.text);

    // Only an explicit Terms-of-Service violation message counts; a bare
    // 403, PERMISSION_DENIED or "disabled" does not.
    let tos = any_phrase(&t, &["terms of service", "terms-of-service"]);
    let violation = any_phrase(
        &t,
        &[
            "violation",
            "violations",
            "violate",
            "violated",
            "violates",
            "violating",
        ],
    );
    let denied = any_phrase(&t, DENIED) || any_phrase(&t, HTTP_403);
    if tos && violation && (denied || any_phrase(&t, &["disabled", "suspended"])) {
        return verdict(
            AccountHealth::TermsSuspended,
            "agy reported an explicit Terms of Service violation message",
            false,
        );
    }
    if any_phrase(&t, REVOKED) {
        return verdict(
            AccountHealth::AccessRevoked,
            "agy reported the access or token was revoked",
            false,
        );
    }
    if any_phrase(
        &t,
        &[
            "token expired",
            "token has expired",
            "expired token",
            "token is expired",
        ],
    ) {
        return verdict(
            AccountHealth::TokenExpired,
            "agy reported an expired token",
            false,
        );
    }
    if any_phrase(
        &t,
        &[
            "not logged in",
            "login required",
            "log in again",
            "please log in",
            "please sign in",
            "sign in again",
            "authentication required",
            "unauthenticated",
            "unauthorized",
            "invalid_grant",
        ],
    ) {
        return verdict(
            AccountHealth::AuthenticationRequired,
            "agy reported that authentication is required",
            false,
        );
    }
    if any_phrase(
        &t,
        &[
            "rate limit",
            "rate-limit",
            "ratelimit",
            "too many requests",
            "resource_exhausted",
            "resource exhausted",
            "quota exceeded",
            "exceeded your quota",
            "quota exhausted",
        ],
    ) {
        return verdict(
            AccountHealth::QuotaLimited,
            "agy reported a rate limit or exhausted quota",
            true,
        );
    }
    if any_phrase(
        &t,
        &[
            "connection refused",
            "connection reset",
            "no such host",
            "dial tcp",
            "i/o timeout",
            "network is unreachable",
            "temporary failure in name resolution",
            "could not resolve host",
            "tls handshake timeout",
            "econnrefused",
            "no route to host",
            "network error",
        ],
    ) {
        return verdict(
            AccountHealth::NetworkUnavailable,
            "agy reported a network error",
            true,
        );
    }
    if denied {
        return verdict(
            AccountHealth::Unknown,
            "access denied (403 / PERMISSION_DENIED) without a stated reason; not enough evidence to call it a suspension or revocation",
            false,
        );
    }
    if sig.timed_out {
        return verdict(AccountHealth::Unknown, "agy did not answer in time", true);
    }
    match (quota, sig.exit_code) {
        (Some(groups), Some(0)) => {
            // Unreachable: handled above, kept for clarity if that check moves.
            if !groups.is_empty() && groups.iter().all(|g| g.disabled) {
                verdict(
                    AccountHealth::Unknown,
                    "agy lists quota groups but none is enabled for this account (plan or entitlement); no usable quota to report",
                    false,
                )
            } else if groups.iter().any(ModelQuota::exhausted) {
                verdict(
                    AccountHealth::QuotaLimited,
                    "an enabled quota group reports 0% remaining",
                    false,
                )
            } else {
                verdict(
                    AccountHealth::Healthy,
                    "agy returned quota data just now",
                    false,
                )
            }
        }
        (_, code) => verdict(
            AccountHealth::Unknown,
            &match code {
                Some(c) => format!("no recognizable signal (agy exit status {c})"),
                None => "no recognizable signal".to_owned(),
            },
            false,
        ),
    }
}
