//! Human-readable rendering. Pure functions returning strings.

use unicode_width::UnicodeWidthStr;

use crate::core::doctor::{DoctorReport, Level};
use crate::core::status::StatusView;
use crate::core::view::{AccountView, CurrentView, ListView};

#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub color: bool,
}

impl Style {
    pub fn bold(self, s: &str) -> String {
        if self.color {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }
    pub fn dim(self, s: &str) -> String {
        if self.color {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }
}

fn pad(s: &str, width: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    format!("{s}{}", " ".repeat(width.saturating_sub(w)))
}

pub fn render_list(view: &ListView, style: Style) -> String {
    if view.accounts.is_empty() {
        return "No saved accounts. Log in with `agy`, then run `agyva add`.\n".to_owned();
    }
    let rows: Vec<[String; 5]> = view
        .accounts
        .iter()
        .map(|a| {
            [
                format!("{}{}", if a.active { "*" } else { " " }, a.number),
                a.alias.clone(),
                a.email.clone().unwrap_or_else(|| "-".into()),
                a.id.clone(),
                a.last_used_at.map_or_else(
                    || "never".into(),
                    |t| t.format("%Y-%m-%d %H:%M").to_string(),
                ),
            ]
        })
        .collect();
    let head = ["#", "ALIAS", "EMAIL", "ID", "LAST USED"];
    let widths: Vec<usize> = (0..5)
        .map(|c| {
            rows.iter()
                .map(|r| UnicodeWidthStr::width(r[c].as_str()))
                .chain([head[c].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[String; 5]| -> String {
        cells
            .iter()
            .zip(&widths)
            .map(|(c, w)| pad(c, *w))
            .collect::<Vec<_>>()
            .join("  ")
            .trim_end()
            .to_owned()
    };
    let mut out = style.dim(&line(&head.map(str::to_owned))) + "\n";
    for (row, acct) in rows.iter().zip(&view.accounts) {
        let l = line(row);
        out.push_str(&if acct.active { style.bold(&l) } else { l });
        out.push('\n');
    }
    out
}

pub fn describe(a: &AccountView) -> String {
    format!(
        "{} <{}> [{}]",
        a.alias,
        a.email.as_deref().unwrap_or("no email"),
        a.id
    )
}

pub fn render_current(v: &CurrentView) -> String {
    match (&v.account, &v.id) {
        (Some(a), _) => format!("{}\n", describe(a)),
        (None, Some(id)) => format!(
            "agy is logged in as {} [{id}], which is not saved in agyva.\nRun `agyva add` to save it.\n",
            v.email.as_deref().unwrap_or("an unknown email")
        ),
        (None, None) => "agy is not logged in.\n".to_owned(),
    }
}

pub fn render_status(s: &StatusView) -> String {
    let mut out = format!(
        "agyva {}\nagy:       {}\nquota:     {}\naccounts:  {}\nactive:    {}\nbackend:   {}\nvault:     {}\nconfig:    {}\nstate:     {}\n",
        s.agyva_version,
        s.agy,
        s.quota_provider,
        s.accounts,
        s.active.as_deref().unwrap_or("none"),
        s.backend,
        s.vault,
        s.config_dir,
        s.state_dir,
    );
    if let Some(w) = s.sessions.warning() {
        out.push_str(&format!("sessions:  {w}\n"));
    }
    if let Some(p) = &s.recovery_pending {
        out.push_str(&format!(
            "RECOVERY REQUIRED: switch to {} was interrupted ({}).\nRun `agyva recover`.\n",
            p.target, p.phase
        ));
    }
    out
}

pub fn render_doctor(r: &DoctorReport) -> String {
    let mut out = String::new();
    for c in &r.checks {
        let tag = match c.level {
            Level::Ok => " ok ",
            Level::Info => "info",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
        };
        out.push_str(&format!("[{tag}] {}: {}\n", c.name, c.detail));
        if let Some(h) = &c.hint {
            out.push_str(&format!("       hint: {h}\n"));
        }
    }
    out
}
