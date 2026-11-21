//! CLI handlers for `agyva endpoint` (`ep`), `agyva bridge`, `agyva env`, and `agyva exec`.

use std::collections::BTreeMap;
use std::io::Read;
use std::net::SocketAddr;
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use zeroize::Zeroizing;

use super::commands::{Io, confirm, print_json, w};
use super::output::Style;
use super::{
    BridgeArgs, EndpointAddArgs, EndpointCommand, EndpointUpdateArgs, EnvArgs, EnvShell, ExecArgs,
};
use crate::core::app::App;
use crate::core::app_integrations::EndpointView;
use crate::core::bridge::{BridgeHandle, start_bridge, start_loopback_bridge};
use crate::core::endpoints::{
    EndpointPatch, EndpointRecord, NewEndpoint, load_registry, resolve_endpoint,
};
use crate::core::error::{Error, ErrorKind, Result, exit};
use crate::core::format::fit;
use crate::core::protocol::{
    self, AuthScheme, EndpointProtocol, FetchedModel, ProbeResult, RateLimitInfo,
};
use crate::core::view::VIEW_SCHEMA;

#[derive(Serialize)]
struct PresetItem {
    name: &'static str,
    display_name: &'static str,
    base_url: &'static str,
    protocol: EndpointProtocol,
    auth_scheme: String,
    default_model: Option<&'static str>,
    api_version: Option<&'static str>,
}

#[derive(Serialize)]
struct PresetsView {
    schema_version: u32,
    presets: Vec<PresetItem>,
}

#[derive(Serialize)]
struct EndpointListView {
    schema_version: u32,
    endpoints: Vec<EndpointView>,
}

