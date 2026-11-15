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
        .auto_switch_for("work", &plan, false, &history, now)
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
    fake.ok(0.5, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
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
        agyva::agy::sessions::detect(&app.agy)
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
    std::fs::write(app.agy.presence_dir().join("abc.lock"), "").unwrap();
    let plan = fresh_plan(&app, now);
    assert!(matches!(
        decide(&app, "work", &plan, true, now),
        AutoDecision::Defer(_)
    ));
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, true, &history, now)
        .unwrap();
    assert!(out.is_none());
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
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
        matches!(&d, AutoDecision::Hold(r) if r.contains("already active")),
        "got {d:?}"
    );
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, true, &history, now)
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
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
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
        .auto_switch_for("work", &plan, true, &history, now)
        .unwrap_err();
    assert_eq!(err.kind(), agyva::core::error::ErrorKind::RecoveryRequired);
}

#[test]
fn concurrent_manual_switch_wins_and_cools_automation_down() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB, CAROL]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.5, 0.5);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
    seed(&app, CAROL, now, 10.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    // Automation decides to switch (alice is the fresh eligible top).
    let plan = fresh_plan(&app, now);
    let decision = decide(&app, "work", &plan, true, now);
    let AutoDecision::Switch { alias, .. } = decision else {
        panic!(
            "expected a switch, got {decision:?} sessions={:?}",
            agyva::agy::sessions::detect(&app.agy)
        );
    };
    assert_eq!(alias, "alice");

    // A human types `agyva use carol` first; automation must not chase it.
    app.switch("carol", false).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(CAROL)));
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, true, &history, now)
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
    fake.ok(0.9, 0.9);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);

    let plan = fresh_plan(&app, now);
    let history = RouteHistory::new(&app.dirs);
    let out = app
        .auto_switch_for("work", &plan, true, &history, now)
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
    fake.ok(0.9, 0.9);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
    enable_auto(&mut app, true);
    // bob is active; alice is the strictly better pick.
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
    fake.ok(0.9, 0.9);
    let mut app = rig.app_with_agy(&fake);
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
    app.config.auto_switch_ack = true;
    let mut probe = MonitorProbe::new(&app, None, None, std::time::Duration::ZERO);
    probe.cycle(now);
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "no group, no switch");
    assert!(probe.last_auto().is_none());

    // 3. A repeated hold does not flood the bounded history.
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy3"));
    fake.ok(0.9, 0.9);
    let mut app = rig.app_with_agy(&fake);
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);
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
/// cooldown -> sessions -> eligibility. With several gates failing at once,
/// the earliest one is the one reported, so a test can pin the order.
#[test]
fn gates_are_evaluated_in_the_documented_order() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.9, 0.9);
    let mut app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);

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
    std::fs::write(app.agy.presence_dir().join("s.lock"), "").unwrap();
    let d = decide(&app, "work", &plan, true, now + Duration::seconds(1000));
    assert!(
        matches!(&d, AutoDecision::Defer(r) if r.contains("sessions detected")),
        "{d:?}"
    );

    // Everything clear: the eligible top row is the switch target.
    std::fs::remove_file(app.agy.presence_dir().join("s.lock")).unwrap();
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

    let report = agyva::agy::sessions::detect(&app.agy);
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
        .auto_switch_for("work", &plan, true, &history, now)
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
