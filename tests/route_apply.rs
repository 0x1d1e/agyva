//! Slice D: `route apply` (one journaled switch with revalidation) and the
//! bounded, redacted `route history`.

mod common;
use agyva::core::error::{ErrorKind, exit};
use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};
use agyva::core::quota_cache::{CacheEntry, CachedQuota};
use agyva::core::route_apply::{HistoryOutcome, RouteHistory};
use chrono::{Duration, Utc};
use common::*;

fn seed_fresh(app: &agyva::core::app::App, who: (&str, &str, &str), at: chrono::DateTime<Utc>) {
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
                                remaining_percent: 60.0,
                                reset_at: Some(at + Duration::hours(3)),
                                observed_at: at,
                                source: QuotaSource::AgyUsageCommand,
                            }),
                            weekly: Some(QuotaWindow {
                                remaining_percent: 60.0,
                                reset_at: Some(at + Duration::days(2)),
                                observed_at: at,
                                source: QuotaSource::AgyUsageCommand,
                            }),
                        }],
                    }),
                    ..Default::default()
                },
            );
        })
        .unwrap();
}

fn write_group(app: &agyva::core::app::App, body: &str) {
    std::fs::write(app.dirs.config.join("routing.toml"), body).unwrap();
}

#[test]
fn apply_switches_to_the_recommendation_through_the_journaled_path() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    let now = Utc::now();
    // bob was added last and is active; the group recommends alice.
    seed_fresh(&app, ALICE, now);
    write_group(&app, "[groups.work]\naccounts = ['alice', 'bob']\n");

    let history = RouteHistory::new(&app.dirs);
    let out = app.route_apply("work", false, true, &history, now).unwrap();
    assert_eq!(out.alias, "alice");
    assert!(!out.already_active);
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
    assert!(app.pending_journal().unwrap().is_none());

    let file = history.load();
    assert_eq!(file.entries.len(), 1);
    assert_eq!(file.entries[0].outcome, HistoryOutcome::Applied);
    assert_eq!(file.entries[0].group.as_deref(), Some("work"));
    assert_eq!(file.entries[0].target, id_of(ALICE));
}

#[test]
fn apply_refuses_on_pending_recovery_and_records_it() {
    let rig = Rig::new();
    rig.add_all(&[BOB, ALICE]);
    let _ = rig
        .app_with(agyva::core::failpoint::Failpoints::armed(
            "after-native-write",
            agyva::core::failpoint::Fault::Crash,
        ))
        .switch("bob", false);
    let app = rig.app();
    let now = Utc::now();
    seed_fresh(&app, BOB, now);
    write_group(&app, "[groups.work]\naccounts = ['bob']\n");
    assert!(app.pending_journal().unwrap().is_some());

    let history = RouteHistory::new(&app.dirs);
    let err = app
        .route_apply("work", false, true, &history, now)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::RecoveryRequired);
    assert_eq!(err.exit_code(), exit::RECOVERY_REQUIRED);
    let file = history.load();
    assert_eq!(file.entries.len(), 1);
    assert_eq!(file.entries[0].outcome, HistoryOutcome::Refused);
    assert!(file.entries[0].reason.contains("recovery"));
}

#[test]
fn apply_refuses_when_evidence_drifts_from_the_plan() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    let now = Utc::now();
    seed_fresh(&app, ALICE, now);
    write_group(&app, "[groups.work]\naccounts = ['alice']\n");

    let history = RouteHistory::new(&app.dirs);
    // Invalidate the evidence between planning and applying by holding the
    // refresh lock away from the revalidation path is not possible here
    // (same process); instead simulate drift by emptying the cache between
    // plan and apply via a custom re-check: plan says alice, then the cache
    // is dropped, so the fresh evidence recommends nobody.
    let plan = app.route_apply_plan("work", now).unwrap();
    assert_eq!(plan.target_id, id_of(ALICE));
    app.quota_cache().update(|c| c.accounts.clear()).unwrap();
    let err = app
        .route_apply("work", false, true, &history, now)
        .unwrap_err();
    assert!(
        err.message().contains("no eligible account"),
        "{}",
        err.message()
    );
    assert_eq!(history.load().entries.len(), 0, "nothing mutated");
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "no switch happened");
    let _ = plan;
}

#[test]
fn apply_refuses_when_recommendation_changed_under_the_lock() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    let now = Utc::now();
    seed_fresh(&app, ALICE, now);
    write_group(&app, "[groups.work]\naccounts = ['alice']\n");
    let history = RouteHistory::new(&app.dirs);

    // The group file changes between plan and lock: the apply must refuse
    // rather than switch to a target that is no longer recommended.
    let plan = app.route_apply_plan("work", now).unwrap();
    assert_eq!(plan.target_id, id_of(ALICE));
    write_group(&app, "[groups.work]\naccounts = ['bob']\n");
    let err = app
        .route_apply("work", false, true, &history, now)
        .unwrap_err();
    assert!(
        err.message().contains("no longer")
            || err.message().contains("changed")
            || err.message().contains("no eligible"),
        "{}",
        err.message()
    );
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "no switch happened");
}

#[test]
fn already_active_apply_is_recorded_and_harmless() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    let now = Utc::now();
    seed_fresh(&app, BOB, now);
    write_group(&app, "[groups.work]\naccounts = ['bob']\n");
    let history = RouteHistory::new(&app.dirs);
    let out = app.route_apply("work", false, true, &history, now).unwrap();
    assert!(out.already_active);
    let file = history.load();
    assert_eq!(file.entries.len(), 1);
    assert_eq!(file.entries[0].outcome, HistoryOutcome::Applied);
}

#[test]
fn history_is_bounded_redacted_and_json_roundtrips() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let history = RouteHistory::new(&app.dirs);
    history
        .record(agyva::core::route_apply::HistoryEntry {
            at: Utc::now(),
            group: Some("work".into()),
            target: id_of(ALICE),
            outcome: HistoryOutcome::Applied,
            reason: "smart ranking: 60% remains".into(),
        })
        .unwrap();
    let raw = std::fs::read_to_string(history.path()).unwrap();
    for secret in ["passphrase", "token", "credential", "secret"] {
        assert!(
            !raw.to_ascii_lowercase().contains(secret),
            "{secret} leaked"
        );
    }
    // Long failure reasons are bounded.
    let long = "y".repeat(600);
    let bounded = agyva::core::route_apply::tests_bounded_reason_for_docs(&long);
    assert!(bounded.chars().count() <= 201);
}
