//! The state machine: `update(state, event) -> jobs`. Pure apart from the
//! state it is given, so every key path is testable without a terminal.

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::{AppState, Overlay, RouteItemKind, View};
use super::backend::{Job, Msg};
use crate::core::usage::RefreshMode;

#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Resize(u16, u16),
    /// Time passed (relative times need redrawing).
    Tick(DateTime<Utc>),
    Msg(Msg),
    /// SIGINT/SIGTERM from outside the terminal.
    Interrupt,
}

const PAGE: usize = 5;

/// Jobs to start when the interface opens: show the cache at once, then ask
/// agy about the active account.
pub fn startup(s: &mut AppState) -> Vec<Job> {
    s.pending_loads = 2;
    s.dirty = true;
    vec![Job::Load(RefreshMode::Skip), Job::Load(RefreshMode::Normal)]
}

pub fn update(s: &mut AppState, e: Event) -> Vec<Job> {
    match e {
        Event::Resize(w, h) => {
            s.size = (w, h);
            s.dirty = true;
            vec![]
        }
        Event::Tick(now) => {
            s.now = now;
            s.dirty = true;
            vec![]
        }
        Event::Interrupt => {
            request_quit(s);
            vec![]
        }
        Event::Msg(m) => on_msg(s, m),
        Event::Key(k) if matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat) => {
            on_key(s, k)
        }
        Event::Key(_) => vec![],
    }
}

/// The background thread died: nothing in flight will ever answer. Clear the
/// busy flags so the interface stays usable and can still quit.
pub fn worker_lost(s: &mut AppState) {
    s.switching = false;
    s.endpoint_busy = false;
    s.pending_loads = 0;
    s.error = Some("The background worker stopped unexpectedly; quit and reopen agyva.".into());
    s.dirty = true;
    if s.quit_after_switch {
        s.quit = true;
    }
}

fn request_quit(s: &mut AppState) {
    if s.switching {
        // Never abandon a credential or settings switch half way.
        s.quit_after_switch = true;
        s.message = Some("finishing the switch before quitting...".into());
        s.dirty = true;
    } else {
        s.quit = true;
    }
}

fn reload_cache(s: &mut AppState, jobs: &mut Vec<Job>) {
    s.pending_loads += 1;
    jobs.push(Job::Load(RefreshMode::Skip));
}

