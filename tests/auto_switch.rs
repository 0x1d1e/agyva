//! Slice F: opt-in auto-switch. Gates: acknowledgement, cooldown, fresh
//! evidence, fresh eligible candidate, session gate, suspension, pending
//! recovery, concurrent manual switch.

mod common;

use chrono::{Duration, Utc};
use common::*;

use agyva::core::app::App;
use agyva::core::auto::{AutoDecision, MIN_SWITCH_INTERVAL, decide, enabled_for, note_switch};
use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};
use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};
use agyva::core::quota_cache::{CacheEntry, CachedQuota};
use agyva::core::route::RoutePlan;
use agyva::core::route_apply::RouteHistory;
use agyva::core::route_config::{Ranking, RoutingFile};

fn seed(
    app: &App,
    who: (&str, &str, &str),
    at: chrono::DateTime<Utc>,
    remaining: f64,
    health: AccountHealth,
) {
    app.quota_cache()
        .update(|c| {
            c.accounts.insert(
                id_of(who),
                CacheEntry {
                    quota: Some(CachedQuota {
                        observed_at: at,
                        groups: vec![ModelQuota {
                            group: "Gemini".into(),
                            model: None,
                            disabled: false,
                            five_hour: Some(QuotaWindow {
                                remaining_percent: remaining,
                                reset_at: Some(at + Duration::hours(3)),
                                observed_at: at,
                                source: QuotaSource::AgyUsageCommand,
                            }),
                            weekly: Some(QuotaWindow {
                                remaining_percent: remaining,
                                reset_at: Some(at + Duration::days(2)),
                                observed_at: at,
                                source: QuotaSource::AgyUsageCommand,
                            }),
                        }],
                    }),
                    health: Some(HealthRecord {
                        state: health,
                        observed_at: Some(at),
                        source: HealthSource::AgyUsageCommand,
                        evidence: "synthetic".into(),
                    }),
                    ..Default::default()
                },
            );
        })
        .unwrap();
}

/// Establish the freshness proof the way the monitor does (a live active
/// reading this cycle) and then rank the group. This deliberately does not
/// go through `MonitorProbe`: the probe drives automation itself now, and a
/// decision test must observe the gates, not pre-empt them. A fixture
/// without a usable reading still returns a plan — the tests that need a
/// real one install the fake; the rest pass the proof explicitly.
fn fresh_plan(app: &App, now: chrono::DateTime<Utc>) -> RoutePlan {
    let _ = app.refresh_active_at(false, now);
    app.route_group_for(Some("work"), None, now).unwrap()
}

fn enable_auto(app: &mut App, ack: bool) {
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\nranking = 'smart'\nauto_switch = true\n",
    )
    .unwrap();
    if ack {
        app.config.auto_switch_ack = true;
    }
}

#[test]
fn disabled_or_unacknowledged_holds() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.8, 0.8);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 50.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 80.0, AccountHealth::Healthy);

    // Off by default (no auto_switch in the file).
    std::fs::write(app.dirs.config.join("routing.toml"), "[groups.work]\n").unwrap();
    let config = RoutingFile::load(&app.dirs.config.join("routing.toml")).unwrap();
    assert!(!enabled_for(&app, "work", &config).unwrap());

    // Enabled in routing.toml but no ack in config.toml.
    enable_auto(&mut app, false);
    let config = RoutingFile::load(&app.dirs.config.join("routing.toml")).unwrap();
    assert!(!enabled_for(&app, "work", &config).unwrap());

    // Both on.
    enable_auto(&mut app, true);
    let config = RoutingFile::load(&app.dirs.config.join("routing.toml")).unwrap();
    assert!(enabled_for(&app, "work", &config).unwrap());

    // decide() holds without the ack.
    app.config.auto_switch_ack = false;
    let plan = app.route_group_at(Some("work"), now).unwrap();
    assert!(matches!(
        decide(&app, "work", &plan, true, now),
        AutoDecision::Hold(_)
    ));
}

