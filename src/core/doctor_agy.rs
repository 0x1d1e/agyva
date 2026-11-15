//! Doctor checks about agy itself and the quota source: where agy is, whether
//! the OS keyring answers, whether usage can be read, and whether a refresh
//! has been failing. Read-only.

use crate::agy::discovery;
use crate::core::app::App;
use crate::core::doctor::{Collector, Level};
use crate::core::health::AccountHealth;
use crate::storage::keyring::StoreErrorKind;

/// A read of an item that does not exist: it asks the keyring "are you
/// there?" without creating or changing anything.
const PROBE_SERVICE: &str = "agyva-doctor";
const PROBE_USER: &str = "probe";

impl App {
    pub(crate) fn check_agy_extras(&self, c: &mut Collector) {
        match discovery::find_agy(&self.env) {
            Some(p) => c.ok("agy-path", p.display().to_string()),
            None => c.add(
                "agy-path",
                Level::Warn,
                "agy was not found on PATH",
                Some("install agy or set AGYVA_AGY_BIN"),
            ),
        }

        match self.secrets.get(PROBE_SERVICE, PROBE_USER) {
            Ok(_) => c.ok("keyring", "the OS keyring answers"),
            Err(e) => {
                let hint = match e.kind {
                    StoreErrorKind::Locked => "unlock your OS keyring",
                    _ => "the passphrase vault works without a keyring",
                };
                c.add("keyring", Level::Info, e.message, Some(hint));
            }
        }

        let provider = self.quota_provider_status();
        c.add(
            "quota-provider",
            if provider.available {
                Level::Ok
            } else {
                Level::Warn
            },
            provider.detail,
            None,
        );

        self.check_cached_health(c);
    }

    /// Cached, never "live": remembered suspensions must stay visible.
    fn check_cached_health(&self, c: &mut Collector) {
        let Ok(reg) = self.load_registry() else {
            return;
        };
        let cache = self.quota_cache().load();
        let mut suspended = Vec::new();
        let mut failing = Vec::new();
        for a in &reg.accounts {
            let Some(e) = cache.accounts.get(&a.id) else {
                continue;
            };
            match e.health.as_ref().map(|h| h.state) {
                Some(AccountHealth::TermsSuspended) => suspended.push(a.alias.as_str()),
                Some(s) if s.is_sticky() => failing.push(a.alias.as_str()),
                _ => {}
            }
        }
        if !suspended.is_empty() {
            c.add(
                "suspension",
                Level::Warn,
                format!(
                    "agy reported a Terms of Service suspension for: {}",
                    suspended.join(", ")
                ),
                Some(crate::core::health::APPEAL_NOTICE),
            );
        }
        if !failing.is_empty() {
            c.add(
                "account-access",
                Level::Warn,
                format!("sign-in problems reported for: {}", failing.join(", ")),
                Some("sign in again with agy, then `agyva add --refresh`"),
            );
        }
    }
}

pub(crate) struct ProviderStatus {
    pub available: bool,
    pub detail: String,
}

impl App {
    pub(crate) fn quota_provider_status(&self) -> ProviderStatus {
        let p = self.quota_provider();
        match p.unavailable() {
            None => ProviderStatus {
                available: true,
                detail: format!("{} is available", p.name()),
            },
            Some(u) => ProviderStatus {
                available: false,
                detail: format!("{} is unavailable: {}", p.name(), u.text()),
            },
        }
    }
}
