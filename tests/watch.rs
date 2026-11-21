//! The watch loop with a fake clock, fake probe and no sleeping.

mod common;

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use agyva::core::health::AccountHealth;
use agyva::core::quota::Freshness;
use agyva::core::usage::{AccountUsage, HealthView, QuotaView, WindowView};
use agyva::core::watch::{
    AppProbe, Clock, CycleReport, Jitter, Probe, Sink, StopReason, WatchConfig, WatchEvent,
    backoff_delay, run_watch,
};
use chrono::{DateTime, Duration as Cd, Utc};
use common::fake_agy::FakeAgy;
use common::*;

struct FakeClock {
    now: Cell<DateTime<Utc>>,
    waits: RefCell<Vec<Duration>>,
    stop_after_waits: Option<(usize, Arc<AtomicBool>)>,
}

impl FakeClock {
    fn new() -> Self {
        Self {
            now: Cell::new(Utc::now()),
            waits: RefCell::new(Vec::new()),
            stop_after_waits: None,
        }
    }
}

impl Clock for FakeClock {
    fn now(&self) -> DateTime<Utc> {
        self.now.get()
    }
    fn wait(&self, d: Duration, _stop: &AtomicBool) {
        self.waits.borrow_mut().push(d);
        self.now.set(self.now.get() + Cd::from_std(d).unwrap());
        if let Some((n, flag)) = &self.stop_after_waits
            && self.waits.borrow().len() >= *n
        {
            flag.store(true, Ordering::SeqCst); // "Ctrl+C"
        }
    }
}

struct Fixed(f64);
impl Jitter for Fixed {
    fn unit(&mut self) -> f64 {
        self.0
    }
}

#[derive(Default)]
struct Events(Vec<WatchEvent>);
impl Sink for Events {
    fn emit(&mut self, _at: DateTime<Utc>, e: &WatchEvent) {
        self.0.push(e.clone());
    }
}

struct Script(VecDeque<CycleReport>);
impl Probe for Script {
    fn cycle(&mut self, _now: DateTime<Utc>) -> CycleReport {
        self.0.pop_front().unwrap_or_default()
    }
}

fn account(alias: &str, state: AccountHealth, live: bool) -> AccountUsage {
    AccountUsage {
        number: 1,
        id: format!("id-{alias}"),
        alias: alias.into(),
        email: None,
        active: true,
        plan: None,
        freshness: Freshness::Fresh,
        observed_at: None,
        last_successful_update: None,
        last_attempt: None,
        refreshed_now: live,
        groups: vec![],
        unavailable: None,
        health: HealthView {
            state,
            label: state.label(),
            evidence: "test".into(),
            observed_at: None,
            source: agyva::core::health::HealthSource::AgyUsageCommand,
            verified_now: live,
            warning: None,
        },
    }
}

fn failing(why: &str) -> CycleReport {
    CycleReport {
        failure: Some(why.into()),
        ..CycleReport::default()
    }
}

fn ok(a: AccountUsage) -> CycleReport {
    CycleReport {
        accounts: vec![a],
        ..CycleReport::default()
    }
}

fn cfg(interval: u64, cycles: u64) -> WatchConfig {
    let mut c = WatchConfig::new(Duration::from_secs(interval));
    c.max_cycles = Some(cycles);
    c
}

#[test]
fn backoff_grows_with_jitter_and_is_capped() {
    let i = Duration::from_secs(60);
    let cap = Duration::from_secs(1800);
    assert_eq!(backoff_delay(i, 1, cap, 0.5), Duration::from_secs(120));
    assert_eq!(backoff_delay(i, 2, cap, 0.5), Duration::from_secs(240));
    assert!(backoff_delay(i, 1, cap, 0.0) < backoff_delay(i, 1, cap, 1.0));
    assert_eq!(backoff_delay(i, 40, cap, 1.0), cap, "capped");
    assert!(
        backoff_delay(i, 0, cap, 0.0) >= i,
        "never below the interval"
    );
}