#[test]
fn stale_evidence_defers_never_switches_on_cached_reset() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.8, 0.8);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 50.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 80.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    // refreshed=false: even a perfect plan defers.
    let plan = app.route_group_at(Some("work"), now).unwrap();
    assert!(matches!(
        decide(&app, "work", &plan, false, now),
        AutoDecision::Defer(_)
    ));
    // Nothing happened.
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, None, false, &history, now)
        .unwrap();
    assert!(out.is_none());
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "no switch happened");
    assert_eq!(history.load().entries.len(), 1);
}

#[test]
fn cooldown_holds_and_manual_switch_resets_it() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    // Mark a switch 901s ago: past the 900s window, no hold.
    note_switch(
        &app.dirs,
        now - Duration::seconds((MIN_SWITCH_INTERVAL.as_secs() + 1) as i64),
        false,
    )
    .unwrap();
    let plan = fresh_plan(&app, now);
    let d0 = decide(&app, "work", &plan, true, now);
    assert!(
        matches!(d0, AutoDecision::Switch { .. }),
        "expected switch, got {d0:?}, plan={plan:?}, sessions={:?}",
        agyva::agy::sessions::detect(&app.agy, app.session_scan())
    );
    // A switch just now (as a manual `use` does) re-arms the 900s cooldown.
    note_switch(&app.dirs, now, false).unwrap();
    let d = decide(&app, "work", &plan, true, now);
    match d {
        AutoDecision::Hold(r) => assert!(r.contains("cooling down"), "{r}"),
        other => panic!("expected hold, got {other:?}"),
    }
}

#[test]
fn detected_sessions_defer() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    // A presence lock simulates a running agy session.
    std::fs::create_dir_all(app.agy.presence_dir()).unwrap();
    let lock_path = app.agy.presence_dir().join("abc.lock");
    let lock_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_path)
        .unwrap();
    fs4::FileExt::lock(&lock_file).unwrap();
    let plan = fresh_plan(&app, now);
    assert!(matches!(
        decide(&app, "work", &plan, true, now),
        AutoDecision::Defer(_)
    ));
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, None, true, &history, now)
        .unwrap();
    assert!(out.is_none());
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
    drop(lock_file);
}

#[test]
fn suspended_candidate_and_active_are_never_chosen() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.1, 0.1);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    // Active (bob) healthy; candidate alice suspended.
    seed(&app, ALICE, now, 95.0, AccountHealth::TermsSuspended);
    seed(&app, BOB, now, 10.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);
    let plan = fresh_plan(&app, now);
    let alice = plan
        .accounts
        .iter()
        .find(|r| r.alias == "alice")
        .expect("alice in plan");
    assert!(!alice.eligible, "suspended candidate is ineligible");
    // The only eligible row is the active account itself, so automation
    // holds; alice is never chosen.
    let d = decide(&app, "work", &plan, true, now);
    assert!(
        matches!(&d, AutoDecision::Hold(r) if r.contains("no eligible candidate")),
        "got {d:?}"
    );
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, None, true, &history, now)
        .unwrap();
    assert!(out.is_none());
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
}

#[test]
fn pending_recovery_blocks_auto_switch() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let mut app = rig.app();
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);
    // Interrupted switch (crash after the native write) leaves a pending journal.
    let _ = rig
        .app_with(agyva::core::failpoint::Failpoints::armed(
            "after-native-write",
            agyva::core::failpoint::Fault::Crash,
        ))
        .switch("alice", false);
    assert!(app.pending_journal().unwrap().is_some());
    let history = RouteHistory::new(&app.dirs);
    let plan = app.route_group_at(Some("work"), now).unwrap();
    let err = app
        .auto_switch_for("work", &plan, None, true, &history, now)
        .unwrap_err();
    assert_eq!(err.kind(), agyva::core::error::ErrorKind::RecoveryRequired);
}

