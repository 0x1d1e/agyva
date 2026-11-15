//! Dispatch from parsed arguments to core operations and rendering.

use std::io::{BufRead, Write};

use clap::CommandFactory;
use clap_complete::{Shell, generate};
use serde::Serialize;

use super::output::{self, Style};
use super::usage_out;
use super::{Cli, Command, ConfigCommand, ShellName};
use crate::config::{ColorChoice, KEYS};
use crate::core::add::AddOptions;
use crate::core::app::App;
use crate::core::error::{Error, Result, exit};
use crate::core::recover::{RecoverMode, RecoverOutcome};
use crate::core::usage::UsageOptions;
use crate::paths::Env;

/// Terminal facts and streams, injected so behavior is testable.
pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub stdout_tty: bool,
    pub stdin_tty: bool,
}

fn w(e: std::io::Error) -> Error {
    Error::io("writing output", e)
}

fn print_json<T: Serialize>(io: &mut Io<'_>, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(|e| Error::format(e.to_string()))?;
    writeln!(io.out, "{text}").map_err(w)
}

fn style(app: &App, io: &Io<'_>, env: &Env) -> Style {
    let color = match app.config.color {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => io.stdout_tty && env.get("NO_COLOR").is_none(),
    };
    Style { color }
}

fn confirm(io: &mut Io<'_>, question: &str) -> Result<bool> {
    if !io.stdin_tty {
        return Err(Error::usage(
            "refusing to continue without confirmation: input is not a terminal",
        )
        .hint("pass --yes to confirm non-interactively"));
    }
    write!(io.err, "{question} [y/N] ").map_err(w)?;
    io.err.flush().map_err(w)?;
    let mut line = String::new();
    std::io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| Error::io("reading answer", e))?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn notice(io: &mut Io<'_>, text: Option<&str>) -> Result<()> {
    if let Some(t) = text {
        writeln!(io.err, "\n{t}\n").map_err(w)?;
    }
    Ok(())
}

/// Fish: complete the first argument of the selector commands with the saved
/// aliases, read from `agyva list --json` (read-only; no keyring access).
const FISH_ALIASES: &str = r#"
# No agyva argument is a file path, so never fall back to file completion.
complete -c agyva -f

# Saved aliases for the commands that take an account selector.
function __agyva_aliases
    agyva list --json 2>/dev/null | string match -rg '"alias":\s*"([^"]+)"'
end

function __agyva_needs_selector
    set -l tokens (commandline -opc)
    set -e tokens[1]
    set -l sub ''
    set -l positional 0
    for t in $tokens
        string match -q -- '-*' $t; and continue
        if test -z "$sub"
            set sub $t
        else
            set positional (math $positional + 1)
        end
    end
    contains -- $sub use rename remove usage health; and test $positional -eq 0
end

complete -c agyva -n __agyva_needs_selector -f -a '(__agyva_aliases)' -d 'saved account'
"#;

/// Run one invocation; returns the process exit code.
pub fn run(cli: Cli, env: Env, io: &mut Io<'_>) -> Result<i32> {
    let Some(command) = cli.command else {
        // Bare `agyva`: the interactive view on a terminal, help otherwise.
        // Both ends must be terminals: it reads keys and draws a full screen.
        if io.stdout_tty && io.stdin_tty {
            return crate::tui::run_tui(env, cli.allow_untested);
        }
        Cli::command().write_long_help(&mut io.out).map_err(w)?;
        writeln!(io.out).map_err(w)?;
        return Ok(exit::OK);
    };
    if let Command::Completions { shell } = &command {
        let shell = match shell {
            ShellName::Fish => Shell::Fish,
            ShellName::Bash => Shell::Bash,
            ShellName::Zsh => Shell::Zsh,
        };
        // Render into memory first: the generator panics on a write error, and
        // `agyva completions fish | head` closes the pipe early.
        let mut script = Vec::new();
        generate(shell, &mut Cli::command(), "agyva", &mut script);
        if matches!(shell, Shell::Fish) {
            script.extend_from_slice(FISH_ALIASES.as_bytes());
        }
        return match io.out.write_all(&script) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(exit::OK),
            other => other.map(|()| exit::OK).map_err(w),
        };
    }
    let app = App::from_env(env.clone(), cli.allow_untested)?;
    let st = style(&app, io, &env);
    dispatch(&app, command, io, st)
}

