//! `agyva export` / `agyva import`: portable, password-encrypted account
//! archives.
//!
//! An archive is a versioned envelope holding an AES-256-GCM ciphertext
//! (through [`crate::storage::vault::seal`]) under the dedicated context
//! `archive:1`, with its key derived from a *separate* archive passphrase
//! (never the vault passphrase, never the vault key) using the production
//! Argon2id profile. The plaintext payload is only a format version and a
//! list of `{ record, snapshot }` entries; registry `active`/`previous`, the
//! vault key and quota caches are never exported.
//!
//! Import validates the entire archive — including entries that will be
//! skipped — before touching any destination state, then publishes new
//! snapshots through encrypted staging files, a durable ownership marker and
//! no-clobber hard links, committing the registry last. A crash at any point
//! is reconciled by the next writer (see [`App::lock`]).

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::agy::companion;
use crate::agy::credentials::{CredentialInfo, parse_credential};
use crate::core::account::{Snapshot, validate_snapshot_bytes};
use crate::core::app::App;
use crate::core::error::{Error, Result};
use crate::core::ids;
use crate::core::registry::{AccountRecord, Registry};
use crate::core::selector;
use crate::storage::fsutil;
use crate::storage::vault::{Vault, open as vault_open, seal as vault_seal};
use crate::storage::vault_key::{KdfParams, MIN_PASSPHRASE_CHARS, derive_key};

/// AAD context binding every archive envelope.
pub(crate) const ARCHIVE_CONTEXT: &str = "archive:1";
/// Format identifier inside the envelope.
const FORMAT: &str = "agyva-archive";
/// Envelope/payload format version. Version 1 accepts only the production
/// Argon2id profile, so a tampered file cannot choose cheaper parameters.
const VERSION: u32 = 1;
/// File-size and decrypted-payload ceilings.
const MAX_ARCHIVE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PAYLOAD_BYTES: usize = 8 * 1024 * 1024;
/// Per-archive cap on entries (sanity; archives hold a handful of accounts).
const MAX_ENTRIES: usize = 256;

/// Staging area under `accounts/` so a published hard link stays on the same
/// filesystem. Private to an in-flight import.
pub(crate) const STAGING_DIR: &str = ".import-staging";
/// Durable marker naming the ids being added; written before any final
/// snapshot is published and removed after commit/rollback.
pub(crate) const PENDING_MARKER: &str = ".import-pending.json";
/// Schema of the marker file.
const MARKER_VERSION: u32 = 1;

// ------------------------------------------------------------ archive format

/// Unauthenticated envelope: format id, version, KDF parameters for the
/// independent archive key, base64 AES-GCM ciphertext of the payload.
#[derive(Serialize, Deserialize)]
struct Envelope {
    format: String,
    version: u32,
    kdf: KdfParams,
    data: String,
}

/// Decrypted payload: a version and whole-account entries.
#[derive(Serialize, Deserialize)]
struct Payload {
    version: u32,
    entries: Vec<Entry>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    record: AccountRecord,
    snapshot: Snapshot,
}

/// `.import-pending.json`: the ids an in-flight import intends to add.
#[derive(Serialize, Deserialize)]
struct PendingMarker {
    schema_version: u32,
    ids: Vec<String>,
}

// ----------------------------------------------------------------- outcomes

/// What `export` did. Counts and account ids only; never credential data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExportOutcome {
    pub accounts: Vec<String>,
    /// Where the archive was written.
    pub file: PathBuf,
}

/// What `import` did: which accounts were added, which were skipped because
/// the same identity already exists (destination untouched), plus non-fatal
/// cleanup warnings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImportOutcome {
    pub added: Vec<String>,
    pub skipped: Vec<String>,
    pub warnings: Vec<String>,
}

