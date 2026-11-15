//! Usage, health and watch through the real binary, against a synthetic fake
//! `agy`. Nothing touches the OS keyring, the real HOME or a real agy.

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use agyva::testing::credential;
use common::fake_agy::FakeAgy;
use tempfile::TempDir;

struct Sandbox {
    tmp: TempDir,
    fake: FakeAgy,
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        for d in ["home", "agyhome"] {
            std::fs::create_dir_all(tmp.path().join(d)).expect("mkdir");
        }
        let fake = FakeAgy::install(&tmp.path().join("fakeagy"));
        Self { tmp, fake }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn login(&self, sub: &str, email: &str, tag: &str) {
        std::fs::write(
            self.p("agyhome/antigravity-oauth-token"),
            credential(sub, email, tag),
        )
        .expect("login");
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agyva"));
        c.args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.p("home"))
            .env("AGYVA_CONFIG_DIR", self.p("cfg"))
            .env("AGYVA_STATE_DIR", self.p("state"))
            .env("AGYVA_CACHE_DIR", self.p("cache"))
            .env("AGYVA_AGY_HOME", self.p("agyhome"))
            .env("AGYVA_AGY_BIN", self.fake.bin())
            .env("AGYVA_BACKEND", "file")
            .env("AGYVA_PASSPHRASE", "synthetic-e2e-passphrase")
            .env("NO_COLOR", "1")
            .env("AGYVA_UPDATE_CHECK", "0")
            .stdin(Stdio::null());
        c
    }

    fn run(&self, args: &[&str]) -> Run {
        let o = self.command(args).output().expect("run agyva");
        Run {
            code: o.status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&o.stdout).into_owned(),
            err: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }

    fn with_account(&self) {
        self.login("sub-alice", "alice@example.invalid", "alice");
        let r = self.run(&["add", "--alias", "alice"]);
        assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    }
}

fn json(r: &Run) -> serde_json::Value {
    serde_json::from_str(&r.out).unwrap_or_else(|e| panic!("not JSON ({e}): {}", r.out))
}

fn all_files(p: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(p) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            all_files(&path, out);
        } else {
            out.push(path);
        }
    }
}