fn on_msg(s: &mut AppState, m: Msg) -> Vec<Job> {
    s.dirty = true;
    match m {
        Msg::Loaded { result, .. } => {
            s.pending_loads = s.pending_loads.saturating_sub(1);
            match *result {
                Ok(d) => {
                    s.apply_dashboard(d);
                    s.error = None;
                }
                Err(e) => s.error = Some(format!("Refresh failed: {e}")),
            }
            vec![]
        }
        Msg::Switched(r) => {
            s.switching = false;
            let mut jobs = vec![];
            match r {
                Ok(done) => {
                    s.error = None;
                    let mut text = if done.already_active {
                        format!("{} is already active.", done.alias)
                    } else {
                        format!("Switched to {}. Restart agy to use it.", done.alias)
                    };
                    for w in &done.warnings {
                        text.push_str(&format!(" Note: {w}"));
                    }
                    s.message = Some(text);
                    reload_cache(s, &mut jobs);
                }
                Err(e) => s.error = Some(format!("Switch failed: {e}")),
            }
            if s.quit_after_switch {
                s.quit = true;
            }
            jobs
        }
        Msg::EndpointSwitched(r) => {
            s.switching = false;
            let mut jobs = vec![];
            match r {
                Ok(done) => {
                    s.error = None;
                    let model = done
                        .default_model
                        .as_deref()
                        .map(|m| format!(" ({m})"))
                        .unwrap_or_default();
                    s.message = Some(format!(
                        "Activated endpoint {}{model}. Restart agy to use it.",
                        done.alias
                    ));
                    reload_cache(s, &mut jobs);
                }
                Err(e) => s.error = Some(format!("Endpoint switch failed: {e}")),
            }
            if s.quit_after_switch {
                s.quit = true;
            }
            jobs
        }
        Msg::EndpointTested(r) => {
            s.endpoint_busy = false;
            match r {
                Ok(done) => {
                    if done.probe.reachable {
                        s.error = None;
                        let lat = done
                            .probe
                            .latency_ms
                            .map(|ms| format!("{ms}ms"))
                            .unwrap_or_else(|| "ok".to_owned());
                        s.message = Some(format!(
                            "Endpoint {} reachable ({lat}, {} model(s)).",
                            done.alias, done.probe.model_count
                        ));
                    } else {
                        let detail = done
                            .probe
                            .error
                            .unwrap_or_else(|| "endpoint unreachable".to_owned());
                        s.error = Some(format!("Endpoint {} test failed: {detail}", done.alias));
                    }
                }
                Err(e) => s.error = Some(format!("Endpoint test failed: {e}")),
            }
            vec![]
        }
        Msg::EndpointModelsFetched(r) => {
            s.endpoint_busy = false;
            let mut jobs = vec![];
            match r {
                Ok(done) => {
                    if done.probe.reachable {
                        s.error = None;
                        let lat = done
                            .probe
                            .latency_ms
                            .map(|ms| format!(" in {ms}ms"))
                            .unwrap_or_default();
                        s.message = Some(format!(
                            "Fetched {} model(s) for {}{lat}.",
                            done.probe.model_count, done.alias
                        ));
                    } else {
                        let detail = done
                            .probe
                            .error
                            .unwrap_or_else(|| "endpoint unreachable".to_owned());
                        s.error = Some(format!("Model fetch failed for {}: {detail}", done.alias));
                    }
                    reload_cache(s, &mut jobs);
                }
                Err(e) => s.error = Some(format!("Model fetch failed: {e}")),
            }
            jobs
        }
        Msg::RouteApplied(r) => {
            s.switching = false;
            let mut jobs = vec![];
            match r {
                Ok(done) => {
                    s.error = None;
                    let mut text = if done.already_active {
                        format!("{} is already active.", done.alias)
                    } else {
                        format!(
                            "Switched to {} via route. Restart agy to use it.",
                            done.alias
                        )
                    };
                    for w in &done.warnings {
                        text.push_str(&format!(" Note: {w}"));
                    }
                    s.message = Some(text);
                    reload_cache(s, &mut jobs);
                }
                Err(e) => s.error = Some(format!("Route apply failed: {e}")),
            }
            if s.quit_after_switch {
                s.quit = true;
            }
            jobs
        }
        Msg::ProfileActivated(r) => {
            s.switching = false;
            let mut jobs = vec![];
            match r {
                Ok(done) => {
                    s.error = None;
                    let mut parts = vec![format!("Activated profile {}", done.name)];
                    if let Some(acct) = &done.switched_account {
                        parts.push(format!("account: {acct}"));
                    }
                    if let Some(ep) = &done.active_endpoint {
                        parts.push(format!("endpoint: {ep}"));
                    }
                    let mut text = format!("{}.", parts.join(", "));
                    for w in &done.warnings {
                        text.push_str(&format!(" Note: {w}"));
                    }
                    s.message = Some(text);
                    reload_cache(s, &mut jobs);
                }
                Err(e) => s.error = Some(format!("Profile activation failed: {e}")),
            }
            if s.quit_after_switch {
                s.quit = true;
            }
            jobs
        }
    }
}

fn on_key(s: &mut AppState, k: KeyEvent) -> Vec<Job> {
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    if ctrl && matches!(k.code, KeyCode::Char('c')) {
        request_quit(s);
        return vec![];
    }
    if let Some(overlay) = s.overlay.clone() {
        return on_overlay_key(s, overlay, k);
    }
    if s.filter_editing {
        return on_filter_key(s, k);
    }
    if s.message.take().is_some() {
        s.dirty = true;
    }
    match k.code {
        KeyCode::Char('q') => request_quit(s),
        KeyCode::Esc if !s.filter.is_empty() => {
            s.filter.clear();
            s.normalize_selection(0);
        }
        KeyCode::Esc => request_quit(s),
        KeyCode::Tab => set_view(s, s.view.step(1)),
        KeyCode::BackTab => set_view(s, s.view.step(-1)),
        KeyCode::F(n @ 1..=5) => set_view(s, View::ALL[usize::from(n) - 1]),
        KeyCode::Up | KeyCode::Char('k') => step(s, -1),
        KeyCode::Down | KeyCode::Char('j') => step(s, 1),
        KeyCode::PageUp => scroll(s, -(PAGE as isize)),
        KeyCode::PageDown => scroll(s, PAGE as isize),
        KeyCode::Home => s.scroll = 0,
        KeyCode::End => s.scroll = super::views::max_scroll(s),
        KeyCode::Enter => return on_enter(s),
        KeyCode::Char('r') => return on_refresh(s),
        KeyCode::Char('s') if s.view == View::Accounts => {
            s.sort_order = s.sort_order.next();
            s.message = Some(format!("Sorted by {}", s.sort_order.label()));
        }
        KeyCode::Char('t') if s.view == View::Endpoints => return on_endpoint_probe(s, false),
        KeyCode::Char('f') if s.view == View::Endpoints => return on_endpoint_probe(s, true),
        KeyCode::Char('d') if s.has_details() => s.open_overlay(Overlay::Details),
        KeyCode::Char('?') => s.open_overlay(Overlay::Help),
        KeyCode::Char('/') if s.view != View::Diagnostics => s.filter_editing = true,
        KeyCode::Char(c @ '1'..='9') => quick_select(s, c),
        _ => return vec![],
    }
    s.dirty = true;
    vec![]
}

