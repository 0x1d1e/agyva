//! Dispatch from parsed arguments to core operations and rendering.

use std::io::{BufRead, Write};

use clap::CommandFactory;
use clap_complete::{Shell, generate};
use serde::Serialize;

use super::output::{self, Style};
use super::profiles_cmd::print_profile_use;
use super::usage_out;
use super::{CheckpointsCommand, Cli, Command, ConfigCommand, ShellName, StatuslineFormat};
use crate::config::KEYS;
use crate::core::add::AddOptions;
use crate::core::app::App;
use crate::core::app_integrations::UnifiedUseOutcome;
use crate::core::checkpoints::CheckpointInfo;
use crate::core::error::{Error, Result, exit};
use crate::core::recover::{RecoverMode, RecoverOutcome};
use crate::core::route_config::RouteGroup;
use crate::core::usage::UsageOptions;
use crate::core::view::VIEW_SCHEMA;
use crate::paths::Env;
use crate::updater::HttpTransport;

pub struct Io<'a> {
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
    pub stdout_tty: bool,
    pub stdin_tty: bool,
}

pub(super) fn w(e: std::io::Error) -> Error {
    Error::io("writing output", e)
}

/// The ranking mode as `routing.toml` spells it.
fn ranking_label(ranking: crate::core::route_config::Ranking) -> &'static str {
    match ranking {
        crate::core::route_config::Ranking::Smart => "smart",
        crate::core::route_config::Ranking::Priority => "priority",
        crate::core::route_config::Ranking::LeastUsed => "least-used",
    }
}

pub(super) fn print_json<T: Serialize>(io: &mut Io<'_>, value: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(value).map_err(|e| Error::format(e.to_string()))?;
    writeln!(io.out, "{text}").map_err(w)
}

fn style(app: &App, io: &Io<'_>, env: &Env) -> Style {
    Style {
        color: output::color_enabled(
            app.config.color,
            env.get("NO_COLOR").is_some_and(|value| !value.is_empty()),
            io.stdout_tty,
        ),
    }
}

pub(super) fn confirm(io: &mut Io<'_>, question: &str) -> Result<bool> {
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

fn notice(io: &mut Io<'_>, text: Option<&str>, quiet: bool) -> Result<()> {
    if quiet {
        return Ok(());
    }
    if let Some(t) = text {
        writeln!(io.err, "\n{t}\n").map_err(w)?;
    }
    Ok(())
}

/// Fish: complete account selectors, custom endpoints (`ep:<name>`), profiles,
/// and checkpoints dynamically (read-only; no keyring access).
const FISH_DYNAMIC_COMPLETIONS: &str = r#"
# No agyva argument is a file path, so never fall back to file completion.
complete -c agyva -f

# Saved aliases for the commands that take an account selector.
function __agyva_aliases
    agyva list --json 2>/dev/null | string match -rg '"alias":\s*"([^"]+)"'
end

function __agyva_endpoints
    agyva endpoint list --json 2>/dev/null | string match -rg '"alias":\s*"([^"]+)"'
end

function __agyva_profiles
    agyva profile list --json 2>/dev/null | string match -rg '"name":\s*"([^"]+)"'
end

function __agyva_checkpoints
    agyva checkpoints list --json 2>/dev/null | string match -rg '"name":\s*"([^"]+)"'
end

function __agyva_use_targets
    __agyva_aliases
    for ep in (__agyva_endpoints)
        echo "ep:$ep"
    end
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
    contains -- $sub rename remove usage health; and test $positional -eq 0
end

function __agyva_needs_use_target
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
    contains -- $sub use switch; and test $positional -eq 0
end

function __agyva_needs_endpoint_selector
    set -l tokens (commandline -opc)
    test (count $tokens) -ge 3; or return 1
    contains -- $tokens[2] endpoint ep; and contains -- $tokens[3] show use test models update rename remove
end

function __agyva_needs_profile_name
    set -l tokens (commandline -opc)
    test (count $tokens) -ge 3; or return 1
    contains -- $tokens[2] profile prof; and contains -- $tokens[3] show delete use set
end

function __agyva_needs_checkpoint_name
    set -l tokens (commandline -opc)
    test (count $tokens) -ge 3; or return 1
    contains -- $tokens[2] checkpoints ckpt; and test "$tokens[3]" = restore
end

complete -c agyva -n __agyva_needs_selector -f -a '(__agyva_aliases)' -d 'saved account'
complete -c agyva -n __agyva_needs_use_target -f -a '(__agyva_use_targets)' -d 'account or ep:<endpoint>'
complete -c agyva -n __agyva_needs_endpoint_selector -f -a '(__agyva_endpoints)' -d 'custom endpoint'
complete -c agyva -n __agyva_needs_profile_name -f -a '(__agyva_profiles)' -d 'profile'
complete -c agyva -n __agyva_needs_checkpoint_name -f -a '(__agyva_checkpoints)' -d 'checkpoint'
complete -c agyva -n '__fish_seen_subcommand_from bridge env exec' -l endpoint -f -a '(__agyva_endpoints)' -d 'custom endpoint'
complete -c agyva -n '__fish_seen_subcommand_from use switch env exec statusline usage watch monitor mon' -l profile -f -a '(__agyva_profiles)' -d 'profile'
complete -c agyva -n '__fish_seen_subcommand_from use switch exec' -l account -f -a '(__agyva_aliases)' -d 'saved account'
"#;

/// Run one invocation; returns the process exit code.
pub fn run(cli: Cli, env: Env, io: &mut Io<'_>) -> Result<i32> {
    let quiet = cli.quiet;
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
            ShellName::Powershell => Shell::PowerShell,
            ShellName::Elvish => Shell::Elvish,
        };
        // Render into memory first: the generator panics on a write error, and
        // `agyva completions fish | head` closes the pipe early.
        let mut script = Vec::new();
        generate(shell, &mut Cli::command(), "agyva", &mut script);
        if matches!(shell, Shell::Fish) {
            script.extend_from_slice(FISH_DYNAMIC_COMPLETIONS.as_bytes());
        }
        return match io.out.write_all(&script) {
            Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(exit::OK),
            other => other.map(|()| exit::OK).map_err(w),
        };
    }
    let app = App::from_env(env.clone(), cli.allow_untested)?;
    let st = style(&app, io, &env);
    dispatch(&app, command, io, st, quiet)
}

