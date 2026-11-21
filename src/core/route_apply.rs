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
    lock: PathBuf,
}

impl RouteHistory {
    pub fn new(dirs: &Dirs) -> Self {
        Self {
            file: dirs.state.join("route-history.json"),
            lock: dirs.state.join("route-history.lock"),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.file
    }

    /// Lenient load: junk or unknown versions read as empty history. An
    /// absent file still reports the current schema version, so `--json`
    /// never presents an empty log as a different contract.
    pub fn load(&self) -> HistoryFile {
        let empty = || HistoryFile {
            schema_version: HISTORY_SCHEMA,
            entries: Vec::new(),
        };
        let Ok(bytes) = fsutil::read_optional(&self.file) else {
            return empty();
        };
        let Some(bytes) = bytes else {
            return empty();
        };
        if bytes.len() > MAX_HISTORY_BYTES {
            return empty();
        }
        match serde_json::from_slice::<HistoryFile>(&bytes) {
            Ok(f) if f.schema_version == HISTORY_SCHEMA => f,
            _ => empty(),
        }
    }

    /// Append one entry. Load → push → write happens under a state lock:
    /// `route apply` and monitor automation may record concurrently, and an
    /// unlocked read-modify-write would drop one side's entry.
    pub fn record(&self, entry: HistoryEntry) -> Result<()> {
        self.record_inner(entry, false)
    }

    /// Record a non-applied outcome unless an identical `(group, target,
    /// outcome, reason)` entry already appears in the trailing uninterrupted
    /// non-`Applied` streak.
    pub(crate) fn record_once(&self, entry: HistoryEntry) -> Result<()> {
        self.record_inner(entry, true)
    }

    fn record_inner(&self, entry: HistoryEntry, dedupe_streak: bool) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            fsutil::create_private_dir(dir).map_err(|e| Error::io("creating state dir", e))?;
        }
        let _guard = crate::storage::lock::StateLock::acquire(
            &self.lock,
            std::time::Duration::from_secs(3),
        )?;
        let mut file = self.load();
        if dedupe_streak
            && entry.outcome != HistoryOutcome::Applied
            && file
                .entries
                .iter()
                .rev()
                .take_while(|prev| prev.outcome != HistoryOutcome::Applied)
                .any(|prev| {
                    prev.group == entry.group
                        && prev.target == entry.target
                        && prev.outcome == entry.outcome
                        && prev.reason == entry.reason
                })
        {
            return Ok(());
        }
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

pub(crate) struct ApplyTarget<'a> {
    pub group: &'a str,
    pub target_id: &'a str,
    pub reason: &'a str,
    pub automatic: bool,
    pub record_cooldown: bool,
}

