//! CLI handlers for `agyva profile` (`prof`).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Serialize;

use super::commands::{Io, confirm, print_json, w};
use super::output::Style;
use super::{
    ProfileCommand, ProfileCreateArgs, ProfileFieldsArgs, ProfileInitArgs, ProfileSetArgs,
};
use crate::core::app::App;
use crate::core::app_integrations::ProfileUseOutcome;
use crate::core::error::{Error, ErrorKind, Result, exit};
use crate::core::format::fit;
use crate::core::profile::{
    self, ProfileSource, ProfileSpec, ProfileSummary, ResolvedProfile, load_global_profiles,
    profiles_file, save_global_profiles, write_local_profile,
};
use crate::core::view::VIEW_SCHEMA;

#[derive(Serialize)]
struct ProfileListView {
    schema_version: u32,
    profiles: Vec<ProfileSummary>,
}

#[derive(Serialize)]
struct ProfileInitView {
    schema_version: u32,
    path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    source: ProfileSource,
    profile: ProfileSpec,
}

#[derive(Serialize)]
struct ProfileShowView {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<ResolvedProfile>,
}

#[derive(Serialize)]
struct ProfileSetView {
    schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    field: String,
    profile: ProfileSpec,
}

#[derive(Serialize)]
struct ProfileDeleteView {
    schema_version: u32,
    deleted: String,
}

pub fn handle_profile(
    app: &App,
    cmd: ProfileCommand,
    json: bool,
    quiet: bool,
    io: &mut Io<'_>,
    st: Style,
) -> Result<i32> {
    match cmd {
        ProfileCommand::Init(a) => cmd_init(app, a, json, io),
        ProfileCommand::Create(a) => cmd_create(app, a, json, io),
        ProfileCommand::List => {
            let profiles = app.list_profiles(None)?;
            if json {
                print_json(
                    io,
                    &ProfileListView {
                        schema_version: VIEW_SCHEMA,
                        profiles,
                    },
                )?;
            } else if profiles.is_empty() {
                writeln!(
                    io.out,
                    "No profiles found. Create one with `agyva profile init` or `agyva profile create <NAME>`."
                )
                .map_err(w)?;
            } else {
                writeln!(
                    io.out,
                    "{}",
                    st.dim(&format!(
                        "{:<3} {:<16} {:<8} {:<14} {:<14} {}",
                        "", "NAME", "SOURCE", "ACCOUNT", "ENDPOINT", "MODEL"
                    ))
                )
                .map_err(w)?;
                for p in &profiles {
                    let marker = if p.is_default { "*" } else { " " };
                    let src = match p.source {
                        ProfileSource::Local => "local",
                        ProfileSource::Global => "global",
                        ProfileSource::Explicit => "explicit",
                    };
                    let line = format!(
                        "{:<3} {} {} {} {} {}",
                        marker,
                        fit(&p.name, 16),
                        fit(src, 8),
                        fit(p.account.as_deref().unwrap_or("-"), 14),
                        fit(p.endpoint.as_deref().unwrap_or("-"), 14),
                        p.model.as_deref().unwrap_or("-"),
                    );
                    writeln!(
                        io.out,
                        "{}",
                        if p.is_default { st.bold(&line) } else { line }
                    )
                    .map_err(w)?;
                }
            }
            Ok(exit::OK)
        }
        ProfileCommand::Show { name } => {
            let resolved = app.resolve_active_profile(name.as_deref(), None)?;
            if json {
                print_json(
                    io,
                    &ProfileShowView {
                        schema_version: VIEW_SCHEMA,
                        profile: resolved,
                    },
                )?;
            } else if let Some(p) = resolved {
                print_resolved(io, &p)?;
            } else {
                writeln!(io.out, "No active profile.").map_err(w)?;
            }
            Ok(exit::OK)
        }
        ProfileCommand::Which => {
            let resolved = app.resolve_active_profile(None, None)?;
            if json {
                print_json(
                    io,
                    &ProfileShowView {
                        schema_version: VIEW_SCHEMA,
                        profile: resolved,
                    },
                )?;
            } else if let Some(p) = resolved {
                let label = p.name.as_deref().unwrap_or("local");
                let src = match p.source {
                    ProfileSource::Local => "local",
                    ProfileSource::Global => "global",
                    ProfileSource::Explicit => "explicit",
                };
                let path = p
                    .path
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |d| d.display().to_string());
                writeln!(io.out, "{label} ({src}: {path})").map_err(w)?;
            } else {
                writeln!(io.out, "none").map_err(w)?;
            }
            Ok(exit::OK)
        }
        ProfileCommand::Set(a) => cmd_set(app, a, json, io),
        ProfileCommand::Delete { name, yes } => {
            if json && !yes {
                return Err(Error::usage(
                    "`profile delete --json` needs --yes: it never prompts on stdout",
                )
                .hint("add --yes to confirm non-interactively"));
            }
            if !yes && !confirm(io, &format!("Delete global profile `{name}`?"))? {
                if !quiet {
                    writeln!(io.err, "Cancelled.").map_err(w)?;
                }
                return Ok(exit::OK);
            }
            app.delete_profile(&name)?;
            if json {
                print_json(
                    io,
                    &ProfileDeleteView {
                        schema_version: VIEW_SCHEMA,
                        deleted: name,
                    },
                )?;
            } else {
                writeln!(io.out, "Deleted profile `{name}`.").map_err(w)?;
            }
            Ok(exit::OK)
        }
        ProfileCommand::Use { name, force } => {
            let outcome = match name.as_deref() {
                Some(n) => app.use_profile(n, None, force)?,
                None => {
                    let resolved = app.resolve_active_profile(None, None)?.ok_or_else(|| {
                        Error::new(ErrorKind::NotFound, "no active profile found")
                            .hint("pass a profile name or run `agyva profile init`")
                    })?;
                    apply_resolved_profile(app, resolved, force)?
                }
            };
            if json {
                print_json(io, &outcome)?;
            } else {
                print_profile_use(io, &outcome, quiet)?;
            }
            Ok(exit::OK)
        }
    }
}

