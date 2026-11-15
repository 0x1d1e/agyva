//! Documentation capture using the production renderer and a cache-only dashboard.
//! Run through scripts/capture-tui.sh, which supplies an isolated replay registry.
use agyva::core::app::App;
use agyva::core::usage::RefreshMode;
use agyva::paths::Env;
use agyva::tui::app::{AppState, View};
use agyva::tui::event::{Event, update};
use agyva::tui::views;
use chrono::Utc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Modifier;
use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;

fn key(state: &mut AppState, code: KeyCode) {
    let jobs = update(state, Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    assert!(jobs.is_empty(), "capture must never refresh or switch");
}

fn capture(state: &AppState, name: &str) -> Result<Value, Box<dyn std::error::Error>> {
    let (width, height) = state.size;
    let mut terminal = Terminal::new(TestBackend::new(width, height))?;
    terminal.draw(|frame| views::render(state, frame))?;
    let buffer = terminal.backend().buffer();
    let mut rows = Vec::new();
    for y in 0..height {
        let mut row = Vec::new();
        let mut x = 0;
        while x < width {
            let cell = &buffer[(x, y)];
            let columns = u16::try_from(cell.symbol().width().max(1))?;
            row.push(json!({"x": x, "text": cell.symbol(), "columns": columns,
                "reverse": cell.modifier.contains(Modifier::REVERSED),
                "bold": cell.modifier.contains(Modifier::BOLD)}));
            x += columns;
        }
        rows.push(row);
    }
    Ok(json!({"name": name, "width": width, "height": height, "rows": rows}))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let env = Env::from_process();
    if env.get("AGYVA_CAPTURE") != Some("1")
        || env.get("AGYVA_BACKEND") != Some("file")
        || [
            "AGYVA_CONFIG_DIR",
            "AGYVA_STATE_DIR",
            "AGYVA_CACHE_DIR",
            "AGYVA_AGY_HOME",
            "AGYVA_AGY_BIN",
        ]
        .iter()
        .any(|name| env.get(name).is_none())
    {
        return Err("use scripts/capture-tui.sh; every capture directory must be isolated".into());
    }
    let app = App::from_env(env, false)?;
    let dashboard = app.dashboard(RefreshMode::Skip)?;
    let mut state = AppState::new((96, 26), Utc::now());
    state.color = false;
    state.apply_dashboard(dashboard);
    let mut frames = vec![capture(&state, "Accounts · 96×26")?];
    key(&mut state, KeyCode::Tab);
    frames.push(capture(&state, "Usage · personal · 96×26")?);
    key(&mut state, KeyCode::Down);
    frames.push(capture(&state, "Usage · work · 96×26")?);
    key(&mut state, KeyCode::Char('d'));
    frames.push(capture(&state, "Details · work · 96×26")?);
    key(&mut state, KeyCode::Esc);
    key(&mut state, KeyCode::Char('?'));
    frames.push(capture(&state, "Help · 96×26")?);
    key(&mut state, KeyCode::Esc);
    state.view = View::Accounts;
    update(&mut state, Event::Resize(40, 12));
    frames.push(capture(&state, "Accounts · narrow 40×12")?);
    key(&mut state, KeyCode::Enter);
    update(&mut state, Event::Resize(20, 5));
    frames.push(capture(&state, "Confirm · short 20×5 · Esc cancels")?);
    key(&mut state, KeyCode::Esc);
    assert!(!state.switching);
    key(&mut state, KeyCode::Char('?'));
    key(&mut state, KeyCode::End);
    frames.push(capture(&state, "Help · last page · 20×5")?);
    key(&mut state, KeyCode::Esc);
    println!("{}", serde_json::to_string(&frames)?);
    Ok(())
}
