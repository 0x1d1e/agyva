//! Phase D: quota, health and refresh behavior against a fake `agy`.

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use agyva::agy::usage_run::RunLimits;
use agyva::core::app::App;
use agyva::core::health::AccountHealth;
use agyva::core::provider::AgyUsageProvider;
use agyva::core::quota::{Freshness, QuotaSource, UnavailableReason};
use agyva::core::refresh::{RefreshOutcome, RefreshStatus};
use agyva::core::usage::{NOT_REFRESHED, RefreshMode, UsageOptions};
use chrono::{Duration as Cd, Utc};
use common::fake_agy::FakeAgy;
use common::*;

struct World {
    rig: Rig,
    fake: FakeAgy,
}

impl World {
    /// bob (active, live) and alice saved.
    fn new() -> Self {
        let rig = Rig::new();
        rig.add_all(&[ALICE, BOB]);
        let fake = FakeAgy::install(&rig.tmp.path().join("fakeagy"));
        Self { rig, fake }
    }
    fn app(&self) -> App {
        self.rig.app_with_agy(&self.fake)
    }
}

fn reason(o: &RefreshOutcome) -> UnavailableReason {
    o.unavailable().map(|u| u.reason).expect("unavailable")
}

fn row<'a>(
    v: &'a [agyva::core::usage::AccountUsage],
    alias: &str,
) -> &'a agyva::core::usage::AccountUsage {
    v.iter().find(|a| a.alias == alias).expect("account row")
}

#[test]
fn live_refresh_reports_groups_with_source_freshness_and_health() {
    let w = World::new();
    w.fake.ok(0.72, 0.4);
    let app = w.app();
    let report = app
        .usage_report(&UsageOptions {
            selector: None,
            all: true,
            refresh: RefreshMode::Normal,
        })
        .unwrap();
    let bob = row(&report.accounts, "bob");
    assert!(bob.refreshed_now && bob.active);
    assert_eq!(bob.freshness, Freshness::Fresh);
    assert_eq!(bob.health.state, AccountHealth::Healthy);
    assert!(bob.health.verified_now);
    assert_eq!(bob.groups.len(), 2, "multiple model groups");
    let gem = &bob.groups[0];
    assert_eq!(gem.group, "Gemini Pro");
    let five = gem.five_hour.as_ref().expect("5h");
    assert!((five.remaining_percent - 72.0).abs() < 1e-6);
    assert_eq!(five.source, QuotaSource::AgyUsageCommand);
    assert!(five.reset_at.is_some());
    let disabled = &bob.groups[1];
    assert!(
        disabled.disabled && !disabled.exhausted,
        "disabled is not exhausted"
    );

    // The other account is cache-only and says so.
    let alice = row(&report.accounts, "alice");
    assert!(!alice.refreshed_now);
    assert_eq!(alice.freshness, Freshness::Missing);
    assert_eq!(alice.health.state, AccountHealth::Unknown);
    assert!(!alice.health.verified_now);
    let why = alice.unavailable.as_ref().expect("why");
    assert_eq!(why.reason, UnavailableReason::NotRefreshable);
    assert_eq!(why.detail, NOT_REFRESHED);
    assert!(
        alice.groups.is_empty(),
        "no fabricated numbers for other accounts"
    );
}

#[test]
fn cached_data_is_labeled_stale_and_never_claims_healthy() {
    let w = World::new();
    w.fake.ok(0.5, 0.5);
    let app = w.app();
    let t0 = Utc::now() - Cd::hours(3);
    let first = app.refresh_active_at(false, t0).unwrap();
    assert!(matches!(first.status, RefreshStatus::Refreshed));

    let rows = app.account_usages(None, Utc::now()).unwrap();
    let bob = row(&rows, "bob");
    assert_eq!(bob.freshness, Freshness::Stale);
    assert!(!bob.refreshed_now);
    assert_eq!(
        bob.groups[0].five_hour.as_ref().unwrap().source,
        QuotaSource::Cache
    );
    assert_ne!(
        bob.health.state,
        AccountHealth::Healthy,
        "cache cannot vouch for health"
    );
    assert!(!bob.health.verified_now);
    assert!(bob.health.evidence.contains("cached"));
    assert!(bob.last_successful_update.is_some());

    let old = Utc::now() + Cd::hours(30);
    let rows = app.account_usages(None, old).unwrap();
    assert_eq!(row(&rows, "bob").freshness, Freshness::Expired);
}