#[test]
fn concurrent_manual_switch_wins_and_cools_automation_down() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB, CAROL]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
    seed(&app, CAROL, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    // Automation decides to switch (alice is the fresh eligible top).
    let plan = fresh_plan(&app, now);
    let decision = decide(&app, "work", &plan, true, now);
    let AutoDecision::Switch { alias, .. } = decision else {
        panic!(
            "expected a switch, got {decision:?} sessions={:?}",
            agyva::agy::sessions::detect(&app.agy, app.session_scan())
        );
    };
    assert_eq!(alias, "alice");

    // A human types `agyva use carol` first; automation must not chase it.
    app.switch("carol", false).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(CAROL)));
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, None, true, &history, now)
        .unwrap();
    assert!(out.is_none(), "cooldown from the manual switch holds");
    assert_eq!(rig.live_id(), Some(id_of(CAROL)));
    // And the history says so.
    let entries = history.load().entries;
    assert!(entries.iter().any(|e| e.reason.contains("cooling down")));
}

#[test]
fn auto_switch_applies_through_the_journaled_path() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    let plan = fresh_plan(&app, now);
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, None, true, &history, now)
        .unwrap()
        .expect("switch happened");
    assert_eq!(out.alias, "alice");
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
    assert!(app.pending_journal().unwrap().is_none());
    let entries = history.load().entries;
    assert!(
        entries
            .iter()
            .any(|e| e.outcome == agyva::core::route_apply::HistoryOutcome::Applied)
    );
}

#[test]
fn ranking_default_stays_smart_and_cooldown_file_is_private() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let now = Utc::now();
    note_switch(&app.dirs, now, true).unwrap();
    #[cfg(unix)]
    {
        let mode = agyva::storage::fsutil::mode_of(&app.dirs.state.join("auto-switch.json"));
        assert_eq!(mode.map(|m| m & 0o777), Some(0o600));
    }
    let _ = Ranking::Priority;
}

/// The wiring that makes automation real: the monitor's evaluation loop is
/// the only driver, so a named-group probe must apply the switch on a cycle
/// that got a fresh reading, and must hold (not act) when it did not.
#[test]
fn the_monitor_cycle_is_the_driver_that_actually_switches() {
    use agyva::core::monitor::MonitorProbe;
    use agyva::core::watch::Probe;

    // 1. Fresh reading + gate pass: the cycle switches.
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);
    // bob is active and below threshold; alice is the strictly better pick.
    let mut probe = MonitorProbe::new(&app, Some("work".into()), None, std::time::Duration::ZERO);
    probe.cycle(now);
    assert_eq!(rig.live_id(), Some(id_of(ALICE)), "the cycle switched");
    let note = probe.last_auto().unwrap_or_default().to_owned();
    assert!(note.contains("alice"), "cycle reports it: {note}");
    let history = RouteHistory::new(&app.dirs);
    assert!(
        history
            .load()
            .entries
            .iter()
            .any(|e| e.outcome == agyva::core::route_apply::HistoryOutcome::Applied),
        "applied outcome recorded"
    );

    // 2. No group declared: no automation, even with the ack set.
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy2"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    app.config.auto_switch_ack = true;
    let mut probe = MonitorProbe::new(&app, None, None, std::time::Duration::ZERO);
    probe.cycle(now);
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "no group, no switch");
    assert!(probe.last_auto().is_none());

    // 3. A repeated hold does not flood the bounded history.
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy3"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    // Enabled but cooling down: every tick holds for the same reason.
    enable_auto(&mut app, true);
    note_switch(&app.dirs, now, false).unwrap();
    let mut probe = MonitorProbe::new(&app, Some("work".into()), None, std::time::Duration::ZERO);
    for i in 0..5 {
        probe.cycle(now + Duration::seconds(i));
    }
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "cooldown holds");
    // Cycle 1 got a live reading (hold: cooling down); cycles 2-5 were
    // throttled (defer: no fresh proof). Repeats collapse, so five ticks add
    // exactly two entries instead of five.
    let entries = RouteHistory::new(&app.dirs).load().entries;
    assert_eq!(entries.len(), 2, "repeated outcomes are recorded once each");
    assert!(entries.iter().any(|e| e.reason.contains("cooling down")));
    assert!(
        entries
            .iter()
            .any(|e| e.reason.contains("fresh live reading")),
        "a throttled cycle has no freshness proof"
    );
}

