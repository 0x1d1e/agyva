//! The per-account model catalog: what `agy models` reported for the account
//! that was active at the time, versioned in `<cache>/models.json`.
//!
//! Non-sensitive by construction (model ids, display names, timestamps). The
//! cache is disposable; a missing, corrupt or unknown-version file reads as
//! empty and never blocks anything.
//!
//! Plans (`Free`/`Pro`/`Ultra`) are *user declarations only* (in
//! `routing.toml`), with explicit provenance: agy never reports a plan, so an
//! account without a declaration is `unknown`, never inferred from quota
//! windows or model lists. The static reference families in
//! [`reference_families`] are descriptive fallback for humans — never proof
//! that an account supports a model.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::agy::models::ModelFamily;
use crate::agy::usage_run::RunLimits;
use crate::core::app::App;
use crate::core::error::Result;
use crate::core::health::AccountHealth;
use crate::paths::Dirs;
use crate::storage::fsutil;
use crate::storage::lock::StateLock;
pub const MODELS_CACHE_SCHEMA: u32 = 1;
const MAX_CACHE_BYTES: usize = 1024 * 1024;

/// A saved catalog observation for one account.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CachedCatalog {
    pub observed_at: DateTime<Utc>,
    pub families: Vec<ModelFamily>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelsCacheFile {
    pub schema_version: u32,
    pub accounts: BTreeMap<String, CachedCatalog>,
}

/// What an account's catalog state is.
#[derive(Debug, Clone, PartialEq)]
pub enum CatalogState {
    /// Fresh enough to use (age <= `models_cache_secs`).
    Fresh(CachedCatalog),
    /// Held from the cache but older than `models_cache_secs`.
    Stale(CachedCatalog),
    /// No usable observation at all.
    Missing,
}

impl CatalogState {
    pub fn families(&self) -> &[ModelFamily] {
        match self {
            CatalogState::Fresh(c) | CatalogState::Stale(c) => &c.families,
            CatalogState::Missing => &[],
        }
    }
    pub fn observed_at(&self) -> Option<DateTime<Utc>> {
        match self {
            CatalogState::Fresh(c) | CatalogState::Stale(c) => Some(c.observed_at),
            CatalogState::Missing => None,
        }
    }
    pub fn is_fresh(&self) -> bool {
        matches!(self, CatalogState::Fresh(_))
    }
}

pub use crate::core::route_config::DeclaredPlan;

/// Descriptive reference families (from public Antigravity material), shown
/// to humans only. NEVER proof that any account supports a model.
pub const REFERENCE_FAMILIES: &[&str] = &[
    "gemini-4-carbon",
    "gemini-4-argon",
    "gemini-4-hydrogen",
    "gemini-3.8-flash",
    "claude-opus-5-5",
    "claude-sonnet-5-5",
    "gpt-oss-120b",
];

#[derive(Debug, Clone)]
pub struct ModelsCache {
    file: PathBuf,
    lock: PathBuf,
}

impl ModelsCache {
    pub fn new(dirs: &Dirs) -> Self {
        Self {
            file: dirs.cache.join("models.json"),
            lock: dirs.cache.join("models.lock"),
        }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.file
    }

    /// Lenient load: corrupt/oversized/unknown-version reads as empty.
    pub fn load(&self) -> ModelsCacheFile {
        let bytes = match fsutil::read_optional(&self.file) {
            Ok(Some(b)) if b.len() <= MAX_CACHE_BYTES => b,
            _ => return ModelsCacheFile::default(),
        };
        match serde_json::from_slice::<ModelsCacheFile>(&bytes) {
            Ok(f) if f.schema_version == MODELS_CACHE_SCHEMA => f,
            _ => ModelsCacheFile::default(),
        }
    }

    pub fn read_state(&self, id: &str, fresh_secs: u64, now: DateTime<Utc>) -> CatalogState {
        let Some(cached) = self.load().accounts.get(id).cloned() else {
            return CatalogState::Missing;
        };
        let fresh = chrono::Duration::seconds(i64::try_from(fresh_secs).unwrap_or(86_400));
        if now >= cached.observed_at && now - cached.observed_at <= fresh {
            CatalogState::Fresh(cached)
        } else {
            CatalogState::Stale(cached)
        }
    }