impl App {
    /// `agyva export <FILE> [--account <SELECTOR>]`: write a portable,
    /// password-encrypted archive of every saved account (or one selected
    /// account) to `file`. The destination is never overwritten.
    pub fn export_archive(&self, file: &Path, account: Option<&str>) -> Result<ExportOutcome> {
        if !file.is_absolute() {
            return Err(Error::usage("the archive path must be absolute")
                .hint("pass an explicit path like /home/you/agyva-export.agy"));
        }
        // Reading saved accounts needs the vault; no native login access and
        // no lock (export reads only; snapshots are immutable while saved).
        let reg = self.load_registry()?;
        let vault = self.vault()?;
        let mut entries = Vec::new();
        let mut ids = Vec::new();
        match account {
            Some(selector) => {
                let rec = selector::resolve(&reg, selector)?.clone();
                let snap = self.read_account_snapshot(vault, &rec)?;
                entries.push(Entry {
                    record: rec.clone(),
                    snapshot: snap,
                });
                ids.push(rec.id);
            }
            None => {
                for rec in &reg.accounts {
                    let snap = self.read_account_snapshot(vault, rec)?;
                    entries.push(Entry {
                        record: rec.clone(),
                        snapshot: snap,
                    });
                    ids.push(rec.id.clone());
                }
            }
        }
        if entries.is_empty() {
            return Err(Error::refused("no accounts are saved yet")
                .hint("run `agyva add` first, or pass --account to export one"));
        }

        let passphrase = self.archive_passphrase(true)?;
        let kdf = KdfParams::archive_production()?;
        let key = derive_key(&passphrase, &kdf)?;
        let plain = Zeroizing::new(
            serde_json::to_vec(&Payload {
                version: VERSION,
                entries,
            })
            .map_err(|_| Error::format("cannot encode archive"))?,
        );
        let sealed = vault_seal(&key, ARCHIVE_CONTEXT, &plain)?;
        let envelope = serde_json::to_vec(&Envelope {
            format: FORMAT.into(),
            version: VERSION,
            kdf,
            data: B64.encode(&sealed),
        })
        .map_err(|e| Error::format(e.to_string()))?;
        fsutil::atomic_create(file, &envelope)
            .map_err(|e| Error::io(format!("writing {}", file.display()), e))?;
        Ok(ExportOutcome {
            accounts: ids,
            file: file.to_path_buf(),
        })
    }

