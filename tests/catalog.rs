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

/// A raw effort id is the same family as its base: `--for-model
/// gemini-4-carbon-high` must inherit the declared family mapping instead of
/// falling back to the whole-account verdict (which would reintroduce the
/// exhausted-unrelated-bucket bug for a documented spelling).
#[test]
fn for_model_variant_id_inherits_the_family_mapping() {
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
                                five_hour: Some(window(60.0, Duration::hours(3))),
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
        "[groups.work]\naccounts = ['alice']\n[groups.work.models]\n'gemini-4-carbon' = 'Gemini Pro'\n",
    )
    .unwrap();
    ModelsCache::new(&app.dirs)
        .record(
            &id_of(ALICE),
            vec![agyva::agy::models::ModelFamily {
                id: "gemini-4-carbon".into(),
                display_name: "Gemini 4 Carbon".into(),
                variants: vec!["high".into(), "low".into()],
            }],
            now,
        )
        .unwrap();

    for asked in [
        "gemini-4-carbon",
        "gemini-4-carbon-high",
        "GEMINI-4-CARBON-LOW",
    ] {
        let plan = app.route_group_for(Some("work"), Some(asked), now).unwrap();
        let alice = plan.accounts.first().unwrap();
        assert!(
            alice.eligible,
            "`--for-model {asked}` must inherit the family mapping, got {:?}",
            alice.why
        );
        assert_eq!(
            alice.remaining_percent,
            Some(60.0),
            "`--for-model {asked}` uses the mapped group, not the exhausted one"
        );
        assert_eq!(plan.recommendation.as_deref(), Some(id_of(ALICE).as_str()));
        assert!(
            alice.why.starts_with(&format!("{asked}:")),
            "the user's own string stays in the reason: {:?}",
            alice.why
        );
    }
}

/// The other direction: an id that merely ENDS in an effort word is a
/// standalone family, not a variant of a base that happens to exist. It must
/// keep its own identity — no silent re-basing onto another family's mapping.
#[test]
fn standalone_effort_suffixed_id_is_not_rebased() {
    use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};
    use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};

    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let now = Utc::now();
    let window = |pct: f64| QuotaWindow {
        remaining_percent: pct,
        reset_at: Some(now + Duration::hours(3)),
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
                                five_hour: Some(window(60.0)),
                                weekly: Some(window(60.0)),
                            },
                            ModelQuota {
                                group: "Claude / GPT".into(),
                                model: None,
                                disabled: false,
                                five_hour: Some(window(0.0)),
                                weekly: Some(window(0.0)),
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
    // The catalog lists gemini-4-carbon as a REAL family AND a standalone
    // "solo-high" model. "solo-high" is not a variant of "solo"; the base
    // does not exist, so its identity is itself.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\naccounts = ['alice']\n[groups.work.models]\n'gemini-4-carbon' = 'Gemini Pro'\n'solo' = 'Gemini Pro'\n'solo-high' = 'Claude / GPT'\n",
    )
    .unwrap();
    ModelsCache::new(&app.dirs)
        .record(
            &id_of(ALICE),
            vec![
                agyva::agy::models::ModelFamily {
                    id: "gemini-4-carbon".into(),
                    display_name: "Gemini 4 Carbon".into(),
                    variants: vec!["high".into(), "low".into()],
                },
                agyva::agy::models::ModelFamily {
                    id: "solo-high".into(),
                    display_name: "Solo High".into(),
                    variants: Vec::new(),
                },
            ],
            now,
        )
        .unwrap();

    // `solo-high` keeps its OWN declared mapping (Claude / GPT, exhausted),
    // so it is excluded — it is not silently re-based onto `solo`'s mapping.
    let plan = app
        .route_group_for(Some("work"), Some("solo-high"), now)
        .unwrap();
    let alice = plan.accounts.first().unwrap();
    assert!(
        !alice.eligible,
        "a standalone id must use its own mapping, got {:?}",
        alice.why
    );
    assert_eq!(alice.why, "solo-high: mapped quota exhausted");

    // Its own share comes from its own mapping, never `solo`'s Gemini Pro.
    assert_eq!(alice.remaining_percent, Some(0.0));

    // And the real family's variant still resolves to the family.
    let plan = app
        .route_group_for(Some("work"), Some("gemini-4-carbon-high"), now)
        .unwrap();
    assert!(plan.accounts.first().unwrap().eligible);
    assert_eq!(plan.accounts.first().unwrap().remaining_percent, Some(60.0));
}

