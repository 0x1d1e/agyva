//! Binary-level scenarios against a tempdir sandbox. `AGYVA_BACKEND=file`
//! keeps the OS keyring out of the picture; logins are synthetic files.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use agyva::testing::{credential, secrets_for};
use tempfile::TempDir;

/// Value for [`Sandbox::cmd`] extras meaning "remove this variable".
const UNSET: &str = "<unset>";

struct Sandbox {
    tmp: TempDir,
}

struct Run {
    code: i32,
    out: String,
    err: String,
}

impl Run {
    fn all(&self) -> String {
        format!("{}{}", self.out, self.err)
    }
}

impl Sandbox {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        for d in ["home", "agyhome"] {
            std::fs::create_dir_all(tmp.path().join(d)).expect("mkdir");
        }
        let agy = tmp.path().join("agy");
        std::fs::write(&agy, "#!/bin/sh\necho 'agy 1.5.9'\n").expect("fake agy");
        std::fs::set_permissions(&agy, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        Self { tmp }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn token_file(&self) -> PathBuf {
        self.p("agyhome/antigravity-oauth-token")
    }

    fn login(&self, sub: &str, email: &str, tag: &str) {
        std::fs::write(self.token_file(), credential(sub, email, tag)).expect("login");
    }

    /// Store an arbitrary synthetic live credential.
    fn login_raw(&self, raw: Vec<u8>) {
        std::fs::write(self.token_file(), raw).expect("login");
    }

    fn live_email(&self) -> String {
        let text = std::fs::read_to_string(self.token_file()).unwrap_or_default();
        ["alice", "bob"]
            .into_iter()
            .find(|t| text.contains(&format!("access-{t}")))
            .unwrap_or("none")
            .to_owned()
    }

    fn cmd(&self, args: &[&str], extra: &[(&str, &str)]) -> Run {
        let mut c = Command::new(env!("CARGO_BIN_EXE_agyva"));
        c.args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.p("home"))
            .env("AGYVA_CONFIG_DIR", self.p("cfg"))
            .env("AGYVA_STATE_DIR", self.p("state"))
            .env("AGYVA_CACHE_DIR", self.p("cache"))
            .env("AGYVA_AGY_HOME", self.p("agyhome"))
            .env("AGYVA_AGY_BIN", self.p("agy"))
            .env("AGYVA_BACKEND", "file")
            .env("AGYVA_TEST_MODE", "1")
            .env("AGYVA_PASSPHRASE", "synthetic-e2e-passphrase")
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null());
        for (k, v) in extra {
            if *v == UNSET {
                c.env_remove(k);
            } else {
                c.env(k, v);
            }
        }
        let Output {
            status,
            stdout,
            stderr,
        } = c.output().expect("run agyva");
        Run {
            code: status.code().unwrap_or(-1),
            out: String::from_utf8_lossy(&stdout).into_owned(),
            err: String::from_utf8_lossy(&stderr).into_owned(),
        }
    }

    fn run(&self, args: &[&str]) -> Run {
        self.cmd(args, &[])
    }
}

fn files_under(p: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(p) else { return };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            files_under(&path, out)
        } else {
            out.push(path)
        }
    }
}