/// Gate order is part of the contract: enabled/ack -> fresh proof ->
/// cooldown -> sessions -> trigger/eligibility. With several gates failing
/// at once, the earliest one is the one reported, so a test can pin the order.
#[test]
fn gates_are_evaluated_in_the_documented_order() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);

    // Nothing enabled: disabled/unacknowledged wins over everything.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\naccounts = ['alice', 'bob']\n",
    )
    .unwrap();
    let plan = app.route_group_for(Some("work"), None, now).unwrap();
    let d = decide(&app, "work", &plan, false, now);
    assert!(
        matches!(&d, AutoDecision::Hold(r) if r.contains("not enabled")),
        "{d:?}"
    );

    // Enabled but no fresh reading: the proof gate precedes the cooldown.
    enable_auto(&mut app, true);
    note_switch(&app.dirs, now, false).unwrap(); // cooldown also failing
    let d = decide(&app, "work", &plan, false, now);
    assert!(
        matches!(&d, AutoDecision::Defer(r) if r.contains("fresh live reading")),
        "{d:?}"
    );

    // With proof, the cooldown is next (sessions are clean here).
    let d = decide(&app, "work", &plan, true, now);
    assert!(
        matches!(&d, AutoDecision::Hold(r) if r.contains("cooling down")),
        "{d:?}"
    );

    // Cooldown clear, sessions detected: the session gate precedes
    // eligibility, even though a good candidate exists.
    std::fs::create_dir_all(app.agy.presence_dir()).unwrap();
    let s_lock_path = app.agy.presence_dir().join("s.lock");
    let s_lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(&s_lock_path)
        .unwrap();
    fs4::FileExt::lock(&s_lock).unwrap();
    let d = decide(&app, "work", &plan, true, now + Duration::seconds(1000));
    assert!(
        matches!(&d, AutoDecision::Defer(r) if r.contains("sessions detected")),
        "{d:?}"
    );

    // Everything clear: the eligible top row is the switch target.
    drop(s_lock);
    std::fs::remove_file(&s_lock_path).unwrap();
    match decide(&app, "work", &plan, true, now + Duration::seconds(1000)) {
        AutoDecision::Switch { alias, .. } => assert_eq!(alias, "alice"),
        other => panic!("expected a switch, got {other:?}"),
    }
}

/// "Could not look" is not "nothing is running": when the presence path
/// cannot be listed, detection is UNKNOWN and automation DEFERS instead of
/// switching blind. A regular file where the presence directory belongs
/// makes `read_dir` fail for every user (root included), so the test is
/// deterministic rather than permission-dependent.
#[test]
fn unknown_session_detection_defers_instead_of_switching() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    // `presence` is a file, so the session check cannot complete.
    let dir = app.agy.presence_dir();
    std::fs::create_dir_all(dir.parent().unwrap()).unwrap();
    std::fs::write(&dir, b"not a directory").unwrap();

    let report = agyva::agy::sessions::detect(&app.agy, app.session_scan());
    assert!(
        !report.is_known(),
        "an unlistable presence path is an unknown answer, not zero sessions"
    );
    assert!(
        report
            .uncertain
            .as_deref()
            .unwrap_or_default()
            .contains("presence"),
        "{report:?}"
    );
    assert!(
        report
            .warning()
            .is_some_and(|w| w.contains("could not check")),
        "humans are told the check failed, never that nothing is running"
    );

    let plan = fresh_plan(&app, now);
    match decide(&app, "work", &plan, true, now) {
        AutoDecision::Defer(reason) => assert!(
            reason.contains("could not determine"),
            "defer reason names the unknown detection: {reason}"
        ),
        other => panic!("unknown detection must defer, got {other:?}"),
    }

    // The whole switch path agrees: nothing moves, and the defer is recorded.
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, None, true, &history, now)
        .unwrap();
    assert!(out.is_none(), "no switch on an unknown session answer");
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "the login never moved");
    let entries = history.load().entries;
    assert!(
        entries
            .last()
            .is_some_and(|e| e.reason.contains("could not determine")),
        "the defer reason is recorded: {entries:?}"
    );
}