#[test]
fn refresh_is_throttled_and_the_cache_survives_failures() {
    let w = World::new();
    w.fake.ok(0.9, 0.9);
    let app = w.app();
    let t0 = Utc::now();
    assert!(matches!(
        app.refresh_active_at(false, t0).unwrap().status,
        RefreshStatus::Refreshed
    ));
    let again = app.refresh_active_at(false, t0 + Cd::seconds(10)).unwrap();
    assert_eq!(reason(&again), UnavailableReason::TooSoon);
    assert_eq!(w.fake.calls(), 1, "no second agy run inside the interval");

    w.fake
        .fail("dial tcp: lookup example.invalid: no such host", 1);
    let later = app.refresh_active_at(false, t0 + Cd::seconds(120)).unwrap();
    assert!(later.is_transient_failure());
    assert_eq!(reason(&later), UnavailableReason::Network);
    assert_eq!(
        later.live.as_ref().unwrap().health.state,
        AccountHealth::NetworkUnavailable
    );
    let rows = app
        .account_usages(Some(&later), t0 + Cd::seconds(121))
        .unwrap();
    let bob = row(&rows, "bob");
    assert_eq!(bob.groups.len(), 2, "last good quota is kept while offline");
    assert_eq!(bob.last_successful_update, Some(t0));
    assert!(bob.unavailable.is_some());
}

#[test]
fn failure_outputs_map_to_explicit_states() {
    let cases = [
        (
            "Error: not logged in. Please run login again.",
            AccountHealth::AuthenticationRequired,
            UnavailableReason::AuthRequired,
        ),
        (
            "429 Too Many Requests: rate limit exceeded",
            AccountHealth::QuotaLimited,
            UnavailableReason::RateLimited,
        ),
        (
            "request failed: connection refused",
            AccountHealth::NetworkUnavailable,
            UnavailableReason::Network,
        ),
    ];
    for (text, health, why) in cases {
        let w = World::new();
        w.fake.fail(text, 1);
        let out = w.app().refresh_active(false).unwrap();
        assert_eq!(reason(&out), why, "{text}");
        assert_eq!(out.live.unwrap().health.state, health, "{text}");
    }
}

#[test]
fn tos_message_marks_suspended_and_stops_retrying() {
    let w = World::new();
    w.fake.fail(
        "PERMISSION_DENIED: this account has been disabled for violating the Terms of Service",
        1,
    );
    let app = w.app();
    let first = app.refresh_active(false).unwrap();
    assert_eq!(reason(&first), UnavailableReason::Suspended);
    assert_eq!(
        first.live.unwrap().health.state,
        AccountHealth::TermsSuspended
    );
    assert_eq!(w.fake.calls(), 1);

    // Later: skipped without running agy, and never shown healthy.
    let second = app.refresh_active(false).unwrap();
    assert_eq!(reason(&second), UnavailableReason::Suspended);
    assert!(matches!(second.status, RefreshStatus::Skipped(_)));
    assert_eq!(w.fake.calls(), 1, "a suspended account is not retried");
    let rows = app.account_usages(Some(&second), Utc::now()).unwrap();
    let bob = row(&rows, "bob");
    assert_eq!(bob.health.state, AccountHealth::TermsSuspended);
    let warning = bob.health.warning.as_deref().expect("warning");
    assert!(warning.contains("https://antigravity.google/docs/faq"));
    assert!(warning.contains("will not work around"));

    // Only an explicit force checks once more.
    let forced = app.refresh_active(true).unwrap();
    assert_eq!(w.fake.calls(), 2);
    assert_eq!(
        forced.live.unwrap().health.state,
        AccountHealth::TermsSuspended
    );

    // A transient offline blip does not erase the suspension.
    w.fake.fail("connection refused", 1);
    let blip = app.refresh_active(true).unwrap();
    assert_eq!(
        blip.live.unwrap().health.state,
        AccountHealth::TermsSuspended
    );
}

