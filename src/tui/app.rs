//! Interface state. Plain data plus small pure helpers; all transitions live
//! in `event.rs`. Nothing here touches the terminal or the disk.

use chrono::{DateTime, Utc};

use crate::core::dashboard::Dashboard;
use crate::core::health::AccountHealth;
use crate::core::quota::{Freshness, FreshnessPolicy};
use crate::core::usage::AccountUsage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Accounts,
    Usage,
    Diagnostics,
}

impl View {
    pub const ALL: [View; 3] = [View::Accounts, View::Usage, View::Diagnostics];

    pub fn title(self) -> &'static str {
        match self {
            View::Accounts => "Accounts",
            View::Usage => "Usage",
            View::Diagnostics => "Diagnostics",
        }
    }

    pub fn step(self, delta: isize) -> View {
        let i = Self::ALL.iter().position(|v| *v == self).unwrap_or(0);
        let n = Self::ALL.len() as isize;
        Self::ALL[(i as isize + delta).rem_euclid(n) as usize]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Details,
    Confirm { id: String, alias: String },
}

#[derive(Debug)]
pub struct AppState {
    pub view: View,
    pub data: Option<Dashboard>,
    pub selected_id: Option<String>,
    pub filter: String,
    pub filter_editing: bool,
    pub overlay: Option<Overlay>,
    /// Loads in flight (cache read or agy refresh).
    pub pending_loads: usize,
    pub switching: bool,
    pub quit_after_switch: bool,
    pub error: Option<String>,
    pub message: Option<String>,
    pub scroll: usize,
    pub overlay_scroll: usize,
    pub color: bool,
    pub size: (u16, u16),
    pub now: DateTime<Utc>,
    pub dirty: bool,
    pub quit: bool,
}

impl AppState {
    pub fn new(size: (u16, u16), now: DateTime<Utc>) -> Self {
        Self {
            view: View::Accounts,
            data: None,
            selected_id: None,
            filter: String::new(),
            filter_editing: false,
            overlay: None,
            pending_loads: 0,
            switching: false,
            quit_after_switch: false,
            error: None,
            message: None,
            scroll: 0,
            size,
            now,
            overlay_scroll: 0,
            color: true,
            dirty: true,
            quit: false,
        }
    }

    pub fn accounts(&self) -> &[AccountUsage] {
        self.data.as_ref().map_or(&[], |d| &d.usage.accounts)
    }

    pub fn total(&self) -> usize {
        self.accounts().len()
    }

    /// Accounts that match the live filter (alias, email or id prefix).
    pub fn visible(&self) -> Vec<&AccountUsage> {
        let q = self.filter.trim().to_lowercase();
        self.accounts()
            .iter()
            .filter(|a| {
                q.is_empty()
                    || a.alias.to_lowercase().contains(&q)
                    || a.email
                        .as_deref()
                        .is_some_and(|e| e.to_lowercase().contains(&q))
                    || a.id.to_lowercase().starts_with(&q)
            })
            .collect()
    }

    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected_id.as_deref()?;
        self.visible().iter().position(|a| a.id == id)
    }

    pub fn selected(&self) -> Option<&AccountUsage> {
        let id = self.selected_id.as_deref()?;
        self.accounts().iter().find(|a| a.id == id)
    }

    pub fn loading(&self) -> bool {
        self.pending_loads > 0
    }

    pub fn suspended(&self) -> Vec<&AccountUsage> {
        self.accounts()
            .iter()
            .filter(|a| a.health.state == AccountHealth::TermsSuspended)
            .collect()
    }

    pub fn sessions_warning(&self) -> Option<String> {
        self.data.as_ref().and_then(|d| d.status.sessions.warning())
    }

    /// Keep the selection on a visible account (the first one by default,
    /// preferring the active account when nothing was chosen yet).
    pub fn normalize_selection(&mut self, fallback_index: usize) {
        let visible = self.visible();
        if visible.is_empty() {
            self.selected_id = None;
            return;
        }
        if self.selected_index().is_some() {
            return;
        }
        let pick = if self.selected_id.is_none() {
            visible.iter().find(|a| a.active).or(visible.first())
        } else {
            visible.get(fallback_index.min(visible.len() - 1))
        };
        self.selected_id = pick.map(|a| a.id.clone());
    }

    /// Replace the data; the selection follows the account id, not the row.
    pub fn apply_dashboard(&mut self, d: Dashboard) {
        let old_index = self.selected_index().unwrap_or(0);
        self.data = Some(d);
        self.normalize_selection(old_index);
        self.dirty = true;
    }

    pub fn move_selection(&mut self, delta: isize) {
        let visible = self.visible();
        if visible.is_empty() {
            return;
        }
        let n = visible.len() as isize;
        let cur = self.selected_index().unwrap_or(0) as isize;
        let next = (cur + delta).rem_euclid(n) as usize;
        self.selected_id = visible.get(next).map(|a| a.id.clone());
        self.scroll = 0;
        self.dirty = true;
    }

    pub fn select_id(&mut self, id: &str) {
        self.selected_id = Some(id.to_owned());
        self.scroll = 0;
        self.dirty = true;
    }
    pub fn freshness(&self, account: &AccountUsage) -> Freshness {
        let seconds = self.data.as_ref().map_or(300, |d| d.quota_fresh_secs);
        FreshnessPolicy::new(seconds).classify(account.observed_at, self.now)
    }

    pub fn health_evidence<'a>(&self, account: &'a AccountUsage) -> &'a str {
        if account.health.state != AccountHealth::Healthy {
            return &account.health.evidence;
        }
        if self.freshness(account) == Freshness::Fresh {
            "agy returned quota data during this session"
        } else {
            "agy returned quota data at the recorded observation time"
        }
    }

    pub fn open_overlay(&mut self, overlay: Overlay) {
        self.overlay = Some(overlay);
        self.overlay_scroll = 0;
    }
}
