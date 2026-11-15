//! Running `agy -p "/usage" --output-format json` safely.
//!
//! The child gets no stdin, no arguments beyond the fixed ones, and no
//! `AGYVA_*` variables (so the vault passphrase never leaks into it). Output
//! is read on helper threads into bounded buffers; on timeout the child is
//! killed. Nothing here sees or passes credentials.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub const USAGE_ARGS: [&str; 4] = ["-p", "/usage", "--output-format", "json"];

#[derive(Debug, Clone, Copy)]
pub struct RunLimits {
    pub timeout: Duration,
    /// Per stream; excess output is drained and dropped.
    pub max_bytes: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(15),
            max_bytes: 256 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunEnd {
    Exited(Option<i32>),
    TimedOut,
}

#[derive(Debug, Clone)]
pub struct RunOutput {
    pub end: RunEnd,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}

fn spawn_reader<R: Read + Send + 'static>(
    mut src: R,
    max: usize,
) -> mpsc::Receiver<(Vec<u8>, bool)> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut kept = Vec::new();
        let mut truncated = false;
        let mut buf = [0u8; 8192];
        loop {
            match src.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = max.saturating_sub(kept.len());
                    if n > room {
                        truncated = true;
                    }
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        let _ = tx.send((kept, truncated));
    });
    rx
}

/// A binary that was only just written (an installer, or a test fixture) can
/// briefly report "text file busy" while another process still holds it open
/// for writing. That clears within milliseconds, so retry a few times.
fn spawn_retrying(cmd: &mut Command) -> std::io::Result<std::process::Child> {
    // Its own process group, so a timeout can stop agy *and* anything it
    // started, instead of leaving helpers running with our pipes open.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.kind() == std::io::ErrorKind::ExecutableFileBusy && tries < 20 => {
                tries += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

/// Kill the child and everything in its process group, then reap it.
fn stop(child: &mut std::process::Child) {
    #[cfg(unix)]
    if let Some(group) = i32::try_from(child.id())
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    {
        let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
    }
    // Idempotent; the group call above does not reap or (on non-unix) exist.
    let _ = child.kill();
    let _ = child.wait();
}

pub fn run_usage(bin: &Path, limits: &RunLimits) -> std::io::Result<RunOutput> {
    run_with_args(bin, &USAGE_ARGS, limits)
}

/// The same run for any fixed argv (`models`): identical spawn hygiene,
/// bounded readers and timeout kill.
pub fn run_with_args(bin: &Path, args: &[&str], limits: &RunLimits) -> std::io::Result<RunOutput> {
    let args: Vec<&str> = args.to_vec();
    let mut cmd = Command::new(bin);
    cmd.args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, _) in std::env::vars_os() {
        if k.to_string_lossy().starts_with("AGYVA_") {
            cmd.env_remove(&k);
        }
    }
    let mut child = spawn_retrying(&mut cmd)?;
    let out_rx = child
        .stdout
        .take()
        .map(|s| spawn_reader(s, limits.max_bytes));
    let err_rx = child
        .stderr
        .take()
        .map(|s| spawn_reader(s, limits.max_bytes));

    let start = Instant::now();
    let mut timed_out = false;
    let end = loop {
        match child.try_wait() {
            Ok(Some(status)) => break RunEnd::Exited(status.code()),
            Ok(None) if start.elapsed() >= limits.timeout => {
                timed_out = true;
                stop(&mut child);
                break RunEnd::TimedOut;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(15)),
            Err(e) => {
                // Never leave the child running because polling failed.
                stop(&mut child);
                return Err(e);
            }
        }
    };
    // A grandchild may keep a pipe open after the kill; do not wait for it.
    let grace = Duration::from_millis(if timed_out { 300 } else { 2000 });
    let collect = |rx: Option<mpsc::Receiver<(Vec<u8>, bool)>>| {
        rx.and_then(|r| r.recv_timeout(grace).ok())
            .map(|(b, t)| (String::from_utf8_lossy(&b).into_owned(), t))
            .unwrap_or_default()
    };
    let (stdout, t1) = collect(out_rx);
    let (stderr, t2) = collect(err_rx);
    Ok(RunOutput {
        end,
        stdout,
        stderr,
        truncated: t1 || t2,
    })
}