#[test]
fn bare_403_is_not_a_suspension() {
    let w = World::new();
    w.fake.fail("googleapi: Error 403: PERMISSION_DENIED", 1);
    let out = w.app().refresh_active(false).unwrap();
    let state = out.live.unwrap().health.state;
    assert_ne!(state, AccountHealth::TermsSuspended);
    assert!(
        matches!(state, AccountHealth::Unknown | AccountHealth::AccessRevoked),
        "insufficient evidence stays conservative: {state:?}"
    );
}

#[test]
fn exhausted_enabled_group_is_quota_limited_but_disabled_is_not() {
    let w = World::new();
    w.fake.ok(0.0, 0.5);
    let out = w.app().refresh_active(false).unwrap();
    assert_eq!(out.live.unwrap().health.state, AccountHealth::QuotaLimited);

    let w = World::new();
    w.fake.ok(0.6, 0.5); // the disabled Claude group sits at 0
    let out = w.app().refresh_active(false).unwrap();
    assert_eq!(out.live.unwrap().health.state, AccountHealth::Healthy);
}

#[test]
fn no_provider_when_agy_is_missing_or_untested() {
    let w = World::new();
    let mut app = w.rig.app();
    app.env = agyva::paths::Env::from_pairs([(
        "AGYVA_AGY_BIN",
        w.rig.tmp.path().join("nope/agy").to_str().unwrap(),
    )]);
    let out = app.refresh_active(false).unwrap();
    assert_eq!(reason(&out), UnavailableReason::NoProvider);
    assert!(out.live.is_none(), "nothing is invented");

    let app = w
        .rig
        .app_with_agy(&w.fake)
        .with_compat(agyva::agy::compatibility::Compat::assess(Ok(
            agyva::agy::compatibility::parse_version("agy 2.0.0").unwrap(),
        )));
    w.fake.ok(0.5, 0.5);
    let out = app.refresh_active(false).unwrap();
    assert_eq!(reason(&out), UnavailableReason::NoProvider);
    assert_eq!(w.fake.calls(), 0, "an unsupported agy is not run");
}

#[test]
fn garbled_or_changed_output_is_schema_changed() {
    for text in [
        "not json at all",
        r#"{"status":"ok"}"#,
        r#"{"command":{"data":{"groups":"nope"}}}"#,
    ] {
        let w = World::new();
        w.fake.stdout(text);
        let out = w.app().refresh_active(false).unwrap();
        assert_eq!(reason(&out), UnavailableReason::SchemaChanged, "{text}");
        let rows = w.app().account_usages(Some(&out), Utc::now()).unwrap();
        assert!(row(&rows, "bob").groups.is_empty(), "never guess");
    }
}

#[test]
fn hung_agy_is_killed_at_the_timeout() {
    let w = World::new();
    w.fake.hang(60);
    let provider = AgyUsageProvider {
        bin: w.fake.bin(),
        limits: RunLimits {
            timeout: Duration::from_millis(400),
            max_bytes: 4096,
        },
    };
    let app = w
        .rig
        .app_with_agy(&w.fake)
        .with_provider(Arc::new(provider));
    let start = Instant::now();
    let out = app.refresh_active(false).unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(reason(&out), UnavailableReason::TimedOut);
}

#[test]
fn mismatched_live_identity_is_not_attributed() {
    let w = World::new();
    w.fake.ok(0.5, 0.5);
    w.rig.login(ALICE); // agy now runs as alice, registry still says bob
    let out = w.app().refresh_active(false).unwrap();
    assert_eq!(reason(&out), UnavailableReason::ActiveLoginDiffers);
    assert!(
        out.unavailable()
            .unwrap()
            .detail
            .contains("differs from registry")
    );
    assert_eq!(w.fake.calls(), 0);
    let cache = w.app().quota_cache().load();
    assert!(cache.accounts.is_empty(), "nothing was cached for anyone");
}

