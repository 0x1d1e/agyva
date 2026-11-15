//! Slice C: the model catalog (`agy models`), its per-account cache, and
//! `--for <model>` eligibility in route planning.

mod common;
use agyva::core::catalog::{CatalogState, ModelsCache};
use chrono::{Duration, Utc};
use common::*;

const MODELS_TSV: &str = "\
Fetching available models...\n\
gemini-4-carbon-high\tGemini 4 Carbon (High)\n\
gemini-4-carbon-low\tGemini 4 Carbon (Low)\n\
gpt-oss-120b\tGPT-OSS 120B\n";

fn seed_fresh(app: &agyva::core::app::App, who: (&str, &str, &str), at: chrono::DateTime<Utc>) {
    use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};
    use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};
    app.quota_cache()
        .update(|c| {
            c.accounts.insert(
                id_of(who),
                agyva::core::quota_cache::CacheEntry {
                    quota: Some(agyva::core::quota_cache::CachedQuota {
                        observed_at: at,
                        groups: vec![
                            ModelQuota {
                                group: "Gemini Pro".into(),
                                model: None,
                                disabled: false,
                                five_hour: Some(QuotaWindow {
                                    remaining_percent: 40.0,
                                    reset_at: Some(at + Duration::hours(3)),
                                    observed_at: at,
                                    source: QuotaSource::AgyUsageCommand,
                                }),
                                weekly: Some(QuotaWindow {
                                    remaining_percent: 70.0,
                                    reset_at: Some(at + Duration::days(2)),
                                    observed_at: at,
                                    source: QuotaSource::AgyUsageCommand,
                                }),
                            },
                            ModelQuota {
                                group: "Claude / GPT".into(),
                                model: None,
                                disabled: false,
                                five_hour: Some(QuotaWindow {
                                    remaining_percent: 5.0,
                                    reset_at: Some(at + Duration::hours(3)),
                                    observed_at: at,
                                    source: QuotaSource::AgyUsageCommand,
                                }),
                                weekly: Some(QuotaWindow {
                                    remaining_percent: 5.0,
                                    reset_at: Some(at + Duration::days(2)),
                                    observed_at: at,
                                    source: QuotaSource::AgyUsageCommand,
                                }),
                            },
                        ],
                    }),
                    health: Some(HealthRecord {
                        state: AccountHealth::Healthy,
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

#[test]
fn models_run_parses_and_caches_for_active_only() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    // bob was added last: bob is active and live.
    rig.login(BOB);
    fake.models(MODELS_TSV);
    let out = app.refresh_models(false).unwrap();
    assert_eq!(
        out.status,
        agyva::core::catalog::ModelsRefreshStatus::Refreshed
    );
    assert_eq!(fake.model_calls(), 1);
    assert_eq!(out.account_id.as_deref(), Some(id_of(BOB).as_str()));

    let state = app
        .models_cache()
        .read_state(&id_of(BOB), 86_400, Utc::now());
    assert!(matches!(state, CatalogState::Fresh(_)));
    let families = state.families();
    assert!(families.iter().any(|f| f.id == "gemini-4-carbon"));
    assert_eq!(
        families
            .iter()
            .find(|f| f.id == "gemini-4-carbon")
            .unwrap()
            .variants,
        ["low", "high"],
        "efforts parsed in EFFORTS order"
    );
    assert!(families.iter().any(|f| f.id == "gpt-oss-120b"));

    // alice (not active) has no observation.
    assert_eq!(
        app.models_cache()
            .read_state(&id_of(ALICE), 86_400, Utc::now()),
        CatalogState::Missing
    );

    // Throttled: within models_cache_secs no second spawn.
    let again = app.refresh_models(false).unwrap();
    assert_eq!(
        again.status,
        agyva::core::catalog::ModelsRefreshStatus::Skipped
    );
    assert_eq!(fake.model_calls(), 1);
}

#[test]
fn models_failure_keeps_prior_observation() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    rig.login(ALICE);
    fake.models(MODELS_TSV);
    assert!(
        app.refresh_models(false).unwrap().status
            == agyva::core::catalog::ModelsRefreshStatus::Refreshed
    );

    // The next run fails; the earlier observation survives untouched.
    fake.models_fail("boom", 3);
    let at_fail = Utc::now();
    let out = app.refresh_models(true).unwrap();
    assert_eq!(
        out.status,
        agyva::core::catalog::ModelsRefreshStatus::Failed
    );
    let state = app
        .models_cache()
        .read_state(&id_of(ALICE), 86_400, at_fail);
    match state {
        CatalogState::Fresh(c) => {
            assert!(!c.families.is_empty(), "prior observation retained");
        }
        other => panic!("expected fresh prior observation, got {other:?}"),
    }
}

#[test]
fn models_skips_when_live_login_differs() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    // bob is the registry's active account, but agy is logged in as alice.
    rig.login(ALICE);
    fake.models(MODELS_TSV);
    let out = app.refresh_models(false).unwrap();
    assert_eq!(
        out.status,
        agyva::core::catalog::ModelsRefreshStatus::Skipped
    );
    assert!(out.note.unwrap().contains("differs"));
    assert_eq!(
        fake.model_calls(),
        0,
        "no spawn when attribution is impossible"
    );
}

#[test]
fn for_model_eligibility_and_specific_pool_share() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    let now = Utc::now();
    seed_fresh(&app, ALICE, now);
    seed_fresh(&app, BOB, now);
    // The user declares which quota group carries the family: "Gemini Pro"
    // does not literally contain "gemini-4-carbon", so only the declared
    // mapping connects them.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups._x.models]\n'gemini-4-carbon' = 'Gemini Pro'\n",
    )
    .unwrap();
    let cache = ModelsCache::new(&app.dirs);
    cache
        .record(
            &id_of(ALICE),
            vec![agyva::agy::models::ModelFamily {
                id: "gemini-4-carbon".into(),
                display_name: "Gemini 4 Carbon".into(),
                variants: vec!["low".into(), "high".into()],
            }],
            now,
        )
        .unwrap();

    // Without a catalog observation, --for never proves support.
    let plan = app
        .route_group_for(None, Some("gemini-4-carbon"), now)
        .unwrap();
    let bob = plan.accounts.iter().find(|r| r.alias == "bob").unwrap();
    assert!(!bob.eligible);
    assert!(bob.why.contains("unknown"), "{}", bob.why);

    // With a fresh observation of the family, alice is eligible; the
    // headline share follows the mapped group ("gemini-4-carbon" is in
    // "Gemini Pro" by name match), not the lowest window of the account.
    let plan = app
        .route_group_for(None, Some("gemini-4-carbon"), now)
        .unwrap();
    let alice = plan.accounts.iter().find(|r| r.alias == "alice").unwrap();
    assert!(alice.eligible, "{}", alice.why);
    assert_eq!(plan.recommendation.as_deref(), Some(id_of(ALICE).as_str()));
    assert_eq!(alice.remaining_percent, Some(40.0), "Gemini Pro 5h window");
    assert_ne!(
        alice.remaining_percent,
        Some(5.0),
        "an exhausted unrelated bucket must not mask the requested family"
    );

    // A family the catalog does not list excludes with a reason.
    let plan = app
        .route_group_for(None, Some("gemini-4-argon"), now)
        .unwrap();
    let alice = plan.accounts.iter().find(|r| r.alias == "alice").unwrap();
    assert!(!alice.eligible);
    assert!(alice.why.contains("does not offer"), "{}", alice.why);
    assert_eq!(plan.recommendation, None);
}

#[test]
fn stale_catalog_offering_excludes_until_refreshed() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let now = Utc::now();
    seed_fresh(&app, ALICE, now);
    let cache = ModelsCache::new(&app.dirs);
    cache
        .record(
            &id_of(ALICE),
            vec![agyva::agy::models::ModelFamily {
                id: "gemini-4-carbon".into(),
                display_name: "Gemini 4 Carbon".into(),
                variants: vec!["high".into()],
            }],
            now - Duration::hours(48),
        )
        .unwrap();
    let plan = app
        .route_group_for(None, Some("gemini-4-carbon"), now)
        .unwrap();
    let alice = plan.accounts.first().unwrap();
    assert!(!alice.eligible, "stale observation never proves support");
    assert!(alice.why.contains("stale"), "{}", alice.why);
}