#[test]
fn full_account_lifecycle_through_the_binary() {
    let sb = Sandbox::new();

    // Not logged in yet.
    assert_eq!(sb.run(&["current"]).out.trim(), "agy is not logged in.");
    assert_eq!(sb.run(&["list"]).code, 0);

    sb.login("sub-alice", "alice@example.invalid", "alice");
    let add = sb.run(&["add"]);
    assert_eq!(add.code, 0, "{}", add.all());
    assert!(add.out.contains("Saved alice"), "{}", add.out);
    assert!(
        add.err.contains("Policy:"),
        "first add prints the policy notice"
    );

    sb.login("sub-bob", "bob@example.invalid", "bob");
    let add = sb.run(&["add", "--alias", "robert"]);
    assert_eq!(add.code, 0, "{}", add.all());
    assert!(!add.all().contains("Policy:"), "policy notice only once");

    let dup = sb.run(&["add"]);
    assert_eq!(dup.code, 1);
    assert!(
        dup.err.contains("error:") && dup.err.contains("hint:"),
        "{}",
        dup.err
    );

    let list = sb.run(&["list", "--json"]);
    let v: serde_json::Value = serde_json::from_str(&list.out).expect("json");
    assert_eq!(v["schema_version"], 1);
    assert_eq!(v["accounts"].as_array().map(Vec::len), Some(2));

    // use / use -
    let u = sb.run(&["use", "alice"]);
    assert_eq!(u.code, 0, "{}", u.all());
    assert!(u.err.contains("Restart agy"), "{}", u.err);
    assert_eq!(sb.live_email(), "alice");
    assert_eq!(sb.run(&["use", "-"]).code, 0);
    assert_eq!(sb.live_email(), "bob");
    assert_eq!(sb.run(&["use", "-"]).code, 0);
    assert_eq!(sb.live_email(), "alice");
    let cur: serde_json::Value =
        serde_json::from_str(&sb.run(&["current", "--json"]).out).expect("json");
    assert_eq!(cur["account"]["alias"], "alice");

    // rename
    assert_eq!(sb.run(&["rename", "robert", "bobby"]).code, 0);
    assert_eq!(sb.run(&["use", "bobby"]).code, 0);
    assert_eq!(sb.live_email(), "bob");

    // remove: needs confirmation without a TTY, refuses the active account
    assert_eq!(sb.run(&["remove", "alice"]).code, 2);
    let r = sb.run(&["remove", "bobby", "--yes"]);
    assert_eq!(r.code, 1, "active account refused");
    assert!(r.err.contains("hint:"));
    assert_eq!(sb.run(&["remove", "alice", "--yes"]).code, 0);
    assert_eq!(sb.live_email(), "bob", "native login untouched by remove");
    assert_eq!(
        sb.run(&["use", "alice"]).code,
        2,
        "a selector that matches nothing is a usage error"
    );

    // Nothing leaked anywhere, and the real HOME stayed empty.
    let mut files = Vec::new();
    files_under(sb.tmp.path(), &mut files);
    for f in files.iter().filter(|f| *f != &sb.token_file()) {
        let text = String::from_utf8_lossy(&std::fs::read(f).expect("read")).into_owned();
        for t in ["alice", "bob"] {
            for s in secrets_for(t) {
                assert!(!text.contains(&s), "{} contains a secret", f.display());
            }
        }
    }
    assert_eq!(
        std::fs::read_dir(sb.p("home")).expect("home").count(),
        0,
        "HOME untouched"
    );
}

#[test]
fn a_consumer_login_without_an_id_token_can_be_added() {
    let sb = Sandbox::new();
    sb.login_raw(agyva::testing::consumer_credential(
        "First.Last@Example.Invalid",
        "realish",
    ));
    let add = sb.run(&["add", "--alias", "main"]);
    assert_eq!(add.code, 0, "{}", add.all());
    assert!(
        add.out.contains("First.Last@Example.Invalid"),
        "{}",
        add.out
    );
    let list = sb.run(&["list", "--json"]);
    assert_eq!(list.code, 0, "{}", list.all());
    let v: serde_json::Value = serde_json::from_str(&list.out).expect("json");
    assert_eq!(v["accounts"][0]["alias"], "main");
    assert_eq!(v["accounts"][0]["email"], "First.Last@Example.Invalid");
    assert_eq!(v["accounts"][0]["id"].as_str().unwrap().len(), 10);
    let cur = sb.run(&["current", "--json"]);
    assert_eq!(cur.code, 0, "{}", cur.all());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&cur.out).unwrap()["account"]["alias"],
        "main"
    );
}