#[test]
fn json_views_carry_no_secrets() {
    let w = World::new();
    w.fake.ok(0.5, 0.5);
    let app = w.app();
    let opts = UsageOptions {
        selector: None,
        all: true,
        refresh: RefreshMode::Normal,
    };
    let usage = serde_json::to_string(&app.usage_report(&opts).unwrap()).unwrap();
    let health = serde_json::to_string(&app.health_report(&opts).unwrap()).unwrap();
    let cache = std::fs::read_to_string(app.quota_cache().path()).unwrap();
    for tag in ["alice", "bob"] {
        for secret in agyva::testing::secrets_for(tag) {
            for text in [&usage, &health, &cache] {
                assert!(!text.contains(&secret), "{secret} leaked");
            }
        }
    }
    assert!(usage.contains("\"schema_version\":1"));
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(app.quota_cache().path())
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn partial_buckets_without_reset_times_still_parse() {
    let w = World::new();
    w.fake.stdout(
        r#"{"command":{"data":{"groups":[{"name":"Gemini","buckets":[
        {"id":"a","name":"x","window":"5h","remaining_fraction":0.25},
        {"id":"b","name":"y","window":"weekly"}]}]}}}"#,
    );
    let app = w.app();
    let out = app.refresh_active(false).unwrap();
    assert!(matches!(out.status, RefreshStatus::Refreshed));
    let rows = app.account_usages(Some(&out), Utc::now()).unwrap();
    let g = &row(&rows, "bob").groups[0];
    let five = g.five_hour.as_ref().unwrap();
    assert!(five.reset_at.is_none());
    assert!(g.weekly.is_none(), "a bucket with no value is not invented");
}

#[test]
fn a_refresh_skips_while_another_holds_the_refresh_lock() {
    let w = World::new();
    w.fake.ok(0.5, 0.5);
    let app = w.app().with_refresh_wait(Duration::from_millis(150));
    let held = agyva::storage::lock::StateLock::acquire(
        &app.dirs.refresh_lock_file(),
        Duration::from_millis(10),
    )
    .expect("hold the refresh lock");
    let started = Instant::now();
    let out = app.refresh_active_at(false, Utc::now()).unwrap();
    assert_eq!(reason(&out), UnavailableReason::RefreshBusy);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "it did not wait long"
    );
    assert_eq!(w.fake.calls(), 0, "agy must not run alongside the holder");
    drop(held);
    let after = app.refresh_active_at(false, Utc::now()).unwrap();
    assert!(
        matches!(after.status, RefreshStatus::Refreshed),
        "{after:?}"
    );
}

#[test]
fn a_switch_waits_for_a_running_refresh_instead_of_racing_it() {
    let w = World::new();
    let app = w
        .app()
        .with_lock_timeout(Duration::from_millis(100))
        .with_refresh_grace(Duration::from_millis(100));
    // Named (not `_`) so it stays held through the switch attempt below.
    let _held = agyva::storage::lock::StateLock::acquire(
        &app.dirs.refresh_lock_file(),
        Duration::from_millis(10),
    )
    .expect("hold the refresh lock");
    let err = app.switch("alice", false).unwrap_err();
    assert_eq!(err.kind(), agyva::core::error::ErrorKind::Locked, "{err:?}");
    // Nothing changed while the refresh lock was held: the registry still
    // names bob's login as the active account.
    let reg = app.load_registry().expect("registry");
    assert_eq!(reg.active, Some(id_of(BOB)));
}

// ------------------------------------------------- conservative classifier

fn sig(text: &str, exit: Option<i32>) -> agyva::core::health::Signals {
    agyva::core::health::Signals {
        exit_code: exit,
        timed_out: false,
        text: text.to_owned(),
    }
}