/// Whatever `agyva models` displays is pasteable into `--for-model`: the id
/// it prints for each recorded family resolves to that same family.
#[test]
fn models_display_ids_are_pasteable_into_for_model() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let now = Utc::now();
    seed_fresh(&app, ALICE, now);
    // One family with two efforts, one lone suffixed id (standalone by the
    // parser's rule) and one plain id.
    let catalog = agyva::agy::models::parse_models(
        "Fetching available models...\n\
gemini-4-carbon-high\tGemini 4 Carbon (High)\n\
gemini-4-carbon-low\tGemini 4 Carbon (Low)\n\
solo-high\tSolo (High)\n\
gpt-oss-120b\tGPT-OSS 120B\n",
    );
    let ids: Vec<String> = catalog.models.iter().map(|f| f.id.clone()).collect();
    assert_eq!(
        ids,
        ["gemini-4-carbon", "gpt-oss-120b", "solo-high"],
        "a lone suffixed id stays standalone with its full raw id"
    );
    ModelsCache::new(&app.dirs)
        .record(&id_of(ALICE), catalog.models, now)
        .unwrap();

    let report = app.models_report(false, now).unwrap();
    let displayed: Vec<String> = report.accounts[0]
        .families
        .iter()
        .map(|f| f.id.clone())
        .collect();
    assert_eq!(displayed, ids, "the table shows exactly the recorded ids");
    for id in &displayed {
        let plan = app.route_group_for(None, Some(id), now).unwrap();
        let row = plan.accounts.first().unwrap();
        assert!(
            !row.why.contains("does not offer"),
            "the displayed id {id} must resolve, got {:?}",
            row.why
        );
    }
    // The lone suffixed id is NOT re-based onto a family named "solo".
    let plan = app.route_group_for(None, Some("solo"), now).unwrap();
    assert!(
        plan.accounts
            .first()
            .unwrap()
            .why
            .contains("does not offer"),
        "`solo` was never a family"
    );
}

/// No mapping is not "eligible by default": the catalog says the account
/// has the family, but nothing local says which windows pay for it, so the
/// whole-account share must not stand in as family evidence.
#[test]
fn unmapped_family_is_excluded_with_the_declared_fix() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let now = Utc::now();
    seed_fresh(&app, ALICE, now);
    // Healthy, 40/70, and the catalog lists the family — but no group is
    // named after it and routing.toml declares no mapping.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\naccounts = ['alice']\n",
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

    let plan = app
        .route_group_for(Some("work"), Some("gemini-4-carbon"), now)
        .unwrap();
    assert_eq!(plan.recommendation, None, "an unmapped family never wins");
    let row = plan.accounts.first().unwrap();
    assert!(!row.eligible);
    assert_eq!(
        row.why,
        "gemini-4-carbon: no quota group reports this family; declare it in routing.toml [groups.<name>.models]"
    );
    // The share stays visible as whole-account evidence, just not decisive.
    assert!(
        row.remaining_percent.is_some(),
        "the raw value remains inspectable"
    );
}