#[test]
fn usage_json_labels_source_and_freshness_and_leaks_nothing() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.ok(0.72, 0.4);

    let r = sb.run(&["usage", "--json"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    let v = json(&r);
    assert_eq!(v["schema_version"], 1);
    let a = &v["accounts"][0];
    assert_eq!(a["alias"], "alice");
    assert_eq!(a["freshness"], "fresh");
    assert_eq!(a["refreshed_now"], true);
    assert_eq!(a["groups"][0]["group"], "Gemini Pro");
    assert_eq!(a["groups"][0]["five_hour"]["source"], "agy_usage_command");
    assert!(
        (a["groups"][0]["five_hour"]["remaining_percent"]
            .as_f64()
            .unwrap()
            - 72.0)
            .abs()
            < 1e-6
    );
    assert_eq!(a["groups"][1]["disabled"], true);
    assert_eq!(a["health"]["state"], "healthy");
    assert_eq!(a["health"]["verified_now"], true);

    // No secret ever reaches the output or the cache.
    let cache_text = std::fs::read_to_string(sb.p("cache/quota.json")).expect("cache written");
    for text in [&r.out, &r.err, &cache_text] {
        assert!(!text.contains("access-alice") && !text.contains("refresh-alice"));
    }
    assert!(
        !cache_text.contains("alice@example.invalid"),
        "the cache carries no email"
    );
    let mut files = Vec::new();
    all_files(&sb.p("cache"), &mut files);
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(sb.p("cache/quota.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);

    // Within the minimum interval the cache answers and agy is left alone.
    let calls = sb.fake.calls();
    let again = sb.run(&["usage", "--json"]);
    assert_eq!(again.code, 0);
    assert_eq!(sb.fake.calls(), calls, "throttled");
    let a2 = &json(&again)["accounts"][0];
    assert_eq!(a2["refreshed_now"], false);
    assert_eq!(a2["groups"][0]["five_hour"]["source"], "cache");

    // Human output states what it is showing.
    let human = sb.run(&["usage", "--no-refresh"]);
    assert_eq!(human.code, 0);
    assert!(
        human.out.contains("alice") && human.out.contains("Gemini Pro"),
        "{}",
        human.out
    );
}

#[test]
fn health_reports_a_suspended_fixture_and_never_retries_it() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.fail(
        "PERMISSION_DENIED: this account is disabled for a violation of the Terms of Service",
        1,
    );

    let r = sb.run(&["health", "--json"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    let v = json(&r);
    let h = &v["accounts"][0]["health"];
    assert_eq!(h["state"], "terms_suspended");
    assert!(
        h["warning"]
            .as_str()
            .unwrap()
            .contains("https://antigravity.google/docs/faq")
    );
    assert_eq!(sb.fake.calls(), 1);

    let human = sb.run(&["health", "--no-refresh"]);
    assert!(
        human.out.contains("TermsSuspended") || human.out.to_lowercase().contains("suspended"),
        "{}",
        human.out
    );
    assert!(
        human.out.contains("antigravity.google/docs/faq"),
        "{}",
        human.out
    );
    assert!(!human.out.to_lowercase().contains("healthy"));

    // Neither usage, health, nor watch asks agy again.
    sb.fake.ok(0.9, 0.9);
    let _ = sb.run(&["usage"]);
    let _ = sb.run(&["health"]);
    let w = sb
        .command(&["watch"])
        .env("AGYVA_WATCH_MAX_CYCLES", "1")
        .output()
        .expect("watch");
    assert_eq!(w.status.code(), Some(0));
    assert_eq!(sb.fake.calls(), 1, "a suspended account is not retried");
    assert!(
        String::from_utf8_lossy(&w.stdout)
            .to_lowercase()
            .contains("suspended")
    );

    // Only an explicit --force-refresh looks again, once.
    let forced = sb.run(&["health", "--force-refresh", "--json"]);
    assert_eq!(forced.code, 0);
    assert_eq!(sb.fake.calls(), 2);
    assert_eq!(json(&forced)["accounts"][0]["health"]["state"], "healthy");
}

#[test]
fn bare_403_is_not_a_suspension() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.fail("HTTP 403 Forbidden", 1);
    let r = sb.run(&["health", "--json"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    assert_ne!(
        json(&r)["accounts"][0]["health"]["state"],
        "terms_suspended"
    );
}

#[test]
fn unrecognized_output_is_unavailable_not_guessed() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.stdout("{\"hello\": \"world\"}");
    let r = sb.run(&["usage", "--json"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    let a = &json(&r)["accounts"][0];
    assert_eq!(a["unavailable"]["reason"], "schema_changed");
    assert!(a["groups"].as_array().unwrap().is_empty());
}

#[test]
fn watch_runs_bounded_cycles_and_exits_cleanly() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.ok(0.5, 0.5);
    let t = Instant::now();
    let w = sb
        .command(&["watch"])
        .env("AGYVA_WATCH_MAX_CYCLES", "1")
        .output()
        .expect("watch");
    assert_eq!(
        w.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&w.stderr)
    );
    assert!(t.elapsed() < Duration::from_secs(20));
    let out = String::from_utf8_lossy(&w.stdout);
    assert!(out.contains("alice"), "{out}");
}

#[test]
fn cached_health_is_marked_unverified_only_once() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.ok(0.5, 0.5);
    assert_eq!(sb.run(&["health"]).code, 0);
    let cached = sb.run(&["health", "--no-refresh"]);
    assert_eq!(cached.code, 0, "{}{}", cached.out, cached.err);
    assert!(cached.out.contains("not verified now"), "{}", cached.out);
    assert_eq!(
        cached.out.matches("not verified now").count(),
        1,
        "the table must not say it twice: {}",
        cached.out
    );
}

#[test]
fn a_recent_refresh_shows_the_cached_reading_as_a_note() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.ok(0.5, 0.5);
    let first = sb.run(&["usage"]);
    assert_eq!(first.code, 0, "{}{}", first.out, first.err);
    assert_eq!(sb.fake.calls(), 1);

    let second = sb.run(&["usage"]);
    assert_eq!(second.code, 0, "{}{}", second.out, second.err);
    assert_eq!(sb.fake.calls(), 1, "the minimum refresh window holds");
    assert!(
        second
            .out
            .contains("Note: using cached reading (refreshed 0s ago)")
            || second
                .out
                .contains("Note: using cached reading (refreshed 1s ago)"),
        "{}",
        second.out
    );
    assert!(!second.out.contains("Unavailable:"), "{}", second.out);
    assert!(
        second.out.contains("source: cache"),
        "the data is still shown: {}",
        second.out
    );
}

#[test]
fn watch_runs_several_quick_cycles_in_test_mode() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.ok(0.5, 0.5);
    let t = Instant::now();
    let w = sb
        .command(&["watch"])
        .env("AGYVA_TEST_MODE", "1")
        .env("AGYVA_WATCH_INTERVAL_MS", "20")
        .env("AGYVA_WATCH_MAX_CYCLES", "3")
        .output()
        .expect("watch");
    assert_eq!(
        w.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&w.stderr)
    );
    // The 60-second minimum would make this take minutes; test mode may
    // shorten it so a multi-cycle run stays fast.
    assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed());
    let out = String::from_utf8_lossy(&w.stdout);
    assert!(out.contains("cycle 3"), "{out}");
    assert!(out.contains("stopped after 3 cycle(s)"), "{out}");

    // Without test mode the variable is ignored: the configured 300s interval
    // still applies, but one bounded cycle exits before it sleeps.
    let ignored = sb
        .command(&["watch"])
        .env("AGYVA_WATCH_INTERVAL_MS", "20")
        .env("AGYVA_WATCH_MAX_CYCLES", "1")
        .output()
        .expect("watch");
    assert_eq!(ignored.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&ignored.stderr).contains("AGYVA_TEST_MODE=1"),
        "{}",
        String::from_utf8_lossy(&ignored.stderr)
    );
}

