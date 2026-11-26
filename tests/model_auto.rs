//! `agyva auto`: explicit one-family rotator. Sandboxed fake agy only.

mod common;

use chrono::{Duration, Utc};
use common::*;

use agyva::core::health::{AccountHealth, HealthRecord, HealthSource};
use agyva::core::model_auto::{ModelAutoOnce, ModelFamily};
use agyva::core::policy::RESTART_NOTICE;
use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};
use agyva::core::quota_cache::{CacheEntry, CachedQuota};

fn seed(
    app: &agyva::core::app::App,
    who: (&str, &str, &str),
    at: chrono::DateTime<Utc>,
    group: &str,
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
                            group: group.into(),
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

fn usage_doc(groups: &[(&str, bool, f64)]) -> String {
    let groups: Vec<_> = groups
        .iter()
        .map(|(name, disabled, frac)| {
            serde_json::json!({
                "name": name,
                "disabled": disabled,
                "buckets": [
                    {"window":"5h","remaining_fraction":frac,"reset_time":"2099-01-01T05:00:00Z"},
                    {"window":"weekly","remaining_fraction":frac,"reset_time":"2099-01-07T00:00:00Z"}
                ]
            })
        })
        .collect();
    serde_json::json!({
        "status": "ok",
        "response": "",
        "usage": {},
        "command": {"data": {"groups": groups}}
    })
    .to_string()
}

fn once(app: &agyva::core::app::App, family: ModelFamily) -> ModelAutoOnce {
    app.model_auto_once(family, Utc::now()).unwrap()
}

fn assert_no_restart_claim(text: &str) {
    assert!(!text.to_ascii_lowercase().contains("no restart"), "{text}");
}

#[test]
fn over_line_gemini_switches_to_the_highest_remaining_and_names_the_restart() {
    let rig = Rig::new();
    rig.add_all(&[BOB, CAROL, ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.05);
    let app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, BOB, now, "Gemini Pro", 80.0, AccountHealth::Healthy);
    seed(&app, CAROL, now, "Gemini Pro", 90.0, AccountHealth::Healthy);

    let out = once(&app, ModelFamily::Gemini);
    assert!(out.switched, "{out:?}");
    assert_eq!(out.target.as_deref(), Some("carol"));
    assert_eq!(rig.live_id(), Some(id_of(CAROL)));
    assert_eq!(out.restart_notice, Some(RESTART_NOTICE));
    assert!(out.restart_notice.unwrap().contains("Restart agy"));
    assert_no_restart_claim(&out.message);
    assert_no_restart_claim(out.restart_notice.unwrap());
    assert!(!out.message.contains("not switching"));
}

#[test]
fn under_line_and_exact_ten_percent_do_not_switch() {
    let rig = Rig::new();
    rig.add_all(&[BOB, ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.5, 0.5);
    let app = rig.app_with_agy(&fake);
    seed(
        &app,
        BOB,
        Utc::now(),
        "Gemini Pro",
        80.0,
        AccountHealth::Healthy,
    );
    let out = once(&app, ModelFamily::Gemini);
    assert!(!out.switched, "{out:?}");
    assert!(out.target.is_none());
    assert!(out.restart_notice.is_none());
    assert!(out.message.contains("not switching"), "{}", out.message);
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));

    fake.ok(0.1, 0.1);
    let out = once(&app, ModelFamily::Gemini);
    assert!(!out.switched, "{out:?}");
    assert!(out.message.contains("10.0%"), "{}", out.message);
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
}