    /// `agyva import <FILE>`: add accounts from a portable archive. Existing
    /// same identities are skipped unchanged; nothing already in the
    /// destination is ever modified. See the module docs for the
    /// staging/marker/commit protocol.
    pub fn import_archive(&self, file: &Path) -> Result<ImportOutcome> {
        let entries = self.decrypt_and_validate_archive(file)?;
        let _lock = self.lock()?;
        self.require_no_pending()?;
        let reg = self.load_registry()?;
        let plan = self.plan_import(&reg, &entries)?;

        if plan.new_entries.is_empty() {
            // Everything skipped: no vault is created, no file is written.
            return Ok(ImportOutcome {
                added: Vec::new(),
                skipped: entries.iter().map(|e| e.record.id.clone()).collect(),
                warnings: Vec::new(),
            });
        }

        // Destination vault: unlocked if it exists, created otherwise. A
        // successfully provisioned empty vault may remain after a later
        // storage error; that is accepted (same as `add`).
        let vault = self.vault_or_create()?;

        // Staging: ciphertext only, fsynced with its directory tree.
        self.failpoints.hit("import-staged")?;
        let staging = self.staging_dir();
        self.clear_staging_dir(&staging)?;
        fsutil::create_private_dir(&staging)
            .map_err(|e| Error::io("creating import staging directory", e))?;
        let mut staged: HashMap<String, PathBuf> = HashMap::new();
        for entry in &plan.new_entries {
            let plain = Zeroizing::new(
                serde_json::to_vec(&entry.snapshot)
                    .map_err(|_| Error::format("cannot encode snapshot"))?,
            );
            let sealed = vault.seal(
                &crate::core::account::snapshot_context(&entry.record.id),
                &plain,
            )?;
            let path = staging.join(format!("{}.snap", entry.record.id));
            fs::write(&path, &sealed)
                .map_err(|e| Error::io(format!("writing {}", path.display()), e))?;
            fs::File::open(&path)
                .and_then(|f| f.sync_all())
                .map_err(|e| Error::io(format!("syncing {}", path.display()), e))?;
            staged.insert(entry.record.id.clone(), path);
        }
        fsutil::sync_dir(&staging).map_err(|e| Error::io("syncing staging directory", e))?;
        fsutil::sync_dir(staging.parent().unwrap_or(Path::new(".")))
            .map_err(|e| Error::io("syncing accounts directory", e))?;

        // Durable marker before any final snapshot exists.
        self.failpoints.hit("import-marker")?;
        let marker = serde_json::to_vec(&PendingMarker {
            schema_version: MARKER_VERSION,
            ids: plan
                .new_entries
                .iter()
                .map(|e| e.record.id.clone())
                .collect(),
        })
        .map_err(|e| Error::format(e.to_string()))?;
        fsutil::atomic_write(&self.pending_marker_path(), &marker)
            .map_err(|e| Error::io("writing import marker", e))?;

        // Publish: hard links fail rather than replace an existing name.
        self.failpoints.hit("import-published")?;
        let accounts_dir = self.dirs.accounts_dir();
        fsutil::create_private_dir(&accounts_dir)
            .map_err(|e| Error::io("creating accounts directory", e))?;
        for entry in &plan.new_entries {
            let Some(src) = staged.get(&entry.record.id) else {
                continue;
            };
            let dst = accounts_dir.join(format!("{}.snap", entry.record.id));
            fs::hard_link(src, &dst).map_err(|e| {
                Error::io(format!("publishing {}", dst.display()), e)
                    .hint("a file with this name already exists; remove it or retry")
            })?;
        }
        fsutil::sync_dir(&accounts_dir).map_err(|e| Error::io("syncing accounts directory", e))?;

        // Registry last.
        self.failpoints.hit("import-registry")?;
        let mut reg = reg;
        for entry in &plan.new_entries {
            reg.accounts.push(entry.record.clone());
        }
        match self.save_registry(&reg) {
            Ok(()) => {}
            Err(e) => {
                // The save may have renamed successfully and only failed its
                // directory fsync: reread before deciding what happened.
                let now = self.load_registry();
                let committed = now.is_ok_and(|r| {
                    plan.new_entries
                        .iter()
                        .all(|e| r.find(&e.record.id).is_some())
                });
                if !committed {
                    return Err(self.rollback_import(e));
                }
                tracing::warn!(error = %e.message(), "registry save reported an error after commit");
            }
        }

        // Commit is durable. Cleanup failure is a warning, not an error.
        let mut warnings = Vec::new();
        match self.reconcile_import() {
            Ok(_) => {}
            Err(e) if e.kind() == crate::core::error::ErrorKind::SimulatedCrash => return Err(e),
            Err(e) => {
                tracing::warn!(error = %e.message(), "post-import cleanup failed");
                warnings.push(format!(
                    "the import completed, but cleanup failed ({}); the next command retries it",
                    e.message()
                ));
            }
        }
        Ok(ImportOutcome {
            added: plan
                .new_entries
                .iter()
                .map(|e| e.record.id.clone())
                .collect(),
            skipped: plan.skipped_ids,
            warnings,
        })
    }

    // ------------------------------------------------------------ internals

    fn read_account_snapshot(&self, vault: &Vault, rec: &AccountRecord) -> Result<Snapshot> {
        crate::core::account::validated_snapshot(vault, &self.dirs.snapshot_file(&rec.id), &rec.id)
    }