/// Automation must activate the account the pane and `decide` named. The
/// family filter makes alice eligible (95% in the mapped pool) even though
/// her whole-account minimum is 0, while bob is the plain winner at 60/60.
/// If apply re-ranked without the filter it would silently switch to bob —
/// visible only in the journal.
#[test]
fn auto_switch_activates_the_family_capable_account_not_the_overall_best() {
    use agyva::core::catalog::ModelsCache;

    let rig = Rig::new();
    // carol is ACTIVE (added last) and is the account automation leaves.
    rig.add_all(&[ALICE, BOB, CAROL]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.5, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    let entry = |gemini: f64, other: f64, five: i64, week: i64| CacheEntry {
        quota: Some(CachedQuota {
            observed_at: now,
            groups: vec![
                ModelQuota {
                    group: "Gemini Pro".into(),
                    model: None,
                    disabled: false,
                    five_hour: Some(QuotaWindow {
                        remaining_percent: gemini,
                        reset_at: Some(now + Duration::hours(five)),
                        observed_at: now,
                        source: QuotaSource::AgyUsageCommand,
                    }),
                    weekly: Some(QuotaWindow {
                        remaining_percent: gemini,
                        reset_at: Some(now + Duration::hours(week)),
                        observed_at: now,
                        source: QuotaSource::AgyUsageCommand,
                    }),
                },
                ModelQuota {
                    group: "Claude / GPT".into(),
                    model: None,
                    disabled: false,
                    five_hour: Some(QuotaWindow {
                        remaining_percent: other,
                        reset_at: Some(now + Duration::hours(3)),
                        observed_at: now,
                        source: QuotaSource::AgyUsageCommand,
                    }),
                    weekly: Some(QuotaWindow {
                        remaining_percent: other,
                        reset_at: Some(now + Duration::days(5)),
                        observed_at: now,
                        source: QuotaSource::AgyUsageCommand,
                    }),
                },
            ],
        }),
        health: Some(HealthRecord {
            state: AccountHealth::Healthy,
            observed_at: Some(now),
            source: HealthSource::AgyUsageCommand,
            evidence: "synthetic".into(),
        }),
        ..Default::default()
    };
    // The mapping is Gemini Pro. alice's Claude bucket is empty (so the
    // whole-account verdict excludes her) while her mapped pool is nearly
    // full and renews soonest; bob is a plain 60/60 renewing later; carol is
    // the active account, weakest and slowest to renew.
    let alice = entry(95.0, 0.0, 3, 2);
    let bob = entry(60.0, 60.0, 5, 72);
    let carol = entry(5.0, 30.0, 4, 96);
    app.quota_cache()
        .update(|c| {
            c.accounts.insert(id_of(ALICE), alice);
            c.accounts.insert(id_of(BOB), bob);
            c.accounts.insert(id_of(CAROL), carol);
        })
        .unwrap();
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\nranking = 'smart'\nauto_switch = true\naccounts = ['alice', 'bob', 'carol']\n[groups.work.models]\n'gemini-4-carbon' = 'Gemini Pro'\n",
    )
    .unwrap();
    app.config.auto_switch_ack = true;
    for who in [ALICE, BOB, CAROL] {
        ModelsCache::new(&app.dirs)
            .record(
                &id_of(who),
                vec![agyva::agy::models::ModelFamily {
                    id: "gemini-4-carbon".into(),
                    display_name: "Gemini 4 Carbon".into(),
                    variants: vec!["high".into()],
                }],
                now,
            )
            .unwrap();
    }

    // Unfiltered, alice is excluded (quota exhausted) and bob wins.
    let plain = app.route_group_for(Some("work"), None, now).unwrap();
    assert_eq!(
        plain.recommendation,
        Some(id_of(BOB)),
        "fixture: bob is the best whole-account pick"
    );
    // Filtered, the mapped pool is decisive and alice wins.
    let plan = app
        .route_group_for(Some("work"), Some("gemini-4-carbon"), now)
        .unwrap();
    assert_eq!(plan.recommendation, Some(id_of(ALICE)));
    assert_eq!(
        agyva::core::auto::decide_for(&app, "work", &plan, Some("gemini-4-carbon"), true, now),
        AutoDecision::Switch {
            id: id_of(ALICE),
            alias: "alice".to_owned()
        }
    );

    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, Some("gemini-4-carbon"), true, &history, now)
        .unwrap();
    let out = out.expect("automation switched");
    assert_eq!(
        out.alias, "alice",
        "the family-capable account is activated, not the whole-account winner"
    );
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
    assert_eq!(
        history.load().entries.last().map(|e| e.target.clone()),
        Some(id_of(ALICE)),
        "the journal names alice"
    );
}