#[test]
fn interrupted_switch_is_detected_blocked_and_recovered() {
    let sb = Sandbox::new();
    sb.login("sub-alice", "alice@example.invalid", "alice");
    assert_eq!(sb.run(&["add"]).code, 0);
    sb.login("sub-bob", "bob@example.invalid", "bob");
    assert_eq!(sb.run(&["add"]).code, 0);

    // Crash mid-switch (credential already replaced): exit 99, no cleanup.
    let crash = sb.cmd(&["use", "alice"], &[("AGYVA_FAILPOINT", "after-companion")]);
    assert_eq!(crash.code, 99, "{}", crash.all());
    assert_eq!(
        sb.live_email(),
        "alice",
        "native credential was replaced before the crash"
    );

    let status = sb.run(&["status"]);
    assert_eq!(status.code, 3);
    assert!(status.out.contains("RECOVERY REQUIRED"));
    let status_json: serde_json::Value =
        serde_json::from_str(&sb.run(&["status", "--json"]).out).expect("json");
    assert!(status_json["recovery_pending"].is_object());
    assert_eq!(sb.run(&["doctor"]).code, 3);
    let blocked = sb.run(&["use", "bob"]);
    assert_eq!(blocked.code, 3);
    assert!(blocked.err.contains("hint:") && blocked.err.contains("agyva recover"));
    assert_eq!(sb.run(&["rename", "alice", "al"]).code, 3);
    assert_eq!(sb.run(&["list"]).code, 0, "read-only commands still work");

    let rec = sb.run(&["recover"]);
    assert_eq!(rec.code, 0, "{}", rec.all());
    assert_eq!(sb.live_email(), "bob", "previous login restored");
    assert_eq!(sb.run(&["status"]).code, 0);
    assert_eq!(sb.run(&["recover"]).out.trim(), "Nothing to recover.");
    assert_eq!(sb.run(&["use", "alice"]).code, 0);

    // Crash after verification rolls forward instead.
    let crash = sb.cmd(
        &["use", "bob"],
        &[("AGYVA_FAILPOINT", "after-verified:crash")],
    );
    assert_eq!(crash.code, 99);
    let rec = sb.run(&["recover"]);
    assert!(rec.out.contains("Completed"), "{}", rec.all());
    assert!(
        sb.run(&["current", "--json"])
            .out
            .contains("\"alias\": \"bob\"")
    );

    // An ordinary failure rolls back by itself.
    let fail = sb.cmd(&["use", "alice"], &[("AGYVA_FAILPOINT", "verify:error")]);
    assert_eq!(fail.code, 1);
    assert_eq!(sb.live_email(), "bob");
    assert_eq!(sb.run(&["status"]).code, 0);
}

#[test]
fn unknown_failpoint_and_bad_usage_are_clean() {
    let sb = Sandbox::new();
    let r = sb.cmd(&["list"], &[("AGYVA_FAILPOINT", "nope")]);
    assert_eq!(r.code, 2);
    assert!(r.err.contains("known failpoints"));
    assert_eq!(sb.run(&["frobnicate"]).code, 2);
    assert_eq!(sb.run(&["use"]).code, 2);
}

#[test]
fn help_completions_config_and_version() {
    let sb = Sandbox::new();
    let help = sb.run(&[]);
    assert_eq!(help.code, 0);
    assert!(help.out.contains("Usage: agyva") && help.out.contains("Exit codes"));
    for task in [
        "Accounts:",
        "Quota and health:",
        "Diagnostics and recovery:",
        "Setup:",
        "Examples:",
        "agyva use ",
    ] {
        assert!(help.out.contains(task), "missing {task}: {}", help.out);
    }
    assert!(!help.out.contains('\u{1b}'));
    for shell in ["fish", "bash", "zsh"] {
        let r = sb.run(&["completions", shell]);
        assert_eq!(r.code, 0);
        assert!(r.out.contains("agyva"), "{shell}");
        // Only Fish completes saved aliases dynamically.
        assert_eq!(
            r.out.contains("__agyva_aliases"),
            shell == "fish",
            "{shell}"
        );
    }
    let unknown = sb.run(&["use", "nobody"]);
    assert_eq!(
        unknown.code,
        2,
        "a bad selector is a usage error: {}",
        unknown.all()
    );
    assert!(unknown.err.contains("error:") && unknown.err.contains("hint:"));
    assert!(sb.run(&["version"]).out.starts_with("agyva 0.1.0"));
    assert!(
        sb.run(&["config", "path"])
            .out
            .trim()
            .ends_with("config.toml")
    );
    assert_eq!(sb.run(&["config", "set", "color", "never"]).code, 0);
    assert!(sb.run(&["config", "show"]).out.contains("color = never"));
    assert_eq!(
        sb.run(&["config", "set", "color", "purple"]).code,
        2,
        "invalid value is a usage error"
    );
    let mem = sb.run(&["config", "set", "backend", "memory"]);
    assert_eq!(mem.code, 2, "{}", mem.all());
    assert!(mem.err.contains("test-only"), "{}", mem.err);
    assert!(mem.err.contains("AGYVA_BACKEND=memory"), "{}", mem.err);
}

