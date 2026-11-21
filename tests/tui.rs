//! The interactive view without a real terminal: the pure state machine, the
//! renderers on ratatui's `TestBackend`, and the worker against the real core
//! (in-memory keyring and credential fakes, a synthetic fake `agy`).

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agyva::core::app_integrations::EndpointView;
use agyva::core::dashboard::{Dashboard, RouteGroupSnapshot};
use agyva::core::doctor::{Check, Level};
use agyva::core::health::{APPEAL_NOTICE, AccountHealth};
use agyva::core::profile::{ProfileSource, ProfileSpec, ProfileSummary, ResolvedProfile};
use agyva::core::protocol::{EndpointProtocol, FetchedModel, ProbeResult, RateLimitInfo};
use agyva::core::quota_history::BurnRateView;
use agyva::core::route::{RoutePlan, RouteRow};
use agyva::core::route_config::Ranking;
use agyva::core::usage::RefreshMode;
use agyva::storage::keyring::StoreErrorKind;
use agyva::tui::app::{AppState, Overlay, SortOrder, View};
use agyva::tui::backend::{
    AppBackend, Backend, EndpointProbeSummary, EndpointSwitchSummary, Job, Msg,
    ProfileSwitchSummary, Reply, SwitchSummary, spawn_worker,
};
use agyva::tui::event::{Event, startup, update};
use agyva::tui::views::{self, column_plan};
use chrono::Utc;
use common::fake_agy::FakeAgy;
use common::{ALICE, BOB, CAROL, Rig, id_of};
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use unicode_width::UnicodeWidthStr;

// ---------------------------------------------------------------- helpers

/// Three saved accounts; carol is the active one, with fresh synthetic quota.
fn base() -> (Rig, FakeAgy, Dashboard) {
    let rig = Rig::new();
    let fake = FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    fake.ok(0.72, 0.4);
    rig.add_all(&[ALICE, BOB, CAROL]);
    let d = rig
        .app_with_agy(&fake)
        .dashboard(RefreshMode::Normal)
        .expect("dashboard");
    (rig, fake, d)
}

fn many(base: &Dashboard, n: usize, alias: impl Fn(usize) -> String) -> Dashboard {
    let proto = base.usage.accounts[0].clone();
    let mut d = base.clone();
    d.usage.accounts = (1..=n)
        .map(|i| {
            let mut a = proto.clone();
            a.number = i;
            a.id = format!("id{i:04}");
            a.alias = alias(i);
            a.email = Some(format!("user{i}@example.invalid"));
            a.active = i == 1;
            a
        })
        .collect();
    d
}

fn state(d: Option<Dashboard>, size: (u16, u16)) -> AppState {
    let mut s = AppState::new(size, Utc::now());
    if let Some(d) = d {
        s.apply_dashboard(d);
    }
    s
}

fn key(code: KeyCode) -> Event {
    Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
}

fn ch(c: char) -> Event {
    key(KeyCode::Char(c))
}

fn press(s: &mut AppState, e: Event) -> Vec<Job> {
    update(s, e)
}

fn type_str(s: &mut AppState, text: &str) {
    for c in text.chars() {
        update(s, ch(c));
    }
}

/// Render to plain text, one string per row. Wide characters occupy two cells
/// but one symbol, so the walk skips the cell they cover.
fn draw(s: &AppState, w: u16, h: u16) -> Vec<String> {
    let mut t = Terminal::new(TestBackend::new(w, h)).expect("terminal");
    t.draw(|f| views::render(s, f)).expect("draw");
    let buf = t.backend().buffer().clone();
    (0..h)
        .map(|y| {
            let mut row = String::new();
            let mut x = 0;
            while x < w {
                let sym = buf[(x, y)].symbol();
                row.push_str(sym);
                x += u16::try_from(sym.width().max(1)).unwrap_or(1);
            }
            row
        })
        .collect()
}

fn text(s: &AppState, w: u16, h: u16) -> String {
    draw(s, w, h).join("\n")
}

// ------------------------------------------------------- a scripted backend

struct Fake {
    data: Mutex<Reply<Dashboard>>,
    loads: Mutex<Vec<RefreshMode>>,
    switches: Mutex<Vec<String>>,
    endpoint_switches: Mutex<Vec<String>>,
    endpoint_tests: Mutex<Vec<String>>,
    endpoint_fetches: Mutex<Vec<String>>,
    route_applies: Mutex<Vec<String>>,
    profile_activates: Mutex<Vec<String>>,
}

impl Fake {
    fn new(d: Reply<Dashboard>) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(d),
            loads: Mutex::default(),
            switches: Mutex::default(),
            endpoint_switches: Mutex::default(),
            endpoint_tests: Mutex::default(),
            endpoint_fetches: Mutex::default(),
            route_applies: Mutex::default(),
            profile_activates: Mutex::default(),
        })
    }
}

impl Backend for Fake {
    fn load(&self, mode: RefreshMode) -> Reply<Dashboard> {
        self.loads.lock().expect("lock").push(mode);
        self.data.lock().expect("lock").clone()
    }
    fn switch(&self, id: &str) -> Reply<SwitchSummary> {
        self.switches.lock().expect("lock").push(id.to_owned());
        Ok(SwitchSummary {
            alias: id.to_owned(),
            already_active: false,
            warnings: vec![],
        })
    }
    fn switch_endpoint(&self, id: &str) -> Reply<EndpointSwitchSummary> {
        self.endpoint_switches
            .lock()
            .expect("lock")
            .push(id.to_owned());
        Ok(EndpointSwitchSummary {
            alias: id.to_owned(),
            default_model: Some("gpt-4o".to_owned()),
        })
    }
    fn test_endpoint(&self, id: &str) -> Reply<EndpointProbeSummary> {
        self.endpoint_tests
            .lock()
            .expect("lock")
            .push(id.to_owned());
        Ok(EndpointProbeSummary {
            id: id.to_owned(),
            alias: id.to_owned(),
            probe: Box::new(ProbeResult {
                reachable: true,
                detected_protocol: Some(EndpointProtocol::OpenAi),
                latency_ms: Some(42),
                model_count: 2,
                models: vec![],
                rate_limits: None,
                error: None,
            }),
        })
    }
    fn fetch_endpoint_models(&self, id: &str) -> Reply<EndpointProbeSummary> {
        self.endpoint_fetches
            .lock()
            .expect("lock")
            .push(id.to_owned());
        Ok(EndpointProbeSummary {
            id: id.to_owned(),
            alias: id.to_owned(),
            probe: Box::new(ProbeResult {
                reachable: true,
                detected_protocol: Some(EndpointProtocol::OpenAi),
                latency_ms: Some(55),
                model_count: 3,
                models: vec![],
                rate_limits: None,
                error: None,
            }),
        })
    }
    fn apply_route(&self, group: &str) -> Reply<SwitchSummary> {
        self.route_applies
            .lock()
            .expect("lock")
            .push(group.to_owned());
        Ok(SwitchSummary {
            alias: "bob".to_owned(),
            already_active: false,
            warnings: vec![],
        })
    }
    fn activate_profile(&self, name: &str) -> Reply<ProfileSwitchSummary> {
        self.profile_activates
            .lock()
            .expect("lock")
            .push(name.to_owned());
        Ok(ProfileSwitchSummary {
            name: name.to_owned(),
            switched_account: Some("alice".to_owned()),
            active_endpoint: None,
            warnings: vec![],
        })
    }
}

