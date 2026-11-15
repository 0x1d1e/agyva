//! `agyva watch`: wires the pure loop to the terminal, Ctrl+C and
//! (optionally) desktop notifications.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::Io;
use super::usage_out::render_event;
use crate::core::app::App;
use crate::core::error::{Error, Result, exit};
use crate::core::watch::{
    AppProbe, Sink, SystemClock, SystemJitter, WatchConfig, WatchEvent, run_watch,
};
use crate::notify::{Desktop, Notifier, Silent};

/// Hidden: stop after this many cycles (tests and scripted runs).
pub const ENV_MAX_CYCLES: &str = "AGYVA_WATCH_MAX_CYCLES";

/// Hidden: override the interval in milliseconds. Honored only together with
/// `AGYVA_TEST_MODE=1`, because the configured minimum of 60 seconds exists to
/// keep `agyva watch` from hammering the quota endpoint.
pub const ENV_INTERVAL_MS: &str = "AGYVA_WATCH_INTERVAL_MS";

struct Printer<'a, 'b> {
    io: &'a mut Io<'b>,
    notifier: Box<dyn Notifier>,
}

impl Sink for Printer<'_, '_> {
    fn emit(&mut self, at: DateTime<Utc>, e: &WatchEvent) {
        // A closed pipe must not make the monitor panic; it simply goes quiet.
        let _ = writeln!(self.io.out, "{}", render_event(at, e));
        let _ = self.io.out.flush();
        match e {
            WatchEvent::HealthChanged { alias, to, .. } if to.is_alarming() => {
                self.notifier.notify(
                    "agyva: account needs attention",
                    &format!("{alias}: {}", to.label()),
                );
            }
            WatchEvent::ResetApproaching {
                alias,
                window,
                group,
                ..
            } => {
                self.notifier.notify(
                    "agyva: quota resets soon",
                    &format!("{alias}: {group} {window} window"),
                );
            }
            WatchEvent::SuspendedSkipped { alias } => {
                self.notifier.notify(
                    "agyva: account suspended",
                    &format!("{alias} is suspended (ToS); see the Antigravity FAQ"),
                );
            }
            _ => {}
        }
    }
}

pub fn watch(app: &App, io: &mut Io<'_>) -> Result<i32> {
    let max_cycles = match app.env.get(ENV_MAX_CYCLES) {
        None => None,
        Some(v) => Some(
            v.parse::<u64>()
                .map_err(|_| Error::usage(format!("invalid {ENV_MAX_CYCLES}: {v}")))?,
        ),
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    if let Err(e) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
        tracing::warn!("cannot install the Ctrl+C handler: {e}");
    }
    let interval = match app.env.get(ENV_INTERVAL_MS) {
        Some(v) if app.env.get(crate::core::failpoint::ENV_TEST_MODE) == Some("1") => {
            let ms = v
                .parse::<u64>()
                .map_err(|_| Error::usage(format!("invalid {ENV_INTERVAL_MS}: {v}")))?;
            if ms == 0 {
                return Err(Error::usage(format!("{ENV_INTERVAL_MS} must be positive")));
            }
            Duration::from_millis(ms)
        }
        Some(_) => {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                eprintln!(
                    "warning: ignoring {ENV_INTERVAL_MS}: it needs {}=1 as well",
                    crate::core::failpoint::ENV_TEST_MODE
                );
            });
            Duration::from_secs(app.config.watch_interval_secs)
        }
        None => Duration::from_secs(app.config.watch_interval_secs),
    };
    let mut cfg = WatchConfig::new(interval);
    cfg.max_cycles = max_cycles;
    let notifier: Box<dyn Notifier> = if app.config.desktop_notifications {
        Box::new(Desktop::new())
    } else {
        Box::new(Silent)
    };
    let mut sink = Printer { io, notifier };
    let mut probe = AppProbe(app);
    run_watch(
        &cfg,
        &SystemClock,
        &mut SystemJitter::new(),
        &mut probe,
        &mut sink,
        &stop,
    );
    Ok(exit::OK)
}
