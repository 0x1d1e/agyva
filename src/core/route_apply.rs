//! `route apply`: activate a routing group's recommended account through the
//! ONE existing journaled switch, and a bounded decision history.
//!
//! The apply revalidates everything under the login lock, immediately before
//! mutation: the recommendation must still match the group's current cached
//! evidence (unless `--force`), no pending recovery may exist, and the group
//! definition is re-read from disk. History records decision reasons only —
//! never credentials, emails or tokens.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::core::app::App;
use crate::core::error::{Error, ErrorKind, Result};
use crate::paths::Dirs;
use crate::storage::fsutil;

pub const HISTORY_SCHEMA: u32 = 1;
const MAX_HISTORY_BYTES: usize = 256 * 1024;
/// Hard cap on retained entries; `route history --limit` may show fewer.
pub const MAX_HISTORY_ENTRIES: usize = 50;

/// One recorded routing decision. Deliberately minimal: aliases, ids and
/// reasons — nothing that reads like a credential.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HistoryEntry {
    pub at: DateTime<Utc>,
    /// The group applied (absent for a plain `use`-style activation).
    pub group: Option<String>,
    /// The account the recommendation named.
    pub target: String,
    /// What actually happened.
    pub outcome: HistoryOutcome,
    /// Human-readable decision reason (already bounded).
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryOutcome {
    Applied,
    Refused,
    Failed,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryFile {
    pub schema_version: u32,
    pub entries: Vec<HistoryEntry>,
}

/// Bounded, redacted decision history at `<state>/route-history.json`.
pub struct RouteHistory {
    file: PathBuf,
}

impl RouteHistory {
    pub fn new(dirs: &Dirs) -> Self {
        Self {
            file: dirs.state.join("route-history.json"),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.file
    }

    /// Lenient load: junk or unknown versions read as empty history.
    pub fn load(&self) -> HistoryFile {
        let Ok(bytes) = fsutil::read_optional(&self.file) else {
            return HistoryFile::default();
        };
        let Some(bytes) = bytes else {
            return HistoryFile::default();
        };
        if bytes.len() > MAX_HISTORY_BYTES {
            return HistoryFile::default();
        }
        match serde_json::from_slice::<HistoryFile>(&bytes) {
            Ok(f) if f.schema_version == HISTORY_SCHEMA => f,
            _ => HistoryFile::default(),
        }
    }

    pub fn record(&self, entry: HistoryEntry) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            fsutil::create_private_dir(dir).map_err(|e| Error::io("creating state dir", e))?;
        }
        let mut file = self.load();
        file.schema_version = HISTORY_SCHEMA;
        file.entries.push(entry);
        // Keep the most recent entries only.
        let excess = file.entries.len().saturating_sub(MAX_HISTORY_ENTRIES);
        if excess > 0 {
            file.entries.drain(..excess);
        }
        let mut bytes =
            serde_json::to_vec_pretty(&file).map_err(|e| Error::format(e.to_string()))?;
        bytes.push(b'\n');
        fsutil::atomic_write(&self.file, &bytes).map_err(|e| Error::io("writing route history", e))
    }
}

/// What `route apply` decided before touching anything.
#[derive(Debug)]
pub struct ApplyPlan {
    pub group: String,
    pub target_id: String,
    pub target_alias: String,
    pub reason: String,
    /// The declared plan for the target, if the user declared one.
    pub declared_plan: Option<String>,
    /// Warnings shown before confirmation (e.g. running agy sessions).
    pub session_warning: Option<String>,
}

/// The applied switch outcome (from the one journaled switch path).
#[derive(Debug)]
pub struct ApplyOutcome {
    pub alias: String,
    pub already_active: bool,
    pub warnings: Vec<String>,
}

impl App {
    /// Decide what `route apply GROUP` would do, without mutating anything.
    /// Re-reads routing.toml and the quota cache so the decision reflects
    /// current evidence; callers re-validate under the lock before switching.
    pub fn route_apply_plan(&self, group: &str, now: DateTime<Utc>) -> Result<ApplyPlan> {
        use crate::core::route_config::Ranking;
        let config =
            crate::core::route_config::RoutingFile::load(&self.dirs.config.join("routing.toml"))?;
        let g = config.group(Some(group))?;
        let plan = self.route_group_at(Some(group), now)?;
        let Some(rec) = plan.recommendation.clone() else {
            return Err(Error::usage(format!(
                "group `{group}` has no eligible account; nothing to apply (see `agyva route inspect {group}`)"
            )));
        };
        let row = plan
            .accounts
            .iter()
            .find(|r| r.id == rec)
            .ok_or_else(|| Error::format("recommendation missing from plan rows"))?;
        let reg = self.load_registry()?;
        let target = crate::core::selector::resolve(&reg, &rec)?.clone();
        let declared_plan = g
            .plans
            .get(&target.alias)
            .or_else(|| g.plans.get(&target.id))
            .map(|p| format!("{p:?}").to_ascii_lowercase());
        // Session warning is informational only: apply never signals or
        // kills anything, and switching does not retarget running sessions.
        let session_warning = crate::agy::sessions::detect(&self.agy).warning();
        let ranking = match g.ranking {
            Ranking::Priority => "priority order",
            Ranking::Smart => "smart ranking",
        };
        Ok(ApplyPlan {
            group: group.to_owned(),
            target_id: target.id.clone(),
            target_alias: target.alias.clone(),
            reason: format!("{ranking}: {}", row.why),
            declared_plan,
            session_warning,
        })
    }