// ------------------------------------------------------------------ layout

#[test]
fn column_plan_drops_columns_progressively() {
    let wide = column_plan(120);
    assert!(wide.five && wide.status && wide.weekly && wide.cache);
    let w40 = column_plan(40);
    assert!(w40.five && w40.status && !w40.weekly && !w40.cache);
    assert!(w40.alias >= 6);
    let w30 = column_plan(30);
    assert!(w30.five && !w30.status);
    let w20 = column_plan(20);
    assert!(!w20.five && !w20.status && !w20.weekly && !w20.cache);
    assert!(w20.alias >= 6);
    assert_eq!(column_plan(1).alias, 1);
    assert_eq!(column_plan(0).alias, 1);
}

#[test]
fn every_view_and_overlay_renders_at_any_size_without_panicking() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (80, 24));
    let overlays = [
        None,
        Some(Overlay::Help),
        Some(Overlay::Details),
        Some(Overlay::Confirm {
            id: s.accounts()[0].id.clone(),
            alias: s.accounts()[0].alias.clone(),
        }),
        Some(Overlay::ConfirmEndpoint {
            id: "ep01".into(),
            alias: "openai-main".into(),
        }),
        Some(Overlay::ConfirmRoute {
            group: "work".into(),
            target: Some("bob".into()),
        }),
        Some(Overlay::ConfirmProfile { name: "dev".into() }),
    ];
    for view in View::ALL {
        for overlay in &overlays {
            s.view = view;
            s.overlay = overlay.clone();
            for (w, h) in [
                (200, 40),
                (120, 40),
                (80, 24),
                (40, 20),
                (20, 20),
                (20, 5),
                (12, 4),
                (4, 3),
                (3, 2),
                (1, 1),
                (0, 0),
                (200, 3),
            ] {
                let _ = draw(&s, w, h);
            }
        }
    }
    // Also with no data at all, and with an error showing.
    let mut empty = state(None, (40, 20));
    empty.error = Some("Refresh failed: x".into());
    for view in View::ALL {
        empty.view = view;
        let _ = draw(&empty, 40, 20);
        let _ = draw(&empty, 20, 20);
    }
}

#[test]
fn accounts_view_shows_marker_alias_quota_status_and_dashes_when_unknown() {
    let (_rig, _fake, d) = base();
    let s = state(Some(d), (100, 20));
    let out = text(&s, 100, 20);
    assert!(
        out.contains("ACCOUNT") && out.contains("5H") && out.contains("WEEKLY"),
        "{out}"
    );
    let active = out
        .lines()
        .find(|l| l.contains("carol") && l.contains(" 3 "))
        .expect("carol row");
    assert!(active.contains('\u{25cf}'), "active marker: {active}");
    assert!(active.contains("72%") && active.contains("40%"), "{active}");
    assert!(active.to_lowercase().contains("healthy"), "{active}");
    // Never read: dashes, never invented numbers.
    let other = out
        .lines()
        .find(|l| l.contains("alice"))
        .expect("alice row");
    assert!(other.contains('\u{2014}'), "{other}");
    assert!(!other.contains('%'), "{other}");
    assert!(!other.contains('\u{25cf}'));
}

#[test]
fn narrow_terminals_keep_the_alias_and_never_overflow() {
    let (_rig, _fake, d) = base();
    let s = state(Some(d), (40, 20));
    for w in [40u16, 30, 20, 12] {
        for row in draw(&s, w, 20) {
            assert!(row.width() <= usize::from(w), "{w}: {row:?}");
        }
        assert!(text(&s, w, 20).contains("carol"), "{w}");
    }
    let t40 = text(&s, 40, 20);
    assert!(t40.contains("5H"), "{t40}");
    assert!(
        !t40.contains("WEEKLY"),
        "weekly column dropped at 40: {t40}"
    );
}

#[test]
fn unicode_aliases_and_emails_are_truncated_by_width() {
    let (_rig, _fake, d) = base();
    let names = [
        "日本語のアカウント名前",
        "🦀🦀🦀 crab account",
        "Zoë Ünïcode",
    ];
    let d = many(&d, 3, |i| names[i - 1].to_owned());
    let mut s = state(Some(d), (40, 20));
    for w in [60u16, 40, 24, 20] {
        for row in draw(&s, w, 20) {
            assert!(row.width() <= usize::from(w), "{w}: {row:?}");
        }
    }
    assert!(
        text(&s, 24, 20).contains('\u{2026}'),
        "long CJK alias is cut with an ellipsis"
    );
    s.overlay = Some(Overlay::Details);
    let _ = draw(&s, 30, 12);
}

#[test]
fn five_hundred_accounts_navigate_and_stay_visible() {
    let (_rig, _fake, d) = base();
    let d = many(&d, 500, |i| format!("acct-{i:03}"));
    let mut s = state(Some(d), (60, 12));
    assert_eq!(
        s.selected().map(|a| a.number),
        Some(1),
        "starts on the active account"
    );
    press(&mut s, key(KeyCode::Up));
    assert_eq!(
        s.selected().map(|a| a.number),
        Some(500),
        "wraps to the end"
    );
    // Rows are narrow here, so the alias may be cut: the number is the anchor.
    assert!(text(&s, 60, 12).contains(" 500 "), "{}", text(&s, 60, 12));
    for _ in 0..250 {
        press(&mut s, key(KeyCode::Up));
    }
    let n = s.selected().map(|a| a.number).expect("selected");
    assert_eq!(n, 250);
    assert!(text(&s, 60, 12).contains(" 250 "), "{}", text(&s, 60, 12));
    press(&mut s, ch('x'));
    // Ten or more digits ambiguity: quick select refuses instead of guessing.
    press(&mut s, ch('3'));
    assert_eq!(s.selected().map(|a| a.number), Some(250));
    assert!(
        s.message
            .as_deref()
            .is_some_and(|m| m.contains("more than 9"))
    );
}

// ------------------------------------------------------------ interaction

#[test]
fn navigation_wraps_and_number_keys_select() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (80, 20));
    let first = s.accounts()[0].id.clone();
    let last = s.accounts()[2].id.clone();
    s.select_id(&first);
    press(&mut s, key(KeyCode::Up));
    assert_eq!(s.selected_id.as_deref(), Some(last.as_str()));
    press(&mut s, ch('j'));
    assert_eq!(s.selected_id.as_deref(), Some(first.as_str()));
    press(&mut s, ch('k'));
    assert_eq!(s.selected_id.as_deref(), Some(last.as_str()));
    press(&mut s, ch('2'));
    assert_eq!(s.selected().map(|a| a.number), Some(2));
    press(&mut s, ch('9'));
    assert_eq!(
        s.selected().map(|a| a.number),
        Some(2),
        "unknown number changes nothing"
    );
    assert!(s.message.is_some());
}