impl App {
    /// Decide what `route apply GROUP` would do, without mutating anything.
    /// Re-reads routing.toml and the quota cache so the decision reflects
    /// current evidence; callers re-validate under the lock before switching.
    ///
    /// `for_model` is the caller's family filter and applies to EVERY plan
    /// computed here: automation decided on a family-aware ranking, so the
    /// switch must revalidate the same ranking (`None` for the plain CLI).
    pub fn route_apply_plan(
        &self,
        group: &str,
        for_model: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<ApplyPlan> {
        use crate::core::route_config::Ranking;
        let config =
            crate::core::route_config::RoutingFile::load(&self.dirs.config.join("routing.toml"))?;
        let g = config.group(Some(group))?;
        let plan = self.route_group_for(Some(group), for_model, now)?;
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
        let session_warning = crate::agy::sessions::detect(&self.agy, self.session_scan).warning();
        let ranking = match g.ranking {
            Ranking::Priority => "priority order",
            Ranking::LeastUsed => "least-used ranking",
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
    /// journaled switch through the existing path while holding the lock.
    /// Refuses on pending recovery, evidence drift, or a changed group
    /// definition.
    pub fn route_apply(
        &self,
        group: &str,
        for_model: Option<&str>,
        force: bool,
        confirmed: bool,
        history: &RouteHistory,
        now: DateTime<Utc>,
    ) -> Result<ApplyOutcome> {
        let plan = self.route_apply_plan(group, for_model, now)?;
        let lock = self.lock_login()?;
        let fresh = self.route_apply_plan(group, for_model, now)?;
        if fresh.target_id != plan.target_id {
            record_refusal_while_locked(
                history,
                refused_entry(
                    group,
                    &plan.target_id,
                    "recommendation changed while preparing the switch",
                    now,
                ),
            );
            return Err(Error::refused(format!(
                "the recommendation for `{group}` changed while preparing the switch (now {}); re-run `agyva route plan {group}`",
                fresh.target_alias
            )));
        }
        if let Some(pending) = self.pending_journal()? {
            record_refusal_while_locked(
                history,
                refused_entry(
                    group,
                    &plan.target_id,
                    "an interrupted switch awaits recovery",
                    now,
                ),
            );
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
        let check = self.route_group_for(Some(group), for_model, now)?;
        if check.recommendation.as_deref() != Some(plan.target_id.as_str()) && !force {
            record_refusal_while_locked(
                history,
                refused_entry(
                    group,
                    &plan.target_id,
                    "cached evidence no longer supports the recommendation",
                    now,
                ),
            );
            return Err(Error::refused(format!(
                "cached evidence no longer supports `{}` as the recommendation for `{group}`; refresh and re-plan, or pass --force",
                fresh.target_alias
            )));
        }
        self.route_apply_target_under_lock(
            &lock,
            ApplyTarget {
                group,
                target_id: &plan.target_id,
                reason: &fresh.reason,
                automatic: false,
                record_cooldown: true,
            },
            history,
            now,
        )
    }

    pub(crate) fn route_apply_target_under_lock(
        &self,
        lock: &crate::core::app::LoginLock,
        target: ApplyTarget<'_>,
        history: &RouteHistory,
        now: DateTime<Utc>,
    ) -> Result<ApplyOutcome> {
        let outcome = self.switch_under_lock(
            lock,
            target.target_id,
            false,
            now,
            target.automatic,
            target.record_cooldown,
        );
        match outcome {
            Ok(mut sw) => {
                if let Err(e) = history.record(HistoryEntry {
                    at: now,
                    group: Some(target.group.to_owned()),
                    target: target.target_id.to_owned(),
                    outcome: HistoryOutcome::Applied,
                    reason: target.reason.to_owned(),
                }) {
                    tracing::warn!(
                        "could not record applied switch in route history: {}",
                        e.message()
                    );
                    sw.warnings.push(format!(
                        "switch succeeded, but route history could not be updated ({})",
                        e.message()
                    ));
                }
                Ok(ApplyOutcome {
                    alias: sw.account.alias,
                    already_active: sw.already_active,
                    warnings: sw.warnings,
                })
            }
            Err(e) => {
                if let Err(he) = history.record_once(HistoryEntry {
                    at: now,
                    group: Some(target.group.to_owned()),
                    target: target.target_id.to_owned(),
                    outcome: HistoryOutcome::Failed,
                    reason: bounded_reason(e.message()),
                }) {
                    tracing::warn!(
                        "could not record failed switch in route history: {}",
                        he.message()
                    );
                }
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

/// Refusals happen while `route_apply` holds the login lock, and the history
/// lock can be held by another process. Waiting for it would turn a clear
/// refusal into "another agyva is running", so a contention failure on the
/// LOG LINE never replaces the refusal itself. The success path still
/// propagates every error.
fn record_refusal_while_locked(history: &RouteHistory, entry: HistoryEntry) {
    if let Err(e) = history.record(entry) {
        tracing::warn!("could not record the routing refusal: {}", e.message());
    }
}

/// A refusal stamped with the decision clock, so an injected clock makes
/// history ordering deterministic (never a second `Utc::now()`).
fn refused_entry(group: &str, target: &str, why: &str, now: DateTime<Utc>) -> HistoryEntry {
    HistoryEntry {
        at: now,
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