fn group(percent: f64) -> agyva::core::quota::ModelQuota {
    use agyva::core::quota::{ModelQuota, QuotaSource, QuotaWindow};
    ModelQuota {
        group: "Gemini Pro".into(),
        model: None,
        disabled: false,
        five_hour: Some(QuotaWindow {
            remaining_percent: percent,
            reset_at: None,
            observed_at: Utc::now(),
            source: QuotaSource::AgyUsageCommand,
        }),
        weekly: None,
    }
}

#[test]
fn a_number_that_merely_contains_403_is_not_access_denied() {
    use agyva::core::health::{AccountHealth, classify};
    for text in [
        "timestamp 20260101T000000.000403Z while parsing",
        "account acc_40342 failed",
        "request-id=8f403a2c-1 done in 403ms",
        "wrote 1403 bytes to cache",
    ] {
        let c = classify(&sig(text, Some(1)), None);
        assert_eq!(c.state, AccountHealth::Unknown, "{text}: {c:?}");
        assert!(
            !c.evidence.contains("access denied"),
            "{text} must not be read as a status: {c:?}"
        );
    }
}

#[test]
fn a_403_in_http_context_is_access_denied_but_not_a_suspension() {
    use agyva::core::health::{AccountHealth, classify};
    let c = classify(
        &sig("googleapi: Error 403: PERMISSION_DENIED", Some(1)),
        None,
    );
    assert_eq!(c.state, AccountHealth::Unknown);
    assert!(c.evidence.contains("access denied"), "{c:?}");
}

#[test]
fn revoked_and_unauthorized_only_match_as_whole_words() {
    use agyva::core::health::{AccountHealth, classify};
    let c = classify(
        &sig("field revoked_at=2026-01-01 unauthorized_users=3", Some(1)),
        None,
    );
    assert_eq!(c.state, AccountHealth::Unknown, "{c:?}");
    let revoked = classify(
        &sig("the access token was revoked by Google", Some(1)),
        None,
    );
    assert_eq!(revoked.state, AccountHealth::AccessRevoked);
    let login = classify(&sig("401 Unauthorized", Some(1)), None);
    assert_eq!(login.state, AccountHealth::AuthenticationRequired);
}

#[test]
fn parsed_quota_at_exit_zero_wins_over_stray_text() {
    use agyva::core::health::{AccountHealth, classify};
    // A banner or a retry log line mentioning 403/revoked must not override
    // a successful reading.
    let c = classify(
        &sig("retrying after HTTP 403; token revoked?", Some(0)),
        Some(&[group(80.0)]),
    );
    assert_eq!(c.state, AccountHealth::Healthy, "{c:?}");
}

#[test]
fn a_failed_cache_write_still_shows_what_agy_just_returned() {
    use std::os::unix::fs::PermissionsExt;
    let w = World::new();
    w.fake.ok(0.42, 0.9);
    let app = w.app();
    let cache_dir = app
        .dirs
        .quota_cache_file()
        .parent()
        .expect("cache dir")
        .to_path_buf();
    std::fs::create_dir_all(&cache_dir).expect("mkdir cache");
    let was = std::fs::metadata(&cache_dir).expect("stat").permissions();
    std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o500))
        .expect("make the cache unwritable");

    let out = app.refresh_active_at(false, Utc::now()).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert!(
            matches!(out.status, RefreshStatus::Refreshed),
            "agy answered: {out:?}"
        );
        let rows = app.account_usages(Some(&out), Utc::now()).unwrap();
        let bob = row(&rows, "bob");
        assert!(bob.refreshed_now, "the fresh reading is still fresh");
        assert_eq!(bob.freshness, Freshness::Fresh);
        assert!(!bob.groups.is_empty(), "no stale/empty cache fallback");
        let five = bob.groups[0].five_hour.as_ref().expect("5h");
        assert!((five.remaining_percent - 42.0).abs() < 1e-6);
        assert_eq!(five.source, QuotaSource::AgyUsageCommand);
    }));
    std::fs::set_permissions(&cache_dir, was).expect("restore permissions");
    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn a_lost_quota_cache_does_not_make_a_suspended_account_refreshable() {
    let w = World::new();
    w.fake.fail(
        "PERMISSION_DENIED: this account has been disabled for violating the Terms of Service",
        1,
    );
    let app = w.app();
    let first = app.refresh_active_at(false, Utc::now()).unwrap();
    assert_eq!(reason(&first), UnavailableReason::Suspended);

    // The cache is disposable by design; the suspension is not.
    std::fs::remove_file(app.dirs.quota_cache_file()).expect("delete quota cache");
    let later = Utc::now() + Cd::hours(2);
    let again = app.refresh_active_at(false, later).unwrap();
    assert_eq!(reason(&again), UnavailableReason::Suspended);
    assert_eq!(
        w.fake.calls(),
        1,
        "agy was not run against a suspended account"
    );

    // Even with no cached health at all, the row still says why.
    let rows = app.account_usages(Some(&again), later).unwrap();
    let bob = row(&rows, "bob");
    assert_eq!(bob.health.state, AccountHealth::TermsSuspended);
    assert!(
        bob.health.evidence.contains("suspended"),
        "{:?}",
        bob.health
    );

    // An explicit forced refresh that succeeds clears the marker.
    w.fake.ok(0.6, 0.6);
    let forced = app.refresh_active_at(true, later).unwrap();
    assert!(matches!(forced.status, RefreshStatus::Refreshed));
    assert_eq!(
        app.suspended_markers().get(id_of(BOB).as_str()),
        None,
        "a successful forced refresh clears the marker"
    );
}