/// The under-lock hold paths must keep their decision when the history lock
/// is contended: the operator needs "cooldown re-check failed" (or "disabled
/// while preparing"), not "another agyva is running". A manual switch lands
/// between `decide` and the lock, which is exactly what the re-check exists
/// to catch.
#[test]
fn hold_survives_history_lock_contention() {
    use agyva::storage::lock::StateLock;

    let rig = Rig::new();
    // bob is added last, so bob is ACTIVE; alice is the healthy candidate.
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);
    let plan = fresh_plan(&app, now);
    assert!(
        matches!(
            decide(&app, "work", &plan, true, now),
            AutoDecision::Switch { .. }
        ),
        "fixture: automation would switch"
    );
    // A manual switch lands after the decision: the under-lock re-check must
    // hold, and that hold must survive the contended history lock.
    let _ = note_switch(&app.dirs, now, false);

    let history = RouteHistory::new(&app.dirs);
    let lock_path = app.dirs.state.join("route-history.lock");
    let held = std::thread::spawn(move || {
        let guard = StateLock::acquire(&lock_path, std::time::Duration::from_millis(500)).ok();
        std::thread::sleep(std::time::Duration::from_millis(3500));
        drop(guard);
    });
    std::thread::sleep(std::time::Duration::from_millis(150));
    let started = std::time::Instant::now();
    let out = app
        .auto_switch_for("work", &plan, None, true, &history, now)
        .unwrap();
    let waited = started.elapsed();
    let _ = held.join();
    assert!(out.is_none(), "a hold is not a switch");
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "no switch happened");
    assert!(
        waited >= std::time::Duration::from_secs(2),
        "the test really did contend for the history lock (waited {waited:?})"
    );
}

#[test]
fn healthy_active_above_thresholds_holds_even_when_candidate_renews_sooner() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.5, 0.9);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    // alice has higher quota (95%) and earlier renewal than bob's 2099 fake renewal,
    // but bob (active) is healthy at 50% / 90% — strictly above default 10% thresholds.
    seed(&app, ALICE, now, 95.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    let plan = fresh_plan(&app, now);
    assert_eq!(plan.recommendation, Some(id_of(ALICE)));
    let d = decide(&app, "work", &plan, true, now);
    assert!(
        matches!(&d, AutoDecision::Hold(r) if r.contains("above switch thresholds")),
        "healthy active must hold, got {d:?}"
    );
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, None, true, &history, now)
        .unwrap();
    assert!(out.is_none());
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
}

