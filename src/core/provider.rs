//! Where quota comes from. Today there is exactly one real source: agy's own
//! `/usage` command, run as a subprocess against the *currently logged-in*
//! agy account. There is no other path (no private endpoints, no OAuth client
//! of our own), so quota for other accounts is only ever read from the cache.

use std::path::PathBuf;

use chrono::{DateTime, Utc};

use crate::agy::usage_parse::parse_usage;
use crate::agy::usage_run::{RunEnd, RunLimits, run_usage};
use crate::core::health::Signals;
use crate::core::quota::{ModelQuota, Unavailable, UnavailableReason};

/// What happened when the provider ran.
#[derive(Debug, Clone)]
pub struct Ran {
    /// Parsed quota, or why it could not be parsed (no agy text inside).
    pub groups: Result<Vec<ModelQuota>, String>,
    pub signals: Signals,
}

#[derive(Debug, Clone)]
pub enum Reading {
    /// Nothing could be run at all.
    NoProvider(Unavailable),
    Ran(Ran),
}

pub trait QuotaProvider: Send + Sync {
    fn name(&self) -> &'static str;
    /// Read the quota of whatever account agy is logged in as right now.
    fn fetch(&self, now: DateTime<Utc>) -> Reading;
    /// Why this source cannot work at all (without running anything).
    fn unavailable(&self) -> Option<Unavailable> {
        None
    }
}

/// Used when agy is missing or its version is not allowed.
pub struct NoProvider {
    pub why: String,
}

impl QuotaProvider for NoProvider {
    fn name(&self) -> &'static str {
        "none"
    }
    fn fetch(&self, _now: DateTime<Utc>) -> Reading {
        Reading::NoProvider(Unavailable::new(UnavailableReason::NoProvider, &self.why))
    }
    fn unavailable(&self) -> Option<Unavailable> {
        Some(Unavailable::new(UnavailableReason::NoProvider, &self.why))
    }
}

pub struct AgyUsageProvider {
    pub bin: PathBuf,
    pub limits: RunLimits,
}

const TEXT_CAP: usize = 64 * 1024;

impl QuotaProvider for AgyUsageProvider {
    fn name(&self) -> &'static str {
        "agy /usage"
    }

    fn fetch(&self, now: DateTime<Utc>) -> Reading {
        let out = match run_usage(&self.bin, &self.limits) {
            Ok(o) => o,
            Err(e) => {
                return Reading::NoProvider(Unavailable::new(
                    UnavailableReason::NoProvider,
                    format!("cannot run agy ({:?})", e.kind()),
                ));
            }
        };
        let (exit_code, timed_out) = match out.end {
            RunEnd::Exited(c) => (c, false),
            RunEnd::TimedOut => (None, true),
        };
        let mut text = out.stderr.clone();
        let groups = if timed_out {
            Err("agy timed out".to_owned())
        } else if exit_code != Some(0) {
            Err(format!("agy exited with status {exit_code:?}"))
        } else {
            parse_usage(&out.stdout, now).map(|p| p.groups)
        };
        if groups.is_err() {
            // Not quota data: its text may carry the explanation.
            text.push('\n');
            text.push_str(&out.stdout);
            let mut cut = TEXT_CAP.min(text.len());
            while !text.is_char_boundary(cut) {
                cut -= 1;
            }
            text.truncate(cut);
        }
        Reading::Ran(Ran {
            groups,
            signals: Signals {
                exit_code,
                timed_out,
                text,
            },
        })
    }
}