    /// The passphrase an archive is encrypted with, and the minimum-length
    /// rule when it is being created (export).
    fn archive_passphrase(&self, confirm: bool) -> Result<SecretString> {
        let p = self.archive_passphrases.passphrase(confirm)?;
        if confirm && p.expose_secret().chars().count() < MIN_PASSPHRASE_CHARS {
            return Err(Error::vault(format!(
                "the archive passphrase is too short (minimum {MIN_PASSPHRASE_CHARS} characters)"
            ))
            .hint("choose a longer passphrase; the archive is only as strong as it is"));
        }
        Ok(p)
    }

    fn decrypt_and_validate_archive(&self, file: &Path) -> Result<Vec<Entry>> {
        let meta =
            fs::metadata(file).map_err(|e| Error::io(format!("reading {}", file.display()), e))?;
        if !meta.is_file() {
            return Err(Error::format(format!(
                "{} is not a regular file",
                file.display()
            )));
        }
        if meta.len() > MAX_ARCHIVE_BYTES {
            return Err(Error::format(format!(
                "{} is larger than the supported archive size",
                file.display()
            )));
        }
        let bytes = Zeroizing::new(
            fs::read(file).map_err(|e| Error::io(format!("reading {}", file.display()), e))?,
        );
        let envelope: Envelope = serde_json::from_slice(&bytes)
            .map_err(|_| Error::format("the file is not a supported agyva archive"))?;
        if envelope.format != FORMAT {
            return Err(Error::format("the file is not a supported agyva archive"));
        }
        if envelope.version != VERSION {
            return Err(Error::format(format!(
                "archive format version {} is not supported",
                envelope.version
            ))
            .hint("upgrade agyva"));
        }
        // Only the production profile is accepted, whatever the file says.
        if let Some(why) = production_profile_error(&envelope.kdf) {
            return Err(Error::format(format!(
                "the archive uses unsupported KDF settings ({why})"
            )));
        }
        envelope.kdf.validate()?;
        let sealed = B64
            .decode(&envelope.data)
            .map_err(|_| Error::format("the archive is malformed"))?;
        let passphrase = self.archive_passphrase(false)?;
        let key = derive_key(&passphrase, &envelope.kdf)?;
        let plain = vault_open(&key, ARCHIVE_CONTEXT, &sealed).map_err(|_| {
            Error::vault("wrong archive passphrase or tampered archive").hint(
                "check the archive passphrase (AGYVA_ARCHIVE_PASSPHRASE or the prompt) \
                 and that the file was not modified; a correct passphrase still fails \
                 on a damaged archive",
            )
        })?;
        if plain.len() > MAX_PAYLOAD_BYTES {
            return Err(Error::format("the archive payload is too large"));
        }
        let mut payload: Payload = serde_json::from_slice(&plain).map_err(|_| {
            Error::vault("wrong archive passphrase or tampered archive").hint(
                "check the archive passphrase (AGYVA_ARCHIVE_PASSPHRASE or the prompt) \
                 and that the file was not modified; a correct passphrase still fails \
                 on a damaged archive",
            )
        })?;
        if payload.version != VERSION {
            return Err(Error::format("unsupported archive payload version"));
        }
        if payload.entries.len() > MAX_ENTRIES {
            return Err(Error::format("the archive holds too many accounts"));
        }
        validate_entries(&mut payload.entries)?;
        Ok(payload.entries)
    }