#[test]
fn views_switch_with_tab_backtab_and_function_keys() {
    let mut s = state(None, (80, 20));
    press(&mut s, key(KeyCode::Tab));
    assert_eq!(s.view, View::Usage);
    press(&mut s, key(KeyCode::Tab));
    assert_eq!(s.view, View::Endpoints);
    press(&mut s, key(KeyCode::Tab));
    assert_eq!(s.view, View::Routes);
    press(&mut s, key(KeyCode::Tab));
    assert_eq!(s.view, View::Diagnostics);
    press(&mut s, key(KeyCode::Tab));
    assert_eq!(s.view, View::Accounts);
    press(&mut s, key(KeyCode::BackTab));
    assert_eq!(s.view, View::Diagnostics);
    press(&mut s, key(KeyCode::F(2)));
    assert_eq!(s.view, View::Usage);
    press(&mut s, key(KeyCode::F(3)));
    assert_eq!(s.view, View::Endpoints);
    press(&mut s, key(KeyCode::F(4)));
    assert_eq!(s.view, View::Routes);
    press(&mut s, key(KeyCode::F(5)));
    assert_eq!(s.view, View::Diagnostics);
    press(&mut s, key(KeyCode::F(1)));
    assert_eq!(s.view, View::Accounts);
}

#[test]
fn filter_narrows_live_keeps_a_selection_and_escape_clears() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (80, 20));
    press(&mut s, ch('/'));
    assert!(s.filter_editing);
    type_str(&mut s, "bob");
    assert_eq!(s.visible().len(), 1);
    assert_eq!(
        s.selected().map(|a| a.alias.clone()).as_deref(),
        Some("bob")
    );
    let out = text(&s, 80, 20);
    assert!(out.contains("/bob"), "{out}");
    assert!(!out.contains("alice"));
    type_str(&mut s, "zzz");
    assert!(s.visible().is_empty());
    assert!(text(&s, 80, 20).contains("No account matches"));
    assert!(
        press(&mut s, key(KeyCode::Enter)).is_empty(),
        "no selection, no switch"
    );
    press(&mut s, key(KeyCode::Esc));
    assert!(!s.filter_editing && s.filter.is_empty());
    assert_eq!(s.visible().len(), 3);
    assert!(s.selected().is_some(), "selection restored");
    assert!(!s.quit, "Esc cleared the filter instead of quitting");
    // Keeping the filter with Enter, then Esc clears it, then Esc quits.
    press(&mut s, ch('/'));
    type_str(&mut s, "ali");
    press(&mut s, key(KeyCode::Enter));
    assert!(!s.filter_editing && s.filter == "ali");
    press(&mut s, key(KeyCode::Esc));
    assert!(s.filter.is_empty() && !s.quit);
    press(&mut s, key(KeyCode::Esc));
    assert!(s.quit);
}

#[test]
fn selection_follows_the_account_id_across_data_updates() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d.clone()), (80, 20));
    let bob = s
        .accounts()
        .iter()
        .find(|a| a.alias == "bob")
        .expect("bob")
        .id
        .clone();
    s.select_id(&bob);

    // Reordered and with an extra account in front.
    let mut moved = d.clone();
    moved.usage.accounts.reverse();
    let mut extra = moved.usage.accounts[0].clone();
    extra.id = "zzz".into();
    extra.alias = "zed".into();
    moved.usage.accounts.insert(0, extra);
    s.apply_dashboard(moved);
    assert_eq!(s.selected_id.as_deref(), Some(bob.as_str()));

    // The selected account disappears: the neighbouring row is selected.
    let mut gone = d.clone();
    gone.usage.accounts.retain(|a| a.id != bob);
    s.apply_dashboard(gone);
    let sel = s.selected().expect("still something selected");
    assert_ne!(sel.id, bob);
}

#[test]
fn overlays_open_and_close_without_quitting_or_acting() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (80, 24));
    press(&mut s, ch('?'));
    assert_eq!(s.overlay, Some(Overlay::Help));
    assert!(text(&s, 80, 24).contains("Help"));
    press(&mut s, key(KeyCode::Esc));
    assert!(s.overlay.is_none() && !s.quit);

    press(&mut s, ch('d'));
    assert_eq!(s.overlay, Some(Overlay::Details));
    let out = text(&s, 80, 24);
    assert!(
        out.contains("Details") && out.contains("carol@example.invalid"),
        "{out}"
    );
    press(&mut s, ch('q'));
    assert!(s.overlay.is_none() && !s.quit, "q closes the overlay first");
    press(&mut s, ch('q'));
    assert!(s.quit);
}

#[test]
fn refresh_is_background_never_forced_and_not_stacked() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (80, 20));
    let jobs = press(&mut s, ch('r'));
    assert_eq!(
        jobs,
        vec![Job::Load(RefreshMode::Normal)],
        "r never forces a check"
    );
    assert!(s.loading());
    assert!(text(&s, 80, 20).contains("refreshing"));
    assert!(
        press(&mut s, ch('r')).is_empty(),
        "no second refresh while one runs"
    );
    assert!(s.message.as_deref().is_some_and(|m| m.contains("already")));
    update(
        &mut s,
        Event::Msg(Msg::Loaded {
            mode: RefreshMode::Normal,
            result: Box::new(Err("agy is not installed".into())),
        }),
    );
    assert!(!s.loading());
    let out = text(&s, 100, 20);
    assert!(
        out.contains("Refresh failed: agy is not installed"),
        "{out}"
    );
    assert!(out.contains("carol"), "the last good data stays on screen");
}

#[test]
fn startup_shows_the_cache_first_then_refreshes() {
    let mut s = state(None, (80, 20));
    let jobs = startup(&mut s);
    assert_eq!(
        jobs,
        vec![Job::Load(RefreshMode::Skip), Job::Load(RefreshMode::Normal)]
    );
    let out = text(&s, 80, 20);
    assert!(out.contains("Loading"), "{out}");
    assert!(
        !out.contains('%'),
        "no numbers before anything was read: {out}"
    );
}

#[test]
fn quit_waits_for_a_running_switch() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (80, 20));
    s.switching = true;
    press(&mut s, ch('q'));
    assert!(!s.quit && s.quit_after_switch);
    update(&mut s, Event::Interrupt);
    assert!(!s.quit);
    let done = SwitchSummary {
        alias: "bob".into(),
        already_active: false,
        warnings: vec![],
    };
    update(&mut s, Event::Msg(Msg::Switched(Ok(done))));
    assert!(s.quit);
}

#[test]
fn ctrl_c_and_interrupts_quit_cleanly() {
    let mut s = state(None, (80, 20));
    update(
        &mut s,
        Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
    );
    assert!(s.quit);
    let mut s = state(None, (80, 20));
    update(&mut s, Event::Interrupt);
    assert!(s.quit);
}

#[test]
fn redraws_only_when_something_changed() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (80, 20));
    s.dirty = false;
    press(&mut s, ch('x'));
    assert!(!s.dirty, "an unmapped key changes nothing");
    let mut release = KeyEvent::new(KeyCode::Down, KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    update(&mut s, Event::Key(release));
    assert!(!s.dirty, "key release is ignored");
    update(&mut s, Event::Tick(Utc::now()));
    assert!(s.dirty);
    s.dirty = false;
    update(&mut s, Event::Resize(50, 10));
    assert!(s.dirty && s.size == (50, 10));
    let out = text(&s, 50, 10);
    assert!(out.contains("carol"));
}

// ------------------------------------------------------- health and views