#[derive(Serialize)]
struct EndpointShowView {
    schema_version: u32,
    id: String,
    alias: String,
    base_url: String,
    protocol: EndpointProtocol,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    detected_protocol: Option<EndpointProtocol>,
    effective_protocol: EndpointProtocol,
    auth_scheme: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    default_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    api_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    models_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    chat_path: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    extra_headers: BTreeMap<String, String>,
    active: bool,
    enabled: bool,
    added_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_used_at: Option<DateTime<Utc>>,
    model_count: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    models: Vec<FetchedModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fetched_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rate_limits: Option<RateLimitInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

#[derive(Serialize)]
struct EndpointAddView {
    schema_version: u32,
    endpoint: EndpointRecord,
    activated: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    probe: Option<ProbeResult>,
}

#[derive(Serialize)]
struct EndpointModelsView {
    schema_version: u32,
    endpoint: String,
    id: String,
    refreshed: bool,
    model_count: usize,
    models: Vec<FetchedModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    fetched_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rate_limits: Option<RateLimitInfo>,
}

#[derive(Serialize)]
struct EndpointUseView {
    schema_version: u32,
    cleared: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<EndpointRecord>,
}

#[derive(Serialize)]
struct EndpointActionView {
    schema_version: u32,
    endpoint: EndpointRecord,
}

pub fn handle_endpoint(
    app: &App,
    cmd: EndpointCommand,
    json: bool,
    quiet: bool,
    io: &mut Io<'_>,
    st: Style,
) -> Result<i32> {
    match cmd {
        EndpointCommand::Presets => {
            let items: Vec<PresetItem> = protocol::presets()
                .iter()
                .map(|p| PresetItem {
                    name: p.name,
                    display_name: p.display_name,
                    base_url: p.base_url,
                    protocol: p.protocol,
                    auth_scheme: p.auth_scheme.to_string(),
                    default_model: p.default_model,
                    api_version: p.api_version,
                })
                .collect();
            if json {
                print_json(
                    io,
                    &PresetsView {
                        schema_version: VIEW_SCHEMA,
                        presets: items,
                    },
                )?;
            } else {
                writeln!(
                    io.out,
                    "{}",
                    st.dim(&format!(
                        "{:<12} {:<16} {:<15} {:<14} {}",
                        "NAME", "PROVIDER", "PROTOCOL", "AUTH", "BASE URL"
                    ))
                )
                .map_err(w)?;
                for p in &items {
                    writeln!(
                        io.out,
                        "{} {} {} {} {}",
                        fit(p.name, 12),
                        fit(p.display_name, 16),
                        fit(p.protocol.as_str(), 15),
                        fit(&p.auth_scheme, 14),
                        p.base_url
                    )
                    .map_err(w)?;
                }
            }
        }
        EndpointCommand::Add(a) => {
            return cmd_add(app, a, json, quiet, io);
        }
        EndpointCommand::List => {
            let endpoints = app.list_endpoints()?;
            if json {
                print_json(
                    io,
                    &EndpointListView {
                        schema_version: VIEW_SCHEMA,
                        endpoints,
                    },
                )?;
            } else if endpoints.is_empty() {
                writeln!(
                    io.out,
                    "No custom endpoints saved. Add one with `agyva endpoint add`."
                )
                .map_err(w)?;
            } else {
                writeln!(
                    io.out,
                    "{}",
                    st.dim(&format!(
                        "{:<4} {:<18} {:<16} {:<7} {:<24} {}",
                        "#", "ALIAS", "PROTOCOL", "MODELS", "MODEL", "BASE URL"
                    ))
                )
                .map_err(w)?;
                for (i, ep) in endpoints.iter().enumerate() {
                    let proto = ep.detected_protocol.unwrap_or(ep.protocol);
                    let marker = if ep.active { "*" } else { " " };
                    let num = format!("{marker}{}", i + 1);
                    let line = format!(
                        "{} {} {} {:<7} {} {}",
                        fit(&num, 4),
                        fit(&ep.alias, 18),
                        fit(proto.as_str(), 16),
                        ep.model_count,
                        fit(ep.default_model.as_deref().unwrap_or("-"), 24),
                        ep.base_url,
                    );
                    writeln!(io.out, "{}", if ep.active { st.bold(&line) } else { line })
                        .map_err(w)?;
                }
            }
        }
        EndpointCommand::Show { selector } => {
            let reg = load_registry(&app.dirs)?;
            let rec = resolve_endpoint(&reg, &selector)?.clone();
            let view = app
                .list_endpoints()?
                .into_iter()
                .find(|v| v.id == rec.id)
                .ok_or_else(|| Error::new(ErrorKind::NotFound, "endpoint disappeared"))?;
            let show = EndpointShowView {
                schema_version: VIEW_SCHEMA,
                id: rec.id.clone(),
                alias: rec.alias.clone(),
                base_url: rec.base_url.clone(),
                protocol: rec.protocol,
                detected_protocol: rec.detected_protocol,
                effective_protocol: rec.effective_protocol(),
                auth_scheme: rec.effective_auth_scheme().to_string(),
                default_model: rec.default_model.clone(),
                api_version: rec.api_version.clone(),
                models_path: rec.models_path.clone(),
                chat_path: rec.chat_path.clone(),
                extra_headers: rec.extra_headers.clone(),
                active: view.active,
                enabled: rec.enabled,
                added_at: rec.added_at,
                updated_at: rec.updated_at,
                last_used_at: rec.last_used_at,
                model_count: view.model_count,
                models: view.models,
                fetched_at: view.fetched_at,
                latency_ms: view.latency_ms,
                rate_limits: view.rate_limits,
                last_error: view.last_error,
            };
            if json {
                print_json(io, &show)?;
            } else {
                writeln!(
                    io.out,
                    "{} [{}]{}",
                    show.alias,
                    show.id,
                    if show.active { " (active)" } else { "" }
                )
                .map_err(w)?;
                writeln!(io.out, "base_url:   {}", show.base_url).map_err(w)?;
                writeln!(
                    io.out,
                    "protocol:   {} (effective: {})",
                    show.protocol, show.effective_protocol
                )
                .map_err(w)?;
                writeln!(io.out, "auth:       {}", show.auth_scheme).map_err(w)?;
                writeln!(
                    io.out,
                    "model:      {}",
                    show.default_model.as_deref().unwrap_or("-")
                )
                .map_err(w)?;
                writeln!(io.out, "enabled:    {}", show.enabled).map_err(w)?;
                writeln!(io.out, "models:     {} cached", show.model_count).map_err(w)?;
                if let Some(lat) = show.latency_ms {
                    writeln!(io.out, "latency:    {lat}ms").map_err(w)?;
                }
                if let Some(err) = &show.last_error {
                    writeln!(io.out, "last_error: {err}").map_err(w)?;
                }
            }
        }
        EndpointCommand::Use { selector, clear } => {
            if clear {
                app.clear_active_endpoint()?;
                if json {
                    print_json(
                        io,
                        &EndpointUseView {
                            schema_version: VIEW_SCHEMA,
                            cleared: true,
                            endpoint: None,
                        },
                    )?;
                } else {
                    writeln!(io.out, "Cleared active custom endpoint.").map_err(w)?;
                }
            } else {
                let sel = selector.ok_or_else(|| Error::usage("provide an endpoint selector"))?;
                let rec = app.use_endpoint(&sel)?;
                if json {
                    print_json(
                        io,
                        &EndpointUseView {
                            schema_version: VIEW_SCHEMA,
                            cleared: false,
                            endpoint: Some(rec),
                        },
                    )?;
                } else {
                    writeln!(
                        io.out,
                        "Activated endpoint {} [{}] ({} -> {}).",
                        rec.alias,
                        rec.id,
                        rec.effective_protocol(),
                        rec.base_url
                    )
                    .map_err(w)?;
                }
            }
        }
        EndpointCommand::Test { selector } => {
            let probe = app.test_endpoint(&selector)?;
            if json {
                print_json(io, &probe)?;
            } else if probe.reachable {
                let proto = probe
                    .detected_protocol
                    .map_or_else(|| "-".to_owned(), |p| p.to_string());
                let lat = probe
                    .latency_ms
                    .map_or_else(|| "-".to_owned(), |m| format!("{m}ms"));
                writeln!(
                    io.out,
                    "OK: reachable (protocol={proto}, latency={lat}, models={})",
                    probe.model_count
                )
                .map_err(w)?;
                if let Some(rl) = &probe.rate_limits
                    && let Some(rem) = rl.credits_remaining
                {
                    writeln!(io.out, "credits_remaining: {rem}").map_err(w)?;
                }
            } else {
                writeln!(
                    io.out,
                    "FAIL: {}",
                    probe.error.as_deref().unwrap_or("unreachable")
                )
                .map_err(w)?;
            }
            if !probe.reachable {
                return Ok(exit::ERROR);
            }
        }
        EndpointCommand::Models { selector, refresh } => {
            let reg = load_registry(&app.dirs)?;
            let rec = resolve_endpoint(&reg, &selector)?.clone();
            if refresh {
                let probe = app.refresh_endpoint_models(&rec.id)?;
                if !probe.reachable {
                    if json {
                        print_json(io, &probe)?;
                    } else {
                        writeln!(
                            io.err,
                            "error: {}",
                            probe.error.as_deref().unwrap_or("endpoint probe failed")
                        )
                        .map_err(w)?;
                    }
                    return Ok(exit::ERROR);
                }
                if json {
                    print_json(
                        io,
                        &EndpointModelsView {
                            schema_version: VIEW_SCHEMA,
                            endpoint: rec.alias,
                            id: rec.id,
                            refreshed: true,
                            model_count: probe.models.len(),
                            models: probe.models,
                            fetched_at: Some(Utc::now()),
                            latency_ms: probe.latency_ms,
                            rate_limits: probe.rate_limits,
                        },
                    )?;
                } else {
                    render_models_list(io, &rec.alias, &probe.models, true)?;
                }
            } else {
                let view = app
                    .list_endpoints()?
                    .into_iter()
                    .find(|v| v.id == rec.id)
                    .ok_or_else(|| Error::new(ErrorKind::NotFound, "endpoint disappeared"))?;
                if json {
                    print_json(
                        io,
                        &EndpointModelsView {
                            schema_version: VIEW_SCHEMA,
                            endpoint: view.alias,
                            id: view.id,
                            refreshed: false,
                            model_count: view.model_count,
                            models: view.models,
                            fetched_at: view.fetched_at,
                            latency_ms: view.latency_ms,
                            rate_limits: view.rate_limits,
                        },
                    )?;
                } else {
                    render_models_list(io, &view.alias, &view.models, false)?;
                }
            }
        }
        EndpointCommand::Update(a) => {
            return cmd_update(app, a, json, io);
        }
        EndpointCommand::Rename {
            selector,
            new_alias,
        } => {
            let updated = app.rename_endpoint(&selector, &new_alias)?;
            if json {
                print_json(
                    io,
                    &EndpointActionView {
                        schema_version: VIEW_SCHEMA,
                        endpoint: updated,
                    },
                )?;
            } else {
                writeln!(
                    io.out,
                    "Renamed endpoint to {} [{}].",
                    updated.alias, updated.id
                )
                .map_err(w)?;
            }
        }
        EndpointCommand::Remove { selector, yes } => {
            let reg = load_registry(&app.dirs)?;
            let target = resolve_endpoint(&reg, &selector)?.clone();
            if json && !yes {
                return Err(Error::usage(
                    "`endpoint remove --json` needs --yes: it never prompts on stdout",
                )
                .hint("add --yes to confirm non-interactively"));
            }
            if !yes
                && !confirm(
                    io,
                    &format!("Remove custom endpoint {} [{}]?", target.alias, target.id),
                )?
            {
                if !quiet {
                    writeln!(io.err, "Cancelled.").map_err(w)?;
                }
                return Ok(exit::OK);
            }
            let removed = app.remove_endpoint(&target.id)?;
            if json {
                print_json(
                    io,
                    &EndpointActionView {
                        schema_version: VIEW_SCHEMA,
                        endpoint: removed,
                    },
                )?;
            } else {
                writeln!(
                    io.out,
                    "Removed endpoint {} [{}].",
                    removed.alias, removed.id
                )
                .map_err(w)?;
            }
        }
    }
    Ok(exit::OK)
}

fn render_models_list(
    io: &mut Io<'_>,
    alias: &str,
    models: &[FetchedModel],
    refreshed: bool,
) -> Result<()> {
    if models.is_empty() {
        writeln!(
            io.out,
            "No models cached for `{alias}`. Run `agyva endpoint models {alias} --refresh`."
        )
        .map_err(w)?;
        return Ok(());
    }
    let src = if refreshed { "live" } else { "cached" };
    writeln!(io.out, "{alias}: {} model(s) ({src})", models.len()).map_err(w)?;
    for m in models {
        let mut details = Vec::new();
        if let Some(ctx) = m.context_window {
            details.push(format!("ctx={ctx}"));
        }
        if let Some(out) = m.max_output_tokens {
            details.push(format!("max_out={out}"));
        }
        if let Some(owner) = &m.owned_by {
            details.push(owner.clone());
        }
        if details.is_empty() {
            writeln!(io.out, "  {}", m.id).map_err(w)?;
        } else {
            writeln!(io.out, "  {} ({})", m.id, details.join(", ")).map_err(w)?;
        }
    }
    Ok(())
}

fn parse_headers(items: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    for raw in items {
        let Some((k, v)) = raw.split_once('=') else {
            return Err(
                Error::usage(format!("invalid --header `{raw}`: expected KEY=VAL"))
                    .hint("use `--header Name=Value`"),
            );
        };
        let key = k.trim();
        if key.is_empty() || key.chars().any(char::is_control) {
            return Err(Error::usage(format!("invalid header name `{key}`")));
        }
        if v.chars().any(char::is_control) {
            return Err(Error::usage(format!(
                "header `{key}` value contains invalid control character"
            )));
        }
        map.insert(key.to_owned(), v.trim().to_owned());
    }
    Ok(map)
}

fn read_api_key(
    app: &App,
    io: &Io<'_>,
    api_key_stdin: bool,
    api_key_env: Option<&str>,
    allow_empty: bool,
) -> Result<Zeroizing<String>> {
    if let Some(var) = api_key_env {
        let val = app.env.get(var).ok_or_else(|| {
            Error::usage(format!("environment variable `{var}` is not set"))
                .hint("export the API key in that variable or pass --api-key-stdin")
        })?;
        let key = Zeroizing::new(val.trim().to_owned());
        if key.is_empty() && !allow_empty {
            return Err(Error::usage(format!(
                "environment variable `{var}` is empty"
            )));
        }
        return Ok(key);
    }
    if api_key_stdin {
        let mut buf = Zeroizing::new(String::new());
        std::io::stdin()
            .lock()
            .read_to_string(&mut buf)
            .map_err(|e| Error::io("reading API key from stdin", e))?;
        let key = Zeroizing::new(buf.trim().to_owned());
        if key.is_empty() && !allow_empty {
            return Err(Error::usage("API key read from stdin is empty"));
        }
        return Ok(key);
    }
    if allow_empty {
        return Ok(Zeroizing::new(String::new()));
    }
    if !io.stdin_tty {
        return Err(
            Error::usage("cannot prompt for API key: input is not a terminal")
                .hint("pass --api-key-stdin or --api-key-env <VAR>"),
        );
    }
    let entered = rpassword::prompt_password("API key: ")
        .map_err(|e| Error::io("reading API key prompt", e))?;
    let key = Zeroizing::new(entered.trim().to_owned());
    if key.is_empty() {
        return Err(Error::usage("API key cannot be empty"));
    }
    Ok(key)
}

fn cmd_add(app: &App, a: EndpointAddArgs, json: bool, quiet: bool, io: &mut Io<'_>) -> Result<i32> {
    let preset = match a.preset.as_deref() {
        Some(name) => Some(protocol::find_preset(name).ok_or_else(|| {
            Error::usage(format!("unknown preset `{name}`"))
                .hint("run `agyva endpoint presets` to see available presets")
        })?),
        None => None,
    };

    let base_url = a
        .base_url
        .or_else(|| preset.map(|p| p.base_url.to_owned()))
        .ok_or_else(|| {
            Error::usage("either --base-url or --preset is required")
                .hint("pass `--base-url https://...` or `--preset <NAME>`")
        })?;

    let protocol = match a.protocol.as_deref() {
        Some(s) => s.parse::<EndpointProtocol>().map_err(Error::usage)?,
        None => preset.map_or(EndpointProtocol::Auto, |p| p.protocol),
    };

    let auth_scheme = match a.auth_scheme.as_deref() {
        Some(s) => Some(s.parse::<AuthScheme>().map_err(Error::usage)?),
        None => preset.map(|p| p.auth_scheme.clone()),
    };

    let effective_auth = auth_scheme
        .clone()
        .unwrap_or_else(|| protocol.default_auth_scheme());
    let allow_empty_key = matches!(effective_auth, AuthScheme::None);
    let api_key = read_api_key(
        app,
        io,
        a.api_key_stdin,
        a.api_key_env.as_deref(),
        allow_empty_key,
    )?;

    let extra_headers = parse_headers(&a.headers)?;

    let spec = NewEndpoint {
        alias: a.alias,
        base_url,
        protocol,
        auth_scheme,
        api_key,
        default_model: a
            .model
            .or_else(|| preset.and_then(|p| p.default_model.map(str::to_owned))),
        api_version: a
            .api_version
            .or_else(|| preset.and_then(|p| p.api_version.map(str::to_owned))),
        models_path: a.models_path,
        chat_path: a.chat_path,
        extra_headers,
        secret_headers: BTreeMap::new(),
        enabled: true,
    };

    let mut rec = app.add_endpoint(spec)?;
    let mut probe = None;
    if !a.no_fetch {
        match app.refresh_endpoint_models(&rec.id) {
            Ok(p) => {
                if !p.reachable
                    && !json
                    && !quiet
                    && let Some(err) = &p.error
                {
                    writeln!(io.err, "note: model discovery failed: {err}").map_err(w)?;
                }
                probe = Some(p);
                if let Ok(reg) = load_registry(&app.dirs)
                    && let Some(updated) = reg.find(&rec.id)
                {
                    rec = updated.clone();
                }
            }
            Err(e) => {
                if !json && !quiet {
                    writeln!(io.err, "note: model discovery failed: {}", e.message()).map_err(w)?;
                }
            }
        }
    }

    if a.activate {
        rec = app.use_endpoint(&rec.id)?;
    }

    if json {
        print_json(
            io,
            &EndpointAddView {
                schema_version: VIEW_SCHEMA,
                endpoint: rec,
                activated: a.activate,
                probe,
            },
        )?;
    } else {
        writeln!(
            io.out,
            "Saved endpoint {} [{}] ({} -> {}).",
            rec.alias,
            rec.id,
            rec.effective_protocol(),
            rec.base_url
        )
        .map_err(w)?;
        if let Some(p) = &probe
            && p.reachable
        {
            writeln!(io.out, "Discovered {} model(s).", p.model_count).map_err(w)?;
        }
        if a.activate {
            writeln!(io.out, "Activated {}.", rec.alias).map_err(w)?;
        }
    }
    Ok(exit::OK)
}

fn cmd_update(app: &App, a: EndpointUpdateArgs, json: bool, io: &mut Io<'_>) -> Result<i32> {
    let has_key = a.api_key_stdin || a.api_key_env.is_some();
    let has_changes = a.base_url.is_some()
        || a.protocol.is_some()
        || a.auth_scheme.is_some()
        || a.model.is_some()
        || a.clear_model
        || a.api_version.is_some()
        || a.models_path.is_some()
        || a.chat_path.is_some()
        || !a.headers.is_empty()
        || a.clear_headers
        || has_key
        || a.enable
        || a.disable;
    if !has_changes {
        return Err(Error::usage("no update flags specified")
            .hint("pass --base-url, --model, --protocol, --api-key-stdin, etc."));
    }

    let mut patch = EndpointPatch::default();
    patch.base_url = a.base_url;
    patch.protocol = a
        .protocol
        .as_deref()
        .map(|s| s.parse::<EndpointProtocol>().map_err(Error::usage))
        .transpose()?;
    patch.auth_scheme = a
        .auth_scheme
        .as_deref()
        .map(|s| s.parse::<AuthScheme>().map_err(Error::usage).map(Some))
        .transpose()?;
    if a.clear_model {
        patch.default_model = Some(None);
    } else if let Some(m) = a.model {
        patch.default_model = Some(Some(m));
    }
    if let Some(v) = a.api_version {
        patch.api_version = Some(Some(v));
    }
    if let Some(mp) = a.models_path {
        patch.models_path = Some(Some(mp));
    }
    if let Some(cp) = a.chat_path {
        patch.chat_path = Some(Some(cp));
    }
    if a.clear_headers {
        patch.extra_headers = Some(BTreeMap::new());
    } else if !a.headers.is_empty() {
        let reg = load_registry(&app.dirs)?;
        let mut merged = resolve_endpoint(&reg, &a.selector)?.extra_headers.clone();
        for (k, v) in parse_headers(&a.headers)? {
            merged.insert(k, v);
        }
        patch.extra_headers = Some(merged);
    }
    if has_key {
        patch.api_key = Some(read_api_key(
            app,
            io,
            a.api_key_stdin,
            a.api_key_env.as_deref(),
            false,
        )?);
    }
    if a.enable {
        patch.enabled = Some(true);
    } else if a.disable {
        patch.enabled = Some(false);
    }

    let updated = app.update_endpoint(&a.selector, patch)?;
    if json {
        print_json(
            io,
            &EndpointActionView {
                schema_version: VIEW_SCHEMA,
                endpoint: updated,
            },
        )?;
    } else {
        writeln!(
            io.out,
            "Updated endpoint {} [{}].",
            updated.alias, updated.id
        )
        .map_err(w)?;
    }
    Ok(exit::OK)
}

pub fn handle_bridge(app: &App, args: BridgeArgs, io: &mut Io<'_>) -> Result<i32> {
    let mut bind_addr: SocketAddr = args.listen.parse().map_err(|_| {
        Error::usage(format!("invalid --listen address `{}`", args.listen))
            .hint("use a loopback address like 127.0.0.1:0 or 127.0.0.1:8787")
    })?;
    if let Some(port) = args.port {
        bind_addr.set_port(port);
    }
    if let Some(ref token_var) = args.local_token_env
        && app.env.get(token_var).is_none()
    {
        return Err(Error::usage(format!(
            "environment variable `{token_var}` is not set"
        )));
    }

    let target = app.bridge_target_for_endpoint(&args.endpoint)?;
    let proto = target.protocol;
    let mut handle = start_bridge(bind_addr, target, app.http_transport())?;
    let base_url = handle.base_url();

    writeln!(
        io.out,
        "Bridge listening on {base_url} (upstream protocol: {proto})"
    )
    .map_err(w)?;
    writeln!(io.out, "  export GOOGLE_GEMINI_BASE_URL={base_url}").map_err(w)?;
    writeln!(io.out, "  export OPENAI_BASE_URL={base_url}/v1").map_err(w)?;
    writeln!(io.out, "  export ANTHROPIC_BASE_URL={base_url}").map_err(w)?;
    writeln!(io.out, "Press Ctrl+C to stop.").map_err(w)?;
    let _ = io.out.flush();

    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    if let Err(e) = ctrlc::set_handler(move || flag.store(true, Ordering::SeqCst)) {
        tracing::warn!("cannot install Ctrl+C handler: {e}");
    }
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
    }
    handle.stop();
    Ok(exit::OK)
}

