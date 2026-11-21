//! Drives the real binary on a pseudo-terminal (via util-linux `script`) with
//! a synthetic fake `agy` and a tempdir sandbox. Skipped, loudly, where
//! `script` is not installed.
#![cfg(unix)]

mod common;

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agyva::testing::credential;
use common::fake_agy::FakeAgy;
use tempfile::TempDir;

struct Sandbox {
    tmp: TempDir,
    fake: FakeAgy,
}

impl Sandbox {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        for d in ["home", "agyhome"] {
            std::fs::create_dir_all(tmp.path().join(d)).expect("mkdir");
        }
        let fake = FakeAgy::install(&tmp.path().join("fakeagy"));
        fake.ok(0.72, 0.4);
        Self { tmp, fake }
    }

    fn p(&self, rel: &str) -> PathBuf {
        self.tmp.path().join(rel)
    }

    fn env(&self, c: &mut Command) {
        c.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.p("home"))
            .env("SHELL", "/bin/sh")
            .env("TERM", "xterm")
            .env("AGYVA_CONFIG_DIR", self.p("cfg"))
            .env("AGYVA_STATE_DIR", self.p("state"))
            .env("AGYVA_CACHE_DIR", self.p("cache"))
            .env("AGYVA_AGY_HOME", self.p("agyhome"))
            .env("AGYVA_AGY_BIN", self.fake.bin())
            .env("AGYVA_BACKEND", "file")
            .env("AGYVA_PASSPHRASE", "synthetic-e2e-passphrase")
            .env("AGYVA_UPDATE_CHECK", "0");
    }

    fn add(&self, sub: &str, email: &str, tag: &str, alias: &str) {
        std::fs::write(
            self.p("agyhome/antigravity-oauth-token"),
            credential(sub, email, tag),
        )
        .expect("login");
        let mut c = Command::new(env!("CARGO_BIN_EXE_agyva"));
        c.args(["add", "--alias", alias]).stdin(Stdio::null());
        self.env(&mut c);
        let o = c.output().expect("add");
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    }
}