    /// Validate the plan under the login lock, then perform exactly one
    /// journaled switch through the existing path. Refuses on pending
    /// recovery, evidence drift, or a changed group definition.
    pub fn route_apply(
        &self,
        group: &str,
        force: bool,
        confirmed: bool,
        history: &RouteHistory,
        now: DateTime<Utc>,
    ) -> Result<ApplyOutcome> {
        let plan = self.route_apply_plan(group, now)?;
        // Revalidation pass under the login lock: the group file, cache or
        // registry may have changed between planning and locking. The lock
        // is RELEASED before the switch: `switch` takes the same locks for
        // the mutation itself and re-checks recovery there (holding them
        // here would self-deadlock).
        {
            let _lock = self.lock_login()?;
            let fresh = self.route_apply_plan(group, now)?;
            if fresh.target_id != plan.target_id {
                history.record(refused_entry(
                    group,
                    &plan.target_id,
                    "recommendation changed while preparing the switch",
                ))?;
                return Err(Error::refused(format!(
                    "the recommendation for `{group}` changed while preparing the switch (now {}); re-run `agyva route plan {group}`",
                    fresh.target_alias
                )));
            }
            if let Some(pending) = self.pending_journal()? {
                history.record(refused_entry(
                    group,
                    &plan.target_id,
                    "an interrupted switch awaits recovery",
                ))?;
                return Err(Error::recovery_required(format!(
                    "an interrupted switch to account {} ({}) must be recovered first",
                    pending.target,
                    pending.phase.as_str()
                )));
            }
            if !confirmed {
                // Nothing happened; record nothing and refuse so the CLI
                // can prompt.
                return Err(Error::new(
                    ErrorKind::Usage,
                    "confirmation required (pass --yes non-interactively)",
                ));
            }
            // The target must still be the recommendation in the evidence
            // as seen under the lock.
            let check = self.route_group_at(Some(group), now)?;
            if check.recommendation.as_deref() != Some(plan.target_id.as_str()) && !force {
                history.record(refused_entry(
                    group,
                    &plan.target_id,
                    "cached evidence no longer supports the recommendation",
                ))?;
                return Err(Error::refused(format!(
                    "cached evidence no longer supports `{}` as the recommendation for `{group}`; refresh and re-plan, or pass --force",
                    plan.target_alias
                )));
            }
        }
        let outcome = self.switch(&plan.target_alias, false);
        match outcome {
            Ok(sw) => {
                history.record(HistoryEntry {
                    at: now,
                    group: Some(group.to_owned()),
                    target: plan.target_id.clone(),
                    outcome: HistoryOutcome::Applied,
                    reason: plan.reason.clone(),
                })?;
                Ok(ApplyOutcome {
                    alias: sw.account.alias,
                    already_active: sw.already_active,
                    warnings: sw.warnings,
                })
            }
            Err(e) => {
                history.record(HistoryEntry {
                    at: now,
                    group: Some(group.to_owned()),
                    target: plan.target_id.clone(),
                    outcome: HistoryOutcome::Failed,
                    reason: bounded_reason(e.message()),
                })?;
                Err(e)
            }
        }
    }
}

/// Test-only re-export so integration tests exercise the same bound.
#[doc(hidden)]
pub fn tests_bounded_reason_for_docs(text: &str) -> String {
    bounded_reason(text)
}

fn refused_entry(group: &str, target: &str, why: &str) -> HistoryEntry {
    HistoryEntry {
        at: Utc::now(),
        group: Some(group.to_owned()),
        target: target.to_owned(),
        outcome: HistoryOutcome::Refused,
        reason: bounded_reason(why),
    }
}

/// Reasons are prose, never credentials: bound their length.
fn bounded_reason(text: &str) -> String {
    const MAX: usize = 200;
    if text.chars().count() <= MAX {
        text.to_owned()
    } else {
        let cut: String = text.chars().take(MAX).collect();
        format!("{cut}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_is_bounded_and_lenient() {
        let dir = tempfile::tempdir().unwrap();
        let dirs = Dirs::under(dir.path());
        let h = RouteHistory::new(&dirs);
        for i in 0..(MAX_HISTORY_ENTRIES + 10) {
            h.record(HistoryEntry {
                at: Utc::now(),
                group: Some("g".into()),
                target: format!("id{i}"),
                outcome: HistoryOutcome::Applied,
                reason: "test".into(),
            })
            .unwrap();
        }
        let f = h.load();
        assert_eq!(f.entries.len(), MAX_HISTORY_ENTRIES);
        assert_eq!(
            f.entries.last().unwrap().target,
            format!("id{}", MAX_HISTORY_ENTRIES + 9)
        );
        // Junk reads as empty.
        std::fs::create_dir_all(dirs.state).unwrap();
        std::fs::write(h.path(), "{{{").unwrap();
        assert!(h.load().entries.is_empty());
    }

    #[test]
    fn reasons_are_bounded() {
        let long = "x".repeat(500);
        assert_eq!(bounded_reason(&long).chars().count(), 201);
    }
}