#[test]
fn suspension_is_a_prominent_warning_with_the_faq_and_no_workaround() {
    let (_rig, _fake, mut d) = base();
    d.usage.accounts[0].health.state = AccountHealth::TermsSuspended;
    d.usage.accounts[0].health.label = AccountHealth::TermsSuspended.label();
    d.usage.accounts[0].health.warning = Some(APPEAL_NOTICE.to_owned());
    let alias = d.usage.accounts[0].alias.clone();
    let mut s = state(Some(d), (120, 30));
    let out = text(&s, 120, 30);
    assert!(out.contains("SUSPENDED") && out.contains(&alias), "{out}");
    assert!(out.contains("https://antigravity.google/docs/faq"), "{out}");
    assert!(!out.to_lowercase().contains("retry now"));
    assert!(out.contains('!'), "row carries a non-color marker");
    // Narrow screens still show the banner.
    assert!(text(&s, 40, 20).contains("SUSPENDED"));
    s.view = View::Diagnostics;
    let diag = text(&s, 120, 40);
    assert!(
        diag.contains("SUSPENDED") && diag.contains("antigravity.google"),
        "{diag}"
    );
}

#[test]
fn usage_view_shows_bars_resets_freshness_and_why_data_is_missing() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (100, 30));
    s.view = View::Usage;
    let out = text(&s, 100, 30);
    assert!(out.contains("carol"), "{out}");
    assert!(
        out.contains("Gemini Pro") && out.contains("72%") && out.contains("40%"),
        "{out}"
    );
    assert!(
        out.contains('\u{2588}') && out.contains('['),
        "bar drawn: {out}"
    );
    assert!(out.contains("resets in") && out.contains("fresh"), "{out}");
    assert!(
        out.contains("not enabled"),
        "disabled group is not shown as empty: {out}"
    );

    let alice = s
        .accounts()
        .iter()
        .find(|a| a.alias == "alice")
        .expect("alice")
        .id
        .clone();
    s.select_id(&alice);
    let out = text(&s, 100, 30);
    assert!(out.contains("Unavailable:"), "{out}");
    assert!(out.contains("No quota reading"), "{out}");
    assert!(
        !out.contains('%'),
        "no number for an account never read: {out}"
    );
}

#[test]
fn diagnostics_lists_agy_backend_checks_and_scrolls() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (100, 12));
    s.view = View::Diagnostics;
    let out = text(&s, 100, 40);
    for want in ["agy:", "credential backend:", "Checks", "journal"] {
        assert!(out.contains(want), "missing {want}: {out}");
    }
    let before = text(&s, 100, 8);
    press(&mut s, key(KeyCode::Down));
    press(&mut s, key(KeyCode::Down));
    assert_eq!(s.scroll, 2);
    assert_ne!(before, text(&s, 100, 8), "scrolled");
    press(&mut s, key(KeyCode::PageUp));
    assert_eq!(s.scroll, 0);
}

#[test]
fn the_view_code_never_builds_quota_numbers() {
    // Every number on screen must come from a reading; nothing under src/tui
    // may construct one.
    fn walk(p: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(p).expect("dir").flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|x| x == "rs") {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    walk(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/tui"),
        &mut files,
    );
    assert!(files.len() >= 10);
    for f in files {
        let src = std::fs::read_to_string(&f).expect("read");
        for bad in [
            "WindowView {",
            "remaining_percent:",
            "QuotaWindow {",
            "BurnRateView {",
        ] {
            assert!(
                !src.contains(bad),
                "{} constructs quota ({bad})",
                f.display()
            );
        }
    }
}

// ----------------------------------------------- worker, loop, real core

fn wait_msg(w: &agyva::tui::backend::Worker) -> Msg {
    w.results
        .recv_timeout(Duration::from_secs(20))
        .expect("worker answered")
}

#[test]
fn enter_confirm_yes_switches_through_the_same_core_path_as_use() {
    let (rig, fake, _d) = base();
    let backend: Arc<dyn Backend> = Arc::new(AppBackend::new(rig.factory(Some(fake.bin()), true)));
    let worker = spawn_worker(backend);
    let mut s = state(None, (100, 24));

    for j in startup(&mut s) {
        worker.jobs.send(j).expect("send");
    }
    for _ in 0..2 {
        let jobs = update(&mut s, Event::Msg(wait_msg(&worker)));
        assert!(jobs.is_empty());
    }
    assert!(!s.loading() && s.total() == 3, "{:?}", s.error);

    let live_before = rig.live_id().expect("logged in");
    assert_eq!(live_before, id_of(CAROL));
    let calls_before = fake.calls();

    press(&mut s, ch('2'));
    assert_eq!(
        s.selected().map(|a| a.alias.clone()).as_deref(),
        Some("bob")
    );

    // Cancel first: nothing happens.
    assert!(press(&mut s, key(KeyCode::Enter)).is_empty());
    assert!(matches!(s.overlay, Some(Overlay::Confirm { .. })));
    assert!(text(&s, 100, 24).contains("Switch to bob"));
    assert!(press(&mut s, ch('n')).is_empty());
    assert!(s.overlay.is_none());
    assert_eq!(rig.live_id().as_deref(), Some(live_before.as_str()));

    // Confirm.
    press(&mut s, key(KeyCode::Enter));
    let jobs = press(&mut s, ch('y'));
    assert_eq!(jobs, vec![Job::Switch { id: id_of(BOB) }]);
    assert!(s.switching);
    worker.jobs.send(jobs[0].clone()).expect("send");
    let jobs = update(&mut s, Event::Msg(wait_msg(&worker)));
    assert!(!s.switching && s.error.is_none(), "{:?}", s.error);
    assert!(
        s.message
            .as_deref()
            .is_some_and(|m| m.contains("Switched to bob") && m.contains("Restart agy"))
    );
    assert_eq!(
        jobs,
        vec![Job::Load(RefreshMode::Skip)],
        "reload from cache only; agy is not run"
    );

    assert_eq!(
        rig.live_id().as_deref(),
        Some(id_of(BOB).as_str()),
        "native login replaced"
    );
    assert_eq!(fake.calls(), calls_before, "switching never runs agy");
    let cur = rig.app().current().expect("current");
    assert_eq!(cur.account.expect("account").alias, "bob");

    worker.jobs.send(jobs[0].clone()).expect("send");
    update(&mut s, Event::Msg(wait_msg(&worker)));
    let bob = s.accounts().iter().find(|a| a.alias == "bob").expect("bob");
    assert!(bob.active, "the list shows the new active account");
}

#[test]
fn the_confirmation_warns_when_agy_appears_to_be_running() {
    let (_rig, _fake, mut d) = base();
    d.status.sessions.agy_pids = vec![4242];
    let mut s = state(Some(d), (100, 24));
    press(&mut s, ch('2'));
    press(&mut s, key(KeyCode::Enter));
    let out = text(&s, 100, 24);
    assert!(
        out.contains("agy process") && out.contains("Restart agy"),
        "{out}"
    );
}

#[test]
fn a_passphrase_vault_is_refused_instead_of_prompting_inside_the_screen() {
    // No OS keyring: the vault is created in passphrase mode.
    let rig = Rig::new();
    rig.keyring.set_failure(Some(StoreErrorKind::Unavailable));
    let fake = FakeAgy::install(&rig.tmp.path().join("fakeagy"));
    rig.add_all(&[ALICE, BOB]);
    let live = rig.live_id();

    // Without AGYVA_PASSPHRASE the vault would prompt on the terminal that
    // the interface owns, so the switch is refused with a pointer to the CLI.
    let backend = AppBackend::new(rig.factory(Some(fake.bin()), false));
    match backend.switch(&id_of(ALICE)) {
        Err(e) => assert!(e.contains("passphrase") && e.contains("agyva use"), "{e}"),
        Ok(_) => panic!("must not switch without a way to unlock the vault"),
    }
    assert_eq!(rig.live_id(), live, "nothing changed");

    // With the passphrase supplied by the environment it proceeds normally.
    let backend = AppBackend::new(rig.factory(Some(fake.bin()), true));
    let done = backend.switch(&id_of(ALICE)).expect("switch");
    assert_eq!(done.alias, "alice");
    assert_eq!(rig.live_id().as_deref(), Some(id_of(ALICE).as_str()));
}

#[test]
fn a_failed_switch_is_shown_and_leaves_the_state_usable() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (100, 24));
    press(&mut s, ch('2'));
    press(&mut s, key(KeyCode::Enter));
    press(&mut s, ch('y'));
    update(
        &mut s,
        Event::Msg(Msg::Switched(Err(
            "recovery is pending (hint: run `agyva recover`)".into(),
        ))),
    );
    assert!(!s.switching);
    let out = text(&s, 100, 24);
    assert!(
        out.contains("Switch failed") && out.contains("agyva recover"),
        "{out}"
    );
    assert!(!s.quit);
}

