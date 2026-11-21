//! Interface state. Plain data plus small pure helpers; all transitions live
//! in `event.rs`. Nothing here touches the terminal or the disk.

use chrono::{DateTime, Utc};

use crate::core::app_integrations::EndpointView;
use crate::core::dashboard::{Dashboard, RouteGroupSnapshot};
use crate::core::health::AccountHealth;
use crate::core::profile::{ProfileSource, ProfileSummary};
use crate::core::quota::{Freshness, FreshnessPolicy};
use crate::core::usage::AccountUsage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum View {
    Accounts,
    Usage,
    Endpoints,
    Routes,
    Diagnostics,
}

impl View {
    pub const ALL: [View; 5] = [
        View::Accounts,
        View::Usage,
        View::Endpoints,
        View::Routes,
        View::Diagnostics,
    ];

    pub fn title(self) -> &'static str {
        match self {
            View::Accounts => "Accounts",
            View::Usage => "Usage",
            View::Endpoints => "Endpoints",
            View::Routes => "Routes",
            View::Diagnostics => "Diagnostics",
        }
    }

    pub fn step(self, delta: isize) -> View {
        let i = Self::ALL.iter().position(|v| *v == self).unwrap_or(0);
        let n = Self::ALL.len() as isize;
        Self::ALL[(i as isize + delta).rem_euclid(n) as usize]
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortOrder {
    #[default]
    Default,
    Quota,
    Alias,
    Freshness,
}

impl SortOrder {
    pub fn next(self) -> Self {
        match self {
            Self::Default => Self::Quota,
            Self::Quota => Self::Alias,
            Self::Alias => Self::Freshness,
            Self::Freshness => Self::Default,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Quota => "quota",
            Self::Alias => "alias",
            Self::Freshness => "freshness",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Details,
    Confirm {
        id: String,
        alias: String,
    },
    ConfirmEndpoint {
        id: String,
        alias: String,
    },
    ConfirmRoute {
        group: String,
        target: Option<String>,
    },
    ConfirmProfile {
        name: String,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum RouteItemKind<'a> {
    Profile(&'a ProfileSummary),
    Group(&'a RouteGroupSnapshot),
}

#[derive(Debug, Clone)]
pub struct RouteItem<'a> {
    pub number: usize,
    pub key: String,
    pub kind: RouteItemKind<'a>,
}

#[derive(Debug)]
pub struct AppState {
    pub view: View,
    pub data: Option<Dashboard>,
    pub selected_id: Option<String>,
    pub selected_endpoint_id: Option<String>,
    pub selected_route_id: Option<String>,
    pub sort_order: SortOrder,
    pub filter: String,
    pub filter_editing: bool,
    pub overlay: Option<Overlay>,
    /// Loads in flight (cache read or agy refresh).
    pub pending_loads: usize,
    pub switching: bool,
    pub endpoint_busy: bool,
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
            selected_endpoint_id: None,
            selected_route_id: None,
            sort_order: SortOrder::Default,
            filter: String::new(),
            filter_editing: false,
            overlay: None,
            pending_loads: 0,
            switching: false,
            endpoint_busy: false,
            quit_after_switch: false,
            error: None,
            message: None,
            scroll: 0,
            overlay_scroll: 0,
            color: true,
            size,
            now,
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

    /// Accounts that match the live filter (alias, email or id prefix),
    /// ordered by the current [`SortOrder`].
    pub fn visible(&self) -> Vec<&AccountUsage> {
        let q = self.filter.trim().to_lowercase();
        let mut out: Vec<&AccountUsage> = self
            .accounts()
            .iter()
            .filter(|a| {
                q.is_empty()
                    || a.alias.to_lowercase().contains(&q)
                    || a.email
                        .as_deref()
                        .is_some_and(|e| e.to_lowercase().contains(&q))
                    || a.id.to_lowercase().starts_with(&q)
            })
            .collect();
        match self.sort_order {
            SortOrder::Default => {}
            SortOrder::Quota => {
                out.sort_by(|a, b| {
                    let qa = min_quota_percent(a);
                    let qb = min_quota_percent(b);
                    match (qa, qb) {
                        (Some(va), Some(vb)) => vb.total_cmp(&va),
                        (Some(_), None) => std::cmp::Ordering::Less,
                        (None, Some(_)) => std::cmp::Ordering::Greater,
                        (None, None) => std::cmp::Ordering::Equal,
                    }
                    .then_with(|| a.number.cmp(&b.number))
                });
            }
            SortOrder::Alias => {
                out.sort_by(|a, b| {
                    a.alias
                        .to_lowercase()
                        .cmp(&b.alias.to_lowercase())
                        .then_with(|| a.number.cmp(&b.number))
                });
            }
            SortOrder::Freshness => {
                out.sort_by(|a, b| {
                    freshness_rank(self.freshness(a))
                        .cmp(&freshness_rank(self.freshness(b)))
                        .then_with(|| b.observed_at.cmp(&a.observed_at))
                        .then_with(|| a.number.cmp(&b.number))
                });
            }
        }
        out
    }

    pub fn selected_index(&self) -> Option<usize> {
        let id = self.selected_id.as_deref()?;
        self.visible().iter().position(|a| a.id == id)
    }

    pub fn selected(&self) -> Option<&AccountUsage> {
        let id = self.selected_id.as_deref()?;
        self.accounts().iter().find(|a| a.id == id)
    }

    pub fn endpoints(&self) -> &[EndpointView] {
        self.data.as_ref().map_or(&[], |d| &d.endpoints)
    }

    pub fn visible_endpoints(&self) -> Vec<&EndpointView> {
        let q = self.filter.trim().to_lowercase();
        self.endpoints()
            .iter()
            .filter(|e| {
                q.is_empty()
                    || e.alias.to_lowercase().contains(&q)
                    || e.base_url.to_lowercase().contains(&q)
                    || e.protocol.as_str().contains(&q)
                    || e.detected_protocol.is_some_and(|p| p.as_str().contains(&q))
                    || e.default_model
                        .as_deref()
                        .is_some_and(|m| m.to_lowercase().contains(&q))
                    || e.id.to_lowercase().starts_with(&q)
            })
            .collect()
    }

    pub fn selected_endpoint_index(&self) -> Option<usize> {
        let id = self.selected_endpoint_id.as_deref()?;
        self.visible_endpoints().iter().position(|e| e.id == id)
    }

    pub fn selected_endpoint(&self) -> Option<&EndpointView> {
        let id = self.selected_endpoint_id.as_deref()?;
        self.endpoints().iter().find(|e| e.id == id)
    }

    pub fn select_endpoint_id(&mut self, id: &str) {
        self.selected_endpoint_id = Some(id.to_owned());
        self.scroll = 0;
        self.dirty = true;
    }

    pub fn route_items(&self) -> Vec<RouteItem<'_>> {
        let Some(d) = &self.data else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(d.profiles.len() + d.routing_groups.len());
        let mut n = 1usize;
        for p in &d.profiles {
            let src = match p.source {
                ProfileSource::Local => "local",
                ProfileSource::Global => "global",
                ProfileSource::Explicit => "explicit",
            };
            out.push(RouteItem {
                number: n,
                key: format!("profile:{src}:{}", p.name),
                kind: RouteItemKind::Profile(p),
            });
            n += 1;
        }
        for g in &d.routing_groups {
            out.push(RouteItem {
                number: n,
                key: format!("group:{}", g.name),
                kind: RouteItemKind::Group(g),
            });
            n += 1;
        }
        out
    }

    pub fn visible_route_items(&self) -> Vec<RouteItem<'_>> {
        let q = self.filter.trim().to_lowercase();
        self.route_items()
            .into_iter()
            .filter(|item| {
                if q.is_empty() {
                    return true;
                }
                match item.kind {
                    RouteItemKind::Profile(p) => {
                        p.name.to_lowercase().contains(&q)
                            || p.description
                                .as_deref()
                                .is_some_and(|s| s.to_lowercase().contains(&q))
                            || p.account
                                .as_deref()
                                .is_some_and(|s| s.to_lowercase().contains(&q))
                            || p.endpoint
                                .as_deref()
                                .is_some_and(|s| s.to_lowercase().contains(&q))
                            || p.group
                                .as_deref()
                                .is_some_and(|s| s.to_lowercase().contains(&q))
                            || p.model
                                .as_deref()
                                .is_some_and(|s| s.to_lowercase().contains(&q))
                    }
                    RouteItemKind::Group(g) => {
                        g.name.to_lowercase().contains(&q)
                            || g.plan
                                .accounts
                                .iter()
                                .any(|r| r.alias.to_lowercase().contains(&q))
                    }
                }
            })
            .collect()
    }

    pub fn selected_route_index(&self) -> Option<usize> {
        let key = self.selected_route_id.as_deref()?;
        self.visible_route_items()
            .iter()
            .position(|item| item.key == key)
    }

    pub fn selected_route_item(&self) -> Option<RouteItem<'_>> {
        let key = self.selected_route_id.as_deref()?;
        self.route_items().into_iter().find(|item| item.key == key)
    }

    pub fn select_route_id(&mut self, key: &str) {
        self.selected_route_id = Some(key.to_owned());
        self.scroll = 0;
        self.dirty = true;
    }

    pub fn visible_count(&self) -> usize {
        match self.view {
            View::Accounts | View::Usage | View::Diagnostics => self.visible().len(),
            View::Endpoints => self.visible_endpoints().len(),
            View::Routes => self.visible_route_items().len(),
        }
    }

    pub fn has_details(&self) -> bool {
        match self.view {
            View::Accounts | View::Usage => self.selected().is_some(),
            View::Endpoints => self.selected_endpoint().is_some(),
            View::Routes => self.selected_route_item().is_some(),
            View::Diagnostics => false,
        }
    }

    pub fn active_profile_name(&self) -> Option<String> {
        let p = self.data.as_ref()?.active_profile.as_ref()?;
        Some(p.name.clone().unwrap_or_else(|| match p.source {
            ProfileSource::Local => "local".to_owned(),
            ProfileSource::Global | ProfileSource::Explicit => "default".to_owned(),
        }))
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

    fn normalize_account_selection(&mut self, fallback_index: usize) {
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

    fn normalize_endpoint_selection(&mut self, fallback_index: usize) {
        let visible = self.visible_endpoints();
        if visible.is_empty() {
            self.selected_endpoint_id = None;
            return;
        }
        if self.selected_endpoint_index().is_some() {
            return;
        }
        let pick = if self.selected_endpoint_id.is_none() {
            visible.iter().find(|e| e.active).or(visible.first())
        } else {
            visible.get(fallback_index.min(visible.len() - 1))
        };
        self.selected_endpoint_id = pick.map(|e| e.id.clone());
    }

    fn normalize_route_selection(&mut self, fallback_index: usize) {
        let visible = self.visible_route_items();
        if visible.is_empty() {
            self.selected_route_id = None;
            return;
        }
        if self.selected_route_index().is_some() {
            return;
        }
        let pick = visible.get(fallback_index.min(visible.len() - 1));
        self.selected_route_id = pick.map(|item| item.key.clone());
    }

    /// Keep selections on visible items for the active view (or all views when
    /// unfiltered).
    pub fn normalize_selection(&mut self, fallback_index: usize) {
        if self.filter.trim().is_empty() {
            self.normalize_account_selection(fallback_index);
            self.normalize_endpoint_selection(fallback_index);
            self.normalize_route_selection(fallback_index);
        } else {
            match self.view {
                View::Accounts | View::Usage | View::Diagnostics => {
                    self.normalize_account_selection(fallback_index);
                }
                View::Endpoints => self.normalize_endpoint_selection(fallback_index),
                View::Routes => self.normalize_route_selection(fallback_index),
            }
        }
    }

    /// Replace the data; selections follow stable ids, not row indices.
    pub fn apply_dashboard(&mut self, d: Dashboard) {
        let old_account = self.selected_index().unwrap_or(0);
        let old_endpoint = self.selected_endpoint_index().unwrap_or(0);
        let old_route = self.selected_route_index().unwrap_or(0);
        let notice = d.update_notice.clone();
        self.data = Some(d);
        // A fresh release notice shows once, without clobbering a message
        // the user is already reading.
        if let Some(n) = notice
            && self.message.is_none()
        {
            self.message = Some(n);
        }
        self.normalize_account_selection(old_account);
        self.normalize_endpoint_selection(old_endpoint);
        self.normalize_route_selection(old_route);
        self.dirty = true;
    }

    pub fn move_selection(&mut self, delta: isize) {
        match self.view {
            View::Accounts | View::Usage => {
                let visible = self.visible();
                if visible.is_empty() {
                    return;
                }
                let n = visible.len() as isize;
                let cur = self.selected_index().unwrap_or(0) as isize;
                let next = (cur + delta).rem_euclid(n) as usize;
                self.selected_id = visible.get(next).map(|a| a.id.clone());
            }
            View::Endpoints => {
                let visible = self.visible_endpoints();
                if visible.is_empty() {
                    return;
                }
                let n = visible.len() as isize;
                let cur = self.selected_endpoint_index().unwrap_or(0) as isize;
                let next = (cur + delta).rem_euclid(n) as usize;
                self.selected_endpoint_id = visible.get(next).map(|e| e.id.clone());
            }
            View::Routes => {
                let visible = self.visible_route_items();
                if visible.is_empty() {
                    return;
                }
                let n = visible.len() as isize;
                let cur = self.selected_route_index().unwrap_or(0) as isize;
                let next = (cur + delta).rem_euclid(n) as usize;
                self.selected_route_id = visible.get(next).map(|item| item.key.clone());
            }
            View::Diagnostics => return,
        }
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

fn min_quota_percent(a: &AccountUsage) -> Option<f64> {
    a.groups
        .iter()
        .filter(|g| !g.disabled)
        .flat_map(|g| [g.five_hour.as_ref(), g.weekly.as_ref()])
        .flatten()
        .map(|w| w.remaining_percent)
        .filter(|v| v.is_finite())
        .reduce(f64::min)
}

fn freshness_rank(f: Freshness) -> u8 {
    match f {
        Freshness::Fresh => 0,
        Freshness::Stale => 1,
        Freshness::Expired => 2,
        Freshness::Missing => 3,
    }
}