/// The re-scope changes each row's evidence, so the order must follow it. The
/// whole-account view ranks bob first (tier 0) while alice lands in tier 1 on
/// her unrelated five-hour bucket; on the mapped pool alice leads on both
/// share and renewal, so `--for-model` must invert them — and eligible rows
/// always precede ineligible ones in the JSON.
#[test]
fn for_model_rows_are_ranked_on_the_mapped_pool() {
    use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};
    use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};

    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    let now = Utc::now();
    let window = |pct: f64, reset_in: Duration| QuotaWindow {
        remaining_percent: pct,
        reset_at: Some(now + reset_in),
        observed_at: now,
        source: QuotaSource::AgyUsageCommand,
    };
    let entry =
        |mapped: (f64, Duration), other: (f64, Duration)| agyva::core::quota_cache::CacheEntry {
            quota: Some(agyva::core::quota_cache::CachedQuota {
                observed_at: now,
                groups: vec![
                    ModelQuota {
                        group: "Gemini Pro".into(),
                        model: None,
                        disabled: false,
                        five_hour: Some(window(mapped.0, Duration::hours(3))),
                        weekly: Some(window(mapped.0, mapped.1)),
                    },
                    ModelQuota {
                        group: "Claude / GPT".into(),
                        model: None,
                        disabled: false,
                        five_hour: Some(window(other.0, Duration::hours(2))),
                        weekly: Some(window(other.0, other.1)),
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
    app.quota_cache()
        .update(|c| {
            // alice: mapped pool 60% renewing in 6h, but her Claude five-hour
            // bucket sits at 5% — that caps the WHOLE-ACCOUNT rate in tier 1.
            c.accounts.insert(
                id_of(ALICE),
                entry((60.0, Duration::hours(6)), (5.0, Duration::days(4))),
            );
            // bob: mapped pool 40% renewing in 12h, nothing exhausted.
            c.accounts.insert(
                id_of(BOB),
                entry((40.0, Duration::hours(12)), (40.0, Duration::days(3))),
            );
        })
        .unwrap();
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\naccounts = ['alice', 'bob']\n[groups.work.models]\n'gemini-4-carbon' = 'Gemini Pro'\n",
    )
    .unwrap();
    for who in [ALICE, BOB] {
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

    let plain: Vec<String> = app
        .route_group_at(Some("work"), now)
        .unwrap()
        .accounts
        .iter()
        .map(|r| r.alias.clone())
        .collect();
    assert_eq!(
        plain,
        ["bob", "alice"],
        "fixture: the whole-account view ranks bob first"
    );

    let plan = app
        .route_group_for(Some("work"), Some("gemini-4-carbon"), now)
        .unwrap();
    let order: Vec<&str> = plan.accounts.iter().map(|r| r.alias.as_str()).collect();
    assert_eq!(
        order,
        ["alice", "bob"],
        "the mapped pool decides the order: 60% renewing in 6h beats 40% in 12h"
    );
    assert_eq!(plan.recommendation, Some(id_of(ALICE)));
    assert_eq!(plan.accounts[0].remaining_percent, Some(60.0));

    // An exhausted mapped pool makes alice ineligible; the JSON keeps her
    // last even though her whole-account view is the fuller one.
    app.quota_cache()
        .update(|c| {
            c.accounts.insert(
                id_of(ALICE),
                entry((0.0, Duration::hours(6)), (99.0, Duration::days(4))),
            );
        })
        .unwrap();
    let plan = app
        .route_group_for(Some("work"), Some("gemini-4-carbon"), now)
        .unwrap();
    assert_eq!(plan.recommendation, Some(id_of(BOB)));
    let json = serde_json::to_value(&plan).unwrap();
    let rows = json["accounts"].as_array().unwrap();
    let eligible: Vec<bool> = rows
        .iter()
        .map(|r| r["eligible"].as_bool().unwrap())
        .collect();
    assert_eq!(
        eligible,
        [true, false],
        "`--json` puts eligible rows before ineligible ones"
    );
    assert_eq!(rows[1]["alias"], "alice");
    assert_eq!(rows[1]["remaining_percent"], 0.0);
}

/// A suspended account is never retried automatically — the monitor's
/// catalog tick must not spawn `agy models` against it either. `--force`
/// (an explicit `agyva models --refresh`) still checks once.
#[test]
fn suspended_account_is_not_polled_for_models() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    rig.login(ALICE);
    fake.models(MODELS_TSV);
    let now = Utc::now();
    app.suspended_markers().mark(&id_of(ALICE), now).unwrap();

    let out = app.refresh_models_at(false, now).unwrap();
    assert_eq!(
        out.status,
        agyva::core::catalog::ModelsRefreshStatus::Skipped
    );
    assert!(
        out.note
            .as_deref()
            .unwrap_or_default()
            .contains("suspended"),
        "the skip note names the reason: {:?}",
        out.note
    );
    assert_eq!(
        fake.model_calls(),
        0,
        "no spawn against a suspended account"
    );

    // The explicit check still works.
    let forced = app.refresh_models_at(true, now).unwrap();
    assert_eq!(
        forced.status,
        agyva::core::catalog::ModelsRefreshStatus::Refreshed
    );
    assert_eq!(fake.model_calls(), 1);
}

/// The cached health marker counts too: the disposable cache is pruned, but
/// while it says `TermsSuspended` the tick must not spawn either.
#[test]
fn cached_suspension_health_also_blocks_the_models_tick() {
    use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};

    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    rig.login(ALICE);
    fake.models(MODELS_TSV);
    let now = Utc::now();
    app.quota_cache()
        .update(|c| {
            c.accounts.insert(
                id_of(ALICE),
                agyva::core::quota_cache::CacheEntry {
                    health: Some(HealthRecord {
                        state: AccountHealth::TermsSuspended,
                        observed_at: Some(now),
                        source: HealthSource::AgyUsageCommand,
                        evidence: "synthetic".into(),
                    }),
                    ..Default::default()
                },
            );
        })
        .unwrap();

    let out = app.refresh_models_at(false, now).unwrap();
    assert_eq!(
        out.status,
        agyva::core::catalog::ModelsRefreshStatus::Skipped
    );
    assert_eq!(fake.model_calls(), 0);
}

/// An agy build we were never verified against is not spawned from a
/// background tick: skip with a note instead of erroring.
#[test]
fn untested_agy_version_skips_the_models_tick() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig
        .app_with_agy(&fake)
        .with_compat(agyva::agy::compatibility::Compat::Unknown(
            "unparseable version".into(),
        ));
    rig.login(ALICE);
    fake.models(MODELS_TSV);
    let now = Utc::now();

    let out = app.refresh_models_at(true, now).unwrap();
    assert_eq!(
        out.status,
        agyva::core::catalog::ModelsRefreshStatus::Skipped
    );
    let note = out.note.clone().unwrap_or_default();
    assert!(
        note.contains("unparseable"),
        "the note carries the compat refusal: {note:?}"
    );
    // The escape hatch is the untested opt-in, not another refresh: `--force`
    // does not bypass the compat gate (same as the quota reader).
    assert!(
        note.contains("--allow-untested") && !note.contains("--refresh"),
        "the note names the real escape hatch: {note:?}"
    );
    assert_eq!(fake.model_calls(), 0, "unsupported agy is never spawned");
}

/// A removed account's catalog entry is pruned instead of lingering forever
/// in cache/models.json, and the prune never fails when no catalog file
/// exists (the common case for an account that was never refreshed).
#[test]
fn removing_an_account_prunes_its_catalog_entry() {
    let rig = Rig::new();
    // alice is added last, so she is the ACTIVE account the catalog run
    // targets (and the live login).
    rig.add_all(&[BOB, ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    rig.login(ALICE);
    fake.models(MODELS_TSV);
    assert_eq!(
        app.refresh_models(true).unwrap().status,
        agyva::core::catalog::ModelsRefreshStatus::Refreshed
    );
    assert!(
        app.models_cache()
            .load()
            .accounts
            .contains_key(&id_of(ALICE))
    );

    // alice is active, so removing her needs --force (agy keeps the login).
    app.remove("alice", true).unwrap();
    assert!(
        !app.models_cache()
            .load()
            .accounts
            .contains_key(&id_of(ALICE)),
        "the dead id is gone from the catalog cache"
    );

    // And a removal when no catalog file exists still succeeds.
    std::fs::remove_file(app.dirs.cache.join("models.json")).ok();
    app.remove("bob", true).unwrap();
    assert!(app.models_cache().load().accounts.is_empty());
}

/// The quota refresh prunes the catalog cache in the same breath, so an
/// account removed while its entry was still cached cannot survive a tick.
#[test]
fn quota_refresh_prunes_the_catalog_cache() {
    let rig = Rig::new();
    rig.add_all(&[BOB, ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    rig.login(ALICE);
    fake.models(MODELS_TSV);
    app.refresh_models(true).unwrap();
    // A stale id that is not in the registry, as `save_registry` racing a
    // crash could leave behind.
    ModelsCache::new(&app.dirs)
        .record(
            "dead-account-id",
            vec![agyva::agy::models::ModelFamily {
                id: "gone".into(),
                display_name: "Gone".into(),
                variants: Vec::new(),
            }],
            Utc::now(),
        )
        .unwrap();
    fake.ok(0.6, 0.6);
    app.refresh_active(true).unwrap();
    assert!(
        !app.models_cache()
            .load()
            .accounts
            .contains_key("dead-account-id"),
        "the refresh pruned the dead id"
    );
    assert!(
        app.models_cache()
            .load()
            .accounts
            .contains_key(&id_of(ALICE)),
        "the live entry survives"
    );
}

/// `least-used` and `priority` exist to spend the cheaper / preferred
/// account first. The `--for-model` re-rank must not demote by tier before
/// the group's ranking: a rate-capped `free` account (tier 1) must still
/// come before an `ultra` one (tier 0), or the filtered plan spends the
/// expensive plan the plain plan avoided.
#[test]
fn for_model_keeps_least_used_and_priority_plans_ahead_of_tier() {
    use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};

    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    let now = Utc::now();
    let window = |pct: f64, reset_in: Duration| QuotaWindow {
        remaining_percent: pct,
        reset_at: Some(now + reset_in),
        observed_at: now,
        source: QuotaSource::AgyUsageCommand,
    };
    // alice: 60% weekly, but a 5% five-hour cap → tier 1 under `smart`.
    // bob: 40/40 → tier 0.
    let alice = agyva::core::quota_cache::CacheEntry {
        quota: Some(agyva::core::quota_cache::CachedQuota {
            observed_at: now,
            groups: vec![ModelQuota {
                group: "Gemini Pro".into(),
                model: None,
                disabled: false,
                five_hour: Some(window(5.0, Duration::hours(2))),
                weekly: Some(window(60.0, Duration::hours(20))),
            }],
        }),
        ..Default::default()
    };
    let bob = agyva::core::quota_cache::CacheEntry {
        quota: Some(agyva::core::quota_cache::CachedQuota {
            observed_at: now,
            groups: vec![ModelQuota {
                group: "Gemini Pro".into(),
                model: None,
                disabled: false,
                five_hour: Some(window(40.0, Duration::hours(3))),
                weekly: Some(window(40.0, Duration::hours(30))),
            }],
        }),
        ..Default::default()
    };
    app.quota_cache()
        .update(|c| {
            c.accounts.insert(id_of(ALICE), alice);
            c.accounts.insert(id_of(BOB), bob);
        })
        .unwrap();
    for who in [ALICE, BOB] {
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

    // least-used: alice is on the cheaper plan and must stay ahead.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\nranking = 'least-used'\naccounts = ['alice', 'bob']\n\
         plans = { alice = 'free', bob = 'ultra' }\n\
         [groups.work.models]\n'gemini-4-carbon' = 'Gemini Pro'\n",
    )
    .unwrap();
    let plain = app.route_group_at(Some("work"), now).unwrap();
    assert_eq!(plain.recommendation, Some(id_of(ALICE)));
    let plan = app
        .route_group_for(Some("work"), Some("gemini-4-carbon"), now)
        .unwrap();
    let order: Vec<&str> = plan.accounts.iter().map(|r| r.alias.as_str()).collect();
    assert_eq!(
        order,
        ["alice", "bob"],
        "least-used stays authoritative under --for-model (no tier demotion)"
    );
    assert_eq!(
        plan.recommendation,
        Some(id_of(ALICE)),
        "the cheap plan is the one applied"
    );

    // priority: alice is the user's preferred account and must stay ahead.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\nranking = 'priority'\naccounts = ['alice', 'bob']\n[groups.work.priorities]\n\
         alice = 10\nbob = 1\n[groups.work.models]\n'gemini-4-carbon' = 'Gemini Pro'\n",
    )
    .unwrap();
    let plain = app.route_group_at(Some("work"), now).unwrap();
    assert_eq!(plain.recommendation, Some(id_of(ALICE)));
    let plan = app
        .route_group_for(Some("work"), Some("gemini-4-carbon"), now)
        .unwrap();
    let order: Vec<&str> = plan.accounts.iter().map(|r| r.alias.as_str()).collect();
    assert_eq!(
        order,
        ["alice", "bob"],
        "declared priority stays authoritative under --for-model"
    );
    assert_eq!(plan.recommendation, Some(id_of(ALICE)));

    // Smart keeps its tier-first behaviour: the rate cap demotes alice.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\nranking = 'smart'\naccounts = ['alice', 'bob']\n[groups.work.models]\n'gemini-4-carbon' = 'Gemini Pro'\n",
    )
    .unwrap();
    let plan = app
        .route_group_for(Some("work"), Some("gemini-4-carbon"), now)
        .unwrap();
    let order: Vec<&str> = plan.accounts.iter().map(|r| r.alias.as_str()).collect();
    assert_eq!(order, ["bob", "alice"], "smart still ranks by tier");
}