#[test]
fn gemini_fine_does_not_switch_when_claude_is_exhausted() {
    let rig = Rig::new();
    rig.add_all(&[BOB, ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.stdout(&usage_doc(&[
        ("Gemini Pro", false, 0.8),
        ("Claude", false, 0.0),
    ]));
    let app = rig.app_with_agy(&fake);
    seed(
        &app,
        BOB,
        Utc::now(),
        "Gemini Pro",
        95.0,
        AccountHealth::Healthy,
    );
    let out = once(&app, ModelFamily::Gemini);
    assert!(!out.switched, "{out:?}");
    assert!(out.message.contains("not switching"), "{}", out.message);
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
}

#[test]
fn suspended_or_over_line_candidate_is_not_selected() {
    let rig = Rig::new();
    rig.add_all(&[BOB, CAROL, ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.05);
    let app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(
        &app,
        BOB,
        now,
        "Gemini Pro",
        95.0,
        AccountHealth::TermsSuspended,
    );
    seed(&app, CAROL, now, "Gemini Pro", 4.0, AccountHealth::Healthy);
    let out = once(&app, ModelFamily::Gemini);
    assert!(!out.switched, "{out:?}");
    assert!(out.message.contains("no eligible"), "{}", out.message);
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
}

#[test]
fn second_once_inside_cooldown_does_not_switch() {
    let rig = Rig::new();
    rig.add_all(&[BOB, CAROL, ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.05);
    let app = rig.app_with_agy(&fake);
    let now = Utc::now();
    seed(&app, BOB, now, "Gemini Pro", 80.0, AccountHealth::Healthy);
    seed(&app, CAROL, now, "Gemini Pro", 70.0, AccountHealth::Healthy);
    let first = once(&app, ModelFamily::Gemini);
    assert!(first.switched, "{first:?}");
    assert_eq!(first.target.as_deref(), Some("bob"));
    let second = once(&app, ModelFamily::Gemini);
    assert!(!second.switched, "{second:?}");
    assert!(
        second.message.contains("cooling down"),
        "{}",
        second.message
    );
    assert!(second.restart_notice.is_none());
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
}

#[test]
fn missing_family_does_not_invent_a_percent() {
    let rig = Rig::new();
    rig.add_all(&[BOB, ALICE]);
    let fake = common::fake_agy::FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.stdout(&usage_doc(&[("Claude", false, 0.2)]));
    let app = rig.app_with_agy(&fake);
    seed(
        &app,
        BOB,
        Utc::now(),
        "Gemini Pro",
        90.0,
        AccountHealth::Healthy,
    );
    let out = once(&app, ModelFamily::Gemini);
    assert!(!out.switched, "{out:?}");
    assert!(out.message.contains("not reported"), "{}", out.message);
    assert!(!out.message.contains('%'), "{}", out.message);
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
}

#[test]
fn command_parses_model_case_insensitively() {
    use clap::Parser as _;
    let cli =
        agyva::cli::Cli::try_parse_from(["agyva", "auto", "--once", "--model", "GEMINI"]).unwrap();
    match cli.command {
        Some(agyva::cli::Command::Auto(args)) => {
            assert!(args.once);
            assert!(matches!(args.model, agyva::cli::AutoModel::Gemini));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn binary_once_switches_and_prints_the_restart_notice() {
    use std::process::{Command, Stdio};

    let tmp = tempfile::tempdir().unwrap();
    let fake = common::fake_agy::FakeAgy::install(&tmp.path().join("fakeagy"));
    fake.ok(0.05, 0.05);
    for d in ["home", "agyhome", "cfg", "state", "cache"] {
        std::fs::create_dir_all(tmp.path().join(d)).unwrap();
    }
    let p = |rel: &str| tmp.path().join(rel);
    let run = |args: &[&str]| {
        let o = Command::new(env!("CARGO_BIN_EXE_agyva"))
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", p("home"))
            .env("AGYVA_CONFIG_DIR", p("cfg"))
            .env("AGYVA_STATE_DIR", p("state"))
            .env("AGYVA_CACHE_DIR", p("cache"))
            .env("AGYVA_AGY_HOME", p("agyhome"))
            .env("AGYVA_AGY_BIN", fake.bin())
            .env("AGYVA_BACKEND", "file")
            .env("AGYVA_PASSPHRASE", "synthetic-e2e-passphrase")
            .env("NO_COLOR", "1")
            .env("AGYVA_UPDATE_CHECK", "0")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (
            o.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&o.stdout).into_owned(),
            String::from_utf8_lossy(&o.stderr).into_owned(),
        )
    };
    std::fs::write(
        p("agyhome/antigravity-oauth-token"),
        agyva::testing::credential("sub-bob", "bob@example.invalid", "bob"),
    )
    .unwrap();
    let (code, out, err) = run(&["add", "--alias", "bob"]);
    assert_eq!(code, 0, "{out}{err}");
    std::fs::write(
        p("agyhome/antigravity-oauth-token"),
        agyva::testing::credential("sub-alice", "alice@example.invalid", "alice"),
    )
    .unwrap();
    let (code, out, err) = run(&["add", "--alias", "alice"]);
    assert_eq!(code, 0, "{out}{err}");

    let now = Utc::now();
    let mut cache = agyva::core::quota_cache::CacheFile::default();
    cache.accounts.insert(
        id_of(BOB),
        CacheEntry {
            quota: Some(CachedQuota {
                observed_at: now,
                groups: vec![ModelQuota {
                    group: "Gemini Pro".into(),
                    model: None,
                    disabled: false,
                    five_hour: Some(QuotaWindow {
                        remaining_percent: 80.0,
                        reset_at: Some(now + Duration::hours(3)),
                        observed_at: now,
                        source: QuotaSource::AgyUsageCommand,
                    }),
                    weekly: Some(QuotaWindow {
                        remaining_percent: 80.0,
                        reset_at: Some(now + Duration::days(2)),
                        observed_at: now,
                        source: QuotaSource::AgyUsageCommand,
                    }),
                }],
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
    std::fs::write(p("cache/quota.json"), serde_json::to_vec(&cache).unwrap()).unwrap();

    let (code, out, err) = run(&["auto", "--once", "--model", "gemini"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("Restart agy"), "{out}\n{err}");
    assert_no_restart_claim(&out);
    assert_no_restart_claim(&err);
    assert!(out.contains("bob"), "{out}");
    let live = std::fs::read(p("agyhome/antigravity-oauth-token")).unwrap();
    let live_id = agyva::agy::credentials::parse_credential(&live)
        .unwrap()
        .account_id();
    assert_eq!(live_id, id_of(BOB));
}
