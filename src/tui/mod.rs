//! The interactive view (`agyva` with no arguments on a terminal).
//!
//! Layers: `backend` (worker thread doing everything slow), `event` (pure
//! state machine), `app` (state), `views` (pure rendering). This file only
//! owns the terminal and the loop that connects them.

pub mod app;
pub mod backend;
pub mod event;
pub mod theme;
pub mod views;

use std::io::{self, IsTerminal, Stdout};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::TryRecvError;
use std::time::{Duration, Instant};

use chrono::Utc;
use crossterm::event::{self as ct, Event as CtEvent};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::{Backend as RatBackend, CrosstermBackend};

use crate::core::error::{Error, Result, exit};
use crate::paths::Env;
use app::AppState;
use backend::{AppBackend, Backend, Job, spawn_worker};
use event::{Event, startup, update};

/// How long to wait for input before checking the worker and Ctrl+C flag.
const POLL: Duration = Duration::from_millis(100);
/// Relative times ("3m ago") are redrawn at this interval.
const TICK: Duration = Duration::from_secs(30);

/// Put the terminal back the way it was. Safe to call more than once.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen, crossterm::cursor::Show);
}

/// Restores the terminal when dropped, however the interface ends.
struct TerminalGuard;

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        // Make a panic on the interface thread restore the terminal before
        // its message is printed. A panic on a worker thread is caught and
        // shown as an error inside the interface, so it must neither touch
        // the terminal nor print over the screen: it is only logged.
        let main_thread = std::thread::current().id();
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() == main_thread {
                restore_terminal();
                previous(info);
            } else {
                tracing::error!("background panic: {info}");
            }
        }));
        enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen)?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn terminal_error(e: io::Error) -> Error {
    Error::io("terminal", e)
}

/// A handle used only to ask whether the terminal still exists.
///
/// crossterm cannot tell us: once the terminal's other end is closed, its
/// reader sees end-of-file forever and retries without ever returning, which
/// pinned a CPU core. So the loop asks the kernel first (hang-up and error
/// conditions) and never calls into crossterm for a terminal that is gone.
struct Tty {
    #[cfg(unix)]
    fd: std::os::fd::OwnedFd,
}

impl Tty {
    /// The descriptor crossterm reads: stdin when it is a terminal,
    /// otherwise the controlling terminal.
    #[cfg(unix)]
    fn open() -> io::Result<Self> {
        use std::os::fd::AsFd;
        let fd = if io::stdin().is_terminal() {
            io::stdin().as_fd().try_clone_to_owned()?
        } else {
            std::fs::File::open("/dev/tty")?.into()
        };
        Ok(Self { fd })
    }

    #[cfg(not(unix))]
    fn open() -> io::Result<Self> {
        Ok(Self {})
    }

    /// Wait up to `timeout` for input or for the terminal to go away.
    /// Returns true once it hung up, errored or was closed.
    #[cfg(unix)]
    fn gone_within(&self, timeout: Duration) -> bool {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        let mut fds = [PollFd::new(&self.fd, PollFlags::IN)];
        let limit = Timespec {
            tv_sec: i64::try_from(timeout.as_secs()).unwrap_or(0),
            tv_nsec: i64::from(timeout.subsec_nanos()),
        };
        match poll(&mut fds, Some(&limit)) {
            Ok(_) => fds[0]
                .revents()
                .intersects(PollFlags::HUP | PollFlags::ERR | PollFlags::NVAL),
            Err(rustix::io::Errno::INTR) => false,
            // A poll that cannot even run on this descriptor is not a
            // terminal we can keep using.
            Err(_) => true,
        }
    }

    #[cfg(not(unix))]
    fn gone_within(&self, timeout: Duration) -> bool {
        std::thread::sleep(timeout);
        false
    }

    fn gone(&self) -> bool {
        self.gone_within(Duration::ZERO)
    }
}

/// Kill this process immediately, without cleanup.
fn die() -> ! {
    let _ = rustix::process::kill_process(rustix::process::getpid(), rustix::process::Signal::KILL);
    std::process::abort()
}

/// Milliseconds since `start`, stored by the loop on every pass so the
/// watchdog can tell a stuck loop from a busy one.
fn beat(heartbeat: &AtomicU64, start: Instant) {
    let ms = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
    heartbeat.store(ms, Ordering::Relaxed);
}

/// Last resort for the narrow race where the terminal hangs up between the
/// loop's check and its call into crossterm (which would then never return).
/// Only acts when the terminal is gone AND the loop has stopped beating. It
/// ends the process outright: the terminal no longer exists to restore, and
/// an ordinary exit can wait on state the stuck thread holds.
fn spawn_watchdog(tty: Tty, heartbeat: Arc<AtomicU64>, start: Instant) {
    const STALE_MS: u64 = 2_000;
    let spawned = std::thread::Builder::new()
        .name("tty-watchdog".into())
        .spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(500));
                if !tty.gone() {
                    continue;
                }
                let now = u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX);
                if now.saturating_sub(heartbeat.load(Ordering::Relaxed)) > STALE_MS {
                    die();
                }
            }
        });
    if let Err(e) = spawned {
        tracing::warn!("cannot start the terminal watchdog: {e}");
    }
}