    /// Build the import plan under the lock: which entries are new, which
    /// are skipped, and every conflict that must fail the whole import.
    fn plan_import<'a>(&self, reg: &Registry, entries: &'a [Entry]) -> Result<ImportPlan<'a>> {
        // 1. Destination identity map from snapshots (not registry labels).
        let vault_exists = self.key_provider().load_meta()?.is_some();
        let mut dest: Vec<(AccountRecord, CredentialInfo)> = Vec::new();
        if vault_exists {
            let unlocked = self.vault()?;
            for rec in &reg.accounts {
                let snap = self.read_account_snapshot(unlocked, rec)?;
                let info = parse_credential(snap.credential.as_bytes())?;
                dest.push((rec.clone(), info));
            }
        }

        let mut plan = ImportPlan::default();
        // In-archive duplicate detection.
        let mut seen_ids: Vec<&str> = Vec::new();
        let mut seen_aliases: Vec<&str> = Vec::new();
        for entry in entries {
            let rec = &entry.record;
            if seen_ids.contains(&rec.id.as_str()) {
                return Err(Error::format(format!(
                    "the archive holds a duplicate account id {}",
                    rec.id
                )));
            }
            if seen_aliases
                .iter()
                .any(|a| a.eq_ignore_ascii_case(&rec.alias))
            {
                return Err(Error::format(format!(
                    "the archive holds a duplicate alias `{}`",
                    rec.alias
                )));
            }
            seen_ids.push(rec.id.as_str());
            seen_aliases.push(rec.alias.as_str());

            let info = parse_credential(entry.snapshot.credential.as_bytes())?;
            // A stored-id match is decided on the full identity first. The
            // symmetric legacy email fallback must not paper over a 40-bit
            // collision with a different full identity.
            if let Some(i) = dest.iter().position(|(d, _)| d.id == rec.id) {
                let (_, dinfo) = &dest[i];
                if !info.same_identity(dinfo) {
                    return Err(Error::conflict(format!(
                        "account id {} is already saved with a different identity",
                        rec.id
                    ))
                    .hint("remove one of the two accounts first, then import again"));
                }
                plan.skipped_ids.push(rec.id.clone());
                continue;
            }
            // Different stored id: a full identity match skips (already
            // saved under another id); so does the subjectless email match.
            let mut matched_dest: Option<usize> = None;
            for (i, (_, dinfo)) in dest.iter().enumerate() {
                if same_account(&info, dinfo) {
                    if matched_dest.is_some() {
                        return Err(Error::conflict(format!(
                            "the archive entry {} matches more than one saved account",
                            rec.alias
                        )));
                    }
                    matched_dest = Some(i);
                }
            }
            match matched_dest {
                Some(_) => plan.skipped_ids.push(rec.id.clone()),
                None => plan.new_entries.push(entry),
            }
        }

        // 2. Alias conflicts apply only to NEW records (a skipped identity
        //    keeps the destination alias; its unused source alias belongs to
        //    whoever owns it in the destination).
        for entry in &plan.new_entries {
            if dest
                .iter()
                .any(|(d, _)| d.alias.eq_ignore_ascii_case(&entry.record.alias))
                || plan.new_entries.iter().any(|other| {
                    other.record.id != entry.record.id
                        && other.record.alias.eq_ignore_ascii_case(&entry.record.alias)
                })
                || entries
                    .iter()
                    .filter(|e| !plan.skipped_ids.contains(&e.record.id))
                    .any(|e| {
                        e.record.id != entry.record.id
                            && e.record.alias.eq_ignore_ascii_case(&entry.record.alias)
                    })
            {
                return Err(Error::conflict(format!(
                    "the alias `{}` is already in use",
                    entry.record.alias
                ))
                .hint("rename the saved account or the archived one, then import again"));
            }
        }

        // 3. Orphan/symlink final-path refusal, only for new accounts and
        //    only when no vault exists (consistent with vault provisioning).
        if !vault_exists && !plan.new_entries.is_empty() {
            let orphans = fsutil::list_with_extension(&self.dirs.accounts_dir(), "snap")
                .map_err(|e| Error::io("scanning saved accounts", e))?;
            if !orphans.is_empty() {
                return Err(
                    Error::vault("saved accounts exist but vault.json is missing").hint(
                        "restore vault.json from a backup; creating a new key would orphan them",
                    ),
                );
            }
        }
        for entry in &plan.new_entries {
            let final_path = self.dirs.snapshot_file(&entry.record.id);
            match fs::symlink_metadata(&final_path) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io(format!("checking {}", final_path.display()), e)),
                Ok(_) => {
                    return Err(Error::conflict(format!(
                        "{} already exists",
                        final_path.display()
                    ))
                    .hint("remove the unrelated file, or import into a clean agyva state"));
                }
            }
        }
        Ok(plan)
    }

    /// Finish or undo an interrupted import, using only filesystem metadata
    /// and the registry (never the vault, agy or the native store). Called
    /// with the state lock held by every mutating command through
    /// [`App::lock`].
    pub(crate) fn reconcile_import(&self) -> Result<()> {
        let marker_path = self.pending_marker_path();
        let Some(raw) = fsutil::read_optional(&marker_path)
            .map_err(|e| Error::io("reading import marker", e))?
        else {
            return self.clean_staging();
        };
        let marker: PendingMarker = serde_json::from_slice(&raw)
            .map_err(|e| Error::format(format!("the import marker is corrupt: {e}")))?;
        if marker.schema_version != MARKER_VERSION {
            return Err(Error::format("unsupported import marker schema"));
        }
        for id in &marker.ids {
            ids::require_account_id(id)?;
        }
        let reg = self.load_registry()?;
        let registered: Vec<&String> = marker
            .ids
            .iter()
            .filter(|id| reg.find(id).is_some())
            .collect();

        if registered.is_empty() {
            // Rollback: none of the ids committed. Remove each final path
            // only when it is still the exact inode this import published.
            for id in &marker.ids {
                let staged = self.staging_dir().join(format!("{id}.snap"));
                let final_path = self.dirs.snapshot_file(id);
                if !owned_by_staging(&final_path, &staged) {
                    continue;
                }
                fsutil::remove_durable(&final_path)
                    .map_err(|e| Error::io(format!("removing {}", final_path.display()), e))?;
            }
            fsutil::sync_dir(&self.dirs.accounts_dir())
                .map_err(|e| Error::io("syncing accounts directory", e))?;
        } else if registered.len() == marker.ids.len() {
            // Committed: the registry names every id, so the final snapshots
            // are authoritative. Sync the registry directory (it may have
            // been saved but not synced if a crash interrupted the fsync),
            // then drop recovery evidence.
            if let Some(dir) = self.dirs.registry_file().parent() {
                fsutil::sync_dir(dir).map_err(|e| Error::io("syncing config directory", e))?;
            }
        } else {
            return Err(Error::recovery_required(format!(
                "an interrupted import of {} account(s) could not be classified; recovery evidence is kept",
                marker.ids.len()
            ))
            .hint("inspect accounts/ and registry.json, then run `agyva import` again"));
        }

        // Marker first, then anchors: a crash during cleanup leaves either a
        // recoverable marker with anchors, or orphan staging files with no
        // pending publication (harmlessly swept later).
        fsutil::remove_durable(&marker_path).map_err(|e| Error::io("removing import marker", e))?;
        self.clear_staging_dir(&self.staging_dir())?;
        Ok(())
    }

    /// Best-effort sweep of staging leftovers when no marker exists.
    fn clean_staging(&self) -> Result<()> {
        let staging = self.staging_dir();
        if !staging.is_dir() {
            return Ok(());
        }
        self.clear_staging_dir(&staging)
    }

    /// Remove recognized staging anchors (`<10-hex>.snap` regular files),
    /// preserving and reporting anything unexpected.
    fn clear_staging_dir(&self, staging: &Path) -> Result<()> {
        let entries = match fs::read_dir(staging) {
            Ok(e) => e,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(Error::io("reading import staging directory", e)),
        };
        let mut unexpected = 0;
        for entry in entries {
            let path = entry
                .map_err(|e| Error::io("reading import staging directory", e))?
                .path();
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned());
            let recognized = name
                .as_deref()
                .is_some_and(|n| n.strip_suffix(".snap").is_some_and(ids::is_account_id));
            if recognized {
                if let Err(e) = fs::remove_file(&path)
                    && e.kind() != std::io::ErrorKind::NotFound
                {
                    return Err(Error::io(format!("removing {}", path.display()), e));
                }
            } else {
                unexpected += 1;
            }
        }
        if unexpected > 0 {
            tracing::warn!(count = unexpected, "unexpected entries in import staging");
        }
        if let Err(e) = fs::remove_dir(staging)
            && e.kind() != std::io::ErrorKind::NotFound
            && unexpected == 0
        {
            // Non-empty (unexpected entries preserved) is fine too.
            if e.kind() != std::io::ErrorKind::DirectoryNotEmpty {
                return Err(Error::io("removing import staging directory", e));
            }
        }
        Ok(())
    }

    fn rollback_import(&self, cause: Error) -> Error {
        match self.reconcile_import() {
            Ok(()) => Error::new(
                cause.kind(),
                format!("{}; the import was rolled back", cause.message()),
            )
            .hint("nothing was changed"),
            Err(rb) => Error::recovery_required(format!(
                "the import failed ({}) and automatic rollback also failed ({})",
                cause.message(),
                rb.message()
            )),
        }
    }

    fn staging_dir(&self) -> PathBuf {
        self.dirs.accounts_dir().join(STAGING_DIR)
    }

    fn pending_marker_path(&self) -> PathBuf {
        self.dirs.config.join(PENDING_MARKER)
    }
}

