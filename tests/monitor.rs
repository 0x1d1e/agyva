//! Slice E: `agyva monitor` — cache-only evaluation loop, sticky pick,
//! 300s single-flight active refresh, least-used ranking degradation.

mod common;

use chrono::{Duration, Utc};
use common::*;
use std::time::Duration as StdDuration;

use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};
use agyva::core::monitor::{MonitorProbe, parse_interval};
use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};
use agyva::core::quota_cache::{CacheEntry, CachedQuota};
use agyva::core::route_config::Ranking;
use agyva::core::watch::Probe;

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
fn ineligible_rows_show_no_share_and_rely_on_the_reason() {
    use agyva::core::quota::Freshness;
    use agyva::core::route::{RoutePlan, RouteRow};

    let now = Utc::now();
    let row = |alias: &str, id: &str, eligible: bool, freshness: Freshness, share: f64| RouteRow {
        id: id.into(),
        alias: alias.into(),
        active: false,
        freshness,
        eligible,
        remaining_percent: Some(share),
        rate_remaining_percent: Some(share),
        renewals: [None, None],
        why: if eligible {
            format!("fresh cache; {share:.0}% remains in fullest window")
        } else {
            "quota stale, incomplete, expired, or reset passed; refresh required".into()
        },
        command: format!("agyva use {alias}"),
    };
    let plan = RoutePlan {
        schema_version: 1,
        generated_at: now,
        recommendation: Some("a1".into()),
        accounts: vec![
            row("alice", "a1", true, Freshness::Fresh, 64.0),
            row("bob", "b1", false, Freshness::Stale, 35.0),
            row("carol", "c1", false, Freshness::Expired, 7.0),
        ],
    };
    let text = agyva::cli::monitor_cmd::render_plan(&plan, now, None);
    assert!(text.contains("64%"), "eligible share stays: {text}");
    // An ineligible row must not advertise its cached share as usable.
    for line in text.lines() {
        if line.contains("bob") || line.contains("carol") {
            assert!(
                !line.contains("35%") && !line.contains("7%"),
                "ineligible share leaked: {line}"
            );
            assert!(line.contains("  -  "), "expected a dash share: {line}");
        }
    }
    assert!(text.contains("refresh required"));
}

#[test]
fn interval_grammar_floor_and_units() {
    assert_eq!(parse_interval("15s").unwrap(), StdDuration::from_secs(15));
    assert_eq!(parse_interval("1m").unwrap(), StdDuration::from_secs(60));
    assert_eq!(parse_interval("1h").unwrap(), StdDuration::from_secs(3600));
    assert_eq!(parse_interval("30").unwrap(), StdDuration::from_secs(30));
    // Below the 5s floor: refused, not clamped silently.
    assert!(parse_interval("4s").is_err());
    assert!(parse_interval("0").is_err());
    assert!(parse_interval("90x").is_err());
    assert!(parse_interval("abc").is_err());
}

#[test]
fn monitor_cycles_are_cache_only_and_sticky() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    // The active account (bob) refreshes on the first cycle; a real payload
    // keeps bob's 80% share in the fresh reading.
    fake.ok(0.8, 0.8);
    let app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 50.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 80.0, AccountHealth::Healthy);
    let mut probe = MonitorProbe::new(&app, None, None, StdDuration::from_secs(300));
    let r1 = probe.cycle(now);
    // First cycle refreshes the ACTIVE account once (single-flight guard);
    // evaluation itself is cache-only — a second cycle adds no spawn.
    let calls_after_first = fake.calls();
    let plan1 = probe.last_plan().unwrap();
    // Smart ranks by weekly renewal first: alice renews in 2 days, bob's
    // fresh reading renews in 2099, so alice tops the plan.
    assert_eq!(plan1.recommendation, Some(id_of(ALICE)));
    assert!(r1.note.unwrap().contains("alice"));

    // Second cycle right after: same evidence, same pick, no new spawn.
    let r2 = probe.cycle(now + Duration::seconds(1));
    assert_eq!(
        fake.calls(),
        calls_after_first,
        "evaluation never spawns agy"
    );
    let plan2 = probe.last_plan().unwrap();
    assert_eq!(plan2.recommendation, Some(id_of(ALICE)));
    let _ = r2;

    // The pick moves only when the sticky pick turns ineligible (here:
    // suspended), never on ranking noise alone.
    seed(
        &app,
        ALICE,
        now + Duration::seconds(2),
        50.0,
        AccountHealth::TermsSuspended,
    );
    probe.cycle(now + Duration::seconds(2));
    assert_eq!(
        probe.last_plan().unwrap().recommendation,
        Some(id_of(BOB)),
        "ineligible sticky pick yields to the next ranked account"
    );
}

#[test]
fn monitor_refresh_runs_on_its_own_cadence_single_flight() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    let now = Utc::now();

    let mut probe = MonitorProbe::new(&app, None, None, StdDuration::from_secs(300));
    // First cycle: no last_refresh -> one refresh attempt (active refresh
    // path; fake-agy answers it).
    probe.cycle(now);
    let after_first = fake.calls();
    // Second cycle seconds later: inside the 300s window -> no refresh.
    probe.cycle(now + Duration::seconds(5));
    assert_eq!(
        fake.calls(),
        after_first,
        "refresh throttled to its cadence"
    );
    // Past the cadence: refresh attempted again.
    probe.cycle(now + Duration::seconds(301));
    assert!(
        fake.calls() > after_first,
        "refresh resumes after the window"
    );
}

#[test]
fn least_used_ranks_by_declared_plan_and_degrades_to_share() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 90.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 10.0, AccountHealth::Healthy);

    // Declared plans: alice pays less share. Least-used picks alice even
    // though bob has more remaining.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\nranking = 'least-used'\n[groups.work.plans]\nalice = 'pro'\nbob = 'ultra'\n",
    )
    .unwrap();
    let plan = app.route_group_at(Some("work"), now).unwrap();
    assert_eq!(plan.accounts[0].alias, "alice");

    // Without declared plans every used-rank is equal, so least-used
    // degrades to smart's ordering (documented): the registry tie keeps
    // alice (50% remaining) ahead of bob.
    std::fs::write(
        app.dirs.config.join("routing.toml"),
        "[groups.work]\nranking = 'least-used'\n",
    )
    .unwrap();
    let plan = app.route_group_at(Some("work"), now).unwrap();
    assert_eq!(plan.accounts[0].alias, "alice");

    // The enum deserializes the kebab spelling; Smart stays the default.
    assert_eq!(
        serde_json::from_str::<Ranking>("\"smart\"").unwrap(),
        Ranking::Smart
    );
    assert_eq!(
        serde_json::from_str::<Ranking>("\"least-used\"").unwrap(),
        Ranking::LeastUsed
    );
}

#[test]
fn priority_ranking_unchanged_and_every_ranking_value_works() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    let app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, ALICE, now, 50.0, AccountHealth::Healthy);
    seed(&app, BOB, now, 50.0, AccountHealth::Healthy);

    for (toml_ranking, first_alias) in [
        ("priority", "alice"), // listed order wins under priority
        ("smart", "alice"),    // tie keeps registry order
        ("least-used", "alice"),
    ] {
        std::fs::write(
            app.dirs.config.join("routing.toml"),
            format!("[groups.work]\naccounts = ['alice', 'bob']\nranking = '{toml_ranking}'\n"),
        )
        .unwrap();
        let plan = app.route_group_at(Some("work"), now).unwrap();
        assert_eq!(plan.accounts[0].alias, first_alias, "{toml_ranking}");
        assert!(plan.accounts[0].eligible);
    }
}
