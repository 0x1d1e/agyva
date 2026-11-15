//! `agyva monitor`: the route-aware foreground loop wired to the terminal.
//! Reuses the watch plumbing (Ctrl+C, backoff, bounded output) and swaps
//! the probe: every tick is cache-only ranking; the active account's quota
//! refresh runs on its own cadence through the existing single-flight
//! guard. Output shows the sticky pick, every account's exclusion reason
//! and freshness, and the next weekly reset countdown.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};

use super::Io;
use super::usage_out::render_event;
use crate::core::app::App;
use crate::core::error::{Result, exit};
use crate::core::format;
use crate::core::monitor::{
    DEFAULT_EVAL_INTERVAL, DEFAULT_REFRESH_INTERVAL, MonitorProbe, parse_interval,
};
use crate::core::route::RoutePlan;
use crate::core::watch::{Sink, SystemClock, SystemJitter, WatchConfig, WatchEvent, run_watch};
/// Hidden: stop after this many cycles (tests and scripted runs).
pub const ENV_MAX_CYCLES: &str = "AGYVA_WATCH_MAX_CYCLES";
/// Hidden: eval-interval override in milliseconds, test-mode only.
pub const ENV_INTERVAL_MS: &str = "AGYVA_MONITOR_INTERVAL_MS";

/// What the sink renders for one cycle: the plan and the alias the probe
/// actually settled on, so the pane can label a held pick as sticky.
struct CycleView {
    plan: RoutePlan,
    pick: Option<String>,
    /// What opt-in automation did this cycle, when it had something to say.
    auto: Option<String>,
}

/// Probe wrapper: after each cycle, publish the plan and pick into a slot
/// the sink reads, so `run_watch`'s fixed probe-then-sink order renders the
/// full route pane every cycle without touching the watch core.
struct PlanSharing<'a> {
    inner: MonitorProbe<'a>,
    slot: Arc<Mutex<Option<CycleView>>>,
}

impl crate::core::watch::Probe for PlanSharing<'_> {
    fn cycle(&mut self, now: DateTime<Utc>) -> crate::core::watch::CycleReport {
        let report = self.inner.cycle(now);
        let view = self.inner.last_plan().cloned().map(|plan| CycleView {
            plan,
            pick: self.inner.last_pick().map(|p| p.alias.clone()),
            auto: self.inner.last_auto().map(str::to_owned),
        });
        *self.slot.lock().expect("plan slot poisoned") = view;
        report
    }
}

struct MonitorSink<'a, 'b> {
    io: &'a mut Io<'b>,
    slot: Arc<Mutex<Option<CycleView>>>,
    stop: Arc<AtomicBool>,
    /// First write failure, if any: the loop is stopped and the caller
    /// turns this into an exit code instead of spinning quietly forever.
    error: Option<std::io::Error>,
}

impl MonitorSink<'_, '_> {
    fn write(&mut self, at: DateTime<Utc>, e: &WatchEvent) -> std::io::Result<()> {
        writeln!(self.io.out, "{}", render_event(at, e))?;
        if let (WatchEvent::CycleOk { .. }, Some(view)) =
            (e, self.slot.lock().expect("plan slot poisoned").as_ref())
        {
            let text = render_plan(&view.plan, at, view.pick.as_deref());
            if let Some(auto) = &view.auto {
                writeln!(self.io.out, "{auto}")?;
            }
            for line in text.lines() {
                writeln!(self.io.out, "{line}")?;
            }
        }
        // Best effort: a flush error is reported by the next write.
        let _ = self.io.out.flush();
        Ok(())
    }
}

impl Sink for MonitorSink<'_, '_> {
    fn emit(&mut self, at: DateTime<Utc>, e: &WatchEvent) {
        if self.error.is_some() {
            return; // the reader is gone; stop poking the pipe
        }
        if let Err(err) = self.write(at, e) {
            // A closed pipe (`agyva monitor | head -3`) must end the loop
            // cleanly, not leave it spinning with nowhere to write.
            self.error = Some(err);
            self.stop.store(true, Ordering::SeqCst);
        }
    }
}