pub fn handle_env(app: &App, args: EnvArgs, io: &mut Io<'_>) -> Result<i32> {
    let vars = collect_env_vars(
        app,
        args.endpoint.as_deref(),
        args.profile.as_deref(),
        args.model.as_deref(),
        args.bridge_url.as_deref(),
    )?;
    for (k, v) in &vars {
        validate_env_pair(k, v)?;
        writeln!(io.out, "{}", format_env_line(args.shell, k, v)).map_err(w)?;
    }
    Ok(exit::OK)
}

fn collect_env_vars(
    app: &App,
    explicit_endpoint: Option<&str>,
    explicit_profile: Option<&str>,
    explicit_model: Option<&str>,
    bridge_url: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let mut map: BTreeMap<String, String> = BTreeMap::new();
    let mut ordered_keys: Vec<String> = Vec::new();
    let mut insert = |k: String, v: String| {
        if !map.contains_key(&k) {
            ordered_keys.push(k.clone());
        }
        map.insert(k, v);
    };

    if let Some(ep) = explicit_endpoint {
        for (k, v) in app.endpoint_env_vars(ep, explicit_model, bridge_url)? {
            insert(k, v);
        }
    } else {
        let profile = match explicit_profile {
            Some(name) => app.resolve_active_profile(Some(name), None)?,
            None => app.resolve_active_profile(None, None)?,
        };
        let active_ep = if profile.is_none() {
            app.active_endpoint_record()?
        } else {
            None
        };

        if profile.is_none() && active_ep.is_none() {
            return Err(
                Error::new(ErrorKind::NotFound, "no active endpoint or profile").hint(
                    "pass --endpoint <SELECTOR> or --profile <NAME>, or run `agyva use ep:<alias>`",
                ),
            );
        }

        if let Some(p) = &profile {
            if let Some(ep) = p.spec.endpoint.as_deref() {
                let model = explicit_model.or(p.spec.model.as_deref());
                for (k, v) in app.endpoint_env_vars(ep, model, bridge_url)? {
                    insert(k, v);
                }
            }
            for (k, v) in &p.spec.env {
                insert(k.clone(), v.clone());
            }
            let pname = p.name.clone().unwrap_or_else(|| "local".to_owned());
            insert("AGYVA_PROFILE".into(), pname);
            if let Some(m) = explicit_model.or(p.spec.model.as_deref()) {
                insert("AGYVA_MODEL".into(), m.to_owned());
            }
        } else if let Some(ep) = active_ep {
            for (k, v) in app.endpoint_env_vars(&ep.id, explicit_model, bridge_url)? {
                insert(k, v);
            }
        }
    }

    Ok(ordered_keys
        .into_iter()
        .filter_map(|k| map.remove(&k).map(|v| (k, v)))
        .collect())
}