fn script_available() -> bool {
    Command::new("script")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Owns the `script` wrapper and the `agyva` it runs. Dropping it kills and
/// reaps both, so a failed assertion never leaves a process behind.
struct Session {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    out: Arc<Mutex<Vec<u8>>>,
    pid_file: PathBuf,
}

/// The pid written by the wrapper shell (which then `exec`s into agyva).
fn read_pid(path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn alive(pid: u32) -> bool {
    // A zombie still has a /proc entry; its state letter tells them apart.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| !s.rsplit(") ").next().unwrap_or("").starts_with('Z'))
        .unwrap_or(false)
}

fn kill_pid(pid: u32) {
    if let Some(p) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process(p, rustix::process::Signal::KILL);
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Some(pid) = read_pid(&self.pid_file) {
            kill_pid(pid);
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Session {
    fn start(sb: &Sandbox) -> Self {
        Self::with_env(sb, &[])
    }

    fn with_env(sb: &Sandbox, env: &[(&str, &str)]) -> Self {
        let pid_file = sb.p("agyva.pid");
        let inner = format!(
            "stty cols 100 rows 30; echo $$ > '{}'; exec {}",
            pid_file.display(),
            env!("CARGO_BIN_EXE_agyva")
        );
        let mut c = Command::new("script");
        c.args(["-q", "-e", "-c", &inner, "/dev/null"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        sb.env(&mut c);
        c.envs(env.iter().copied());
        let mut child = c.spawn().expect("spawn script");
        let stdin = Some(child.stdin.take().expect("stdin"));
        let mut stdout = child.stdout.take().expect("stdout");
        let out = Arc::new(Mutex::new(Vec::new()));
        let sink = out.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 4096];
            while let Ok(n) = stdout.read(&mut buf) {
                if n == 0 {
                    break;
                }
                sink.lock().expect("lock").extend_from_slice(&buf[..n]);
            }
        });
        Self {
            child,
            stdin,
            out,
            pid_file,
        }
    }

    fn seen(&self) -> String {
        String::from_utf8_lossy(&self.out.lock().expect("lock")).into_owned()
    }

    fn wait_for(&self, needle: &str) {
        let end = Instant::now() + Duration::from_secs(20);
        while Instant::now() < end {
            if self.seen().contains(needle) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        panic!("never saw {needle:?}; output so far: {:?}", self.seen());
    }

    fn send(&mut self, bytes: &[u8]) {
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(bytes).expect("send");
        stdin.flush().expect("flush");
    }

    fn finish(mut self) -> (Option<i32>, String) {
        let end = Instant::now() + Duration::from_secs(20);
        let code = loop {
            if let Some(s) = self.child.try_wait().expect("poll") {
                break s.code();
            }
            if Instant::now() > end {
                let _ = self.child.kill();
                panic!("agyva did not exit; output: {:?}", self.seen());
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        std::thread::sleep(Duration::from_millis(100));
        (code, self.seen())
    }
}

fn check_terminal_restored(out: &str) {
    let enter = out
        .rfind("\x1b[?1049h")
        .expect("entered the alternate screen");
    let leave = out.rfind("\x1b[?1049l").expect("left the alternate screen");
    assert!(leave > enter, "alternate screen left after it was entered");
    assert!(
        out[leave..].contains("\x1b[?25h") || out[enter..].contains("\x1b[?25h"),
        "cursor shown again"
    );
}

fn scenario(keys: &[(&str, &[u8])], quit: &[u8]) -> (Option<i32>, String) {
    let sb = Sandbox::new();
    sb.add("sub-alice", "alice@example.invalid", "alice", "alice");
    let mut s = Session::start(&sb);
    s.wait_for("alice");
    s.wait_for("72%");
    for (expect, bytes) in keys {
        s.send(bytes);
        s.wait_for(expect);
    }
    s.send(quit);
    s.finish()
}

#[test]
fn bare_agyva_on_a_pty_draws_navigates_and_restores_the_terminal() {
    if !script_available() {
        eprintln!("SKIPPED: util-linux `script` is not installed");
        return;
    }
    let (code, out) = scenario(
        &[
            ("Gemini Pro", b"\t"),
            ("journal: no interrupted switch", b"\t"),
        ],
        b"q",
    );
    assert_eq!(code, Some(0), "{out:?}");
    check_terminal_restored(&out);
}

#[test]
fn ctrl_c_on_a_pty_quits_and_restores_the_terminal() {
    if !script_available() {
        eprintln!("SKIPPED: util-linux `script` is not installed");
        return;
    }
    let (code, out) = scenario(&[("Help", b"?")], b"\x1b\x03");
    assert_eq!(code, Some(0), "{out:?}");
    check_terminal_restored(&out);
}

#[test]
fn switching_from_the_pty_replaces_the_login_like_use_does() {
    if !script_available() {
        eprintln!("SKIPPED: util-linux `script` is not installed");
        return;
    }
    let sb = Sandbox::new();
    sb.add("sub-alice", "alice@example.invalid", "alice", "alice");
    sb.add("sub-bob", "bob@example.invalid", "bob", "bob");
    let token = sb.p("agyhome/antigravity-oauth-token");
    assert!(
        std::fs::read_to_string(&token)
            .expect("login")
            .contains("access-bob")
    );

    let mut s = Session::start(&sb);
    s.wait_for("bob");
    s.wait_for("72%");
    s.send(b"1"); // select alice (number 1)
    s.send(b"\r");
    s.wait_for("Switch to alice");
    s.send(b"n");
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        std::fs::read_to_string(&token)
            .expect("login")
            .contains("access-bob"),
        "cancel changes nothing"
    );
    s.send(b"\r");
    s.send(b"y");
    s.wait_for("to alice. Restart");
    s.send(b"q");
    let (code, out) = s.finish();
    assert_eq!(code, Some(0), "{out:?}");
    check_terminal_restored(&out);
    assert!(
        std::fs::read_to_string(&token)
            .expect("login")
            .contains("access-alice"),
        "native login replaced"
    );
}

/// The terminal vanishing (the pty master closing) must end the interface
/// promptly; it once left the process spinning at full CPU forever.
#[test]
fn the_interface_exits_when_its_terminal_goes_away() {
    if !script_available() {
        eprintln!("SKIPPED: util-linux `script` is not installed");
        return;
    }
    let sb = Sandbox::new();
    sb.add("sub-alice", "alice@example.invalid", "alice", "alice");
    let mut s = Session::start(&sb);
    s.wait_for("alice");
    s.wait_for("72%");
    let pid = read_pid(&s.pid_file).expect("pid file written before exec");
    assert!(alive(pid));

    // Close the master side: kill the wrapper that holds it.
    drop(s.stdin.take());
    s.child.kill().expect("kill script");
    s.child.wait().expect("reap script");

    let end = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        !alive(pid),
        "agyva (pid {pid}) is still running after its terminal went away"
    );
}

#[test]
fn terminal_color_policy_keeps_selection_in_auto_never_and_no_color() {
    if !script_available() {
        eprintln!("SKIPPED: util-linux `script` is not installed");
        return;
    }
    for (choice, no_color, colored) in [
        ("auto", None, true),
        ("never", None, false),
        ("always", Some("1"), false),
        ("auto", Some(""), true),
    ] {
        let sb = Sandbox::new();
        sb.add("sub-color", "color@example.invalid", "color", "color");
        let mut config = Command::new(env!("CARGO_BIN_EXE_agyva"));
        config.args(["config", "set", "color", choice]);
        sb.env(&mut config);
        assert!(config.output().expect("config").status.success());
        let extra: Vec<_> = no_color
            .map(|value| ("NO_COLOR", value))
            .into_iter()
            .collect();
        let mut session = Session::with_env(&sb, &extra);
        session.wait_for("72%");
        session.send(b"q");
        let (code, out) = session.finish();
        assert_eq!(code, Some(0));
        check_terminal_restored(&out);
        let sgr: Vec<u16> = out
            .split("\x1b[")
            .skip(1)
            .filter_map(|seq| {
                let end = seq.find(|ch: char| ch.is_ascii_alphabetic())?;
                (seq.as_bytes()[end] == b'm').then_some(&seq[..end])
            })
            .flat_map(|params| params.split(';').filter_map(|param| param.parse().ok()))
            .collect();
        let has_colors = sgr
            .iter()
            .any(|n| (30..=38).contains(n) || (40..=48).contains(n) || (90..=107).contains(n));
        assert_eq!(has_colors, colored, "color={choice}, NO_COLOR={no_color:?}");
        assert!(sgr.contains(&7), "reverse selection survives monochrome");
    }
}