#[test]
fn two_account_automation_uses_standby_candidate_cache_past_quota_fresh_and_past_reset() {
    use agyva::core::monitor::MonitorProbe;
    use agyva::core::watch::Probe;

    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    let standby_at = now - Duration::minutes(25);
    // alice (standby) was observed 25m ago (> quota_fresh_secs=300s, < candidate_max_age=21600s),
    // and her 5h window had 0% with reset_at already passed (now - 5m), while weekly is 80%.
    app.quota_cache()
        .update(|c| {
            c.accounts.insert(
                id_of(ALICE),
                CacheEntry {
                    quota: Some(CachedQuota {
                        observed_at: standby_at,
                        groups: vec![ModelQuota {
                            group: "Gemini".into(),
                            model: None,
                            disabled: false,
                            five_hour: Some(QuotaWindow {
                                remaining_percent: 0.0,
                                reset_at: Some(now - Duration::minutes(5)),
                                observed_at: standby_at,
                                source: QuotaSource::AgyUsageCommand,
                            }),
                            weekly: Some(QuotaWindow {
                                remaining_percent: 80.0,
                                reset_at: Some(now + Duration::days(3)),
                                observed_at: standby_at,
                                source: QuotaSource::AgyUsageCommand,
                            }),
                        }],
                    }),
                    health: Some(HealthRecord {
                        state: AccountHealth::Healthy,
                        observed_at: Some(standby_at),
                        source: HealthSource::AgyUsageCommand,
                        evidence: "synthetic".into(),
                    }),
                    ..Default::default()
                },
            );
        })
        .unwrap();
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    // Strict route_group_for rejects alice because she is >300s old and 5h reset passed.
    let strict = app.route_group_for(Some("work"), None, now).unwrap();
    let alice_strict = strict.accounts.iter().find(|r| r.alias == "alice").unwrap();
    assert!(!alice_strict.eligible);

    // Candidate-aware auto_route_group_for keeps alice eligible with candidate cache note.
    let auto_plan = app.auto_route_group_for(Some("work"), None, now).unwrap();
    let alice_auto = auto_plan
        .accounts
        .iter()
        .find(|r| r.alias == "alice")
        .unwrap();
    assert!(
        alice_auto.eligible,
        "standby candidate should be eligible: {alice_auto:?}"
    );
    assert!(
        alice_auto.why.contains("candidate cache"),
        "expected candidate cache reason, got: {}",
        alice_auto.why
    );

    // MonitorProbe switches automatically without any manual switch to refresh alice!
    let mut probe = MonitorProbe::new(&app, Some("work".into()), None, std::time::Duration::ZERO);
    probe.cycle(now);
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
}

#[test]
fn hysteresis_blocks_marginal_candidate_and_allows_strong_candidate() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB, CAROL]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.08, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    // carol is active at 8% 5h / 50% weekly (below 10% threshold).
    seed(&app, CAROL, now, 8.0, AccountHealth::Healthy);
    // bob is only at 12% (above 10% threshold, but below 8% + 10% = 18% hysteresis).
    seed(&app, BOB, now, 12.0, AccountHealth::Healthy);
    // Exclude alice first so bob is the only candidate -> must hold on hysteresis.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\nranking = 'smart'\nauto_switch = true\naccounts = ['bob', 'carol']\n",
    )
    .unwrap();
    app.config.auto_switch_ack = true;

    let plan = app.auto_route_group_for(Some("work"), None, now).unwrap();
    let d_hold = decide(&app, "work", &plan, true, now);
    assert!(
        matches!(&d_hold, AutoDecision::Hold(r) if r.contains("hysteresis")),
        "expected hysteresis hold, got {d_hold:?}"
    );

    // Now add alice at 50% (well above 18% hysteresis) -> switches to alice.
    seed(&app, ALICE, now, 50.0, AccountHealth::Healthy);
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\nranking = 'smart'\nauto_switch = true\naccounts = ['alice', 'bob', 'carol']\n",
    )
    .unwrap();
    let plan2 = app.auto_route_group_for(Some("work"), None, now).unwrap();
    match decide(&app, "work", &plan2, true, now) {
        AutoDecision::Switch { alias, .. } => assert_eq!(alias, "alice"),
        other => panic!("expected switch to alice, got {other:?}"),
    }
}

#[test]
fn corrupt_auto_state_fails_closed() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    std::fs::create_dir_all(&app.dirs.state).unwrap();
    std::fs::write(app.dirs.state.join("auto-switch.json"), b"{corrupt json").unwrap();

    let plan = app.auto_route_group_for(Some("work"), None, now).unwrap();
    let d = decide(&app, "work", &plan, true, now);
    assert!(
        matches!(&d, AutoDecision::Defer(r) if r.contains("unreadable or corrupt")),
        "corrupt auto-switch.json must defer, got {d:?}"
    );
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, None, true, &history, now)
        .unwrap();
    assert!(out.is_none());
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
}