fn validate_env_pair(key: &str, value: &str) -> Result<()> {
    let mut chars = key.chars();
    let valid_key = chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid_key {
        return Err(Error::usage(format!("invalid environment key `{key}`"))
            .hint("use ASCII letters, digits, and underscores, starting with a letter or `_`"));
    }
    if value.contains('\0') {
        return Err(Error::usage(format!(
            "environment value for `{key}` contains a NUL byte"
        )));
    }
    Ok(())
}

fn format_env_line(shell: EnvShell, key: &str, val: &str) -> String {
    match shell {
        EnvShell::Bash | EnvShell::Zsh => {
            let escaped = val.replace('\'', "'\\''");
            format!("export {key}='{escaped}'")
        }
        EnvShell::Fish => {
            let escaped = val.replace('\\', "\\\\").replace('\'', "\\'");
            format!("set -gx {key} '{escaped}'")
        }
        EnvShell::Powershell => {
            let escaped = val.replace('\'', "''");
            format!("$env:{key} = '{escaped}'")
        }
        EnvShell::Dotenv => {
            let mut escaped = String::with_capacity(val.len());
            for ch in val.chars() {
                match ch {
                    '\\' => escaped.push_str("\\\\"),
                    '"' => escaped.push_str("\\\""),
                    '$' => escaped.push_str("\\$"),
                    '\n' => escaped.push_str("\\n"),
                    '\r' => escaped.push_str("\\r"),
                    c => escaped.push(c),
                }
            }
            format!("{key}=\"{escaped}\"")
        }
    }
}

