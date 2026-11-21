//! `agyva monitor`: the route-aware foreground loop wired to the terminal.
//! Reuses the watch plumbing (Ctrl+C, backoff, bounded output) and swaps
//! the probe: every tick is cache-only ranking; the active account's quota
//! refresh runs on its own cadence through the existing single-flight
//! guard. Output shows the sticky pick, every account's exclusion reason,
//! freshness, burn rate / ETA, active endpoint/profile header, and reset countdowns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::Io;
use super::commands::print_json;
use super::usage_out::{format_duration_short, render_event};
use super::watch_cmd::resolve_thresholds;
use crate::core::app::App;
use crate::core::error::{Error, Result, exit};
use crate::core::format;
use crate::core::monitor::{
    DEFAULT_EVAL_INTERVAL, DEFAULT_REFRESH_INTERVAL, MonitorProbe, MonitorSnapshot, parse_interval,
};
use crate::core::route::RoutePlan;
use crate::core::watch::{Sink, SystemClock, SystemJitter, WatchConfig, WatchEvent, run_watch};

/// Hidden: stop after this many cycles (tests and scripted runs).
pub const ENV_MAX_CYCLES: &str = "AGYVA_WATCH_MAX_CYCLES";
/// Hidden: eval-interval override in milliseconds, test-mode only.
pub const ENV_INTERVAL_MS: &str = "AGYVA_MONITOR_INTERVAL_MS";

/// What the sink renders for one cycle: the plan, snapshot, and the alias the
/// probe actually settled on, so the pane can label a held pick as sticky.
struct CycleView {
    plan: RoutePlan,
    snapshot: Option<MonitorSnapshot>,
    pick: Option<String>,
    /// What opt-in automation did this cycle, when it had something to say.
    auto: Option<String>,
}

/// Probe wrapper: after each cycle, publish the plan and snapshot into a slot
/// the sink reads, so `run_watch`'s fixed probe-then-sink order renders the
/// full route pane every cycle without touching the watch core.
struct PlanSharing<'a> {
    inner: MonitorProbe<'a>,
    slot: Arc<Mutex<Option<CycleView>>>,
    profile_override: Option<String>,
}

impl crate::core::watch::Probe for PlanSharing<'_> {
    fn cycle(&mut self, now: DateTime<Utc>) -> crate::core::watch::CycleReport {
        let report = self.inner.cycle(now);
        let mut snapshot = self.inner.last_snapshot().cloned();
        if let (Some(snap), Some(prof)) = (&mut snapshot, &self.profile_override) {
            snap.active_profile = Some(prof.clone());
        }
        let view = self.inner.last_plan().cloned().map(|plan| CycleView {
            plan,
            snapshot,
            pick: self.inner.last_pick().map(|p| p.alias.clone()),
            auto: self.inner.last_auto().map(str::to_owned),
        });
        *self.slot.lock().expect("plan slot poisoned") = view;
        report
    }
}

#[derive(Serialize)]
struct JsonStreamLine<'a> {
    at: DateTime<Utc>,
    #[serde(flatten)]
    event: &'a WatchEvent,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    snapshot: Option<&'a MonitorSnapshot>,
}

struct MonitorSink<'a, 'b> {
    io: &'a mut Io<'b>,
    slot: Arc<Mutex<Option<CycleView>>>,
    stop: Arc<AtomicBool>,
    json_only: bool,
    json_stream: bool,
    /// First write failure, if any: the loop is stopped and the caller
    /// turns this into an exit code instead of spinning quietly forever.
    error: Option<std::io::Error>,
}