#[test]
fn untested_agy_versions_need_an_explicit_flag() {
    let sb = Sandbox::new();
    std::fs::write(sb.p("agy"), "#!/bin/sh\necho 'agy 9.9.9'\n").expect("fake agy");
    sb.login("sub-alice", "alice@example.invalid", "alice");
    let r = sb.run(&["add"]);
    assert_ne!(r.code, 0, "{}", r.all());
    assert!(r.err.contains("hint:"));
    assert_eq!(sb.run(&["--allow-untested", "add"]).code, 0);
}

// ---------------------------------------------------------------- round 3

fn two_saved(sb: &Sandbox) {
    sb.login("sub-alice", "alice@example.invalid", "alice");
    assert_eq!(sb.run(&["add"]).code, 0);
    sb.login("sub-bob", "bob@example.invalid", "bob");
    assert_eq!(sb.run(&["add"]).code, 0);
}

/// Fix 5: the sandbox hook only works when every location is redirected.
#[test]
fn backend_hook_needs_every_sandbox_directory() {
    for missing in [
        "AGYVA_AGY_HOME",
        "AGYVA_STATE_DIR",
        "AGYVA_CONFIG_DIR",
        "AGYVA_CACHE_DIR",
    ] {
        let sb = Sandbox::new();
        let r = sb.cmd(&["list"], &[(missing, UNSET)]);
        assert_eq!(r.code, 2, "{missing}: {}", r.all());
        assert!(
            r.err.contains(missing) && r.err.contains("sandbox hook") && r.err.contains("hint:"),
            "{missing}: {}",
            r.err
        );
        assert!(
            !sb.p("state").exists() && !sb.p("cfg").exists() && !sb.p("cache").exists(),
            "{missing}: nothing was created before the check"
        );
    }
}

/// Fix 6: without AGYVA_TEST_MODE=1 a failpoint is ignored, with one warning.
#[test]
fn failpoint_is_ignored_with_one_warning_outside_test_mode() {
    let sb = Sandbox::new();
    two_saved(&sb);
    let r = sb.cmd(
        &["use", "alice"],
        &[
            ("AGYVA_FAILPOINT", "after-companion"),
            ("AGYVA_TEST_MODE", UNSET),
        ],
    );
    assert_eq!(r.code, 0, "{}", r.all());
    assert_eq!(sb.live_email(), "alice", "the switch ran normally");
    assert_eq!(
        r.err.matches("ignoring AGYVA_FAILPOINT").count(),
        1,
        "{}",
        r.err
    );
    assert!(r.err.contains("AGYVA_TEST_MODE=1"));
    assert_eq!(sb.run(&["status"]).code, 0, "no journal left");
}

/// Fix 7 through the binary: cleanup trouble after a committed switch is a
/// note on stderr and exit 0.
#[test]
fn post_commit_cleanup_failure_exits_zero_with_a_note() {
    let sb = Sandbox::new();
    two_saved(&sb);
    let r = sb.cmd(
        &["use", "alice"],
        &[("AGYVA_FAILPOINT", "post-commit-cleanup:error")],
    );
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(r.out.contains("Switched to"), "{}", r.out);
    assert!(
        r.err.contains("note:") && r.err.contains("cleanup failed"),
        "{}",
        r.err
    );
    assert_eq!(sb.live_email(), "alice");
    assert_eq!(sb.run(&["status"]).code, 0);
}

/// Fix 1 through the binary: --force over an unsaved login says where it is
/// kept and that it is not pruned.
#[test]
fn force_switch_reports_where_the_old_login_is_kept() {
    let sb = Sandbox::new();
    two_saved(&sb);
    std::fs::write(
        sb.token_file(),
        credential("sub-carol", "carol@example.invalid", "carol"),
    )
    .expect("login carol");
    let refused = sb.run(&["use", "alice"]);
    assert_eq!(refused.code, 1, "{}", refused.all());
    assert!(refused.err.contains("not saved"), "{}", refused.err);

    let r = sb.run(&["use", "alice", "--force"]);
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(
        r.err.contains("NOT pruned automatically") && r.err.contains(".keep.ckpt"),
        "{}",
        r.err
    );
    let mut files = Vec::new();
    files_under(&sb.p("state"), &mut files);
    assert!(
        files
            .iter()
            .any(|f| f.to_string_lossy().ends_with(".keep.ckpt")),
        "{files:?}"
    );
}

