//! The interactive view without a real terminal: the pure state machine, the
//! renderers on ratatui's `TestBackend`, and the worker against the real core
//! (in-memory keyring and credential fakes, a synthetic fake `agy`).

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agyva::core::dashboard::Dashboard;
use agyva::core::health::{APPEAL_NOTICE, AccountHealth};
use agyva::core::usage::RefreshMode;
use agyva::storage::keyring::StoreErrorKind;
use agyva::tui::app::{AppState, Overlay, View};
use agyva::tui::backend::{AppBackend, Backend, Job, Msg, Reply, SwitchSummary, spawn_worker};
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
}

impl Fake {
    fn new(d: Reply<Dashboard>) -> Arc<Self> {
        Arc::new(Self {
            data: Mutex::new(d),
            loads: Mutex::default(),
            switches: Mutex::default(),
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
}

// ------------------------------------------------------------------ layout

#[test]
fn column_plan_drops_columns_progressively() {
    let wide = column_plan(120);
    assert!(wide.five && wide.status && wide.weekly && wide.plan);
    let w40 = column_plan(40);
    assert!(w40.five && w40.status && !w40.weekly && !w40.plan);
    assert!(w40.alias >= 6);
    let w30 = column_plan(30);
    assert!(w30.five && !w30.status);
    let w20 = column_plan(20);
    assert!(!w20.five && !w20.status && !w20.weekly && !w20.plan);
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
    ];
    for view in View::ALL {
        for overlay in &overlays {
            s.view = view;
            s.overlay = overlay.clone();
            for (w, h) in [
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
        .find(|l| l.contains("carol"))
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
    assert_eq!(s.view, View::Diagnostics);
    press(&mut s, key(KeyCode::Tab));
    assert_eq!(s.view, View::Accounts);
    press(&mut s, key(KeyCode::BackTab));
    assert_eq!(s.view, View::Diagnostics);
    press(&mut s, key(KeyCode::F(2)));
    assert_eq!(s.view, View::Usage);
    press(&mut s, key(KeyCode::F(1)));
    assert_eq!(s.view, View::Accounts);
    press(&mut s, key(KeyCode::F(3)));
    assert_eq!(s.view, View::Diagnostics);
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
    assert!(files.len() >= 8);
    for f in files {
        let src = std::fs::read_to_string(&f).expect("read");
        for bad in ["WindowView {", "remaining_percent:", "QuotaWindow {"] {
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
    assert!(text(&s, 100, 24).contains("Switch agy to bob"));
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
