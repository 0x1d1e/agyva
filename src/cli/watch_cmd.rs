//! `agyva watch`: wires the pure loop to the terminal, Ctrl+C and
//! (optionally) desktop notifications.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

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

#[derive(Serialize)]
struct JsonWatchEvent<'a> {
    at: DateTime<Utc>,
    #[serde(flatten)]
    event: &'a WatchEvent,
}

struct Printer<'a, 'b> {
    io: &'a mut Io<'b>,
    notifier: Box<dyn Notifier>,
    stop: Arc<AtomicBool>,
    json_stream: bool,
    /// First write failure, if any: the loop is stopped and the caller
    /// turns this into an exit code instead of spinning quietly forever.
    error: Option<std::io::Error>,
}

impl Printer<'_, '_> {
    fn write(&mut self, at: DateTime<Utc>, e: &WatchEvent) -> std::io::Result<()> {
        if self.json_stream {
            let line = serde_json::to_string(&JsonWatchEvent { at, event: e })
                .unwrap_or_else(|_| "{}".to_owned());
            writeln!(self.io.out, "{line}")?;
        } else {
            writeln!(self.io.out, "{}", render_event(at, e))?;
        }
        // Best effort: a flush error surfaces on the next write.
        let _ = self.io.out.flush();
        Ok(())
    }
}

impl Sink for Printer<'_, '_> {
    fn emit(&mut self, at: DateTime<Utc>, e: &WatchEvent) {
        if self.error.is_some() {
            return; // the reader is gone; stop poking the pipe
        }
        if let Err(err) = self.write(at, e) {
            // A closed pipe (`agyva watch | head -3`) must end the loop
            // cleanly, not leave it spinning with nowhere to write.
            self.error = Some(err);
            self.stop.store(true, Ordering::SeqCst);
            return;
        }
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
            WatchEvent::Note { text }
                if text.starts_with("CRITICAL ") || text.starts_with("WARN ") =>
            {
                self.notifier.notify("agyva: quota threshold", text);
            }
            _ => {}
        }
    }
}

pub(super) fn resolve_thresholds(
    warn_cli: Option<f64>,
    crit_cli: Option<f64>,
    warn_prof: Option<f64>,
    crit_prof: Option<f64>,
    warn_cfg: f64,
    crit_cfg: f64,
) -> Result<(f64, f64)> {
    let warn = warn_cli.or(warn_prof).unwrap_or(warn_cfg);
    let crit = crit_cli.or(crit_prof).unwrap_or(crit_cfg);
    if !warn.is_finite() || !(0.0..=100.0).contains(&warn) {
        return Err(Error::usage(
            "--warn-below must be finite and between 0 and 100",
        ));
    }
    if !crit.is_finite() || !(0.0..=100.0).contains(&crit) {
        return Err(Error::usage(
            "--critical-below must be finite and between 0 and 100",
        ));
    }
    if crit > warn {
        return Err(Error::usage(
            "--critical-below must be less than or equal to --warn-below",
        ));
    }
    Ok((warn, crit))
}

pub fn watch(app: &App, io: &mut Io<'_>, args: &super::WatchArgs) -> Result<i32> {
    let profile = match args.profile.as_deref() {
        Some(name) => app.resolve_active_profile(Some(name), None)?,
        None => app.resolve_active_profile(None, None).ok().flatten(),
    };
    let (warn_pct, crit_pct) = resolve_thresholds(
        args.warn_below,
        args.critical_below,
        profile.as_ref().and_then(|p| p.spec.warn_percent),
        profile.as_ref().and_then(|p| p.spec.critical_percent),
        app.config.warn_percent,
        app.config.critical_percent,
    )?;

    let max_cycles = if args.once {
        Some(1)
    } else {
        match app.env.get(ENV_MAX_CYCLES) {
            None => None,
            Some(v) => Some(
                v.parse::<u64>()
                    .map_err(|_| Error::usage(format!("invalid {ENV_MAX_CYCLES}: {v}")))?,
            ),
        }
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
    let mut cfg = WatchConfig::new(interval).with_thresholds(Some(warn_pct), Some(crit_pct));
    cfg.max_cycles = max_cycles;
    let notifier: Box<dyn Notifier> = if args.notify || app.config.desktop_notifications {
        Box::new(Desktop::new())
    } else {
        Box::new(Silent)
    };
    let mut sink = Printer {
        io,
        notifier,
        stop: stop.clone(),
        json_stream: args.json_stream,
        error: None,
    };
    let mut probe = AppProbe(app);
    run_watch(
        &cfg,
        &SystemClock,
        &mut SystemJitter::new(),
        &mut probe,
        &mut sink,
        &stop,
    );
    match sink.error {
        // A closed reader (`| head -3`) ends the loop without an error.
        None => Ok(exit::OK),
        Some(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(exit::OK),
        Some(e) => Err(Error::io("writing watch output", e)),
    }
}