/// Fix 2 through the binary: recover will not clobber an unrelated login
/// without --force.
#[test]
fn recover_refuses_over_an_unrelated_login_and_force_keeps_a_copy() {
    let sb = Sandbox::new();
    two_saved(&sb); // bob live
    let crash = sb.cmd(&["use", "alice"], &[("AGYVA_FAILPOINT", "after-companion")]);
    assert_eq!(crash.code, 99);
    std::fs::write(
        sb.token_file(),
        credential("sub-carol", "carol@example.invalid", "carol"),
    )
    .expect("login carol");

    let r = sb.run(&["recover"]);
    assert_eq!(r.code, 1, "{}", r.all());
    assert!(r.err.contains("recover --force"), "{}", r.err);
    assert_eq!(sb.run(&["status"]).code, 3, "still pending");

    let r = sb.run(&["recover", "--force"]);
    assert_eq!(r.code, 0, "{}", r.all());
    assert!(r.out.contains("NOT pruned automatically"), "{}", r.out);
    assert_eq!(sb.live_email(), "bob");
    assert_eq!(sb.run(&["status"]).code, 0);
}

/// `agyva completions fish | head` closes the pipe early; that must end
/// quietly, not with a panic.
#[test]
fn a_closed_output_pipe_is_not_a_crash() {
    let sb = Sandbox::new();
    let mut child = Command::new(env!("CARGO_BIN_EXE_agyva"))
        .args(["completions", "fish"])
        .env_clear()
        .env("HOME", sb.p("home"))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .expect("spawn");
    drop(child.stdout.take()); // the reader goes away immediately
    let out = child.wait_with_output().expect("wait");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(!err.contains("panicked"), "{err}");
    assert_eq!(out.status.code(), Some(0), "{err}");
}

/// Smoke 9: the documented table says bad selectors are usage errors (2).
#[test]
fn a_missing_or_ambiguous_selector_exits_two() {
    let sb = Sandbox::new();
    two_saved(&sb);
    let missing = sb.run(&["use", "nobody"]);
    assert_eq!(missing.code, 2, "{}", missing.all());
    assert!(!missing.err.is_empty(), "{}", missing.all());
    assert!(missing.err.contains("hint:"), "{}", missing.err);

    // Two ids that share their first hex digit (synthetic subjects chosen so
    // the derived ids collide there); a one-character query is ambiguous.
    let amb = Sandbox::new();
    for (sub, email, alias) in [
        ("sub-x0", "x0@example.invalid", "one"),
        ("sub-x3", "x3@example.invalid", "two"),
    ] {
        amb.login(sub, email, alias);
        assert_eq!(amb.run(&["add", "--alias", alias]).code, 0);
    }
    let both = amb.run(&["use", "3"]);
    assert_eq!(both.code, 2, "{}", both.all());
    assert!(both.err.contains("matches several"), "{}", both.err);
    assert!(both.err.contains("hint:"), "{}", both.err);
}

#[test]
fn redirected_human_and_json_output_stay_plain_with_color_always() {
    let sb = Sandbox::new();
    sb.login("sub-stream", "stream@example.invalid", "stream");
    assert_eq!(sb.run(&["add", "--alias", "stream"]).code, 0);
    assert_eq!(sb.run(&["config", "set", "color", "always"]).code, 0);
    for args in [&["list"][..], &["list", "--json"][..]] {
        let result = sb.cmd(args, &[("NO_COLOR", UNSET)]);
        assert_eq!(result.code, 0, "{}", result.all());
        assert!(!result.out.contains('\u{1b}') && !result.err.contains('\u{1b}'));
        if args.contains(&"--json") {
            let value: serde_json::Value = serde_json::from_str(&result.out).expect("JSON");
            assert_eq!(value["schema_version"], 1);
            assert_eq!(value["accounts"][0]["alias"], "stream");
        }
    }
}
