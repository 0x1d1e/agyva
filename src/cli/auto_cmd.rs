//! `agyva auto`: foreground rotator for one model family.
//!
//! `--once` is the test path. The loop honors the same hidden interval and
//! cycle overrides as `watch`, and only when test mode is on, so a real run
//! does not hammer the quota endpoint.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use chrono::Utc;

use super::Io;
use super::commands::{print_json, w};
use crate::core::app::App;
use crate::core::error::{Error, Result, exit};
use crate::core::failpoint::ENV_TEST_MODE;
use crate::core::model_auto::{ModelAutoOnce, ModelFamily};
use crate::paths::Env;

const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);
const ENV_INTERVAL_MS: &str = "AGYVA_WATCH_INTERVAL_MS";
const ENV_MAX_CYCLES: &str = "AGYVA_WATCH_MAX_CYCLES";

pub fn auto_cmd(
    app: &App,
    io: &mut Io<'_>,
    model: ModelFamily,
    once: bool,
    json: bool,
) -> Result<i32> {
    let once = once || json;
    let max_cycles = if once {
        Some(1)
    } else {
        cycle_limit(&app.env)?
    };
    let interval = resolve_interval(&app.env)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    if let Err(e) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
        tracing::warn!("cannot install the Ctrl+C handler: {e}");
    }
    let mut cycles = 0u64;
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(exit::OK);
        }
        let result = app.model_auto_once(model, Utc::now())?;
        if json {
            print_json(io, &result)?;
        } else {
            write_human(io, &result)?;
        }
        cycles += 1;
        if max_cycles.is_some_and(|n| cycles >= n) || stop.load(Ordering::SeqCst) {
            return Ok(exit::OK);
        }
        if !sleep_or_stop(interval, &stop) {
            return Ok(exit::OK);
        }
    }
}

fn write_human(io: &mut Io<'_>, result: &ModelAutoOnce) -> Result<()> {
    writeln!(io.out, "{}", result.message).map_err(w)?;
    for warning in &result.warnings {
        writeln!(io.out, "note: {warning}").map_err(w)?;
    }
    if let Some(notice) = result.restart_notice {
        writeln!(io.out, "{notice}").map_err(w)?;
    }
    Ok(())
}

fn cycle_limit(env: &Env) -> Result<Option<u64>> {
    match env.get(ENV_MAX_CYCLES) {
        None => Ok(None),
        Some(v) => Ok(Some(v.parse::<u64>().map_err(|_| {
            Error::usage(format!("invalid {ENV_MAX_CYCLES}: {v}"))
        })?)),
    }
}

fn resolve_interval(env: &Env) -> Result<Duration> {
    match env.get(ENV_INTERVAL_MS) {
        Some(v) if env.get(ENV_TEST_MODE) == Some("1") => {
            let ms = v
                .parse::<u64>()
                .map_err(|_| Error::usage(format!("invalid {ENV_INTERVAL_MS}: {v}")))?;
            if ms == 0 {
                return Err(Error::usage(format!("{ENV_INTERVAL_MS} must be positive")));
            }
            Ok(Duration::from_millis(ms))
        }
        Some(_) => {
            tracing::warn!("ignoring {ENV_INTERVAL_MS}: it needs {ENV_TEST_MODE}=1 as well");
            Ok(DEFAULT_INTERVAL)
        }
        None => Ok(DEFAULT_INTERVAL),
    }
}

fn sleep_or_stop(total: Duration, stop: &AtomicBool) -> bool {
    let step = Duration::from_millis(200);
    let mut left = total;
    while left > Duration::ZERO {
        if stop.load(Ordering::SeqCst) {
            return false;
        }
        let slice = left.min(step);
        thread::sleep(slice);
        left = left.saturating_sub(slice);
    }
    !stop.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::policy::RESTART_NOTICE;
    use crate::core::view::VIEW_SCHEMA;

    #[test]
    fn switched_output_always_includes_the_restart_notice() {
        let result = ModelAutoOnce {
            schema_version: VIEW_SCHEMA,
            model: "gemini",
            switched: true,
            target: Some("bob".into()),
            message: "switched to bob because Gemini remaining is 5.0% (usage over 90%)".into(),
            restart_notice: Some(RESTART_NOTICE),
            warnings: vec!["the login already changed to bob; still set: endpoints.json active endpoint `0123456789`; run `agyva use bob` again".into()],
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        write_human(
            &mut Io {
                out: &mut out,
                err: &mut err,
                stdout_tty: false,
                stdin_tty: false,
            },
            &result,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Restart agy"), "{text}");
        assert!(!text.to_ascii_lowercase().contains("no restart"), "{text}");
        assert!(text.contains("note:"), "{text}");
        assert!(err.is_empty());
    }
}