pub fn handle_exec(app: &App, args: ExecArgs, _io: &mut Io<'_>) -> Result<i32> {
    let profile = match args.profile.as_deref() {
        Some(name) => app.resolve_active_profile(Some(name), None)?,
        None if args.account.is_none() && args.endpoint.is_none() => {
            app.resolve_active_profile(None, None)?
        }
        None => None,
    };

    let mut env_pairs: BTreeMap<String, String> = BTreeMap::new();

    if let Some(p) = &profile {
        for (k, v) in &p.spec.env {
            validate_env_pair(k, v)?;
            env_pairs.insert(k.clone(), v.clone());
        }
        let pname = p.name.clone().unwrap_or_else(|| "local".to_owned());
        env_pairs.insert("AGYVA_PROFILE".into(), pname);
    }

    let mut endpoint_sel = args.endpoint.clone();
    if let Some(acct_sel) = args.account.as_deref() {
        match app.unified_use(acct_sel, false)? {
            crate::core::app_integrations::UnifiedUseOutcome::Account { account, .. } => {
                env_pairs.insert("AGYVA_ACCOUNT".into(), account.alias);
                env_pairs.insert("AGYVA_ACCOUNT_ID".into(), account.id);
            }
            crate::core::app_integrations::UnifiedUseOutcome::Endpoint { endpoint } => {
                if endpoint_sel.is_none() {
                    endpoint_sel = Some(endpoint.id);
                }
            }
        }
    } else if endpoint_sel.is_none()
        && let Some(p) = &profile
    {
        if let Some(ep) = &p.spec.endpoint {
            endpoint_sel = Some(ep.clone());
        } else if let Some(acct) = &p.spec.account {
            match app.unified_use(acct, false)? {
                crate::core::app_integrations::UnifiedUseOutcome::Account { account, .. } => {
                    env_pairs.insert("AGYVA_ACCOUNT".into(), account.alias);
                    env_pairs.insert("AGYVA_ACCOUNT_ID".into(), account.id);
                }
                crate::core::app_integrations::UnifiedUseOutcome::Endpoint { endpoint } => {
                    endpoint_sel = Some(endpoint.id);
                }
            }
        }
    }

    if endpoint_sel.is_none() && args.account.is_none() {
        endpoint_sel = app.active_endpoint_record()?.map(|e| e.id);
    }

    let effective_model = args
        .model
        .as_deref()
        .or_else(|| profile.as_ref().and_then(|p| p.spec.model.as_deref()));

    let mut bridge_handle: Option<BridgeHandle> = None;
    if args.bridge {
        let ep = endpoint_sel.as_deref().ok_or_else(|| {
            Error::usage("--bridge requires a target endpoint")
                .hint("pass --endpoint <SELECTOR> or configure an active endpoint")
        })?;
        let target = app.bridge_target_for_endpoint(ep)?;
        bridge_handle = Some(start_loopback_bridge(target, app.http_transport())?);
    }

    let bridge_url = bridge_handle.as_ref().map(BridgeHandle::base_url);
    if let Some(ep) = endpoint_sel.as_deref() {
        for (k, v) in app.endpoint_env_vars(ep, effective_model, bridge_url.as_deref())? {
            validate_env_pair(&k, &v)?;
            env_pairs.insert(k, v);
        }
    } else if let Some(m) = effective_model {
        validate_env_pair("AGYVA_MODEL", m)?;
        env_pairs.insert("AGYVA_MODEL".into(), m.to_owned());
    }

    let mut cmd = ProcessCommand::new(&args.command[0]);
    cmd.args(&args.command[1..])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    for (k, v) in &env_pairs {
        cmd.env(k, v);
    }
    // Zeroize our local copy of env_pairs values as soon as Command has cloned them.
    let status = cmd
        .status()
        .map_err(|e| Error::io(format!("executing `{}`", args.command[0]), e));
    protocol::zeroize_string_map(&mut env_pairs);
    if let Some(mut handle) = bridge_handle {
        handle.stop();
    }

    let status = status?;
    Ok(status.code().unwrap_or(1))
}
