mod common;
use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};
use agyva::core::quota::{Freshness, ModelQuota, QuotaSource, QuotaWindow};
use agyva::core::quota_cache::{CacheEntry, CachedQuota};
use chrono::{Duration, Utc};
use common::*;

fn seed(
    app: &agyva::core::app::App,
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

#[test]
fn routing_three_accounts_is_cache_only_and_read_only() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB, CAROL]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 50.0, AccountHealth::Healthy);
    seed(
        &app,
        BOB,
        now - Duration::hours(2),
        80.0,
        AccountHealth::Healthy,
    );
    seed(&app, CAROL, now, 100.0, AccountHealth::TermsSuspended);
    let before: Vec<_> = rig
        .all_files()
        .into_iter()
        .map(|p| {
            let b = std::fs::read(&p).unwrap();
            (p, b)
        })
        .collect();
    let native = rig.native_text();
    let plan = app.route_plan_at(now).unwrap();
    assert_eq!(plan.recommendation, Some(id_of(ALICE)));
    assert_eq!(plan.accounts[0].alias, "alice");
    let bob = plan.accounts.iter().find(|a| a.alias == "bob").unwrap();
    assert_eq!(bob.freshness, Freshness::Stale);
    assert!(!bob.eligible, "stale cache is advisory only");
    assert!(
        !plan
            .accounts
            .iter()
            .find(|a| a.alias == "carol")
            .unwrap()
            .eligible
    );
    assert_eq!(fake.calls(), 0);
    assert_eq!(native, rig.native_text());
    for (p, b) in before {
        assert_eq!(std::fs::read(p).unwrap(), b);
    }
}

#[test]
fn routing_ties_keep_registry_order_and_unknown_is_last() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB, CAROL]);
    let app = rig.app();
    let now = Utc::now();
    for who in [ALICE, BOB] {
        seed(&app, who, now, 50.0, AccountHealth::Healthy);
    }
    let p = app.route_plan_at(now).unwrap();
    assert_eq!(p.accounts[0].id, id_of(ALICE));
    assert_eq!(p.accounts[1].id, id_of(BOB));
    assert!(!p.accounts[2].eligible);
    assert!(
        app.route_plan_at(now + Duration::days(2))
            .unwrap()
            .recommendation
            .is_none()
    );
}

#[test]
fn routing_named_groups_priorities_and_reset_expiry() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB, CAROL]);
    let app = rig.app();
    let now = Utc::now();
    for who in [ALICE, BOB, CAROL] {
        seed(&app, who, now, 50.0, AccountHealth::Healthy);
    }
    std::fs::write(app.dirs.config.join("routing.toml"), "[groups.work]\naccounts = ['bob', 'alice']\nranking = 'priority'\n[groups.work.priorities]\nalice = 10\n").unwrap();
    let p = app.route_group_at(Some("work"), now).unwrap();
    assert_eq!(p.recommendation, Some(id_of(ALICE)));
    assert_eq!(p.accounts.len(), 2);
    assert!(app.route_group_at(Some("missing"), now).is_err());
    let mut accounts = app.account_usages(None, now).unwrap();
    accounts[0].groups[0].five_hour.as_mut().unwrap().reset_at = Some(now);
    accounts[1].groups[0].weekly = None;
    let p = agyva::core::route::rank(
        &accounts,
        now,
        agyva::core::quota::FreshnessPolicy::new(300),
    );
    assert!(
        !p.accounts
            .iter()
            .find(|r| r.id == id_of(ALICE))
            .unwrap()
            .eligible
    );
    assert!(
        !p.accounts
            .iter()
            .find(|r| r.id == id_of(BOB))
            .unwrap()
            .eligible
    );
}