    /// Record a successful observation for `id` (read-modify-write, locked).
    pub fn record(&self, id: &str, families: Vec<ModelFamily>, now: DateTime<Utc>) -> Result<()> {
        if let Some(dir) = self.file.parent() {
            fsutil::create_private_dir(dir)
                .map_err(|e| crate::core::error::Error::io("creating cache dir", e))?;
        }
        let _guard = StateLock::acquire(&self.lock, Duration::from_secs(3))?;
        let mut file = self.load();
        file.schema_version = MODELS_CACHE_SCHEMA;
        file.accounts.insert(
            id.to_owned(),
            CachedCatalog {
                observed_at: now,
                families,
            },
        );
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|e| crate::core::error::Error::format(e.to_string()))?;
        fsutil::atomic_write(&self.file, &bytes)
            .map_err(|e| crate::core::error::Error::io("writing models cache", e))
    }

    /// Drop accounts that no longer exist in the registry.
    pub fn retain(&self, ids: &[String]) -> Result<()> {
        let _guard = StateLock::acquire(&self.lock, Duration::from_secs(3))?;
        let mut file = self.load();
        let before = file.accounts.len();
        file.accounts.retain(|k, _| ids.contains(k));
        if file.accounts.len() == before {
            return Ok(());
        }
        let bytes = serde_json::to_vec_pretty(&file)
            .map_err(|e| crate::core::error::Error::format(e.to_string()))?;
        fsutil::atomic_write(&self.file, &bytes)
            .map_err(|e| crate::core::error::Error::io("writing models cache", e))
    }
}