#[derive(Default)]
struct ImportPlan<'a> {
    new_entries: Vec<&'a Entry>,
    skipped_ids: Vec<String>,
}

/// Whether two credentials describe the same person: exact typed identity,
/// or the symmetric legacy email match (equal normalized emails where at
/// least one side has no subject).
fn same_account(a: &CredentialInfo, b: &CredentialInfo) -> bool {
    if a.same_identity(b) {
        return true;
    }
    let (Some(ea), Some(eb)) = (a.normalized_email(), b.normalized_email()) else {
        return false;
    };
    ea == eb && (a.is_subjectless() || b.is_subjectless())
}

/// Validate every entry structurally and authentically — including ones that
/// will be skipped — before any destination state is read or changed.
fn validate_entries(entries: &mut [Entry]) -> Result<()> {
    use crate::core::selector::validate_alias;
    for entry in entries.iter_mut() {
        let rec = &entry.record;
        ids::require_account_id(&rec.id)?;
        validate_alias(&rec.alias)?;
        let snap = &entry.snapshot;
        if snap.schema_version != crate::core::account::SNAPSHOT_SCHEMA {
            return Err(Error::format(format!(
                "snapshot schema {} is not supported",
                snap.schema_version
            ))
            .hint("upgrade agyva"));
        }
        // Full snapshot validation (credential shape, credential-derived id)
        // without a vault: serialize then reuse the shared validator.
        let plain =
            serde_json::to_vec(snap).map_err(|_| Error::format("cannot decode snapshot"))?;
        let snap = validate_snapshot_bytes(&plain, &rec.id)?;
        let info = parse_credential(snap.credential.as_bytes())?;
        // Fail closed on records that disagree with their credential.
        if rec.auth_method != info.auth_method {
            return Err(Error::format(format!(
                "account {} records auth method `{}` but its credential says `{}`",
                rec.id, rec.auth_method, info.auth_method
            )));
        }
        let email_agrees = match (&rec.email, info.email.as_deref()) {
            (None, None) => true,
            (Some(r), Some(c)) => r.to_lowercase() == c.to_lowercase(),
            _ => false,
        };
        if !email_agrees {
            return Err(Error::format(format!(
                "account {} records an email that disagrees with its credential",
                rec.id
            )));
        }
        // Companion: a present installation id must be a UUID and is
        // persisted normalized; gcp stays any bounded JSON value.
        if let Some(raw) = entry.snapshot.companion.installation_id.clone() {
            let normalized = companion::normalize_installation_id(&raw).ok_or_else(|| {
                Error::format(format!(
                    "account {} holds a malformed companion installation id",
                    entry.record.id
                ))
                .hint("export the archive again from the source machine")
            })?;
            entry.snapshot.companion.installation_id = Some(normalized);
        }
        if let Some(v) = &snap.companion.gcp {
            let encoded = serde_json::to_vec(v)
                .map_err(|_| Error::format("companion data cannot be encoded"))?;
            if encoded.len() > 64 * 1024 {
                return Err(Error::format(format!(
                    "account {} holds unbounded companion data",
                    rec.id
                )));
            }
        }
    }
    // Ambiguity inside the archive: two entries must not both match as the
    // same account unless their ids agree (duplicate ids were rejected).
    for (i, a) in entries.iter().enumerate() {
        let ia = parse_credential(a.snapshot.credential.as_bytes())?;
        for b in &entries[i + 1..] {
            let ib = parse_credential(b.snapshot.credential.as_bytes())?;
            if same_account(&ia, &ib) {
                return Err(Error::conflict(format!(
                    "the archive holds two entries for the same identity ({} and {})",
                    a.record.alias, b.record.alias
                )));
            }
        }
    }
    Ok(())
}

