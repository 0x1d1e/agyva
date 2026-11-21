//! Human-readable rendering. Pure functions returning strings.

use serde::Serialize;
use unicode_width::UnicodeWidthStr;

use super::StatuslineFormat;
use crate::core::app_integrations::StatuslineData;
use crate::core::checkpoints::CheckpointInfo;
use crate::core::doctor::{DoctorFixReport, DoctorReport, Level};
use crate::core::error::{Error, Result};
use crate::core::status::StatusView;
use crate::core::view::{AccountView, CurrentView, ListView};

#[derive(Debug, Clone, Copy)]
pub struct Style {
    pub color: bool,
}

pub fn color_enabled(choice: crate::config::ColorChoice, no_color: bool, tty: bool) -> bool {
    tty && !no_color && choice != crate::config::ColorChoice::Never
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

pub fn render_doctor_fix(r: &DoctorFixReport) -> String {
    let mut out = String::new();
    if r.actions.is_empty() {
        out.push_str("No safe fixes needed.\n");
    } else {
        for act in &r.actions {
            out.push_str(&format!("fix: {act}\n"));
        }
    }
    out.push_str(&render_doctor(&r.report));
    out
}

pub fn render_checkpoints(items: &[CheckpointInfo], style: Style) -> String {
    if items.is_empty() {
        return "No saved checkpoints.\n".to_owned();
    }
    let rows: Vec<[String; 5]> = items
        .iter()
        .map(|c| {
            [
                c.name.clone(),
                if c.pinned { "pinned" } else { "auto" }.to_owned(),
                if c.valid { "valid" } else { "invalid" }.to_owned(),
                c.created_at.map_or_else(
                    || "-".into(),
                    |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
                ),
                format!("{} B", c.size_bytes),
            ]
        })
        .collect();
    let head = ["NAME", "KIND", "STATE", "CREATED", "SIZE"];
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
    for (row, item) in rows.iter().zip(items) {
        let l = line(row);
        out.push_str(&if item.pinned { style.bold(&l) } else { l });
        out.push('\n');
    }
    out
}

pub fn render_statusline(data: &StatuslineData, fmt: StatuslineFormat) -> Result<String> {
    match fmt {
        StatuslineFormat::Compact => data.render("compact"),
        StatuslineFormat::Full | StatuslineFormat::Plain => data.render("full"),
        StatuslineFormat::Json => data.render("json"),
        StatuslineFormat::Starship => {
            let compact = data.render("compact")?;
            Ok(format!("agyva:{compact}"))
        }
        StatuslineFormat::Tmux => {
            let compact = data.render("compact")?;
            let color = if data.suspended
                || data.recovery_pending
                || data.threshold_alert.as_deref() == Some("critical")
            {
                "red"
            } else if data.threshold_alert.as_deref() == Some("warn") {
                "yellow"
            } else {
                "green"
            };
            Ok(format!("#[fg={color}]{compact}#[default]"))
        }
        StatuslineFormat::Waybar => {
            #[derive(Serialize)]
            struct WaybarPayload {
                text: String,
                tooltip: String,
                class: &'static str,
            }
            let text = data.render("compact")?;
            let tooltip = data.render("full")?;
            let class = if data.suspended {
                "suspended"
            } else if data.recovery_pending || data.threshold_alert.as_deref() == Some("critical") {
                "critical"
            } else if data.threshold_alert.as_deref() == Some("warn") {
                "warning"
            } else {
                "ok"
            };
            serde_json::to_string(&WaybarPayload {
                text,
                tooltip,
                class,
            })
            .map_err(|e| Error::format(format!("encoding waybar statusline json: {e}")))
        }
    }
}