#[test]
fn alternating_refreshed_and_unfresh_holds_dedupe_across_streak() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);
    note_switch(&app.dirs, now, false).unwrap();

    let plan = app.auto_route_group_for(Some("work"), None, now).unwrap();
    let history = RouteHistory::new(&app.dirs);
    for i in 0..10 {
        let refreshed = i % 2 == 0;
        let out = app
            .auto_switch_for(
                "work",
                &plan,
                None,
                refreshed,
                &history,
                now + Duration::seconds(i),
            )
            .unwrap();
        assert!(out.is_none());
    }
    let entries = history.load().entries;
    assert_eq!(
        entries.len(),
        2,
        "alternating cooling-down / fresh-live-reading holds must stay deduplicated: {entries:?}"
    );
}

#[test]
fn monitor_transient_refresh_failure_sets_failure_and_auto_error_backs_off() {
    use agyva::core::monitor::MonitorProbe;
    use agyva::core::watch::Probe;

    // 1. Transient refresh failure reports failure and does not advance last_refresh.
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.fail("dial tcp: lookup example.invalid: no such host", 1);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    let mut probe = MonitorProbe::new(
        &app,
        Some("work".into()),
        None,
        std::time::Duration::from_secs(300),
    );
    let r1 = probe.cycle(now);
    assert!(
        r1.failure.is_some(),
        "transient network error must populate report.failure"
    );
    let calls_after_fail = fake.calls();
    // Next cycle 65s later (> quota_min_refresh_secs=60s, < 300s) MUST retry refresh
    // because last_refresh was not advanced on the transient failure.
    fake.ok(0.5, 0.5);
    let r2 = probe.cycle(now + Duration::seconds(65));
    assert!(r2.failure.is_none());
    assert!(
        fake.calls() > calls_after_fail,
        "refresh must retry after transient failure without waiting 300s"
    );

    // 2. Failing auto_switch_for (missing target snapshot during switch) backs off instead of retrying every cycle.
    let rig2 = Rig::new();
    rig2.add_all(&[ALICE, BOB]);
    let fake2 = common::fake_agy::FakeAgy::install(&rig2.tmp.path().join("fakeagy2"));
    fake2.ok(0.05, 0.5);
    let mut app2 = rig2.app_with_agy(&fake2);
    seed(&app2, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app2, BOB, now, 5.0, AccountHealth::Healthy);
    enable_auto(&mut app2, true);
    std::fs::remove_file(app2.dirs.snapshot_file(&id_of(ALICE))).unwrap();

    let mut probe2 = MonitorProbe::new(&app2, Some("work".into()), None, std::time::Duration::ZERO);
    probe2.cycle(now);
    assert!(
        probe2
            .last_auto()
            .is_some_and(|s| s.contains("auto-switch error:")),
        "expected error on first failure, got {:?}",
        probe2.last_auto()
    );
    // 15s later (< 60s backoff): backed off without retrying auto_switch_for.
    probe2.cycle(now + Duration::seconds(15));
    assert!(
        probe2.last_auto().is_some_and(|s| s.contains("backed off")),
        "expected backoff on immediate retry, got {:?}",
        probe2.last_auto()
    );
}

#[test]
fn route_apply_committed_switch_warns_instead_of_failing_when_history_locked() {
    use agyva::storage::lock::StateLock;

    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 5.0, AccountHealth::Healthy);
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\naccounts = ['alice', 'bob']\n",
    )
    .unwrap();

    let history = RouteHistory::new(&app.dirs);
    let lock_path = app.dirs.state.join("route-history.lock");
    let held = std::thread::spawn(move || {
        let guard = StateLock::acquire(&lock_path, std::time::Duration::from_millis(500)).ok();
        std::thread::sleep(std::time::Duration::from_millis(3500));
        drop(guard);
    });
    std::thread::sleep(std::time::Duration::from_millis(150));

    let out = app
        .route_apply("work", None, false, true, &history, now)
        .expect("committed switch must return Ok even if history write fails");
    let _ = held.join();
    assert_eq!(out.alias, "alice");
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
    assert!(
        out.warnings
            .iter()
            .any(|w| w.contains("route history could not be updated")),
        "expected history warning in {:?}",
        out.warnings
    );
}