/// Open the interface against the real environment.
pub fn run_tui(env: Env, allow_untested: bool) -> Result<i32> {
    let settings = crate::core::app::App::from_env(env.clone(), allow_untested)?;
    let color = crate::cli::output::color_enabled(
        settings.config.color,
        env.get("NO_COLOR").is_some_and(|value| !value.is_empty()),
        true,
    );
    drop(settings);
    let backend: Arc<dyn Backend> = Arc::new(AppBackend::from_env(env, allow_untested));
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    // Signals that do not arrive as key presses (kill -INT/-TERM, hangup).
    if let Err(e) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
        tracing::warn!("cannot install the Ctrl+C handler: {e}");
    }
    let tty = Tty::open().map_err(terminal_error)?;
    let guard = TerminalGuard::enter().map_err(terminal_error)?;
    let start = Instant::now();
    let heartbeat = Arc::new(AtomicU64::new(0));
    match Tty::open() {
        Ok(second) => spawn_watchdog(second, heartbeat.clone(), start),
        Err(e) => tracing::warn!("no terminal watchdog: {e}"),
    }
    let result = (|| {
        let mut terminal: Terminal<CrosstermBackend<Stdout>> =
            Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let r = drive(
            &mut terminal,
            backend,
            &stop,
            Some(&tty),
            &heartbeat,
            start,
            color,
        );
        // Dropping a ratatui terminal tries to show the cursor and, when
        // that fails (the terminal is gone), prints to stderr, which panics
        // if stderr is the same dead terminal. The guard restores the screen.
        std::mem::forget(terminal);
        r
    })();
    drop(guard);
    result.map_err(terminal_error)?;
    Ok(exit::OK)
}

fn send(jobs: &std::sync::mpsc::Sender<Job>, list: Vec<Job>) {
    for j in list {
        // The worker only stops when we drop the sender, so this cannot fail
        // while the loop runs; ignoring keeps shutdown quiet.
        let _ = jobs.send(j);
    }
}

/// The event loop. `read_terminal` is false in tests that drive no real input.
pub fn run_loop<B: RatBackend>(
    terminal: &mut Terminal<B>,
    backend: Arc<dyn Backend>,
    stop: &AtomicBool,
    read_terminal: bool,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let tty = if read_terminal {
        Some(Tty::open()?)
    } else {
        None
    };
    let heartbeat = AtomicU64::new(0);
    drive(
        terminal,
        backend,
        stop,
        tty.as_ref(),
        &heartbeat,
        Instant::now(),
        true,
    )
}

/// The loop proper. `tty` is None when no real input is read.
fn drive<B: RatBackend>(
    terminal: &mut Terminal<B>,
    backend: Arc<dyn Backend>,
    stop: &AtomicBool,
    tty: Option<&Tty>,
    heartbeat: &AtomicU64,
    start: Instant,
    color: bool,
) -> io::Result<()>
where
    B::Error: std::error::Error + Send + Sync + 'static,
{
    let backend_err = io::Error::other;
    let size = terminal.size().map_err(backend_err)?;
    let mut state = AppState::new((size.width, size.height), Utc::now());
    state.color = color;
    let worker = spawn_worker(backend);
    send(&worker.jobs, startup(&mut state));
    let mut last_tick = Instant::now();
    let mut hung_up = false;
    let mut worker_lost = false;
    while !state.quit {
        beat(heartbeat, start);
        // A vanished terminal is a quit request, handled like Ctrl+C: a
        // switch in progress still finishes. Nothing is drawn or read after.
        // Wait on the terminal ourselves first: crossterm's reader retries
        // end-of-file forever and would never hand control back.
        if !hung_up {
            let gone = match tty {
                Some(t) => t.gone_within(POLL),
                None => {
                    std::thread::sleep(POLL);
                    false
                }
            };
            if gone {
                hung_up = true;
                send(&worker.jobs, update(&mut state, Event::Interrupt));
                if state.quit {
                    break;
                }
            }
        } else {
            std::thread::sleep(POLL);
        }
        if hung_up {
            state.dirty = false;
        }
        if state.dirty {
            state.dirty = false;
            terminal
                .draw(|f| views::render(&state, f))
                .map_err(backend_err)?;
        }
        loop {
            match worker.results.try_recv() {
                Ok(m) => send(&worker.jobs, update(&mut state, Event::Msg(m))),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    // The worker is gone (it should never be): do not wait
                    // for replies that cannot come, and let quit work.
                    if !worker_lost {
                        worker_lost = true;
                        event::worker_lost(&mut state);
                    }
                    break;
                }
            }
        }
        if stop.load(Ordering::SeqCst) {
            send(&worker.jobs, update(&mut state, Event::Interrupt));
            // One interrupt is enough; a later Ctrl+C is handled as a key.
            stop.store(false, Ordering::SeqCst);
        }
        if state.quit {
            break;
        }
        // Only now is it safe to read: the terminal was just seen alive, so
        // this never blocks (zero timeout) and cannot hit end-of-file.
        if !hung_up && tty.is_some() {
            while ct::poll(Duration::ZERO)? {
                match ct::read()? {
                    CtEvent::Key(k) => send(&worker.jobs, update(&mut state, Event::Key(k))),
                    CtEvent::Resize(w, h) => {
                        send(&worker.jobs, update(&mut state, Event::Resize(w, h)))
                    }
                    _ => {}
                }
            }
        }
        if last_tick.elapsed() >= TICK {
            last_tick = Instant::now();
            send(&worker.jobs, update(&mut state, Event::Tick(Utc::now())));
        }
    }
    Ok(())
}
