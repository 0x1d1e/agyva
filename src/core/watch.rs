//! `agyva watch`: a foreground, read-only monitor.
//!
//! It only ever *observes*: it never switches accounts, never sends any
//! request to warm up quota, and never refreshes an account marked
//! Terms-suspended (the refresh layer enforces that; the loop reports it).
//! The loop itself is pure: time, sleeping, randomness, the data source and
//! the output are all injected, so it is tested without waiting or agy.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::config::Config;
use crate::core::app::App;
use crate::core::health::AccountHealth;
use crate::core::quota::UnavailableReason;
use crate::core::refresh::RefreshStatus;
use crate::core::usage::AccountUsage;

#[derive(Debug, Clone)]
pub struct WatchConfig {
    pub interval: Duration,
    pub max_backoff: Duration,
    pub max_cycles: Option<u64>,
    /// Announce a quota window that resets within this long.
    pub reset_window: Duration,
    /// Optional warning threshold (% remaining).
    pub warn_percent: Option<f64>,
    /// Optional critical threshold (% remaining).
    pub critical_percent: Option<f64>,
}

impl WatchConfig {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            max_backoff: Duration::from_secs(30 * 60),
            max_cycles: None,
            reset_window: Duration::from_secs(10 * 60),
            warn_percent: None,
            critical_percent: None,
        }
    }

    pub fn from_config(interval: Duration, cfg: &Config) -> Self {
        Self::new(interval).with_thresholds(Some(cfg.warn_percent), Some(cfg.critical_percent))
    }

    #[must_use]
    pub fn with_thresholds(mut self, warn: Option<f64>, critical: Option<f64>) -> Self {
        self.warn_percent = warn;
        self.critical_percent = critical;
        self
    }
}

/// What one observation produced.
#[derive(Debug, Clone, Default)]
pub struct CycleReport {
    pub accounts: Vec<AccountUsage>,
    /// A transient failure worth backing off from (offline, timeout, ...).
    pub failure: Option<String>,
    /// The active account is suspended and was deliberately not refreshed.
    pub skipped_suspended: Option<String>,
    /// Anything else worth telling the user (not a failure).
    pub note: Option<String>,
}

pub trait Probe {
    fn cycle(&mut self, now: DateTime<Utc>) -> CycleReport;
}

pub trait Clock {
    fn now(&self) -> DateTime<Utc>;
    /// Wait up to `d`, returning early once `stop` is set.
    fn wait(&self, d: Duration, stop: &AtomicBool);
}

pub trait Jitter {
    /// A value in `0.0..1.0`.
    fn unit(&mut self) -> f64;
}