pub(super) fn apply_resolved_profile(
    app: &App,
    resolved: ResolvedProfile,
    force: bool,
) -> Result<ProfileUseOutcome> {
    let mut switched_account = None;
    let mut active_endpoint = None;
    let mut warnings = Vec::new();
    if let Some(acct) = resolved.spec.account.as_deref() {
        match app.unified_use(acct, force)? {
            crate::core::app_integrations::UnifiedUseOutcome::Account {
                account,
                warnings: w,
                ..
            } => {
                switched_account = Some(account);
                warnings.extend(w);
            }
            crate::core::app_integrations::UnifiedUseOutcome::Endpoint { endpoint } => {
                active_endpoint = Some(endpoint);
            }
        }
    }
    if let Some(ep) = resolved.spec.endpoint.as_deref() {
        active_endpoint = Some(app.use_endpoint(ep)?);
    }
    Ok(ProfileUseOutcome {
        profile: resolved,
        switched_account,
        active_endpoint,
        warnings,
    })
}

pub(super) fn print_profile_use(
    io: &mut Io<'_>,
    outcome: &ProfileUseOutcome,
    quiet: bool,
) -> Result<()> {
    let label = outcome.profile.name.as_deref().unwrap_or("local");
    writeln!(io.out, "Activated profile `{label}`.").map_err(w)?;
    if let Some(acct) = &outcome.switched_account {
        writeln!(io.out, "  account: {}", acct.alias).map_err(w)?;
    }
    if let Some(ep) = &outcome.active_endpoint {
        writeln!(io.out, "  endpoint: {} ({})", ep.alias, ep.base_url).map_err(w)?;
    }
    if !quiet {
        for warning in &outcome.warnings {
            writeln!(io.err, "note: {warning}").map_err(w)?;
        }
    }
    Ok(())
}

fn parse_env_pairs(items: &[String]) -> Result<BTreeMap<String, String>> {
    let mut env = BTreeMap::new();
    for raw in items {
        let Some((k, v)) = raw.split_once('=') else {
            return Err(Error::usage(format!(
                "invalid --env `{raw}`: expected KEY=VAL"
            )));
        };
        env.insert(k.trim().to_owned(), v.to_owned());
    }
    Ok(env)
}

fn build_spec(fields: ProfileFieldsArgs, name: Option<String>) -> Result<ProfileSpec> {
    let spec = ProfileSpec {
        name,
        description: fields.description,
        account: fields.account,
        endpoint: fields.endpoint,
        group: fields.group,
        model: fields.model,
        warn_percent: fields.warn_below,
        critical_percent: fields.critical_below,
        monitor_interval: fields.monitor_interval,
        env: parse_env_pairs(&fields.env_vars)?,
    };
    spec.validate()?;
    Ok(spec)
}