#[test]
fn the_loop_loads_draws_and_stops_on_interrupt() {
    let (_rig, _fake, d) = base();
    let fake = Fake::new(Ok(d));
    let backend: Arc<dyn Backend> = fake.clone();
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    let stopper = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(600));
        flag.store(true, Ordering::SeqCst);
    });
    let mut t = Terminal::new(TestBackend::new(80, 20)).expect("terminal");
    let started = Instant::now();
    agyva::tui::run_loop(&mut t, backend, &stop, false).expect("loop ends cleanly");
    stopper.join().expect("stopper");
    assert!(started.elapsed() < Duration::from_secs(10));
    assert_eq!(
        *fake.loads.lock().expect("lock"),
        vec![RefreshMode::Skip, RefreshMode::Normal]
    );
    let buf = t.backend().buffer().clone();
    let all: String = buf.content().iter().map(|c| c.symbol()).collect();
    assert!(all.contains("carol"), "{all}");
}

/// A backend whose every job panics, as a bug somewhere in the core might.
struct Crashing;

impl Backend for Crashing {
    fn load(&self, _mode: RefreshMode) -> Reply<Dashboard> {
        panic!("synthetic crash in a load job");
    }
    fn switch(&self, _id: &str) -> Reply<SwitchSummary> {
        panic!("synthetic crash in a switch job");
    }
    fn switch_endpoint(&self, _id: &str) -> Reply<EndpointSwitchSummary> {
        panic!("synthetic crash in an endpoint switch job");
    }
    fn test_endpoint(&self, _id: &str) -> Reply<EndpointProbeSummary> {
        panic!("synthetic crash in an endpoint test job");
    }
    fn fetch_endpoint_models(&self, _id: &str) -> Reply<EndpointProbeSummary> {
        panic!("synthetic crash in an endpoint fetch job");
    }
    fn apply_route(&self, _group: &str) -> Reply<SwitchSummary> {
        panic!("synthetic crash in a route apply job");
    }
    fn activate_profile(&self, _name: &str) -> Reply<ProfileSwitchSummary> {
        panic!("synthetic crash in a profile activate job");
    }
}

#[test]
fn a_crashing_job_becomes_an_error_and_the_interface_can_still_quit() {
    // The worker survives a panic and answers every job, so the busy flags
    // in the interface always reset.
    let w = spawn_worker(Arc::new(Crashing));
    w.jobs
        .send(Job::Switch { id: "x".into() })
        .expect("send switch");
    w.jobs
        .send(Job::Load(RefreshMode::Skip))
        .expect("send load");
    let first = wait_msg(&w);
    let second = wait_msg(&w);
    assert!(
        matches!(&first, Msg::Switched(Err(e)) if e.contains("crashed")),
        "{first:?}"
    );
    assert!(
        matches!(&second, Msg::Loaded { result, .. } if matches!(**result, Err(ref e) if e.contains("crashed"))),
        "{second:?}"
    );

    // And through the state machine: the switching flag resets, the failure
    // is shown, and `q` quits.
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (100, 24));
    press(&mut s, ch('2'));
    press(&mut s, key(KeyCode::Enter));
    press(&mut s, ch('y'));
    assert!(s.switching);
    update(&mut s, Event::Msg(first));
    assert!(!s.switching);
    assert!(text(&s, 100, 24).contains("Switch failed"));
    press(&mut s, ch('q'));
    assert!(s.quit);
}

#[test]
fn losing_the_worker_clears_the_busy_flags_so_quit_still_works() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (100, 24));
    press(&mut s, ch('2'));
    press(&mut s, key(KeyCode::Enter));
    press(&mut s, ch('y'));
    update(&mut s, Event::Interrupt);
    assert!(s.switching && s.quit_after_switch && !s.quit);
    agyva::tui::event::worker_lost(&mut s);
    assert!(
        !s.switching && s.quit,
        "a lost worker must not block quitting"
    );
}

#[test]
fn short_confirmation_pins_controls_and_escape_never_switches() {
    let (_rig, _fake, d) = base();
    for size in [(40, 6), (20, 5), (12, 4)] {
        let mut s = state(Some(d.clone()), size);
        press(&mut s, ch('2'));
        press(&mut s, key(KeyCode::Enter));
        let out = text(&s, size.0, size.1);
        assert!(out.contains('y') && out.contains("Esc"), "{size:?}: {out}");
        assert!(press(&mut s, key(KeyCode::Esc)).is_empty());
        assert!(s.overlay.is_none() && !s.switching && !s.quit);
    }
}

#[test]
fn wrapped_views_and_overlays_reach_the_last_line_and_scroll_back() {
    let (_rig, _fake, mut d) = base();
    let last = "END-OF-CONTENT";
    d.doctor.checks.last_mut().expect("check").hint =
        Some(format!("{} {last}", "wrapped diagnostic ".repeat(30)));
    for a in &mut d.usage.accounts {
        if let Some(group) = a.groups.last_mut() {
            group.group = format!("{} {last}", "wrapped group ".repeat(30));
        }
        a.health.warning = Some(format!("{} {last}", "wrapped details ".repeat(30)));
    }
    let mut s = state(Some(d), (28, 8));
    for view in [View::Usage, View::Diagnostics] {
        s.view = view;
        s.scroll = 0;
        for _ in 0..100 {
            press(&mut s, key(KeyCode::PageDown));
        }
        assert!(
            text(&s, 28, 8).contains(last),
            "{:?}: {}",
            view,
            text(&s, 28, 8)
        );
        let end = s.scroll;
        press(&mut s, key(KeyCode::PageUp));
        assert!(
            s.scroll < end,
            "scroll back immediately from the clamped end"
        );
    }
    for overlay in [Overlay::Help, Overlay::Details] {
        s.open_overlay(overlay.clone());
        press(&mut s, key(KeyCode::End));
        let out = text(&s, 28, 8);
        let expected = if overlay == Overlay::Help {
            "switch."
        } else {
            last
        };
        assert!(out.contains(expected) && out.contains("Esc close"), "{out}");
        let end = s.overlay_scroll;
        press(&mut s, key(KeyCode::Up));
        assert_eq!(s.overlay_scroll, end.saturating_sub(1));
        press(&mut s, key(KeyCode::Esc));
    }
}