fn set_view(s: &mut AppState, v: View) {
    s.view = v;
    s.scroll = 0;
    s.normalize_selection(0);
}

/// Arrows move the selection, except in Diagnostics where they scroll.
fn step(s: &mut AppState, delta: isize) {
    if s.view == View::Diagnostics {
        scroll(s, delta);
    } else {
        s.move_selection(delta);
    }
}

fn scroll(s: &mut AppState, delta: isize) {
    let max = super::views::max_scroll(s);
    let current = if s.overlay.is_some() {
        &mut s.overlay_scroll
    } else {
        &mut s.scroll
    };
    *current = (*current).min(max).saturating_add_signed(delta).min(max);
}

fn on_enter(s: &mut AppState) -> Vec<Job> {
    s.dirty = true;
    if s.view == View::Diagnostics {
        return vec![];
    }
    if s.switching {
        s.message = Some("a switch is already running".into());
        return vec![];
    }
    match s.view {
        View::Accounts | View::Usage => {
            let Some(a) = s.selected() else {
                return vec![];
            };
            if a.active {
                s.message = Some(format!("{} is already active.", a.alias));
                return vec![];
            }
            s.open_overlay(Overlay::Confirm {
                id: a.id.clone(),
                alias: a.alias.clone(),
            });
        }
        View::Endpoints => {
            let Some(ep) = s.selected_endpoint() else {
                return vec![];
            };
            if !ep.enabled {
                s.message = Some(format!("endpoint {} is disabled.", ep.alias));
                return vec![];
            }
            if ep.active {
                s.message = Some(format!("endpoint {} is already active.", ep.alias));
                return vec![];
            }
            s.open_overlay(Overlay::ConfirmEndpoint {
                id: ep.id.clone(),
                alias: ep.alias.clone(),
            });
        }
        View::Routes => {
            let Some(item) = s.selected_route_item() else {
                return vec![];
            };
            match item.kind {
                RouteItemKind::Profile(p) => {
                    s.open_overlay(Overlay::ConfirmProfile {
                        name: p.name.clone(),
                    });
                }
                RouteItemKind::Group(g) => {
                    let Some(rec_id) = g.plan.recommendation.as_deref() else {
                        s.message =
                            Some(format!("route group `{}` has no eligible account.", g.name));
                        return vec![];
                    };
                    let target = g
                        .plan
                        .accounts
                        .iter()
                        .find(|r| r.id == rec_id)
                        .map(|r| r.alias.clone());
                    s.open_overlay(Overlay::ConfirmRoute {
                        group: g.name.clone(),
                        target,
                    });
                }
            }
        }
        View::Diagnostics => {}
    }
    vec![]
}

fn on_endpoint_probe(s: &mut AppState, fetch_models: bool) -> Vec<Job> {
    s.dirty = true;
    if s.endpoint_busy {
        s.message = Some("endpoint probe already running".into());
        return vec![];
    }
    let Some(ep) = s.selected_endpoint().cloned() else {
        return vec![];
    };
    s.endpoint_busy = true;
    s.error = None;
    if fetch_models {
        s.message = Some(format!("fetching models for {}...", ep.alias));
        vec![Job::FetchEndpointModels { id: ep.id }]
    } else {
        s.message = Some(format!("testing {}...", ep.alias));
        vec![Job::TestEndpoint { id: ep.id }]
    }
}

fn on_refresh(s: &mut AppState) -> Vec<Job> {
    s.dirty = true;
    if s.loading() {
        s.message = Some("already refreshing".into());
        return vec![];
    }
    s.pending_loads += 1;
    s.error = None;
    vec![Job::Load(RefreshMode::Normal)]
}

