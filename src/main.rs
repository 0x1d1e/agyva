#![forbid(unsafe_code)]

use std::io::IsTerminal;
use std::io::Write as _;
use std::process::ExitCode;

use clap::Parser;

use agyva::cli::{Cli, Command, Io, run};
use agyva::core::error::ErrorKind;
use agyva::paths::Env;

fn init_logging() {
    let filter = tracing_subscriber::EnvFilter::try_from_env("AGYVA_LOG")
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .init();
}

fn is_broken_pipe(e: &agyva::core::error::Error) -> bool {
    use std::error::Error as _;
    e.kind() == ErrorKind::Io
        && e.source()
            .and_then(|s| s.downcast_ref::<std::io::Error>())
            .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
}

/// Whether the passive startup update check may run: the config says so and
/// the `AGYVA_UPDATE_CHECK=0` kill-switch is unset. Config problems fall
/// back to the default (`true`): help and version output must never fail
/// because of this check.
fn update_check_enabled(env: &Env) -> bool {
    if env.get("AGYVA_UPDATE_CHECK").is_some_and(|v| v == "0") {
        return false;
    }
    agyva::paths::Dirs::resolve(env)
        .ok()
        .and_then(|dirs| agyva::config::Config::load(&dirs.config_file()).ok())
        .is_none_or(|c| c.check_updates)
}

/// Whether this invocation shows human-readable CLI output on stdout (so an
/// update notice may go to stderr beside it). Not the interactive view
/// (stderr would corrupt the screen), not `update` (it checks itself), not
/// machine output (json/completions must stay exactly parseable).
fn notice_allowed(cli: &Cli) -> bool {
    let json = match &cli.command {
        Some(Command::List(f) | Command::Current(f) | Command::Status(f) | Command::Doctor(f)) => {
            f.json
        }
        Some(Command::Usage(a)) => a.json,
        Some(Command::Health(a)) => a.json,
        Some(Command::Models(a)) => a.json,
        Some(Command::Route(a)) => a.json,
        _ => false,
    };
    !json
        && !matches!(
            cli.command,
            None | Some(Command::Completions { .. }) | Some(Command::Update { .. })
        )
}

fn main() -> ExitCode {
    // try_parse (not parse): help/version and usage errors come back here,
    // so every enabled startup — including those exits — can run the update
    // check, while the check itself stays passive and can never turn a
    // `--help` into a network failure.
    let cli = match Cli::try_parse() {
        Ok(cli) => Some(cli),
        Err(e) => {
            // Help/version print to stdout and exit 0; usage errors to
            // stderr with code 2. Only the informational exits check.
            let informational = !e.use_stderr();
            let _ = e.print();
            let env = Env::from_process();
            if informational
                && update_check_enabled(&env)
                && let Some(notice) = agyva::updater::startup_check_cached(
                    true,
                    agyva::paths::Dirs::resolve(&env)
                        .ok()
                        .map(|d| d.update_check_cache_file())
                        .as_deref(),
                    chrono::Utc::now(),
                )
                .and_then(|c| c.notice())
            {
                eprintln!("note: {notice}");
            }
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let cli = cli.expect("try_parse returned Ok above");
    let env = Env::from_process();
    let stdout_tty = std::io::stdout().is_terminal();
    let stdin_tty = std::io::stdin().is_terminal();
    // The interactive view owns the screen: log lines on stderr would corrupt it.
    let tui = cli.command.is_none() && stdout_tty && stdin_tty;
    if !tui {
        init_logging();
    }
    let mut stdout = std::io::stdout().lock();
    let mut stderr = std::io::stderr().lock();
    let mut io = Io {
        stdout_tty,
        stdin_tty,
        out: &mut stdout,
        err: &mut stderr,
    };
    // The interactive view gets its notice on the dashboard (delivered by
    // the TUI worker, never stderr in the alternate screen). Every other
    // enabled startup with human-readable output checks here; `agyva update`
    // performs its own single metadata request instead, and `--json` /
    // `completions` never trigger an update check.
    if !tui
        && notice_allowed(&cli)
        && update_check_enabled(&env)
        && let Some(notice) = agyva::updater::startup_check_cached(
            true,
            agyva::paths::Dirs::resolve(&env)
                .ok()
                .map(|d| d.update_check_cache_file())
                .as_deref(),
            chrono::Utc::now(),
        )
        .and_then(|c| c.notice())
    {
        let _ = writeln!(io.err, "note: {notice}");
    }
    let code = match run(cli, env, &mut io) {
        Ok(code) => code,
        // A reader that closed the pipe early (`agyva list | head`) is not an error.
        Err(e) if is_broken_pipe(&e) => 0,
        Err(e) => {
            let _ = std::io::stdout().flush();
            if e.kind() == ErrorKind::SimulatedCrash {
                eprintln!("{}", e.message());
            } else {
                eprintln!("error: {}", e.message());
                if let Some(h) = e.user_hint() {
                    eprintln!("hint: {h}");
                }
            }
            e.exit_code()
        }
    };
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notice_allowed_skips_models_and_route_json() {
        let models_json = Cli::try_parse_from(["agyva", "models", "--json"]).unwrap();
        assert!(!notice_allowed(&models_json));

        let route_plan_json =
            Cli::try_parse_from(["agyva", "route", "plan", "work", "--json"]).unwrap();
        assert!(!notice_allowed(&route_plan_json));

        let models_plain = Cli::try_parse_from(["agyva", "models"]).unwrap();
        assert!(notice_allowed(&models_plain));

        let route_plan_plain = Cli::try_parse_from(["agyva", "route", "plan", "work"]).unwrap();
        assert!(notice_allowed(&route_plan_plain));
    }
}