fn dispatch(app: &App, command: Command, io: &mut Io<'_>, st: Style) -> Result<i32> {
    match command {
        Command::Add(a) => {
            let r = app.add(&AddOptions {
                alias: a.alias,
                refresh: a.refresh,
            })?;
            let verb = if r.refreshed { "Updated" } else { "Saved" };
            writeln!(io.out, "{verb} {}", output::describe(&r.account)).map_err(w)?;
            notice(io, r.policy_notice)?;
        }
        Command::List(f) => {
            let v = app.list()?;
            if f.json {
                print_json(io, &v)?
            } else {
                write!(io.out, "{}", output::render_list(&v, st)).map_err(w)?
            }
        }
        Command::Use(a) => {
            let r = app.switch(&a.selector, a.force)?;
            if r.already_active {
                writeln!(io.out, "{} is already active.", r.account.alias).map_err(w)?;
            } else {
                writeln!(io.out, "Switched to {}", output::describe(&r.account)).map_err(w)?;
            }
            for warning in &r.warnings {
                writeln!(io.err, "note: {warning}").map_err(w)?;
            }
            notice(io, r.policy_notice)?;
        }
        Command::Current(f) => {
            let v = app.current()?;
            if f.json {
                print_json(io, &v)?
            } else {
                write!(io.out, "{}", output::render_current(&v)).map_err(w)?
            }
        }
        Command::Rename(a) => {
            let before = app.resolve(&a.selector)?;
            let after = app.rename(&a.selector, &a.new_alias)?;
            writeln!(io.out, "Renamed {} to {}", before.alias, after.alias).map_err(w)?;
        }
        Command::Remove(a) => {
            let target = app.resolve(&a.selector)?;
            if !a.yes
                && !confirm(
                    io,
                    &format!("Remove saved account {}?", output::describe(&target)),
                )?
            {
                writeln!(io.err, "Cancelled.").map_err(w)?;
                return Ok(exit::OK);
            }
            let r = app.remove(&a.selector, a.force)?;
            writeln!(
                io.out,
                "Removed {}. agy's own login was not touched.",
                r.alias
            )
            .map_err(w)?;
        }
        Command::Usage(a) => {
            let report = app.usage_report(&UsageOptions {
                selector: a.selector.clone(),
                all: a.all,
                refresh: a.mode(),
            })?;
            if a.json {
                print_json(io, &report)?
            } else {
                write!(io.out, "{}", usage_out::render_usage(&report, st)).map_err(w)?
            }
        }
        Command::Watch => return super::watch_cmd::watch(app, io),
        Command::Health(a) => {
            let report = app.health_report(&UsageOptions {
                selector: a.selector.clone(),
                all: true,
                refresh: a.mode(),
            })?;
            if a.json {
                print_json(io, &report)?
            } else {
                write!(io.out, "{}", usage_out::render_health(&report, st)).map_err(w)?
            }
        }
        Command::Status(f) => {
            let v = app.status()?;
            if f.json {
                print_json(io, &v)?
            } else {
                write!(io.out, "{}", output::render_status(&v)).map_err(w)?
            }
            if v.recovery_pending.is_some() {
                return Ok(exit::RECOVERY_REQUIRED);
            }
        }
        Command::Doctor(f) => {
            let r = app.doctor();
            if f.json {
                print_json(io, &r)?
            } else {
                write!(io.out, "{}", output::render_doctor(&r)).map_err(w)?
            }
            if r.recovery_pending {
                return Ok(exit::RECOVERY_REQUIRED);
            }
            if !r.healthy {
                return Ok(exit::ERROR);
            }
        }
        Command::Recover(a) => {
            let mode = if a.rollback {
                RecoverMode::Rollback
            } else {
                RecoverMode::Auto
            };
            let msg = match app.recover_with(mode, a.force)? {
                RecoverOutcome::NothingToDo => "Nothing to recover.".to_owned(),
                RecoverOutcome::RolledBack {
                    target,
                    kept_checkpoint,
                } => {
                    let mut m = format!(
                        "Rolled back the interrupted switch to {target}; your previous login is restored."
                    );
                    if let Some(name) = kept_checkpoint {
                        m.push_str(&format!(
                            "\nThe login that was in place before this rollback is kept, encrypted, in {} and is NOT pruned automatically.",
                            app.dirs.checkpoint_dir().join(name).display()
                        ));
                    }
                    m
                }
                RecoverOutcome::RolledForward { target } => {
                    format!("Completed the interrupted switch to {target}. Restart agy.")
                }
            };
            writeln!(io.out, "{msg}").map_err(w)?;
        }
        Command::Config(c) => config(app, c, io)?,
        Command::Version => {
            writeln!(
                io.out,
                "agyva {}\n{}",
                env!("CARGO_PKG_VERSION"),
                app.compat().summary()
            )
            .map_err(w)?;
        }
        Command::Completions { .. } => {}
    }
    Ok(exit::OK)
}

fn config(app: &App, c: ConfigCommand, io: &mut Io<'_>) -> Result<()> {
    match c {
        ConfigCommand::Show => {
            for (key, help) in KEYS {
                let value = app.config.get(key).unwrap_or_default();
                writeln!(io.out, "{key} = {value}    # {help}").map_err(w)?;
            }
        }
        ConfigCommand::Set { key, value } => {
            let _lock = app.lock_for_config()?;
            let mut cfg = crate::config::Config::load(&app.dirs.config_file())?;
            cfg.set(&key, &value)?;
            cfg.save(&app.dirs.config_file())?;
            writeln!(io.out, "{key} = {}", cfg.get(&key).unwrap_or_default()).map_err(w)?;
        }
        ConfigCommand::Path => {
            writeln!(io.out, "{}", app.dirs.config_file().display()).map_err(w)?
        }
    }
    Ok(())
}