/// A digit picks that item number, unless more digits could still follow
/// (with 10+ items, `1` might start `10`): then it asks to use arrows.
fn quick_select(s: &mut AppState, c: char) {
    let n = c.to_digit(10).unwrap_or(0) as usize;
    match s.view {
        View::Accounts | View::Usage => {
            if s.total() >= n * 10 {
                s.message = Some("more than 9 accounts: use the arrow keys or / to filter".into());
                return;
            }
            let id = s
                .visible()
                .iter()
                .find(|a| a.number == n)
                .map(|a| a.id.clone());
            match id {
                Some(id) => s.select_id(&id),
                None => s.message = Some(format!("no account {n} in this list")),
            }
        }
        View::Endpoints => {
            let total = s.endpoints().len();
            if total >= n * 10 {
                s.message = Some("more than 9 endpoints: use the arrow keys or / to filter".into());
                return;
            }
            let target_id = s
                .endpoints()
                .get(n.saturating_sub(1))
                .map(|e| e.id.as_str());
            let visible_id = target_id.and_then(|id| {
                s.visible_endpoints()
                    .iter()
                    .find(|e| e.id == id)
                    .map(|e| e.id.clone())
            });
            match visible_id {
                Some(id) => s.select_endpoint_id(&id),
                None => s.message = Some(format!("no endpoint {n} in this list")),
            }
        }
        View::Routes => {
            let total = s.route_items().len();
            if total >= n * 10 {
                s.message =
                    Some("more than 9 route items: use the arrow keys or / to filter".into());
                return;
            }
            let key = s
                .visible_route_items()
                .into_iter()
                .find(|item| item.number == n)
                .map(|item| item.key);
            match key {
                Some(k) => s.select_route_id(&k),
                None => s.message = Some(format!("no item {n} in this list")),
            }
        }
        View::Diagnostics => {}
    }
}

fn on_overlay_key(s: &mut AppState, overlay: Overlay, k: KeyEvent) -> Vec<Job> {
    s.dirty = true;
    match (&overlay, k.code) {
        (Overlay::Confirm { id, .. }, KeyCode::Char('y' | 'Y')) => {
            s.overlay = None;
            s.switching = true;
            s.error = None;
            s.message = Some("switching...".into());
            return vec![Job::Switch { id: id.clone() }];
        }
        (Overlay::ConfirmEndpoint { id, .. }, KeyCode::Char('y' | 'Y')) => {
            s.overlay = None;
            s.switching = true;
            s.error = None;
            s.message = Some("activating endpoint...".into());
            return vec![Job::SwitchEndpoint { id: id.clone() }];
        }
        (Overlay::ConfirmRoute { group, .. }, KeyCode::Char('y' | 'Y')) => {
            s.overlay = None;
            s.switching = true;
            s.error = None;
            s.message = Some(format!("applying route `{group}`..."));
            return vec![Job::ApplyRoute {
                group: group.clone(),
            }];
        }
        (Overlay::ConfirmProfile { name }, KeyCode::Char('y' | 'Y')) => {
            s.overlay = None;
            s.switching = true;
            s.error = None;
            s.message = Some(format!("activating profile `{name}`..."));
            return vec![Job::ActivateProfile { name: name.clone() }];
        }
        (
            Overlay::Confirm { .. }
            | Overlay::ConfirmEndpoint { .. }
            | Overlay::ConfirmRoute { .. }
            | Overlay::ConfirmProfile { .. },
            KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc,
        ) => {
            s.overlay = None;
        }
        (
            Overlay::Confirm { .. }
            | Overlay::ConfirmEndpoint { .. }
            | Overlay::ConfirmRoute { .. }
            | Overlay::ConfirmProfile { .. },
            KeyCode::Enter | KeyCode::Char('?' | 'd'),
        ) => {}
        (_, KeyCode::Esc | KeyCode::Char('q' | '?' | 'd') | KeyCode::Enter) => s.overlay = None,
        (_, KeyCode::Up | KeyCode::Char('k')) => scroll(s, -1),
        (_, KeyCode::Down | KeyCode::Char('j')) => scroll(s, 1),
        (_, KeyCode::PageUp) => scroll(s, -(PAGE as isize)),
        (_, KeyCode::PageDown) => scroll(s, PAGE as isize),
        (_, KeyCode::Home) => s.overlay_scroll = 0,
        (_, KeyCode::End) => s.overlay_scroll = super::views::max_scroll(s),
        _ => {}
    }
    vec![]
}

fn on_filter_key(s: &mut AppState, k: KeyEvent) -> Vec<Job> {
    s.dirty = true;
    match k.code {
        KeyCode::Esc => {
            s.filter.clear();
            s.filter_editing = false;
        }
        KeyCode::Enter => s.filter_editing = false,
        KeyCode::Backspace => {
            s.filter.pop();
        }
        KeyCode::Up => s.move_selection(-1),
        KeyCode::Down => s.move_selection(1),
        KeyCode::Char('?') => s.open_overlay(Overlay::Help),
        KeyCode::Char(c) if !c.is_control() && s.filter.chars().count() < 64 => s.filter.push(c),
        _ => {}
    }
    s.normalize_selection(0);
    vec![]
}