#[test]
fn narrow_footer_keeps_help_and_quit_during_errors_and_filters() {
    let (_rig, _fake, d) = base();
    let mut s = state(Some(d), (20, 10));
    for view in View::ALL {
        s.view = view;
        s.error = Some("a long actionable refresh error".into());
        s.filter = "carol".into();
        let out = text(&s, 20, 10);
        let footer = out.lines().last().expect("footer");
        assert!(
            footer.contains("? help") && footer.contains("q quit"),
            "{out}"
        );
        if view == View::Diagnostics {
            assert!(!footer.contains("Enter"));
        }
        press(&mut s, ch('?'));
        assert_eq!(s.overlay, Some(Overlay::Help));
        press(&mut s, key(KeyCode::Esc));
    }
    s.filter_editing = true;
    assert!(text(&s, 20, 10).contains("^C quit"));
    press(&mut s, ch('?'));
    assert_eq!(s.overlay, Some(Overlay::Help));
    assert_eq!(s.filter, "carol");
}

#[test]
fn monochrome_keeps_selection_and_text_status_and_freshness_ages() {
    use agyva::cli::output::color_enabled;
    use agyva::config::ColorChoice;
    use agyva::core::quota::Freshness;
    use ratatui::style::{Color, Modifier};
    for choice in [ColorChoice::Auto, ColorChoice::Always, ColorChoice::Never] {
        assert!(!color_enabled(choice, true, true));
        assert!(!color_enabled(choice, false, false));
    }
    assert!(!color_enabled(ColorChoice::Never, false, true));
    assert!(color_enabled(ColorChoice::Auto, false, true));
    let (_rig, _fake, mut d) = base();
    d.quota_fresh_secs = 10;
    let mut s = state(Some(d), (100, 24));
    s.color = false;
    let observed = s
        .selected()
        .expect("selected")
        .observed_at
        .expect("observed");
    s.now = observed;
    assert_eq!(
        s.freshness(s.selected().expect("selected")),
        Freshness::Fresh
    );
    update(
        &mut s,
        Event::Tick(observed + chrono::Duration::seconds(11)),
    );
    assert_eq!(
        s.freshness(s.selected().expect("selected")),
        Freshness::Stale
    );
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("terminal");
    terminal.draw(|f| views::render(&s, f)).expect("draw");
    let cells = terminal.backend().buffer().content();
    assert!(
        cells
            .iter()
            .all(|cell| cell.fg == Color::Reset && cell.bg == Color::Reset)
    );
    assert!(
        cells
            .iter()
            .any(|cell| cell.modifier.contains(Modifier::REVERSED))
    );
    assert!(text(&s, 100, 24).contains("stale"));
    s.view = View::Usage;
    let out = text(&s, 100, 24);
    assert!(out.contains("stale") && !out.contains("just now"), "{out}");
}

// ------------------------------------------------ 5-tab dashboard features

fn sample_endpoints(now: chrono::DateTime<Utc>) -> Vec<EndpointView> {
    vec![
        EndpointView {
            id: "0123456789".into(),
            alias: "openai-main".into(),
            base_url: "https://api.openai.com/v1".into(),
            protocol: EndpointProtocol::OpenAi,
            detected_protocol: Some(EndpointProtocol::OpenAi),
            default_model: Some("gpt-4o".into()),
            active: true,
            enabled: true,
            model_count: 2,
            models: vec![
                FetchedModel {
                    id: "gpt-4o".into(),
                    display_name: Some("GPT-4o".into()),
                    context_window: Some(128_000),
                    max_output_tokens: None,
                    owned_by: Some("openai".into()),
                    capabilities: vec![],
                },
                FetchedModel {
                    id: "gpt-4o-mini".into(),
                    display_name: None,
                    context_window: None,
                    max_output_tokens: None,
                    owned_by: None,
                    capabilities: vec![],
                },
            ],
            fetched_at: Some(now - chrono::Duration::minutes(2)),
            latency_ms: Some(42),
            rate_limits: Some(RateLimitInfo {
                requests_remaining: Some(95),
                requests_limit: Some(100),
                ..RateLimitInfo::default()
            }),
            last_error: None,
        },
        EndpointView {
            id: "abcdef0123".into(),
            alias: "anthropic-alt".into(),
            base_url: "https://api.anthropic.com".into(),
            protocol: EndpointProtocol::Anthropic,
            detected_protocol: None,
            default_model: Some("claude-sonnet-4".into()),
            active: false,
            enabled: true,
            model_count: 0,
            models: vec![],
            fetched_at: None,
            latency_ms: None,
            rate_limits: None,
            last_error: Some("connection timed out".into()),
        },
        EndpointView {
            id: "fedcba9876".into(),
            alias: "local-ollama".into(),
            base_url: "http://localhost:11434".into(),
            protocol: EndpointProtocol::Ollama,
            detected_protocol: Some(EndpointProtocol::Ollama),
            default_model: Some("qwen2.5-coder".into()),
            active: false,
            enabled: true,
            model_count: 1,
            models: vec![],
            fetched_at: Some(now - chrono::Duration::minutes(5)),
            latency_ms: Some(8),
            rate_limits: None,
            last_error: None,
        },
    ]
}

fn enrich_routes(d: &mut Dashboard, now: chrono::DateTime<Utc>) {
    d.active_profile = Some(ResolvedProfile {
        name: Some("dev".into()),
        source: ProfileSource::Local,
        path: Some(std::path::PathBuf::from("/workspace/.agyva.toml")),
        spec: ProfileSpec {
            name: Some("dev".into()),
            description: Some("Local project profile".into()),
            account: Some("carol".into()),
            endpoint: Some("openai-main".into()),
            group: Some("work".into()),
            model: Some("gpt-4o".into()),
            ..ProfileSpec::default()
        },
    });
    d.profiles = vec![
        ProfileSummary {
            name: "dev".into(),
            description: Some("Local project profile".into()),
            source: ProfileSource::Local,
            is_default: false,
            path: Some(std::path::PathBuf::from("/workspace/.agyva.toml")),
            account: Some("carol".into()),
            endpoint: Some("openai-main".into()),
            group: Some("work".into()),
            model: Some("gpt-4o".into()),
            warn_percent: Some(25.0),
            critical_percent: Some(10.0),
            monitor_interval: Some("60s".into()),
            env: std::collections::BTreeMap::new(),
        },
        ProfileSummary {
            name: "prod".into(),
            description: Some("Production defaults".into()),
            source: ProfileSource::Global,
            is_default: true,
            path: None,
            account: Some("bob".into()),
            endpoint: None,
            group: Some("work".into()),
            model: None,
            warn_percent: None,
            critical_percent: None,
            monitor_interval: None,
            env: std::collections::BTreeMap::new(),
        },
    ];
    d.routing_groups = vec![RouteGroupSnapshot {
        name: "work".into(),
        ranking: Ranking::Smart,
        auto_switch: true,
        plan: RoutePlan {
            schema_version: 1,
            generated_at: now,
            recommendation: Some(id_of(BOB)),
            accounts: vec![
                RouteRow {
                    id: id_of(BOB),
                    alias: "bob".into(),
                    active: false,
                    freshness: agyva::core::quota::Freshness::Fresh,
                    eligible: true,
                    remaining_percent: Some(85.0),
                    rate_remaining_percent: Some(90.0),
                    renewals: [Some(now + chrono::Duration::hours(48)), None],
                    why: "fresh cache; 85% remains".into(),
                    command: "agyva use bob".into(),
                },
                RouteRow {
                    id: id_of(ALICE),
                    alias: "alice".into(),
                    active: false,
                    freshness: agyva::core::quota::Freshness::Missing,
                    eligible: false,
                    remaining_percent: None,
                    rate_remaining_percent: None,
                    renewals: [None, None],
                    why: "quota unknown".into(),
                    command: "agyva use alice".into(),
                },
            ],
        },
    }];
}

