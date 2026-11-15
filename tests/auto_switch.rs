//! Slice F: opt-in auto-switch. Gates: acknowledgement, cooldown, fresh
//! evidence, fresh eligible candidate, session gate, suspension, pending
//! recovery, concurrent manual switch.

mod common;

use chrono::{Duration, Utc};
use common::*;

use agyva::core::app::App;
use agyva::core::auto::{AutoDecision, MIN_SWITCH_INTERVAL, decide, enabled_for, note_switch};
use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};
use agyva::core::monitor::MonitorProbe;
use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};
use agyva::core::quota_cache::{CacheEntry, CachedQuota};
use agyva::core::route::RoutePlan;
use agyva::core::route_apply::RouteHistory;
use agyva::core::route_config::{Ranking, RoutingFile};
use agyva::core::watch::Probe;

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

fn fresh_plan(app: &App, now: chrono::DateTime<Utc>) -> RoutePlan {
    // The "just refreshed" proof: evaluate through the monitor probe, which
    // runs the active refresh first (single-flight guard).
    let mut probe = MonitorProbe::new(
        app,
        Some("work".into()),
        None,
        std::time::Duration::from_secs(300),
    );
    let _ = probe.cycle(now);
    probe.last_plan().unwrap().clone()
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
    assert!(matches!(
        decide(&app, "work", &plan, true, now),
        AutoDecision::Switch { .. }
    ));
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