#[test]
fn watch_stops_on_sigint() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.ok(0.5, 0.5);
    let mut child = sb
        .command(&["watch"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn watch");
    // Let the first cycle finish and the loop settle into its wait.
    std::thread::sleep(Duration::from_millis(1500));
    assert!(child.try_wait().expect("poll").is_none(), "still watching");
    let st = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .expect("kill");
    assert!(st.success());
    let deadline = Instant::now() + Duration::from_secs(10);
    let code = loop {
        if let Some(s) = child.try_wait().expect("poll") {
            break s.code();
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("watch ignored SIGINT");
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(code, Some(0), "clean exit on Ctrl+C");
}

/// A reader that closes its end of the pipe (`| head -3`) must end the
/// foreground loop instead of leaving it spinning with nowhere to write.
#[test]
fn monitor_and_watch_exit_cleanly_when_their_reader_closes() {
    use std::io::Read as _;
    for cmd in ["monitor", "watch"] {
        let sb = Sandbox::new();
        sb.with_account();
        sb.fake.ok(0.6, 0.6);
        let mut child = sb
            .command(&[cmd])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| panic!("spawn {cmd}: {e}"));
        // Read a little, then drop our end: the process must not hang.
        let mut stdout = child.stdout.take().expect("piped stdout");
        let mut buf = [0u8; 256];
        let got = stdout.read(&mut buf).expect("read start banner");
        assert!(got > 0, "{cmd} wrote nothing");
        drop(stdout);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().expect("poll") {
                assert_eq!(status.code(), Some(0), "{cmd} exited uncleanly");
                break;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("{cmd} kept running after its stdout reader closed");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[test]
fn doctor_and_status_cover_agy_and_the_quota_provider() {
    let sb = Sandbox::new();
    sb.with_account();
    let s = sb.run(&["status", "--json"]);
    assert_eq!(s.code, 0, "{}{}", s.out, s.err);
    let v = json(&s);
    assert!(v["agy_path"].as_str().unwrap().ends_with("fakeagy/agy"));
    assert!(v["quota_provider"].as_str().unwrap().contains("available"));

    let human = sb.run(&["doctor"]);
    assert_eq!(human.code, 0, "{}{}", human.out, human.err);
    assert!(
        human.out.contains("checkpoints: 0 of max 5 kept"),
        "{}",
        human.out
    );

    let d = sb.run(&["doctor", "--json"]);
    assert_eq!(d.code, 0, "{}{}", d.out, d.err);
    let names: Vec<String> = json(&d)["checks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap().to_owned())
        .collect();
    for want in ["agy", "agy-path", "keyring", "quota-provider", "journal"] {
        assert!(names.iter().any(|n| n == want), "missing {want}: {names:?}");
    }

    // With no agy at all, the provider is reported unavailable (not an error).
    let o = sb
        .command(&["doctor"])
        .env("AGYVA_AGY_BIN", sb.p("nonexistent/agy"))
        .output()
        .expect("doctor");
    assert!(String::from_utf8_lossy(&o.stdout).contains("quota-provider"));
}

#[test]
fn route_plan_and_inspect_json_expose_the_documented_shape() {
    let sb = Sandbox::new();
    sb.with_account();
    sb.fake.ok(0.72, 0.4);
    std::fs::write(
        sb.p("cfg/routing.toml"),
        "[groups.work]\naccounts = [\"alice\"]\n",
    )
    .expect("routing.toml");

    // Route planning is cache-only, so warm the cache with a real run
    // first; the plan itself never spawns agy.
    let warm = sb.run(&["usage"]);
    assert_eq!(warm.code, 0, "{}{}", warm.out, warm.err);
    let r = sb.run(&["route", "plan", "work", "--json"]);
    assert_eq!(r.code, 0, "{}{}", r.out, r.err);
    let v = json(&r);
    assert_eq!(v["schema_version"], 1);
    assert!(
        v["generated_at"].as_str().is_some(),
        "generated_at must be a timestamp"
    );
    let rec = v["recommendation"].as_str().expect("recommendation id");
    let rows = v["accounts"].as_array().expect("accounts array");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(rec, row["id"], "recommendation names the top row");
    for key in [
        "id",
        "alias",
        "active",
        "freshness",
        "eligible",
        "remaining_percent",
        "rate_remaining_percent",
        "renewals",
        "why",
        "command",
    ] {
        assert!(row.get(key).is_some(), "missing key {key}: {row}");
    }
    assert_eq!(row["alias"], "alice");
    assert_eq!(row["eligible"], true);
    assert_eq!(row["freshness"], "fresh");
    // The headline share is the fullest window (weekly 40), the rate
    // window carries the 5h 72.
    assert!((row["remaining_percent"].as_f64().unwrap() - 40.0).abs() < 1e-6);
    assert!((row["rate_remaining_percent"].as_f64().unwrap() - 72.0).abs() < 1e-6);
    assert_eq!(row["renewals"].as_array().unwrap().len(), 2);

    // inspect shares the plan shape for a named group.
    let i = sb.run(&["route", "inspect", "work", "--json"]);
    assert_eq!(i.code, 0, "{}{}", i.out, i.err);
    let iv = json(&i);
    assert_eq!(iv["schema_version"], 1);
    assert!(iv["accounts"].as_array().is_some());

    // Human output for the same run stays plain and starred.
    let human = sb.run(&["route", "plan", "work"]);
    assert_eq!(human.code, 0);
    assert!(human.out.contains("* alice"), "{}", human.out);

    // Age the cache past quota_fresh_secs: the row is still listed with
    // its reason, but its share must not read as available quota.
    let cache = sb.p("cache/quota.json");
    let mut doc: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cache).expect("cache")).expect("json");
    let old_ts = "2020-01-01T00:00:00.000000Z";
    for acct in doc["accounts"]
        .as_object_mut()
        .expect("accounts")
        .values_mut()
    {
        if let Some(q) = acct.get_mut("quota") {
            q["observed_at"] = old_ts.into();
            for g in q["groups"].as_array_mut().into_iter().flatten() {
                for w in ["five_hour", "weekly"] {
                    if !g[w].is_null() {
                        g[w]["observed_at"] = old_ts.into();
                    }
                }
            }
        }
    }
    std::fs::write(&cache, serde_json::to_string(&doc).expect("json")).expect("write cache");
    let stale = sb.run(&["route", "plan", "work"]);
    assert_eq!(stale.code, 0, "no eligible account is not an error");
    assert!(
        !stale.out.contains("72%") && !stale.out.contains("40%"),
        "a stale row must not advertise a share: {}",
        stale.out
    );
    assert!(
        stale.out.contains("No eligible account in the cache."),
        "{}",
        stale.out
    );
    let sj = json(&sb.run(&["route", "plan", "work", "--json"]));
    assert!(sj["recommendation"].is_null());
    assert_eq!(sj["accounts"][0]["eligible"], false);
    assert!(sj["accounts"][0]["remaining_percent"].as_f64().is_some());
}