#[test]
fn sort_order_cycles_with_s_and_keeps_selected_account() {
    let (_rig, _fake, mut d) = base();
    // Give bob a higher quota than carol so Quota sort reorders visibly.
    let mut bob_group = d.usage.accounts[2].groups[0].clone();
    if let Some(w) = bob_group.five_hour.as_mut() {
        w.remaining_percent = 95.0;
    }
    if let Some(w) = bob_group.weekly.as_mut() {
        w.remaining_percent = 90.0;
    }
    d.usage.accounts[1].groups = vec![bob_group];

    let mut s = state(Some(d), (100, 24));
    assert_eq!(s.sort_order, SortOrder::Default);
    let bob_id = id_of(BOB);
    s.select_id(&bob_id);

    press(&mut s, ch('s'));
    assert_eq!(s.sort_order, SortOrder::Quota);
    assert_eq!(s.selected_id.as_deref(), Some(bob_id.as_str()));
    assert_eq!(s.visible()[0].alias, "bob");

    press(&mut s, ch('s'));
    assert_eq!(s.sort_order, SortOrder::Alias);
    assert_eq!(s.visible()[0].alias, "alice");
    assert_eq!(s.selected_id.as_deref(), Some(bob_id.as_str()));

    press(&mut s, ch('s'));
    assert_eq!(s.sort_order, SortOrder::Freshness);

    press(&mut s, ch('s'));
    assert_eq!(s.sort_order, SortOrder::Default);

    // Pressing `s` on non-Accounts view does not change sort order.
    s.view = View::Endpoints;
    press(&mut s, ch('s'));
    assert_eq!(s.sort_order, SortOrder::Default);
}

#[test]
fn accounts_and_usage_display_summary_strip_smooth_bars_and_burn_rate_eta() {
    let (_rig, _fake, mut d) = base();
    let now = d.generated_at;
    d.endpoints = sample_endpoints(now);
    for g in &mut d.usage.accounts[2].groups {
        if let Some(w) = g.weekly.as_mut() {
            w.burn_rate = Some(BurnRateView {
                percent_per_hour: Some(12.5),
                eta_exhaustion_secs: Some(42 * 60),
                exhausts_before_reset: Some(true),
                samples: 4,
            });
        }
    }
    let mut s = state(Some(d), (120, 28));
    s.now = now;
    let out = text(&s, 120, 28);
    assert!(out.contains("Active Target"), "{out}");
    assert!(out.contains("Lowest & Burn"), "{out}");
    assert!(out.contains("Endpoints"), "{out}");
    assert!(out.contains("Next Reset"), "{out}");
    assert!(out.contains("~42m"), "ETA hint shown: {out}");
    assert!(
        out.contains('\u{2593}') || out.contains('\u{2588}'),
        "smooth block bar chars: {out}"
    );

    // Usage tab shows framed rounded card, burn rate %/h, ETA, and warning.
    s.view = View::Usage;
    let usage_out = text(&s, 100, 30);
    assert!(usage_out.contains("╭─"), "rounded card: {usage_out}");
    assert!(usage_out.contains("12.5%/h"), "{usage_out}");
    assert!(usage_out.contains("~42m"), "{usage_out}");
    assert!(usage_out.contains("exhausts before reset"), "{usage_out}");
}

#[test]
fn endpoints_tab_renders_badges_limits_errors_filters_and_actions() {
    let (_rig, _fake, mut d) = base();
    let now = d.generated_at;
    d.endpoints = sample_endpoints(now);
    d.active_endpoint = Some("openai-main".into());

    let mut s = state(Some(d), (120, 28));
    s.now = now;
    press(&mut s, key(KeyCode::F(3)));
    assert_eq!(s.view, View::Endpoints);

    let out = text(&s, 120, 28);
    assert!(out.contains("[OPENAI]"), "{out}");
    assert!(out.contains("[ANTHROPIC]"), "{out}");
    assert!(out.contains("[OLLAMA]"), "{out}");
    assert!(out.contains("42ms"), "{out}");
    assert!(out.contains("req 95/100"), "{out}");
    assert!(out.contains("err: connection"), "{out}");
    assert!(out.contains("gpt-4o"), "{out}");

    // Number key selects endpoint #2 (anthropic-alt), `d` shows details without secrets.
    press(&mut s, ch('2'));
    assert_eq!(
        s.selected_endpoint().map(|e| e.alias.as_str()),
        Some("anthropic-alt")
    );
    assert!(
        text(&s, 120, 28).contains("Error: connection timed out"),
        "preview card shows full error"
    );
    press(&mut s, ch('d'));
    assert_eq!(s.overlay, Some(Overlay::Details));
    let det = text(&s, 100, 24);
    assert!(det.contains("https://api.anthropic.com"), "{det}");
    assert!(!det.contains("sk-"), "{det}");
    press(&mut s, key(KeyCode::Esc));

    // `t` tests connection, `f` fetches models.
    let jobs = press(&mut s, ch('t'));
    assert_eq!(
        jobs,
        vec![Job::TestEndpoint {
            id: "abcdef0123".into()
        }]
    );
    assert!(s.endpoint_busy);
    update(
        &mut s,
        Event::Msg(Msg::EndpointTested(Ok(EndpointProbeSummary {
            id: "abcdef0123".into(),
            alias: "anthropic-alt".into(),
            probe: Box::new(ProbeResult {
                reachable: true,
                detected_protocol: Some(EndpointProtocol::Anthropic),
                latency_ms: Some(33),
                model_count: 4,
                models: vec![],
                rate_limits: None,
                error: None,
            }),
        }))),
    );
    assert!(!s.endpoint_busy);
    assert!(
        s.message
            .as_deref()
            .is_some_and(|m| m.contains("33ms") && m.contains("4 model"))
    );

    let jobs = press(&mut s, ch('f'));
    assert_eq!(
        jobs,
        vec![Job::FetchEndpointModels {
            id: "abcdef0123".into()
        }]
    );
    let reload = update(
        &mut s,
        Event::Msg(Msg::EndpointModelsFetched(Ok(EndpointProbeSummary {
            id: "abcdef0123".into(),
            alias: "anthropic-alt".into(),
            probe: Box::new(ProbeResult {
                reachable: true,
                detected_protocol: Some(EndpointProtocol::Anthropic),
                latency_ms: Some(29),
                model_count: 5,
                models: vec![],
                rate_limits: None,
                error: None,
            }),
        }))),
    );
    assert_eq!(reload, vec![Job::Load(RefreshMode::Skip)]);

    // `Enter` on inactive endpoint opens ConfirmEndpoint, `y` dispatches SwitchEndpoint.
    press(&mut s, key(KeyCode::Enter));
    assert!(matches!(s.overlay, Some(Overlay::ConfirmEndpoint { .. })));
    let confirm_out = text(&s, 100, 24);
    assert!(
        confirm_out.contains("Activate endpoint anthropic-alt"),
        "{confirm_out}"
    );
    let jobs = press(&mut s, ch('y'));
    assert_eq!(
        jobs,
        vec![Job::SwitchEndpoint {
            id: "abcdef0123".into()
        }]
    );
    assert!(s.switching);
    update(
        &mut s,
        Event::Msg(Msg::EndpointSwitched(Ok(EndpointSwitchSummary {
            alias: "anthropic-alt".into(),
            default_model: Some("claude-sonnet-4".into()),
        }))),
    );
    assert!(!s.switching);

    // Filter `/` works on Endpoints.
    press(&mut s, ch('/'));
    type_str(&mut s, "ollama");
    assert_eq!(s.visible_endpoints().len(), 1);
    assert_eq!(
        s.selected_endpoint().map(|e| e.alias.as_str()),
        Some("local-ollama")
    );
    press(&mut s, key(KeyCode::Esc));
    assert_eq!(s.visible_endpoints().len(), 3);
}