#[test]
fn declared_mapping_precedes_name_matching() {
    use agyva::core::catalog::{FamilyGroups, GroupView, map_family_to_groups};
    let groups = [
        GroupView {
            group: "Broad Pool",
            model: None,
        },
        GroupView {
            group: "Narrow Pool",
            model: Some("gemini-4-carbon-high"),
        },
    ];
    // Specific pool (model set) wins over broad without any declaration.
    assert_eq!(
        map_family_to_groups("gemini-4-carbon", None, groups),
        FamilyGroups::Mapped(("Narrow Pool".into(), Some("gemini-4-carbon-high".into())))
    );
    // A declared mapping is authoritative even for a broad pool.
    assert_eq!(
        map_family_to_groups("gemini-4-carbon", Some("Broad Pool"), groups,),
        FamilyGroups::Mapped(("Broad Pool".into(), None))
    );
    // Unknown family: not eligible, never guessed.
    assert_eq!(
        map_family_to_groups("gemini-9-unobtainium", None, groups),
        FamilyGroups::Unknown
    );
}

#[test]
fn static_reference_catalog_never_proves_support() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let now = Utc::now();
    seed_fresh(&app, ALICE, now);
    // REFERENCE_FAMILIES lists gemini-4-carbon, but the account has NO
    // observed catalog: planning must not treat the static list as proof.
    let known_static = agyva::core::catalog::REFERENCE_FAMILIES
        .iter()
        .find(|f| **f == "gemini-4-carbon")
        .expect("static reference includes the family");
    let plan = app.route_group_for(None, Some(known_static), now).unwrap();
    let alice = plan.accounts.first().unwrap();
    assert!(!alice.eligible, "static catalog is descriptive only");
    assert!(alice.why.contains("unknown"), "{}", alice.why);
}