/// `None` when the KDF meets the minimum production floor (>= 19 MiB, t >= 2,
/// p >= 1, Argon2id, 16-byte salt) and stays within [`KdfParams::validate`]
/// upper bounds.
fn production_profile_error(kdf: &KdfParams) -> Option<&'static str> {
    if kdf.alg != "argon2id" {
        return Some("algorithm");
    }
    if kdf.m_kib < 19 * 1024 {
        return Some("memory cost");
    }
    if kdf.t < 2 {
        return Some("time cost");
    }
    if kdf.p < 1 {
        return Some("parallelism");
    }
    let Ok(salt) = B64.decode(&kdf.salt) else {
        return Some("salt");
    };
    if salt.len() != 16 {
        return Some("salt length");
    }
    if kdf.validate().is_err() {
        return Some("bounds");
    }
    None
}

/// Whether `final_path` is a regular file whose `(dev, ino)` equals the
/// staged anchor's: proof this import created it. Symlinks are never
/// followed.
fn owned_by_staging(final_path: &Path, staged: &Path) -> bool {
    let Ok(meta) = fs::symlink_metadata(final_path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    let Ok(staged_meta) = fs::symlink_metadata(staged) else {
        return false;
    };
    meta.dev() == staged_meta.dev() && meta.ino() == staged_meta.ino()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_profile_enforces_floor_and_upper_bounds() {
        assert!(production_profile_error(&KdfParams::production().unwrap()).is_none());
        assert!(production_profile_error(&KdfParams::archive_production().unwrap()).is_none());

        let cheap_mem = KdfParams::with_cost(19 * 1024 - 1, 2, 1).unwrap();
        assert_eq!(production_profile_error(&cheap_mem), Some("memory cost"));

        let cheap_t = KdfParams::with_cost(19 * 1024, 1, 1).unwrap();
        assert_eq!(production_profile_error(&cheap_t), Some("time cost"));

        let cheap = KdfParams::with_cost(8, 1, 1).unwrap();
        assert!(production_profile_error(&cheap).is_some());

        let excessive = KdfParams::with_cost(1_048_576, 5, 1).unwrap();
        assert_eq!(production_profile_error(&excessive), Some("bounds"));
    }

    #[test]
    fn marker_rejects_non_hex_ids() {
        let m = PendingMarker {
            schema_version: MARKER_VERSION,
            ids: vec!["../escape".into()],
        };
        let bytes = serde_json::to_vec(&m).unwrap();
        let back: PendingMarker = serde_json::from_slice(&bytes).unwrap();
        assert!(
            back.ids
                .iter()
                .all(|id| ids::require_account_id(id).is_err())
        );
    }
}