/// One rendered plan block: pick, then every account with freshness and
/// exclusion reasons, then reset countdowns. Human text; no credentials.
pub fn render_plan(plan: &RoutePlan, now: DateTime<Utc>, sticky_alias: Option<&str>) -> String {
    let mut out = String::new();
    match (&plan.recommendation, sticky_alias) {
        (Some(_), Some(alias)) => out.push_str(&format!("pick: {alias} (sticky)\n")),
        (Some(_), None) => out.push_str("pick: (none yet)\n"),
        (None, _) => out.push_str("pick: none eligible\n"),
    }
    for row in &plan.accounts {
        let mark = if plan.recommendation.as_deref() == Some(row.id.as_str()) {
            '*'
        } else {
            ' '
        };
        // Only an eligible row's share is usable quota: a stale reading's
        // percentage must not read as available, so ineligible rows show
        // a dash and rely on `why` for the reason.
        let share = match (row.eligible, row.remaining_percent) {
            (true, Some(p)) => format::percent_text(p),
            _ => "-".into(),
        };
        out.push_str(&format!(
            "{mark} {:<12} {:>4}  {:<9} {}\n",
            row.alias,
            share,
            row.freshness.label(),
            row.why
        ));
        if let Some(reset) = row.renewals.iter().flatten().min() {
            out.push_str(&format!(
                "    weekly resets {}\n",
                format::until(now, *reset)
            ));
        }
    }
    out
}

pub fn monitor(app: &App, io: &mut Io<'_>, args: &super::MonitorArgs) -> Result<i32> {
    let max_cycles = match app.env.get(ENV_MAX_CYCLES) {
        None => None,
        Some(v) => Some(v.parse::<u64>().map_err(|e| {
            crate::core::error::Error::usage(format!("invalid {ENV_MAX_CYCLES}: {e}"))
        })?),
    };
    let interval = match app.env.get(ENV_INTERVAL_MS) {
        Some(v) if app.env.get(crate::core::failpoint::ENV_TEST_MODE) == Some("1") => {
            let ms = v.parse::<u64>().map_err(|_| {
                crate::core::error::Error::usage(format!("invalid {ENV_INTERVAL_MS}: {v}"))
            })?;
            if ms == 0 {
                return Err(crate::core::error::Error::usage(format!(
                    "{ENV_INTERVAL_MS} must be positive"
                )));
            }
            Duration::from_millis(ms)
        }
        _ => match &args.interval {
            Some(text) => parse_interval(text)?,
            None => DEFAULT_EVAL_INTERVAL,
        },
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    if let Err(e) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
        tracing::warn!("cannot install the Ctrl+C handler: {e}");
    }
    let mut cfg = WatchConfig::new(interval);
    cfg.max_cycles = max_cycles;
    let slot = Arc::new(std::sync::Mutex::new(None::<CycleView>));
    let mut probe = PlanSharing {
        inner: MonitorProbe::new(
            app,
            args.group.clone(),
            args.for_model.clone(),
            DEFAULT_REFRESH_INTERVAL,
        ),
        slot: slot.clone(),
    };
    let mut sink = MonitorSink {
        io,
        slot,
        stop: stop.clone(),
        error: None,
    };
    let mut jitter = SystemJitter::new();
    run_watch(
        &cfg,
        &SystemClock,
        &mut jitter,
        &mut probe,
        &mut sink,
        &stop,
    );
    match sink.error {
        // A closed reader (`| head -3`) is not an error; the loop simply
        // ends instead of spinning with nowhere to write.
        None => Ok(exit::OK),
        Some(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(exit::OK),
        Some(e) => Err(crate::core::error::Error::io("writing monitor output", e)),
    }
}