/// Whether an account's cached catalog offers a model family: `--for
/// gemini-4-carbon` matches the family id or any of its raw effort ids
/// (`gemini-4-carbon-high`). Unknown (Missing) or stale observations never
/// claim support; a stale-but-present observation is reported as such, not
/// used as proof.
pub fn offers_family(state: &CatalogState, wanted: &str) -> FamilySupport {
    let wanted = wanted.trim().to_ascii_lowercase();
    if wanted.is_empty() {
        return FamilySupport::NotSought;
    }
    let normalized: Vec<(String, String)> = state
        .families()
        .iter()
        .flat_map(|f| {
            let id = f.id.to_ascii_lowercase();
            let mut out = vec![(id.clone(), id.clone())];
            for e in &f.variants {
                out.push((format!("{id}-{e}"), id.clone()));
            }
            out
        })
        .collect();
    match state {
        CatalogState::Missing => FamilySupport::UnknownCatalog,
        CatalogState::Stale(_) => {
            if normalized.iter().any(|(raw, _)| *raw == wanted) {
                FamilySupport::StaleSupport
            } else {
                FamilySupport::UnknownCatalog
            }
        }
        CatalogState::Fresh(_) => {
            if normalized.iter().any(|(raw, _)| *raw == wanted) {
                FamilySupport::Supported(
                    normalized
                        .iter()
                        .find(|(raw, _)| *raw == wanted)
                        .map(|(_, family)| family.clone())
                        .unwrap_or_default(),
                )
            } else if state.families().is_empty() {
                FamilySupport::UnknownCatalog
            } else {
                FamilySupport::Absent
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilySupport {
    /// The fresh catalog lists the family (carries the family id).
    Supported(String),
    /// The catalog is fresh and does not list it.
    Absent,
    /// A stale observation listed it; refresh before trusting it.
    StaleSupport,
    /// No usable catalog (never proof of absence).
    UnknownCatalog,
    /// No `--for` was requested.
    NotSought,
}

impl FamilySupport {
    /// Whether the account may be recommended for the requested family.
    /// Only fresh, observed support counts; everything else excludes with a
    /// reason, never silently.
    pub fn eligible(&self) -> bool {
        matches!(self, FamilySupport::Supported(_) | FamilySupport::NotSought)
    }

    pub fn reason(&self) -> &'static str {
        match self {
            FamilySupport::Supported(_) | FamilySupport::NotSought => "",
            FamilySupport::Absent => "fresh catalog does not offer the requested model",
            FamilySupport::StaleSupport => "stale catalog offered it; refresh required",
            FamilySupport::UnknownCatalog => "model catalog unknown for this account",
        }
    }
}

/// Default limits for the bounded `agy models` run (same discipline as
/// `/usage`; slightly tighter because it is metadata, not quota).
pub fn models_run_limits() -> RunLimits {
    RunLimits {
        timeout: Duration::from_secs(10),
        max_bytes: 256 * 1024,
    }
}

/// Map a requested model family onto the quota groups an account actually
/// reports. The user's declared mapping (`routing.toml [groups.X.models]`)
/// is authoritative when present; otherwise the quota group whose name
/// contains the family id matches. A specific pool (one `model` set, or a
/// group name naming exactly this family) is preferred over a broad pool
/// (no model set); if neither exists the mapping is unknown and the
/// account is not eligible for `--for`.
#[derive(Clone, Copy)]
pub struct GroupView<'a> {
    pub group: &'a str,
    pub model: Option<&'a str>,
}

pub fn map_family_to_groups<'a>(
    wanted: &str,
    declared: Option<&str>,
    groups: impl IntoIterator<Item = GroupView<'a>>,
) -> FamilyGroups {
    let wanted = wanted.trim().to_ascii_lowercase();
    let mut specific: Option<(String, Option<String>)> = None;
    let mut broad: Option<(String, Option<String>)> = None;
    for g in groups.into_iter() {
        let matched = match declared {
            // A declared mapping is authoritative: only the named group
            // applies, no name/model guessing.
            Some(d) => d.eq_ignore_ascii_case(g.group),
            None => {
                let model_match = g
                    .model
                    .map(|m| {
                        let m = m.to_ascii_lowercase();
                        m.contains(&wanted) || wanted.contains(&m)
                    })
                    .unwrap_or(false);
                g.group.to_ascii_lowercase().contains(&wanted) || model_match
            }
        };
        if !matched {
            continue;
        }
        let has_model = g.model.is_some_and(|m| !m.is_empty());
        let entry = (g.group.to_owned(), g.model.map(str::to_owned));
        if has_model && specific.is_none() {
            specific = Some(entry);
        } else if !has_model && broad.is_none() {
            broad = Some(entry);
        }
    }
    match (specific, broad) {
        (Some(g), _) | (None, Some(g)) => FamilyGroups::Mapped(g),
        (None, None) => FamilyGroups::Unknown,
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FamilyGroups {
    /// (group name, optional model) whose windows apply to the family.
    Mapped((String, Option<String>)),
    /// No mapping could be established; the account is not eligible.
    Unknown,
}

impl App {
    /// The per-account model catalog cache.
    pub fn models_cache(&self) -> ModelsCache {
        ModelsCache::new(&self.dirs)
    }

    /// Run bounded `agy models` for the ACTIVE account only, under the same
    /// guards as a quota refresh: refresh lock, no pending recovery, no
    /// pending journal, `models_cache_secs` throttle, and the live agy login
    /// must equal the registry's active account before AND after the run.
    /// A failure keeps any prior valid observation (with its timestamp) —
    /// the cache is only ever replaced by a successful parse.
    pub fn refresh_models_at(&self, force: bool, now: DateTime<Utc>) -> Result<ModelsRefresh> {
        use crate::agy::models::{self, ModelsRun};
        use crate::core::error::ErrorKind;
        use crate::storage::lock::StateLock;

        let reg = self.load_registry()?;
        let Some(active) = reg.active.clone() else {
            return Ok(ModelsRefresh::skipped(
                None,
                "no account is active; run `agyva use`",
            ));
        };
        let id = Some(active.clone());
        let _guard = match StateLock::acquire(&self.dirs.refresh_lock_file(), self.refresh_wait) {
            Ok(g) => g,
            Err(e) if e.kind() == ErrorKind::Locked => {
                return Ok(ModelsRefresh::skipped(
                    id,
                    "another agyva is switching accounts or refreshing; try again shortly",
                ));
            }
            Err(e) => return Err(e),
        };
        if self.pending_journal().ok().flatten().is_some() {
            return Ok(ModelsRefresh::skipped(
                id,
                "an interrupted switch awaits `agyva recover`",
            ));
        }
        let cache = self.models_cache();
        if !force
            && let Some(at) = cache
                .load()
                .accounts
                .get(&active)
                .filter(|c| {
                    now >= c.observed_at
                        && now - c.observed_at
                            <= chrono::Duration::seconds(
                                i64::try_from(self.config.models_cache_secs).unwrap_or(86_400),
                            )
                })
                .map(|c| c.observed_at)
        {
            let age = (now - at).num_seconds().max(0);
            return Ok(ModelsRefresh::skipped(
                id,
                format!("using saved catalog (observed {age}s ago)"),
            ));
        }
        // Same suspension gate as `refresh_active_at`: a suspended account
        // is never retried automatically (only an explicit `--force`), so
        // automation must not spawn `agy models` against it either. The
        // persisted marker outlives the disposable cache.
        if !force {
            let entry = self.quota_cache().load().accounts.remove(&active);
            let flagged = entry
                .and_then(|e| e.health)
                .is_some_and(|h| h.state == AccountHealth::TermsSuspended)
                || self.suspended_markers().get(&active).is_some();
            if flagged {
                return Ok(ModelsRefresh::skipped(
                    id,
                    "account is suspended; not running `agy models` (use `agyva models --refresh` to check once)",
                ));
            }
        }
        // Same compatibility gate as `add`/`switch`: an agy build we were
        // never verified against is not spawned at all. Like the quota
        // reader, `--force` does not bypass this gate — only the explicit
        // `--allow-untested` / `allow_untested_agy` opt-in does, so the note
        // names that instead of suggesting a check that would refuse again.
        if let Err(e) = self.require_compat() {
            return Ok(ModelsRefresh::skipped(
                id,
                format!(
                    "{}; catalog refresh skipped (see `agyva version`; `--allow-untested` opts in)",
                    e.message()
                ),
            ));
        }
        // Same attribution rule as quota: only the account agy is logged in
        // as may be credited with an observation.
        match self.live_account_id() {
            Ok(live) if live != active => {
                return Ok(ModelsRefresh::skipped(
                    id,
                    "active login differs from registry; catalog not attributed",
                ));
            }
            Ok(_) => {}
            Err(e) => return Ok(ModelsRefresh::skipped(id, e.text())),
        }
        let Some(bin) = crate::agy::discovery::find_agy(&self.env) else {
            return Ok(ModelsRefresh::skipped(
                id,
                "agy was not found on PATH (set AGYVA_AGY_BIN to override)",
            ));
        };
        let run = models::run_models(&bin, &models_run_limits());
        if self.live_account_id().as_deref() != Ok(active.as_str()) {
            return Ok(ModelsRefresh::skipped(
                id,
                "the agy login changed during the catalog run",
            ));
        }
        match run {
            Ok(ModelsRun::Ok(catalog)) => {
                if let Err(e) = cache.record(&active, catalog.models, now) {
                    tracing::warn!("could not update the models cache: {}", e.message());
                }
                Ok(ModelsRefresh {
                    account_id: id,
                    status: ModelsRefreshStatus::Refreshed,
                    note: None,
                })
            }
            Ok(ModelsRun::Failed(note)) => Ok(ModelsRefresh {
                account_id: id,
                status: ModelsRefreshStatus::Failed,
                note: Some(note),
            }),
            // Spawn failure is not a catalog observation; prior data stays.
            Err(e) => Ok(ModelsRefresh {
                account_id: id,
                status: ModelsRefreshStatus::Failed,
                note: Some(format!("could not run agy models: {e}")),
            }),
        }
    }

    pub fn refresh_models(&self, force: bool) -> Result<ModelsRefresh> {
        self.refresh_models_at(force, Utc::now())
    }
}

/// One account's cached catalog, as `agyva models` reports it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AccountCatalog {
    pub id: String,
    pub alias: String,
    pub active: bool,
    /// When the observation was made; `None` when there is no catalog yet.
    pub observed_at: Option<DateTime<Utc>>,
    /// Age of the observation in seconds (negative if the clock moved back).
    pub age_secs: Option<i64>,
    /// Within `models_cache_secs`, i.e. usable as `--for-model` proof.
    pub fresh: bool,
    /// `fresh` | `stale` | `missing`.
    pub state: &'static str,
    pub families: Vec<ModelFamily>,
}

impl App {
    /// The cached model catalogs of every saved account, optionally after
    /// one bounded refresh of the active account. Read-only on the refresh
    /// path beyond that single `agy models` run.
    pub fn models_report(&self, refresh: bool, now: DateTime<Utc>) -> Result<ModelsReport> {
        let refresh = if refresh {
            Some(self.refresh_models_at(true, now)?)
        } else {
            None
        };
        let reg = self.load_registry()?;
        let cache = self.models_cache();
        let accounts = reg
            .accounts
            .iter()
            .map(|a| {
                let state = cache.read_state(&a.id, self.config.models_cache_secs, now);
                let observed_at = state.observed_at();
                AccountCatalog {
                    id: a.id.clone(),
                    alias: a.alias.clone(),
                    active: reg.active.as_deref() == Some(a.id.as_str()),
                    observed_at,
                    age_secs: observed_at.map(|at| (now - at).num_seconds()),
                    fresh: state.is_fresh(),
                    state: match state {
                        CatalogState::Fresh(_) => "fresh",
                        CatalogState::Stale(_) => "stale",
                        CatalogState::Missing => "missing",
                    },
                    families: state.families().to_vec(),
                }
            })
            .collect();
        Ok(ModelsReport {
            schema_version: 1,
            generated_at: now,
            refresh,
            accounts,
        })
    }
}

/// `agyva models`: every saved account's cached catalog plus, when
/// `--refresh` asked for one, what that run did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelsReport {
    pub schema_version: u32,
    pub generated_at: DateTime<Utc>,
    /// Present only for `--refresh`.
    pub refresh: Option<ModelsRefresh>,
    pub accounts: Vec<AccountCatalog>,
}

/// Outcome of a catalog refresh attempt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ModelsRefresh {
    pub account_id: Option<String>,
    pub status: ModelsRefreshStatus,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelsRefreshStatus {
    Refreshed,
    Skipped,
    Failed,
}

impl ModelsRefresh {
    fn skipped(id: Option<String>, note: impl Into<String>) -> Self {
        Self {
            account_id: id,
            status: ModelsRefreshStatus::Skipped,
            note: Some(note.into()),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agy::models::ModelFamily;

    fn catalog(ids: &[(&str, &[&str])]) -> CachedCatalog {
        CachedCatalog {
            observed_at: Utc::now(),
            families: ids
                .iter()
                .map(|(id, vars)| ModelFamily {
                    id: (*id).to_owned(),
                    display_name: (*id).to_owned(),
                    variants: vars.iter().map(|v| v.to_string()).collect(),
                })
                .collect(),
        }
    }

    #[test]
    fn family_support_never_invents_or_trusts_stale() {
        let fresh =
            CatalogState::Fresh(catalog(&[("gemini-4-carbon", &["low", "medium", "high"])]));
        assert!(matches!(
            offers_family(&fresh, "gemini-4-carbon"),
            FamilySupport::Supported(_)
        ));
        // Raw effort id resolves to its family.
        assert!(matches!(
            offers_family(&fresh, "gemini-4-carbon-high"),
            FamilySupport::Supported(_)
        ));
        assert_eq!(
            offers_family(&fresh, "gemini-4-argon"),
            FamilySupport::Absent
        );
        assert_eq!(offers_family(&fresh, ""), FamilySupport::NotSought);

        let mut stale_obs = catalog(&[("gemini-4-carbon", &["high"])]);
        stale_obs.observed_at = Utc::now() - chrono::Duration::hours(48);
        let stale = CatalogState::Stale(stale_obs);
        assert_eq!(
            offers_family(&stale, "gemini-4-carbon"),
            FamilySupport::StaleSupport
        );
        assert!(!offers_family(&stale, "gemini-4-carbon").eligible());
        assert_eq!(
            offers_family(&CatalogState::Missing, "anything"),
            FamilySupport::UnknownCatalog
        );
    }

    #[test]
    fn cache_roundtrips_and_reads_state() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ModelsCache::new(&Dirs::under(dir.path()));
        let now = Utc::now();
        cache
            .record("abc123", catalog(&[("family-x", &[])]).families, now)
            .unwrap();
        match cache.read_state("abc123", 86_400, now) {
            CatalogState::Fresh(c) => assert_eq!(c.families.len(), 1),
            other => panic!("{other:?}"),
        }
        // Outside the freshness window it is stale, not missing.
        match cache.read_state("abc123", 60, now + chrono::Duration::hours(2)) {
            CatalogState::Stale(_) => {}
            other => panic!("{other:?}"),
        }
        // Unknown account.
        assert_eq!(cache.read_state("nope", 86_400, now), CatalogState::Missing);
        // Retain drops others.
        cache.retain(&["nope".to_owned()]).unwrap();
        assert_eq!(
            cache.read_state("abc123", 86_400, now),
            CatalogState::Missing
        );
    }

    #[test]
    fn corrupt_cache_reads_empty() {
        let dir = tempfile::tempdir().unwrap();
        let cache = ModelsCache::new(&Dirs::under(dir.path()));
        std::fs::create_dir_all(dir.path().join("cache")).unwrap();
        std::fs::write(cache.path(), "{{{").unwrap();
        assert_eq!(cache.load().accounts.len(), 0);
        assert_eq!(cache.read_state("x", 60, Utc::now()), CatalogState::Missing);
    }
}
