//! Serializable views for `--json` output. Views never contain secrets;
//! every top-level view carries `schema_version`.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::core::registry::{AccountRecord, Registry};

pub const VIEW_SCHEMA: u32 = 1;

#[derive(Debug, Clone, Serialize)]
pub struct AccountView {
    pub number: usize,
    pub id: String,
    pub alias: String,
    pub email: Option<String>,
    pub auth_method: String,
    pub active: bool,
    pub previous: bool,
    pub added_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

impl AccountView {
    pub fn of(reg: &Registry, rec: &AccountRecord) -> Self {
        Self {
            number: reg
                .accounts
                .iter()
                .position(|a| a.id == rec.id)
                .map_or(0, |i| i + 1),
            id: rec.id.clone(),
            alias: rec.alias.clone(),
            email: rec.email.clone(),
            auth_method: rec.auth_method.clone(),
            active: reg.active.as_deref() == Some(&rec.id),
            previous: reg.previous.as_deref() == Some(&rec.id),
            added_at: rec.added_at,
            last_used_at: rec.last_used_at,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ListView {
    pub schema_version: u32,
    pub active: Option<String>,
    pub accounts: Vec<AccountView>,
}

impl ListView {
    pub fn of(reg: &Registry) -> Self {
        Self {
            schema_version: VIEW_SCHEMA,
            active: reg.active.clone(),
            accounts: reg
                .accounts
                .iter()
                .map(|a| AccountView::of(reg, a))
                .collect(),
        }
    }
}

/// What agy is logged in as right now.
#[derive(Debug, Clone, Serialize)]
pub struct CurrentView {
    pub schema_version: u32,
    pub logged_in: bool,
    /// Whether the live login is one agyva has saved.
    pub managed: bool,
    pub id: Option<String>,
    pub email: Option<String>,
    pub auth_method: Option<String>,
    pub account: Option<AccountView>,
}