fn process_alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| !s.rsplit(") ").next().unwrap_or("").starts_with('Z'))
        .unwrap_or(false)
}

#[test]
fn a_timeout_kills_the_whole_agy_process_group() {
    let w = World::new();
    w.fake.spawn_grandchild();
    let app = w.app().with_provider(Arc::new(AgyUsageProvider {
        bin: w.fake.bin(),
        limits: RunLimits {
            timeout: Duration::from_millis(500),
            max_bytes: 64 * 1024,
        },
    }));
    let started = Instant::now();
    let out = app.refresh_active_at(false, Utc::now()).unwrap();
    assert_eq!(reason(&out), UnavailableReason::TimedOut);
    assert!(started.elapsed() < Duration::from_secs(10), "it gave up");
    let pid = (0..40)
        .find_map(|_| {
            std::thread::sleep(Duration::from_millis(50));
            w.fake.grandchild_pid()
        })
        .expect("the grandchild recorded its pid");
    let mut alive_for = Duration::ZERO;
    while process_alive(pid) && alive_for < Duration::from_secs(3) {
        std::thread::sleep(Duration::from_millis(50));
        alive_for += Duration::from_millis(50);
    }
    assert!(
        !process_alive(pid),
        "the grandchild (pid {pid}) outlived the timeout"
    );
}

#[test]
fn hostile_group_names_are_cleaned_before_they_are_shown() {
    use agyva::agy::usage_parse::parse_usage;
    let hostile = "Gemini\u{1b}[2J Pro\u{7}\u{0}\n".to_owned() + &"x".repeat(200);
    let json = format!(
        r#"{{"command":{{"data":{{"groups":[
{{"name":{name},"buckets":[{{"id":"g5","window":"5h","remaining_fraction":0.5}}]}}]}}}}}}"#,
        name = serde_json::to_string(&hostile).expect("json")
    );
    let parsed = parse_usage(&json, Utc::now()).expect("parses");
    for g in &parsed.groups {
        let label = g.label();
        assert!(
            !label.contains('\u{1b}') && !label.contains('\u{7}'),
            "{label:?}"
        );
        assert!(
            !label.contains('\u{0}') && !label.contains('\n'),
            "{label:?}"
        );
        assert!(label.chars().count() <= 64, "capped: {}", label.len());
    }
}

#[test]
fn an_all_disabled_reply_is_not_healthy() {
    use agyva::core::health::{AccountHealth, classify};
    let mut g = group(100.0);
    g.disabled = true;
    let c = classify(&sig("", Some(0)), Some(&[g]));
    assert_eq!(c.state, AccountHealth::Unknown, "{c:?}");
    assert!(c.evidence.contains("none is enabled"), "{c:?}");
}