/// The review's counterexample: an exhausted bucket in an UNRELATED group
/// must not exclude an account whose requested family still has quota. Here
/// whole-account ranking says "quota exhausted" (Claude / GPT is at 0%), but
/// Gemini Pro weekly still holds 60%.
#[test]
fn for_model_ignores_exhausted_unrelated_bucket() {
    use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};
    use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};

    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let now = Utc::now();
    let window = |pct: f64, reset_in: Duration| QuotaWindow {
        remaining_percent: pct,
        reset_at: Some(now + reset_in),
        observed_at: now,
        source: QuotaSource::AgyUsageCommand,
    };
    app.quota_cache()
        .update(|c| {
            c.accounts.insert(
                id_of(ALICE),
                agyva::core::quota_cache::CacheEntry {
                    quota: Some(agyva::core::quota_cache::CachedQuota {
                        observed_at: now,
                        groups: vec![
                            ModelQuota {
                                group: "Gemini Pro".into(),
                                model: None,
                                disabled: false,
                                five_hour: Some(window(90.0, Duration::hours(3))),
                                weekly: Some(window(60.0, Duration::days(2))),
                            },
                            ModelQuota {
                                group: "Claude / GPT".into(),
                                model: None,
                                disabled: false,
                                five_hour: Some(window(0.0, Duration::hours(3))),
                                weekly: Some(window(0.0, Duration::days(2))),
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
                },
            );
        })
        .unwrap();
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups._x.models]\n'gemini-4-carbon' = 'Gemini Pro'\n",
    )
    .unwrap();
    ModelsCache::new(&app.dirs)
        .record(
            &id_of(ALICE),
            vec![agyva::agy::models::ModelFamily {
                id: "gemini-4-carbon".into(),
                display_name: "Gemini 4 Carbon".into(),
                variants: vec!["high".into()],
            }],
            now,
        )
        .unwrap();

    // The plain plan excludes the account: some bucket is empty.
    let plain = app.route_group_at(None, now).unwrap();
    let row = plain.accounts.first().unwrap();
    assert!(!row.eligible, "whole-account plan sees the empty bucket");
    assert_eq!(row.why, "quota exhausted");

    // Asking for the family re-scopes eligibility to its own group.
    let plan = app
        .route_group_for(None, Some("gemini-4-carbon"), now)
        .unwrap();
    let alice = plan.accounts.first().unwrap();
    assert!(
        alice.eligible,
        "Gemini Pro still holds quota; why was {:?}",
        alice.why
    );
    assert_eq!(
        alice.remaining_percent,
        Some(60.0),
        "the mapped group's tightest window, not the account-wide 0%"
    );
    assert_eq!(plan.recommendation.as_deref(), Some(id_of(ALICE).as_str()));

    // ...and when the requested group itself empties, the exclusion names it.
    app.quota_cache()
        .update(|c| {
            if let Some(entry) = c.accounts.get_mut(&id_of(ALICE))
                && let Some(q) = entry.quota.as_mut()
            {
                for g in &mut q.groups {
                    if g.group == "Gemini Pro" {
                        for w in [&mut g.five_hour, &mut g.weekly].into_iter().flatten() {
                            w.remaining_percent = 0.0;
                        }
                    }
                }
            }
        })
        .unwrap();
    let plan = app
        .route_group_for(None, Some("gemini-4-carbon"), now)
        .unwrap();
    let alice = plan.accounts.first().unwrap();
    assert!(!alice.eligible);
    assert!(
        alice.why.contains("mapped quota exhausted"),
        "{}",
        alice.why
    );
}

/// `agyva models` is the documented way to make `--for-model` work on a real
/// install: a missing catalog names it as the fix, and `--refresh` records one.
#[test]
fn models_command_makes_for_model_reachable() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    rig.login(ALICE);
    let now = Utc::now();
    seed_fresh(&app, ALICE, now);
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups._x.models]\n'gemini-4-carbon' = 'Gemini Pro'\n",
    )
    .unwrap();

    // With no catalog, the exclusion reason names the command that fixes it.
    let plan = app
        .route_group_for(None, Some("gemini-4-carbon"), now)
        .unwrap();
    let alice = plan.accounts.first().unwrap();
    assert!(!alice.eligible);
    assert!(
        alice.why.contains("agyva models"),
        "the reason must name the fix: {}",
        alice.why
    );

    // The report is cache-only until --refresh is asked for.
    let quiet = app.models_report(false, now).unwrap();
    assert!(quiet.refresh.is_none(), "no spawn without --refresh");
    assert_eq!(quiet.accounts.len(), 1);
    assert_eq!(quiet.accounts[0].state, "missing");
    assert!(quiet.accounts[0].families.is_empty());
    assert!(quiet.accounts[0].active);
    assert_eq!(fake.model_calls(), 0, "reading the cache never spawns agy");

    // `--refresh` runs one bounded `agy models` for the active account.
    fake.models(MODELS_TSV);
    let report = app.models_report(true, now).unwrap();
    let refresh = report.refresh.expect("refresh outcome reported");
    assert_eq!(
        refresh.status,
        agyva::core::catalog::ModelsRefreshStatus::Refreshed
    );
    assert_eq!(fake.model_calls(), 1);
    assert_eq!(report.accounts[0].state, "fresh");
    assert_eq!(report.accounts[0].age_secs, Some(0));
    assert!(
        report.accounts[0]
            .families
            .iter()
            .any(|f| f.id == "gemini-4-carbon"),
        "{:?}",
        report.accounts[0].families
    );

    // Now the same query is answerable.
    let plan = app
        .route_group_for(None, Some("gemini-4-carbon"), now)
        .unwrap();
    let alice = plan.accounts.first().unwrap();
    assert!(alice.eligible, "{}", alice.why);
    assert_eq!(alice.remaining_percent, Some(40.0));
    // The monitor's tick uses the THROTTLED path (`force = false`): a fresh
    // catalog means no spawn at all, so a 15s loop cannot hammer agy.
    let calls = fake.model_calls();
    assert_eq!(
        app.refresh_models_at(false, now).unwrap().status,
        agyva::core::catalog::ModelsRefreshStatus::Skipped
    );
    assert_eq!(
        fake.model_calls(),
        calls,
        "no spawn when the catalog is fresh"
    );

    // `--refresh` is an explicit request, so it always runs.
    assert_eq!(
        app.models_report(true, now)
            .unwrap()
            .refresh
            .unwrap()
            .status,
        agyva::core::catalog::ModelsRefreshStatus::Refreshed
    );
    assert_eq!(fake.model_calls(), calls + 1);
}