pub trait Sink {
    fn emit(&mut self, at: DateTime<Utc>, event: &WatchEvent);
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum WatchEvent {
    Started {
        interval_secs: u64,
    },
    CycleOk {
        cycle: u64,
        summary: String,
    },
    CycleFailed {
        cycle: u64,
        reason: String,
        retry_in_secs: u64,
    },
    Note {
        text: String,
    },
    HealthChanged {
        alias: String,
        from: Option<AccountHealth>,
        to: AccountHealth,
    },
    ResetApproaching {
        alias: String,
        window: String,
        group: String,
        reset_at: DateTime<Utc>,
    },
    SuspendedSkipped {
        alias: String,
    },
    Stopped {
        cycles: u64,
        reason: StopReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Interrupted,
    CycleLimit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchSummary {
    pub cycles: u64,
    pub reason: StopReason,
}

/// `interval * 2^failures`, jittered by +-20%, never above `cap`.
pub fn backoff_delay(interval: Duration, failures: u32, cap: Duration, unit: f64) -> Duration {
    let base = interval.as_secs_f64() * 2f64.powi(failures.min(16) as i32);
    let jittered = base * (0.8 + 0.4 * unit.clamp(0.0, 1.0));
    let floor = interval.min(cap).as_secs_f64();
    Duration::from_secs_f64(jittered.max(floor).min(cap.as_secs_f64()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum ThresholdLevel {
    Normal,
    Warn,
    Critical,
}

impl ThresholdLevel {
    fn classify(remaining: f64, cfg: &WatchConfig) -> Option<Self> {
        if cfg.warn_percent.is_none() && cfg.critical_percent.is_none() {
            return None;
        }
        if cfg.critical_percent.is_some_and(|c| remaining <= c) {
            Some(Self::Critical)
        } else if cfg.warn_percent.is_some_and(|w| remaining <= w) {
            Some(Self::Warn)
        } else {
            Some(Self::Normal)
        }
    }
}

struct Tracker {
    health: BTreeMap<String, AccountHealth>,
    announced: BTreeSet<String>,
    suspended_noted: BTreeSet<String>,
    thresholds: BTreeMap<String, ThresholdLevel>,
}

impl Tracker {
    fn events(
        &mut self,
        report: &CycleReport,
        now: DateTime<Utc>,
        cfg: &WatchConfig,
    ) -> Vec<WatchEvent> {
        let mut out = Vec::new();
        for a in &report.accounts {
            // Cached health is a memory, not an observation: only live
            // readings and alarming states can change what we announce.
            if a.health.verified_now || a.health.state.is_alarming() {
                let prev = self.health.insert(a.id.clone(), a.health.state);
                if prev != Some(a.health.state) && (prev.is_some() || a.health.state.is_alarming())
                {
                    out.push(WatchEvent::HealthChanged {
                        alias: a.alias.clone(),
                        from: prev,
                        to: a.health.state,
                    });
                }
            }
            let window = chrono::Duration::from_std(cfg.reset_window).unwrap_or_default();
            for g in a.groups.iter().filter(|g| !g.disabled) {
                for (name, w) in [("5h", &g.five_hour), ("weekly", &g.weekly)] {
                    let Some(w) = w else { continue };
                    if let Some(level) = ThresholdLevel::classify(w.remaining_percent, cfg) {
                        let tkey = format!("{}|{}|{name}", a.id, g.label());
                        let prev = self.thresholds.insert(tkey, level);
                        let changed = prev.is_some_and(|p| p != level)
                            || (prev.is_none() && level != ThresholdLevel::Normal);
                        if changed {
                            let pct = crate::core::format::percent_text(w.remaining_percent);
                            let msg = match level {
                                ThresholdLevel::Critical => format!(
                                    "CRITICAL {}: {} {name} remaining {pct} (<= {:.0}%)",
                                    a.alias,
                                    g.label(),
                                    cfg.critical_percent.unwrap_or(0.0)
                                ),
                                ThresholdLevel::Warn => format!(
                                    "WARN {}: {} {name} remaining {pct} (<= {:.0}%)",
                                    a.alias,
                                    g.label(),
                                    cfg.warn_percent.unwrap_or(0.0)
                                ),
                                ThresholdLevel::Normal => format!(
                                    "RECOVERED {}: {} {name} remaining {pct}",
                                    a.alias,
                                    g.label()
                                ),
                            };
                            out.push(WatchEvent::Note { text: msg });
                        }
                    }
                    let Some(reset) = w.reset_at else { continue };
                    if w.remaining_percent < 100.0 && reset > now && reset - now <= window {
                        let key = format!("{}|{}|{name}|{}", a.id, g.label(), reset.timestamp());
                        if self.announced.insert(key) {
                            out.push(WatchEvent::ResetApproaching {
                                alias: a.alias.clone(),
                                window: name.to_owned(),
                                group: g.label(),
                                reset_at: reset,
                            });
                        }
                    }
                }
            }
        }
        if let Some(alias) = &report.skipped_suspended
            && self.suspended_noted.insert(alias.clone())
        {
            out.push(WatchEvent::SuspendedSkipped {
                alias: alias.clone(),
            });
        }
        out
    }
}

fn summary_line(report: &CycleReport) -> String {
    let Some(active) = report.accounts.iter().find(|a| a.active) else {
        return report
            .note
            .clone()
            .unwrap_or_else(|| "no active account".to_owned());
    };
    let (five, week) = active.lowest();
    let pct = |w: Option<&crate::core::usage::WindowView>| {
        w.map_or_else(
            || "-".to_owned(),
            |w| crate::core::format::percent_text(w.remaining_percent),
        )
    };
    let mut s = format!(
        "{}: 5H {} WEEKLY {} ({}, {})",
        active.alias,
        pct(five),
        pct(week),
        active.health.label,
        active.freshness.label()
    );
    if let Some(n) = &report.note {
        s.push_str(&format!("; {n}"));
    }
    s
}

pub fn run_watch(
    cfg: &WatchConfig,
    clock: &dyn Clock,
    jitter: &mut dyn Jitter,
    probe: &mut dyn Probe,
    sink: &mut dyn Sink,
    stop: &AtomicBool,
) -> WatchSummary {
    let mut tracker = Tracker {
        health: BTreeMap::new(),
        announced: BTreeSet::new(),
        suspended_noted: BTreeSet::new(),
        thresholds: BTreeMap::new(),
    };
    sink.emit(
        clock.now(),
        &WatchEvent::Started {
            interval_secs: cfg.interval.as_secs(),
        },
    );
    let (mut cycles, mut failures) = (0u64, 0u32);
    loop {
        if stop.load(Ordering::SeqCst) {
            return finish(clock, sink, cycles, StopReason::Interrupted);
        }
        let now = clock.now();
        let report = probe.cycle(now);
        cycles += 1;
        for e in tracker.events(&report, now, cfg) {
            sink.emit(now, &e);
        }
        let delay = match &report.failure {
            Some(reason) => {
                failures = failures.saturating_add(1);
                let d = backoff_delay(cfg.interval, failures, cfg.max_backoff, jitter.unit());
                sink.emit(
                    now,
                    &WatchEvent::CycleFailed {
                        cycle: cycles,
                        reason: reason.clone(),
                        retry_in_secs: d.as_secs(),
                    },
                );
                d
            }
            None => {
                failures = 0;
                sink.emit(
                    now,
                    &WatchEvent::CycleOk {
                        cycle: cycles,
                        summary: summary_line(&report),
                    },
                );
                cfg.interval
            }
        };
        if cfg.max_cycles.is_some_and(|m| cycles >= m) {
            return finish(clock, sink, cycles, StopReason::CycleLimit);
        }
        clock.wait(delay, stop);
    }
}

fn finish(clock: &dyn Clock, sink: &mut dyn Sink, cycles: u64, reason: StopReason) -> WatchSummary {
    sink.emit(clock.now(), &WatchEvent::Stopped { cycles, reason });
    WatchSummary { cycles, reason }
}

// ------------------------------------------------------------ real parts

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
    fn wait(&self, d: Duration, stop: &AtomicBool) {
        let end = std::time::Instant::now() + d;
        while !stop.load(Ordering::SeqCst) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            std::thread::sleep(left.min(Duration::from_millis(200)));
        }
    }
}

/// Tiny xorshift generator seeded from the clock; jitter needs no quality.
pub struct SystemJitter(u64);

impl SystemJitter {
    pub fn new() -> Self {
        let seed = Utc::now().timestamp_subsec_nanos() as u64 ^ (std::process::id() as u64) << 32;
        Self(seed | 1)
    }
}

impl Default for SystemJitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Jitter for SystemJitter {
    fn unit(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Observes through [`App::refresh_active_at`]; read-only.
pub struct AppProbe<'a>(pub &'a App);

impl Probe for AppProbe<'_> {
    fn cycle(&mut self, now: DateTime<Utc>) -> CycleReport {
        let outcome = match self.0.refresh_active_at(false, now) {
            Ok(o) => o,
            Err(e) => {
                return CycleReport {
                    failure: Some(e.message().to_owned()),
                    ..CycleReport::default()
                };
            }
        };
        let accounts = self
            .0
            .account_usages(Some(&outcome), now)
            .unwrap_or_default();
        let mut report = CycleReport {
            accounts,
            ..CycleReport::default()
        };
        if let RefreshStatus::Skipped(u) | RefreshStatus::Failed(u) = &outcome.status {
            match u.reason {
                UnavailableReason::Suspended => {
                    report.skipped_suspended = report
                        .accounts
                        .iter()
                        .find(|a| a.active)
                        .map(|a| a.alias.clone());
                }
                UnavailableReason::TooSoon | UnavailableReason::RefreshBusy => {}
                _ if outcome.is_transient_failure() => report.failure = Some(u.text()),
                _ => report.note = Some(u.text()),
            }
        }
        report
    }
}