fn dispatch(app: &App, command: Command, io: &mut Io<'_>, st: Style, quiet: bool) -> Result<i32> {
    match command {
        Command::Add(a) => {
            let r = app.add(&AddOptions {
                alias: a.alias,
                refresh: a.refresh,
            })?;
            let verb = if r.refreshed { "Updated" } else { "Saved" };
            writeln!(io.out, "{verb} {}", output::describe(&r.account)).map_err(w)?;
            notice(io, r.policy_notice, quiet)?;
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
            return cmd_use(app, a, io, quiet);
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
                if !quiet {
                    writeln!(io.err, "Cancelled.").map_err(w)?;
                }
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
        Command::Endpoint(a) => {
            return super::endpoints_cmd::handle_endpoint(app, a.command, a.json, quiet, io, st);
        }
        Command::Bridge(a) => {
            return super::endpoints_cmd::handle_bridge(app, a, io);
        }
        Command::Env(a) => {
            return super::endpoints_cmd::handle_env(app, a, io);
        }
        Command::Exec(a) => {
            return super::endpoints_cmd::handle_exec(app, a, io);
        }
        Command::Profile(a) => {
            return super::profiles_cmd::handle_profile(app, a.command, a.json, quiet, io, st);
        }
        Command::Statusline(a) => {
            let fmt = match a.format {
                Some(f) => f,
                None => match app.config.statusline_format.as_str() {
                    "full" => StatuslineFormat::Full,
                    "json" => StatuslineFormat::Json,
                    _ => StatuslineFormat::Compact,
                },
            };
            let data = app.statusline(a.profile.as_deref(), None)?;
            let line = output::render_statusline(&data, fmt)?;
            writeln!(io.out, "{line}").map_err(w)?;
        }
        Command::Usage(a) => {
            let profile = match a.profile.as_deref() {
                Some(name) => app.resolve_active_profile(Some(name), None)?,
                None => None,
            };
            let selector = a.selector.clone().or_else(|| {
                if a.all {
                    None
                } else {
                    profile.as_ref().and_then(|p| p.spec.account.clone())
                }
            });
            let report = app.usage_report(&UsageOptions {
                selector,
                all: a.all,
                refresh: a.mode(),
            })?;
            if a.json {
                print_json(io, &report)?
            } else if a.compact {
                write!(io.out, "{}", usage_out::render_usage_compact(&report, st)).map_err(w)?
            } else {
                write!(io.out, "{}", usage_out::render_usage(&report, st)).map_err(w)?
            }
        }
        Command::Route(a) => {
            return cmd_route(app, a, io, quiet);
        }
        Command::Watch(a) => return super::watch_cmd::watch(app, io, &a),
        Command::Monitor(a) => return super::monitor_cmd::monitor(app, io, &a),
        Command::Auto(a) => {
            let family = match a.model {
                super::AutoModel::Gemini => crate::core::model_auto::ModelFamily::Gemini,
                super::AutoModel::Claude => crate::core::model_auto::ModelFamily::Claude,
            };
            return super::auto_cmd::auto_cmd(app, io, family, a.once, a.json);
        }
        Command::Models(a) => {
            let report = app.models_report(a.refresh, chrono::Utc::now())?;
            if a.json {
                print_json(io, &report)?
            } else {
                write!(io.out, "{}", usage_out::render_models(&report, st)).map_err(w)?
            }
        }
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
        Command::Doctor(a) => {
            if a.fix {
                let r = app.doctor_fix()?;
                if a.json {
                    print_json(io, &r)?
                } else {
                    write!(io.out, "{}", output::render_doctor_fix(&r)).map_err(w)?
                }
                if r.report.recovery_pending {
                    return Ok(exit::RECOVERY_REQUIRED);
                }
                if !r.report.healthy {
                    return Ok(exit::ERROR);
                }
            } else {
                let r = app.doctor();
                if a.json {
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
        }
        Command::Checkpoints(a) => {
            return cmd_checkpoints(app, a.command, a.json, quiet, io, st);
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
        Command::Rekey(a) => {
            let r = app.rekey(a.mode.map(Into::into))?;
            writeln!(
                io.out,
                "Rekeyed vault (mode: {}, {} snapshot(s), {} checkpoint(s)).",
                r.mode, r.snapshots, r.checkpoints
            )
            .map_err(w)?;
            if !quiet {
                for warning in &r.warnings {
                    writeln!(io.err, "note: {warning}").map_err(w)?;
                }
            }
        }
        Command::Export(a) => {
            let r = app.export_archive(&a.file, a.account.as_deref())?;
            writeln!(
                io.out,
                "Exported {} account(s) to {}",
                r.accounts.len(),
                r.file.display()
            )
            .map_err(w)?;
        }
        Command::Import { file } => {
            let r = app.import_archive(&file)?;
            writeln!(
                io.out,
                "Imported {} account(s), skipped {} already saved.",
                r.added.len(),
                r.skipped.len()
            )
            .map_err(w)?;
            if !quiet {
                for warning in &r.warnings {
                    writeln!(io.err, "note: {warning}").map_err(w)?;
                }
            }
        }
        Command::Update { check, .. } => {
            let r = crate::updater::install_update(check, Box::new(HttpTransport), None)?;
            match r.notice() {
                Some(text) => writeln!(io.out, "{text}").map_err(w)?,
                None => writeln!(io.out, "agyva {} is up to date.", r.current).map_err(w)?,
            }
        }
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

fn cmd_use(app: &App, a: super::UseArgs, io: &mut Io<'_>, quiet: bool) -> Result<i32> {
    if let Some(prof_name) = a.profile.as_deref() {
        let outcome = app.use_profile(prof_name, None, a.force)?;
        if a.json {
            print_json(io, &outcome)?;
        } else {
            print_profile_use(io, &outcome, quiet)?;
        }
        return Ok(exit::OK);
    }

    if let Some(ep_sel) = a.endpoint.as_deref() {
        let ep = app.use_endpoint(ep_sel)?;
        let outcome = UnifiedUseOutcome::Endpoint { endpoint: ep };
        if a.json {
            print_json(io, &outcome)?;
        } else if let UnifiedUseOutcome::Endpoint { endpoint } = &outcome {
            writeln!(
                io.out,
                "Switched to endpoint {} [{}] ({} -> {}).",
                endpoint.alias,
                endpoint.id,
                endpoint.effective_protocol(),
                endpoint.base_url
            )
            .map_err(w)?;
        }
        return Ok(exit::OK);
    }

    let target = if let Some(acct_sel) = a.account {
        if acct_sel.trim().starts_with("ep:") {
            return Err(
                Error::usage("`--account` cannot take an `ep:` endpoint selector").hint(
                    "use `--endpoint <SELECTOR>` or pass `ep:<name>` as a positional argument",
                ),
            );
        }
        acct_sel
    } else {
        a.selector.ok_or_else(|| {
            Error::usage("provide a selector, --account, --endpoint, or --profile")
        })?
    };

    let outcome = app.unified_use(&target, a.force)?;
    if a.json {
        print_json(io, &outcome)?;
        return Ok(exit::OK);
    }
    match outcome {
        UnifiedUseOutcome::Account {
            account,
            already_active,
            warnings,
            policy_notice,
        } => {
            if already_active {
                writeln!(io.out, "{} is already active.", account.alias).map_err(w)?;
            } else {
                writeln!(io.out, "Switched to {}", output::describe(&account)).map_err(w)?;
            }
            if !quiet {
                for warning in &warnings {
                    writeln!(io.err, "note: {warning}").map_err(w)?;
                }
            }
            notice(io, policy_notice, quiet)?;
        }
        UnifiedUseOutcome::Endpoint { endpoint } => {
            writeln!(
                io.out,
                "Switched to endpoint {} [{}] ({} -> {}).",
                endpoint.alias,
                endpoint.id,
                endpoint.effective_protocol(),
                endpoint.base_url
            )
            .map_err(w)?;
        }
    }
    Ok(exit::OK)
}

fn cmd_route(app: &App, a: super::RouteArgs, io: &mut Io<'_>, quiet: bool) -> Result<i32> {
    let routing_path = app.dirs.config.join("routing.toml");
    match &a.command {
        Some(super::RouteCommand::Init { yes: _ }) => {
            let reg = app.load_registry()?;
            let aliases: Vec<String> = reg.accounts.iter().map(|acc| acc.alias.clone()).collect();
            let file = crate::core::route_config::init_default(&routing_path, &aliases)?;
            if a.json {
                print_json(io, &file)?;
            } else {
                writeln!(
                    io.out,
                    "Initialized {} with group `default` ({} account(s)).",
                    routing_path.display(),
                    aliases.len()
                )
                .map_err(w)?;
            }
            return Ok(exit::OK);
        }
        Some(super::RouteCommand::Add {
            group,
            accounts,
            ranking,
            auto_switch,
        }) => {
            let rg = RouteGroup {
                accounts: accounts.clone(),
                ranking: (*ranking).into(),
                auto_switch: *auto_switch,
                ..RouteGroup::default()
            };
            let file = crate::core::route_config::upsert_group(&routing_path, group, rg)?;
            if a.json {
                print_json(io, &file)?;
            } else {
                writeln!(io.out, "Saved routing group `{group}`.").map_err(w)?;
            }
            return Ok(exit::OK);
        }
        Some(super::RouteCommand::Remove { group, yes }) => {
            if a.json && !*yes {
                return Err(Error::usage(
                    "`route remove --json` needs --yes: it never prompts on stdout",
                )
                .hint("add --yes to confirm non-interactively"));
            }
            if !*yes && !confirm(io, &format!("Remove routing group `{group}`?"))? {
                if !quiet {
                    writeln!(io.err, "Cancelled.").map_err(w)?;
                }
                return Ok(exit::OK);
            }
            let file = crate::core::route_config::remove_group(&routing_path, group)?;
            if a.json {
                print_json(io, &file)?;
            } else {
                writeln!(io.out, "Removed routing group `{group}`.").map_err(w)?;
            }
            return Ok(exit::OK);
        }
        Some(super::RouteCommand::List) => {
            let cfg = crate::core::route_config::RoutingFile::load(&routing_path)?;
            if a.json {
                print_json(io, &cfg)?;
            } else {
                let ack = app.config.auto_switch_ack;
                for (name, group) in &cfg.groups {
                    let mode = match (group.auto_switch, ack) {
                        (false, _) => "manual activation".to_owned(),
                        (true, false) => {
                            "auto-switch opted in; run `agyva config set auto_switch_ack true`"
                                .to_owned()
                        }
                        (true, true) => "auto-switch enabled (acknowledged)".to_owned(),
                    };
                    writeln!(
                        io.out,
                        "{name}: {}, {mode}, {} listed account(s)",
                        ranking_label(group.ranking),
                        group.accounts.len()
                    )
                    .map_err(w)?;
                }
                if cfg.groups.is_empty() {
                    writeln!(io.out, "No named groups; define groups in routing.toml.")
                        .map_err(w)?;
                }
            }
            return Ok(exit::OK);
        }
        Some(super::RouteCommand::Apply { group, yes, force }) => {
            let history = crate::core::route_apply::RouteHistory::new(&app.dirs);
            let now = chrono::Utc::now();
            let plan = app.route_apply_plan(group, None, now)?;
            let mut plan_warnings = Vec::new();
            if let Some(w) = &plan.session_warning {
                plan_warnings.push(w.clone());
            }
            if a.json && !*yes {
                for warning in &plan_warnings {
                    writeln!(io.err, "warning: {warning}").map_err(w)?;
                }
                return Err(Error::usage(
                    "`route apply --json` needs --yes: it never prompts on stdout",
                )
                .hint("add --yes to confirm non-interactively"));
            }
            if !*yes {
                let mut question = format!(
                    "Switch to {} (recommendation for `{}`)?",
                    plan.target_alias, plan.group
                );
                if let Some(w) = &plan.session_warning {
                    question.push_str(&format!("\nwarning: {w}"));
                }
                let ok = confirm(io, &question)?;
                if !ok {
                    writeln!(io.out, "Cancelled; nothing changed.").map_err(w)?;
                    return Ok(exit::OK);
                }
            }
            let now = chrono::Utc::now();
            let outcome = app.route_apply(group, None, *force, true, &history, now)?;
            if a.json {
                #[derive(Serialize)]
                struct AppliedJson<'a> {
                    group: &'a str,
                    target: &'a str,
                    target_alias: &'a str,
                    changed: bool,
                    already_active: bool,
                    reason: &'a str,
                    declared_plan: &'a Option<String>,
                    warnings: Vec<String>,
                }
                print_json(
                    io,
                    &AppliedJson {
                        group: &plan.group,
                        target: &plan.target_id,
                        target_alias: &plan.target_alias,
                        changed: !outcome.already_active,
                        already_active: outcome.already_active,
                        reason: &plan.reason,
                        declared_plan: &plan.declared_plan,
                        warnings: plan_warnings
                            .iter()
                            .cloned()
                            .chain(outcome.warnings.iter().cloned())
                            .collect(),
                    },
                )?;
            } else {
                if outcome.already_active {
                    writeln!(io.out, "{} is already active.", outcome.alias).map_err(w)?;
                } else {
                    writeln!(io.out, "Switched to {}", outcome.alias).map_err(w)?;
                }
                if !quiet {
                    for warning in &outcome.warnings {
                        writeln!(io.err, "note: {warning}").map_err(w)?;
                    }
                }
            }
            return Ok(exit::OK);
        }
        Some(super::RouteCommand::History { limit }) => {
            let limit = (*limit).clamp(1, crate::core::route_apply::MAX_HISTORY_ENTRIES);
            let mut file = crate::core::route_apply::RouteHistory::new(&app.dirs).load();
            file.entries = file
                .entries
                .split_off(file.entries.len().saturating_sub(limit));
            file.entries.reverse();
            if a.json {
                print_json(io, &file)?;
            } else {
                for e in &file.entries {
                    writeln!(
                        io.out,
                        "{} {} group={} target={} {}",
                        e.at.format("%Y-%m-%d %H:%M:%S UTC"),
                        match e.outcome {
                            crate::core::route_apply::HistoryOutcome::Applied => "applied",
                            crate::core::route_apply::HistoryOutcome::Refused => "refused",
                            crate::core::route_apply::HistoryOutcome::Failed => "failed",
                        },
                        e.group.as_deref().unwrap_or("-"),
                        e.target,
                        e.reason,
                    )
                    .map_err(w)?;
                }
                if file.entries.is_empty() {
                    writeln!(io.out, "No routing decisions recorded yet.").map_err(w)?;
                }
            }
            return Ok(exit::OK);
        }
        _ => {}
    }

    let (group, for_model) = match &a.command {
        Some(super::RouteCommand::Inspect { group, for_model }) => {
            (Some(group.as_str()), for_model.as_deref())
        }
        Some(super::RouteCommand::Plan { group, for_model }) => {
            (group.as_deref(), for_model.as_deref())
        }
        _ => (None, None),
    };
    let report = app.route_group_for(group, for_model, chrono::Utc::now())?;
    if a.json {
        print_json(io, &report)?
    } else {
        for row in &report.accounts {
            writeln!(
                io.out,
                "{}{}: {} [{}]",
                if report.recommendation.as_ref() == Some(&row.id) {
                    "* "
                } else {
                    "  "
                },
                row.alias,
                row.why,
                row.command
            )
            .map_err(w)?;
        }
        if report.recommendation.is_none() {
            writeln!(io.out, "No eligible account in the cache.").map_err(w)?;
        }
    }
    Ok(exit::OK)
}

fn cmd_checkpoints(
    app: &App,
    cmd: CheckpointsCommand,
    json: bool,
    quiet: bool,
    io: &mut Io<'_>,
    st: Style,
) -> Result<i32> {
    #[derive(Serialize)]
    struct CheckpointsListView {
        schema_version: u32,
        checkpoints: Vec<CheckpointInfo>,
    }

    match cmd {
        CheckpointsCommand::List => {
            let items = app.list_checkpoints()?;
            if json {
                print_json(
                    io,
                    &CheckpointsListView {
                        schema_version: VIEW_SCHEMA,
                        checkpoints: items,
                    },
                )?;
            } else {
                write!(io.out, "{}", output::render_checkpoints(&items, st)).map_err(w)?;
            }
        }
        CheckpointsCommand::Restore { name, force, yes } => {
            if json && !yes {
                return Err(Error::usage(
                    "`checkpoints restore --json` needs --yes: it never prompts on stdout",
                )
                .hint("add --yes to confirm non-interactively"));
            }
            if !yes && !confirm(io, &format!("Restore checkpoint `{name}`?"))? {
                if !quiet {
                    writeln!(io.err, "Cancelled.").map_err(w)?;
                }
                return Ok(exit::OK);
            }
            let res = app.restore_checkpoint(&name, force)?;
            if json {
                print_json(io, &res)?;
            } else {
                let acct = res.active_account.as_deref().unwrap_or("none");
                writeln!(
                    io.out,
                    "Restored checkpoint `{}` (active account: {acct}).",
                    res.restored
                )
                .map_err(w)?;
                if !quiet {
                    for warning in &res.warnings {
                        writeln!(io.err, "note: {warning}").map_err(w)?;
                    }
                }
            }
        }
        CheckpointsCommand::Prune {
            include_pinned,
            yes,
        } => {
            if json && !yes {
                return Err(Error::usage(
                    "`checkpoints prune --json` needs --yes: it never prompts on stdout",
                )
                .hint("add --yes to confirm non-interactively"));
            }
            if !yes && !confirm(io, "Prune old checkpoints?")? {
                if !quiet {
                    writeln!(io.err, "Cancelled.").map_err(w)?;
                }
                return Ok(exit::OK);
            }
            let res = app.prune_checkpoints(include_pinned)?;
            if json {
                print_json(io, &res)?;
            } else {
                writeln!(
                    io.out,
                    "Removed {} checkpoint(s); {} remaining ({} pinned).",
                    res.removed_count, res.remaining, res.pinned_remaining
                )
                .map_err(w)?;
            }
        }
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
        ConfigCommand::Reset { key } => {
            let _lock = app.lock_for_config()?;
            let mut cfg = crate::config::Config::load(&app.dirs.config_file())?;
            cfg.reset_key(&key)?;
            cfg.save(&app.dirs.config_file())?;
            writeln!(io.out, "{key} = {}", cfg.get(&key).unwrap_or_default()).map_err(w)?;
        }
        ConfigCommand::Path => {
            writeln!(io.out, "{}", app.dirs.config_file().display()).map_err(w)?
        }
    }
    Ok(())
}