#[test]
fn transient_failures_back_off_then_recover_without_real_sleeping() {
    let clock = FakeClock::new();
    let mut probe = Script(
        [
            failing("offline"),
            failing("offline"),
            ok(account("a", AccountHealth::Healthy, true)),
            ok(account("a", AccountHealth::Healthy, true)),
        ]
        .into(),
    );
    let mut sink = Events::default();
    let stop = AtomicBool::new(false);
    let started = std::time::Instant::now();
    let s = run_watch(
        &cfg(300, 4),
        &clock,
        &mut Fixed(0.5),
        &mut probe,
        &mut sink,
        &stop,
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!((s.cycles, s.reason), (4, StopReason::CycleLimit));
    let waits: Vec<u64> = clock.waits.borrow().iter().map(Duration::as_secs).collect();
    assert_eq!(waits, [600, 1200, 300], "2x, 4x, then back to the interval");
    let fails = sink
        .0
        .iter()
        .filter(|e| matches!(e, WatchEvent::CycleFailed { .. }))
        .count();
    assert_eq!(fails, 2);
}

#[test]
fn ctrl_c_stops_the_loop_cleanly() {
    let stop = Arc::new(AtomicBool::new(false));
    let mut clock = FakeClock::new();
    clock.stop_after_waits = Some((2, stop.clone()));
    let mut probe = Script(
        (0..10)
            .map(|_| ok(account("a", AccountHealth::Healthy, true)))
            .collect(),
    );
    let mut sink = Events::default();
    let mut c = WatchConfig::new(Duration::from_secs(60));
    c.max_cycles = None;
    let s = run_watch(&c, &clock, &mut Fixed(0.5), &mut probe, &mut sink, &stop);
    assert_eq!(s.reason, StopReason::Interrupted);
    assert_eq!(s.cycles, 2);
    assert!(matches!(
        sink.0.last(),
        Some(WatchEvent::Stopped {
            reason: StopReason::Interrupted,
            ..
        })
    ));
}

#[test]
fn health_changes_are_announced_and_cached_flips_are_not() {
    let clock = FakeClock::new();
    let mut cached = account("a", AccountHealth::Unknown, false);
    cached.health.verified_now = false;
    let mut probe = Script(
        [
            ok(account("a", AccountHealth::Healthy, true)),
            ok(cached), // throttled cycle: only a memory, not news
            ok(account("a", AccountHealth::AuthenticationRequired, true)),
        ]
        .into(),
    );
    let mut sink = Events::default();
    run_watch(
        &cfg(60, 3),
        &clock,
        &mut Fixed(0.0),
        &mut probe,
        &mut sink,
        &AtomicBool::new(false),
    );
    let changes: Vec<_> = sink
        .0
        .iter()
        .filter_map(|e| match e {
            WatchEvent::HealthChanged { from, to, .. } => Some((*from, *to)),
            _ => None,
        })
        .collect();
    assert_eq!(
        changes,
        [(
            Some(AccountHealth::Healthy),
            AccountHealth::AuthenticationRequired
        )],
        "first sight of a healthy account is baseline, cached memory is ignored"
    );
}

#[test]
fn reset_times_are_announced_once() {
    let clock = FakeClock::new();
    let reset = clock.now.get() + Cd::minutes(5);
    let mut a = account("a", AccountHealth::Healthy, true);
    a.groups = vec![QuotaView {
        group: "Gemini".into(),
        model: None,
        disabled: false,
        exhausted: false,
        five_hour: Some(WindowView {
            remaining_percent: 10.0,
            reset_at: Some(reset),
            observed_at: clock.now.get(),
            source: agyva::core::quota::QuotaSource::AgyUsageCommand,
            burn_rate: None,
        }),
        weekly: None,
    }];
    let mut probe = Script([ok(a.clone()), ok(a)].into());
    let mut sink = Events::default();
    run_watch(
        &cfg(60, 2),
        &clock,
        &mut Fixed(0.0),
        &mut probe,
        &mut sink,
        &AtomicBool::new(false),
    );
    let n = sink
        .0
        .iter()
        .filter(|e| matches!(e, WatchEvent::ResetApproaching { .. }))
        .count();
    assert_eq!(n, 1);
}

#[test]
fn watch_skips_a_suspended_account_and_never_runs_agy_for_it() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let fake = FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.fail(
        "account disabled: violation of the Terms of Service (PERMISSION_DENIED)",
        1,
    );
    let app = rig.app_with_agy(&fake);
    let clock = FakeClock::new();
    let mut probe = AppProbe(&app);
    let mut sink = Events::default();
    run_watch(
        &cfg(60, 4),
        &clock,
        &mut Fixed(0.0),
        &mut probe,
        &mut sink,
        &AtomicBool::new(false),
    );
    assert_eq!(
        fake.calls(),
        1,
        "one look, then the suspended account is left alone"
    );
    let skipped = sink
        .0
        .iter()
        .filter(|e| matches!(e, WatchEvent::SuspendedSkipped { .. }))
        .count();
    assert_eq!(skipped, 1, "reported once, not every cycle");
    assert!(sink.0.iter().any(|e| matches!(
        e,
        WatchEvent::HealthChanged {
            to: AccountHealth::TermsSuspended,
            ..
        }
    )));
}

#[test]
fn threshold_alerts_fire_only_on_state_transitions() {
    let clock = FakeClock::new();
    let make_acc = |pct: f64| {
        let mut a = account("a", AccountHealth::Healthy, true);
        a.groups = vec![QuotaView {
            group: "Gemini".into(),
            model: None,
            disabled: false,
            exhausted: pct <= 0.0,
            five_hour: Some(WindowView {
                remaining_percent: pct,
                reset_at: None,
                observed_at: clock.now.get(),
                source: agyva::core::quota::QuotaSource::AgyUsageCommand,
                burn_rate: None,
            }),
            weekly: None,
        }];
        a
    };
    let mut probe = Script(
        [
            ok(make_acc(50.0)), // normal -> silent
            ok(make_acc(20.0)), // crosses warn (25%) -> WARN
            ok(make_acc(18.0)), // still warn -> no spam
            ok(make_acc(8.0)),  // crosses critical (10%) -> CRITICAL
            ok(make_acc(5.0)),  // still critical -> no spam
            ok(make_acc(80.0)), // recovers -> RECOVERED
        ]
        .into(),
    );
    let mut sink = Events::default();
    let cfg = cfg(60, 6).with_thresholds(Some(25.0), Some(10.0));
    run_watch(
        &cfg,
        &clock,
        &mut Fixed(0.0),
        &mut probe,
        &mut sink,
        &AtomicBool::new(false),
    );
    let notes: Vec<&str> = sink
        .0
        .iter()
        .filter_map(|e| match e {
            WatchEvent::Note { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(notes.len(), 3, "{notes:?}");
    assert!(notes[0].starts_with("WARN "));
    assert!(notes[1].starts_with("CRITICAL "));
    assert!(notes[2].starts_with("RECOVERED "));
}
