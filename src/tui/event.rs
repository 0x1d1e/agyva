//! The state machine: `update(state, event) -> jobs`. Pure apart from the
//! state it is given, so every key path is testable without a terminal.

use chrono::{DateTime, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::app::{AppState, Overlay, View};
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
    s.pending_loads = 0;
    s.error = Some("The background worker stopped unexpectedly; quit and reopen agyva.".into());
    s.dirty = true;
    if s.quit_after_switch {
        s.quit = true;
    }
}

fn request_quit(s: &mut AppState) {
    if s.switching {
        // Never abandon a credential switch half way.
        s.quit_after_switch = true;
        s.message = Some("finishing the switch before quitting...".into());
        s.dirty = true;
    } else {
        s.quit = true;
    }
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
                    s.pending_loads += 1;
                    jobs.push(Job::Load(RefreshMode::Skip));
                }
                Err(e) => s.error = Some(format!("Switch failed: {e}")),
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
        KeyCode::F(n @ 1..=3) => set_view(s, View::ALL[usize::from(n) - 1]),
        KeyCode::Up | KeyCode::Char('k') => step(s, -1),
        KeyCode::Down | KeyCode::Char('j') => step(s, 1),
        KeyCode::PageUp => s.scroll = s.scroll.saturating_sub(PAGE),
        KeyCode::PageDown => s.scroll += PAGE,
        KeyCode::Enter => return on_enter(s),
        KeyCode::Char('r') => return on_refresh(s),
        KeyCode::Char('d') if s.selected().is_some() => s.overlay = Some(Overlay::Details),
        KeyCode::Char('?') => s.overlay = Some(Overlay::Help),
        KeyCode::Char('/') => s.filter_editing = true,
        KeyCode::Char(c @ '1'..='9') => quick_select(s, c),
        _ => return vec![],
    }
    s.dirty = true;
    vec![]
}

fn set_view(s: &mut AppState, v: View) {
    s.view = v;
    s.scroll = 0;
}

/// Arrows move the selection, except in Diagnostics where they scroll.
fn step(s: &mut AppState, delta: isize) {
    if s.view == View::Diagnostics {
        s.scroll = if delta < 0 {
            s.scroll.saturating_sub(1)
        } else {
            s.scroll + 1
        };
    } else {
        s.move_selection(delta);
    }
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
    let Some(a) = s.selected() else {
        return vec![];
    };
    if a.active {
        s.message = Some(format!("{} is already active.", a.alias));
        return vec![];
    }
    s.overlay = Some(Overlay::Confirm {
        id: a.id.clone(),
        alias: a.alias.clone(),
    });
    vec![]
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

/// A digit picks that account number, unless more digits could still follow
/// (with 10+ accounts, `1` might start `10`): then it asks to use arrows.
fn quick_select(s: &mut AppState, c: char) {
    let n = c.to_digit(10).unwrap_or(0) as usize;
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
        (Overlay::Confirm { .. }, KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc) => {
            s.overlay = None;
        }
        (Overlay::Confirm { .. }, _) => {}
        (_, KeyCode::Esc | KeyCode::Char('q' | '?' | 'd') | KeyCode::Enter) => s.overlay = None,
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
        KeyCode::Char(c) if !c.is_control() && s.filter.chars().count() < 64 => s.filter.push(c),
        _ => {}
    }
    s.normalize_selection(0);
    vec![]
}