#[test]
fn routes_tab_renders_profiles_and_ranked_candidates_and_activates() {
    let (_rig, _fake, mut d) = base();
    let now = d.generated_at;
    enrich_routes(&mut d, now);

    let mut s = state(Some(d), (120, 28));
    s.now = now;
    press(&mut s, key(KeyCode::F(4)));
    assert_eq!(s.view, View::Routes);

    let out = text(&s, 120, 28);
    assert!(out.contains("Active Project Profile"), "{out}");
    assert!(out.contains("dev"), "{out}");
    assert!(out.contains("prod"), "{out}");
    assert!(out.contains("work"), "{out}");

    // Confirm profile activation on item 1 (`dev`).
    press(&mut s, ch('1'));
    press(&mut s, key(KeyCode::Enter));
    assert!(matches!(s.overlay, Some(Overlay::ConfirmProfile { .. })));
    let jobs = press(&mut s, ch('y'));
    assert_eq!(jobs, vec![Job::ActivateProfile { name: "dev".into() }]);
    update(
        &mut s,
        Event::Msg(Msg::ProfileActivated(Ok(ProfileSwitchSummary {
            name: "dev".into(),
            switched_account: Some("carol".into()),
            active_endpoint: Some("openai-main".into()),
            warnings: vec![],
        }))),
    );
    assert!(!s.switching);
    assert!(
        s.message
            .as_deref()
            .is_some_and(|m| m.contains("Activated profile dev"))
    );

    // Select route group (`3`) -> shows ranked candidates, share %, exclusion reason.
    press(&mut s, ch('3'));
    let group_out = text(&s, 120, 28);
    assert!(group_out.contains("85%"), "{group_out}");
    assert!(group_out.contains("quota unknown"), "{group_out}");
    assert!(group_out.contains("[OK]"), "{group_out}");
    assert!(group_out.contains("[NO]"), "{group_out}");

    // `d` opens route group details.
    press(&mut s, ch('d'));
    assert_eq!(s.overlay, Some(Overlay::Details));
    assert!(text(&s, 100, 24).contains("Route Group: work"));
    press(&mut s, key(KeyCode::Esc));

    // `Enter` on route group opens ConfirmRoute, `y` dispatches ApplyRoute.
    press(&mut s, key(KeyCode::Enter));
    assert!(matches!(s.overlay, Some(Overlay::ConfirmRoute { .. })));
    let jobs = press(&mut s, ch('y'));
    assert_eq!(
        jobs,
        vec![Job::ApplyRoute {
            group: "work".into()
        }]
    );
    update(
        &mut s,
        Event::Msg(Msg::RouteApplied(Ok(SwitchSummary {
            alias: "bob".into(),
            already_active: false,
            warnings: vec![],
        }))),
    );
    assert!(!s.switching);

    // Filter `/` works on Routes.
    press(&mut s, ch('/'));
    type_str(&mut s, "prod");
    assert_eq!(s.visible_route_items().len(), 1);
    press(&mut s, key(KeyCode::Esc));
    assert_eq!(s.visible_route_items().len(), 3);
}

#[test]
fn diagnostics_renders_all_seven_cards_and_tag_badges() {
    let (_rig, _fake, mut d) = base();
    let now = d.generated_at;
    d.endpoints = sample_endpoints(now);
    enrich_routes(&mut d, now);
    d.doctor.checks.push(Check {
        name: "sample-info",
        level: Level::Info,
        detail: "info detail".into(),
        hint: None,
    });
    d.doctor.checks.push(Check {
        name: "sample-warn",
        level: Level::Warn,
        detail: "warn detail".into(),
        hint: None,
    });
    d.doctor.checks.push(Check {
        name: "sample-fail",
        level: Level::Fail,
        detail: "fail detail".into(),
        hint: None,
    });

    let mut s = state(Some(d), (110, 44));
    s.view = View::Diagnostics;
    let out = text(&s, 110, 44);
    for want in [
        "agyva:",
        "agy:",
        "credential backend:",
        "vault:",
        "endpoints:",
        "profiles:",
        "checkpoints:",
        "[ OK ]",
        "[INFO]",
        "[WARN]",
        "[FAIL]",
    ] {
        assert!(out.contains(want), "missing {want} in:\n{out}");
    }
}

#[test]
fn worker_dispatches_all_new_backend_methods() {
    let (_rig, _fake, d) = base();
    let fake = Fake::new(Ok(d));
    let worker = spawn_worker(fake.clone());

    worker
        .jobs
        .send(Job::SwitchEndpoint { id: "ep1".into() })
        .expect("send");
    assert!(matches!(
        wait_msg(&worker),
        Msg::EndpointSwitched(Ok(ref s)) if s.alias == "ep1"
    ));

    worker
        .jobs
        .send(Job::TestEndpoint { id: "ep1".into() })
        .expect("send");
    assert!(matches!(
        wait_msg(&worker),
        Msg::EndpointTested(Ok(ref s)) if s.probe.reachable
    ));

    worker
        .jobs
        .send(Job::FetchEndpointModels { id: "ep1".into() })
        .expect("send");
    assert!(matches!(
        wait_msg(&worker),
        Msg::EndpointModelsFetched(Ok(ref s)) if s.probe.model_count == 3
    ));

    worker
        .jobs
        .send(Job::ApplyRoute {
            group: "work".into(),
        })
        .expect("send");
    assert!(matches!(
        wait_msg(&worker),
        Msg::RouteApplied(Ok(ref s)) if s.alias == "bob"
    ));

    worker
        .jobs
        .send(Job::ActivateProfile { name: "dev".into() })
        .expect("send");
    assert!(matches!(
        wait_msg(&worker),
        Msg::ProfileActivated(Ok(ref s)) if s.name == "dev"
    ));

    assert_eq!(*fake.endpoint_switches.lock().expect("lock"), vec!["ep1"]);
    assert_eq!(*fake.endpoint_tests.lock().expect("lock"), vec!["ep1"]);
    assert_eq!(*fake.endpoint_fetches.lock().expect("lock"), vec!["ep1"]);
    assert_eq!(*fake.route_applies.lock().expect("lock"), vec!["work"]);
    assert_eq!(*fake.profile_activates.lock().expect("lock"), vec!["dev"]);
}