impl MonitorSink<'_, '_> {
    fn write(&mut self, at: DateTime<Utc>, e: &WatchEvent) -> std::io::Result<()> {
        if self.json_only {
            return Ok(());
        }
        if self.json_stream {
            let guard = self.slot.lock().expect("plan slot poisoned");
            let snap = match e {
                WatchEvent::CycleOk { .. } => guard.as_ref().and_then(|v| v.snapshot.as_ref()),
                _ => None,
            };
            let line = serde_json::to_string(&JsonStreamLine {
                at,
                event: e,
                snapshot: snap,
            })
            .unwrap_or_else(|_| "{}".to_owned());
            writeln!(self.io.out, "{line}")?;
            let _ = self.io.out.flush();
            return Ok(());
        }
        writeln!(self.io.out, "{}", render_event(at, e))?;
        if let (WatchEvent::CycleOk { .. }, Some(view)) =
            (e, self.slot.lock().expect("plan slot poisoned").as_ref())
        {
            if let Some(snap) = &view.snapshot
                && let Some(header) = format_context_header(snap)
            {
                writeln!(self.io.out, "{header}")?;
            }
            if let Some(auto) = &view.auto {
                writeln!(self.io.out, "{auto}")?;
            }
            let text = render_plan_with_snapshot(
                &view.plan,
                view.snapshot.as_ref(),
                at,
                view.pick.as_deref(),
            );
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

fn format_context_header(snap: &MonitorSnapshot) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(ep) = &snap.active_endpoint {
        parts.push(format!("endpoint:{ep}"));
    }
    if let Some(prof) = &snap.active_profile {
        parts.push(format!("profile:{prof}"));
    }
    if let Some(g) = &snap.group {
        parts.push(format!("group:{g}"));
    }
    if let Some(m) = &snap.for_model {
        parts.push(format!("model:{m}"));
    }
    (!parts.is_empty()).then(|| format!("context: {}", parts.join(" | ")))
}

fn render_plan_with_snapshot(
    plan: &RoutePlan,
    snapshot: Option<&MonitorSnapshot>,
    now: DateTime<Utc>,
    sticky_alias: Option<&str>,
) -> String {
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
        if let Some(snap_row) = snapshot.and_then(|s| s.rows.iter().find(|r| r.id == row.id))
            && let Some(br) = &snap_row.burn_rate
            && let Some(rate) = br.percent_per_hour
            && rate > 0.0
        {
            let eta = snap_row
                .eta_exhaustion_secs
                .map(|s| format!(" (ETA {})", format_duration_short(s)))
                .unwrap_or_default();
            out.push_str(&format!("    burn {rate:.1}%/h{eta}\n"));
        }
        if let Some(reset) = row.renewals.iter().flatten().min() {
            out.push_str(&format!(
                "    weekly resets {}\n",
                format::until(now, *reset)
            ));
        }
    }
    out
}

/// One rendered plan block: pick, then every account with freshness and
/// exclusion reasons, then reset countdowns. Human text; no credentials.
pub fn render_plan(plan: &RoutePlan, now: DateTime<Utc>, sticky_alias: Option<&str>) -> String {
    render_plan_with_snapshot(plan, None, now, sticky_alias)
}

pub fn monitor(app: &App, io: &mut Io<'_>, args: &super::MonitorArgs) -> Result<i32> {
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

    let group = if args.all {
        None
    } else {
        args.group
            .clone()
            .or_else(|| profile.as_ref().and_then(|p| p.spec.group.clone()))
    };
    let for_model = args
        .for_model
        .clone()
        .or_else(|| profile.as_ref().and_then(|p| p.spec.model.clone()));

    let max_cycles = if args.once || args.json {
        Some(1)
    } else {
        match app.env.get(ENV_MAX_CYCLES) {
            None => None,
            Some(v) => Some(
                v.parse::<u64>()
                    .map_err(|e| Error::usage(format!("invalid {ENV_MAX_CYCLES}: {e}")))?,
            ),
        }
    };
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
        _ => match args.interval.as_deref().or_else(|| {
            profile
                .as_ref()
                .and_then(|p| p.spec.monitor_interval.as_deref())
        }) {
            Some(text) => parse_interval(text)?,
            None => DEFAULT_EVAL_INTERVAL,
        },
    };
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    if let Err(e) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
        tracing::warn!("cannot install the Ctrl+C handler: {e}");
    }
    let mut cfg = WatchConfig::new(interval).with_thresholds(Some(warn_pct), Some(crit_pct));
    cfg.max_cycles = max_cycles;
    let slot = Arc::new(Mutex::new(None::<CycleView>));
    let mut probe = PlanSharing {
        inner: MonitorProbe::new(app, group, for_model, DEFAULT_REFRESH_INTERVAL),
        slot: slot.clone(),
        profile_override: args.profile.clone(),
    };
    let mut sink = MonitorSink {
        io,
        slot: slot.clone(),
        stop: stop.clone(),
        json_only: args.json,
        json_stream: args.json_stream,
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
    let write_err = sink.error;
    match write_err {
        // A closed reader (`| head -3`) is not an error; the loop simply
        // ends instead of spinning with nowhere to write.
        None => {
            if args.json {
                let snap = slot
                    .lock()
                    .expect("plan slot poisoned")
                    .as_ref()
                    .and_then(|v| v.snapshot.clone())
                    .ok_or_else(|| Error::backend("monitor did not produce a snapshot"))?;
                print_json(io, &snap)?;
            }
            Ok(exit::OK)
        }
        Some(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(exit::OK),
        Some(e) => Err(Error::io("writing monitor output", e)),
    }
}