fn cmd_init(app: &App, a: ProfileInitArgs, json: bool, io: &mut Io<'_>) -> Result<i32> {
    let force = a.fields.force;
    let spec = build_spec(a.fields, a.global.clone())?;

    if let Some(name) = a.global {
        let path = if force {
            profile::validate_profile_name(&name)?;
            let p = profiles_file(&app.dirs);
            let mut file = load_global_profiles(&p)?;
            file.profiles.insert(name.clone(), spec.clone());
            if a.default {
                file.default = Some(name.clone());
            }
            save_global_profiles(&p, &file)?;
            p
        } else {
            app.init_profile(Some(&name), None, spec.clone(), a.default)?
        };
        if json {
            print_json(
                io,
                &ProfileInitView {
                    schema_version: VIEW_SCHEMA,
                    path,
                    name: Some(name),
                    source: ProfileSource::Global,
                    profile: spec,
                },
            )?;
        } else {
            writeln!(
                io.out,
                "Initialized global profile `{name}` in {}.",
                path.display()
            )
            .map_err(w)?;
        }
    } else {
        let cwd = std::env::current_dir()
            .map_err(|e| Error::io("determining current working directory", e))?;
        let path = if force {
            write_local_profile(&cwd, &spec)?
        } else {
            app.init_profile(None, Some(&cwd), spec.clone(), false)?
        };
        if json {
            print_json(
                io,
                &ProfileInitView {
                    schema_version: VIEW_SCHEMA,
                    path,
                    name: spec.name.clone(),
                    source: ProfileSource::Local,
                    profile: spec,
                },
            )?;
        } else {
            writeln!(io.out, "Initialized local profile at {}.", path.display()).map_err(w)?;
        }
    }
    Ok(exit::OK)
}

fn cmd_create(app: &App, a: ProfileCreateArgs, json: bool, io: &mut Io<'_>) -> Result<i32> {
    let force = a.fields.force;
    let spec = build_spec(a.fields, Some(a.name.clone()))?;
    let path = if force {
        profile::validate_profile_name(&a.name)?;
        let p = profiles_file(&app.dirs);
        let mut file = load_global_profiles(&p)?;
        file.profiles.insert(a.name.clone(), spec.clone());
        if a.default {
            file.default = Some(a.name.clone());
        }
        save_global_profiles(&p, &file)?;
        p
    } else {
        app.init_profile(Some(&a.name), None, spec.clone(), a.default)?
    };
    if json {
        print_json(
            io,
            &ProfileInitView {
                schema_version: VIEW_SCHEMA,
                path,
                name: Some(a.name),
                source: ProfileSource::Global,
                profile: spec,
            },
        )?;
    } else {
        writeln!(
            io.out,
            "Created global profile `{}` in {}.",
            a.name,
            path.display()
        )
        .map_err(w)?;
    }
    Ok(exit::OK)
}

fn cmd_set(app: &App, a: ProfileSetArgs, json: bool, io: &mut Io<'_>) -> Result<i32> {
    let (target, field, value) = match a.args.as_slice() {
        [k, v] => (None, k.as_str(), v.as_str()),
        [name, k, v] => {
            if a.local {
                return Err(Error::usage(
                    "`--local` cannot be combined with an explicit profile name",
                ));
            }
            (Some(name.as_str()), k.as_str(), v.as_str())
        }
        _ => {
            return Err(Error::usage(
                "expected `agyva profile set [NAME] <KEY> <VALUE>`",
            ));
        }
    };

    let updated = app.set_profile_field(target, None, field, Some(value))?;
    if json {
        print_json(
            io,
            &ProfileSetView {
                schema_version: VIEW_SCHEMA,
                name: target.map(str::to_owned).or_else(|| updated.name.clone()),
                field: field.to_owned(),
                profile: updated,
            },
        )?;
    } else {
        writeln!(io.out, "{field} = {value}").map_err(w)?;
    }
    Ok(exit::OK)
}

fn print_resolved(io: &mut Io<'_>, p: &ResolvedProfile) -> Result<()> {
    let label = p.name.as_deref().unwrap_or("local");
    let src = match p.source {
        ProfileSource::Local => "local",
        ProfileSource::Global => "global",
        ProfileSource::Explicit => "explicit",
    };
    writeln!(io.out, "profile: {label} ({src})").map_err(w)?;
    if let Some(path) = &p.path {
        writeln!(io.out, "path:    {}", path.display()).map_err(w)?;
    }
    if let Some(desc) = &p.spec.description {
        writeln!(io.out, "desc:    {desc}").map_err(w)?;
    }
    if let Some(acct) = &p.spec.account {
        writeln!(io.out, "account: {acct}").map_err(w)?;
    }
    if let Some(ep) = &p.spec.endpoint {
        writeln!(io.out, "endpoint:{ep}").map_err(w)?;
    }
    if let Some(g) = &p.spec.group {
        writeln!(io.out, "group:   {g}").map_err(w)?;
    }
    if let Some(m) = &p.spec.model {
        writeln!(io.out, "model:   {m}").map_err(w)?;
    }
    if let Some(w_pct) = p.spec.warn_percent {
        writeln!(io.out, "warn:    {w_pct}%").map_err(w)?;
    }
    if let Some(c_pct) = p.spec.critical_percent {
        writeln!(io.out, "crit:    {c_pct}%").map_err(w)?;
    }
    if let Some(iv) = &p.spec.monitor_interval {
        writeln!(io.out, "interval:{iv}").map_err(w)?;
    }
    for (k, v) in &p.spec.env {
        writeln!(io.out, "env.{k}: {v}").map_err(w)?;
    }
    Ok(())
}
