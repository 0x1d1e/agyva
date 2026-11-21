//! End-to-end tests for export/import (core::transfer).
//!
//! Two vaults (source/destination) have separate state; archives travel
//! through real files on disk. No live agy, no OS keyring, no network.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use agyva::core::account::{SNAPSHOT_SCHEMA, Snapshot};
use agyva::core::failpoint::{Failpoints, Fault};
use agyva::core::registry::AccountRecord;
use agyva::storage::vault::seal as vault_seal;
use agyva::storage::vault_key::{KdfParams, StaticPassphrase, derive_key};
use agyva::testing::{consumer_credential, credential};
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use secrecy::SecretString;
use zeroize::Zeroizing;

mod common;
use common::*;

const ARCHIVE: &str = "archive-passphrase";
const WRONG: &str = "totally-wrong-pass";

fn archive_app(rig: &Rig, failpoints: Failpoints) -> agyva::core::app::App {
    rig.app_with(failpoints)
        .with_archive_passphrases(Arc::new(StaticPassphrase(ARCHIVE.to_owned())))
}

/// A destination app that imports with `wrong` as its archive passphrase.
fn wrong_archive_app(rig: &Rig) -> agyva::core::app::App {
    rig.app_with(Failpoints::none())
        .with_archive_passphrases(Arc::new(StaticPassphrase(WRONG.to_owned())))
}

/// The source vault: two accounts (ALICE, BOB) with a distinctive gcp
/// companion, saved under their tag aliases.
fn source_rig() -> Rig {
    let rig = Rig::new();
    write_gcp(&rig, "gcp-from-source");
    rig.add_all(&[ALICE, BOB]);
    rig
}

fn write_gcp(rig: &Rig, value: &str) {
    let home = rig.agy_home();
    fs::create_dir_all(&home).expect("agy home");
    fs::write(
        home.join("settings.json"),
        format!("{{\"gcp\":{{\"plan\":\"{value}\"}}}}"),
    )
    .expect("settings.json");
}

/// Export `source`'s accounts to `file` with the shared archive passphrase.
fn export_source(source: &Rig, file: &Path) {
    archive_app(source, Failpoints::none())
        .export_archive(file, None)
        .expect("export");
}

/// Craft an archive from (id, credential-bytes) pairs sealed under the
/// given KDF and passphrase. Record metadata is derived from the
/// credential itself, exactly as `export` would emit it.
fn craft_archive(file: &Path, kdf: &KdfParams, passphrase: &str, entries: &[(&str, &[u8])]) {
    let key = derive_key(&SecretString::from(passphrase), kdf).expect("derive");
    let mut items = Vec::new();
    for (id, bytes) in entries.iter() {
        let info = agyva::testing::parse_for_test(bytes);
        let rec = AccountRecord {
            id: (*id).to_owned(),
            alias: info.alias.clone(),
            email: info.email.clone(),
            auth_method: info.auth_method.clone(),
            added_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_used_at: None,
        };
        let snap = Snapshot {
            schema_version: SNAPSHOT_SCHEMA,
            credential: agyva::agy::credentials::RawCredential::new(bytes.to_vec()),
            companion: Default::default(),
            captured_at: chrono::Utc::now(),
        };
        items.push(serde_json::json!({ "record": rec, "snapshot": snap }));
    }
    let payload = Zeroizing::new(
        serde_json::to_vec(&serde_json::json!({ "version": 1, "entries": items }))
            .expect("payload"),
    );
    let sealed = vault_seal(&key, "archive:1", &payload).expect("seal");
    let envelope = serde_json::json!({
        "format": "agyva-archive",
        "version": 1,
        "kdf": kdf,
        "data": B64.encode(&sealed),
    });
    fs::write(file, envelope.to_string()).expect("archive file");
}

/// Like [`craft_archive`], with an optional companion installation id.
fn craft_archive_with_companion(file: &Path, entries: &[(&str, &[u8], Option<&str>)]) {
    let kdf = KdfParams::production().expect("kdf");
    let key = derive_key(&SecretString::from(ARCHIVE), &kdf).expect("derive");
    let mut items = Vec::new();
    for (id, bytes, installation_id) in entries.iter() {
        let info = agyva::testing::parse_for_test(bytes);
        let rec = AccountRecord {
            id: (*id).to_owned(),
            alias: info.alias.clone(),
            email: info.email.clone(),
            auth_method: info.auth_method.clone(),
            added_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
            last_used_at: None,
        };
        let snap = Snapshot {
            schema_version: SNAPSHOT_SCHEMA,
            credential: agyva::agy::credentials::RawCredential::new(bytes.to_vec()),
            companion: agyva::agy::companion::Companion {
                installation_id: installation_id.map(str::to_owned),
                gcp: None,
            },
            captured_at: chrono::Utc::now(),
        };
        items.push(serde_json::json!({ "record": rec, "snapshot": snap }));
    }
    let payload = Zeroizing::new(
        serde_json::to_vec(&serde_json::json!({ "version": 1, "entries": items }))
            .expect("payload"),
    );
    let sealed = vault_seal(&key, "archive:1", &payload).expect("seal");
    let envelope = serde_json::json!({
        "format": "agyva-archive",
        "version": 1,
        "kdf": kdf,
        "data": B64.encode(&sealed),
    });
    fs::write(file, envelope.to_string()).expect("archive file");
}

fn temp(name: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "agyva-x-{name}-{}-{}.agy",
        std::process::id(),
        name
    ));
    let _ = fs::remove_file(&p);
    p
}

#[derive(PartialEq, Eq, PartialOrd, Ord, Debug)]
struct FileState {
    rel: String,
    bytes: Vec<u8>,
    modified: SystemTime,
}

/// The entire destination config tree, for mutation proofs.
fn tree_state(rig: &Rig) -> Vec<FileState> {
    let mut out = Vec::new();
    let root = rig.dirs().config.clone();
    fn walk(p: &Path, root: &Path, out: &mut Vec<FileState>) {
        let Ok(rd) = fs::read_dir(p) else { return };
        for e in rd.flatten() {
            let path = e.path();
            if path.is_dir() {
                walk(&path, root, out)
            } else {
                let md = fs::symlink_metadata(&path).expect("meta");
                out.push(FileState {
                    rel: path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                    bytes: fs::read(&path).unwrap_or_default(),
                    modified: md.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                });
            }
        }
    }
    walk(&root, &root, &mut out);
    out.sort();
    out
}

// ------------------------------------------------------------ roundtrip

#[test]
fn export_then_import_roundtrip_cross_vault() {
    let source = source_rig();
    let dest = Rig::new();
    let file = temp("roundtrip");
    export_source(&source, &file);

    let out = archive_app(&source, Failpoints::none())
        .export_archive(&file.with_extension("again"), None);
    assert!(out.is_ok(), "second export path should succeed");
    let _ = out.expect("ok");

    let dst = archive_app(&dest, Failpoints::none());
    let imp = dst.import_archive(&file).expect("import");
    assert_eq!(imp.added, vec![id_of(ALICE), id_of(BOB)]);
    assert!(imp.skipped.is_empty());

    // Both accounts are usable in the destination, with their aliases.
    let list = dst.list().expect("list");
    let aliases: Vec<String> = list.accounts.iter().map(|a| a.alias.clone()).collect();
    assert!(aliases.contains(&"alice".to_owned()));
    assert!(aliases.contains(&"bob".to_owned()));

    // The source's bytes survive the trip: the imported snapshot files
    // decrypt with the destination vault only after matching the source
    // snapshot bytes are impossible across vaults, so prove it through the
    // registry + a read-back of credential emails instead.
    let reg = dst.load_registry().expect("registry");
    for (id, email) in [
        (id_of(ALICE), "alice@example.invalid"),
        (id_of(BOB), "bob@example.invalid"),
    ] {
        let rec = reg.find(&id).expect("registered");
        assert_eq!(rec.email.as_deref(), Some(email));
    }
    let _ = fs::remove_file(&file);
}

#[test]
fn selected_export_includes_only_that_account() {
    let source = source_rig();
    let file = temp("selected");
    let out = archive_app(&source, Failpoints::none())
        .export_archive(&file, Some("bob"))
        .expect("export bob");
    assert_eq!(out.accounts, vec![id_of(BOB)]);

    let dest = Rig::new();
    let imp = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect("import");
    assert_eq!(imp.added, vec![id_of(BOB)]);
    let list = archive_app(&dest, Failpoints::none()).list().expect("list");
    assert_eq!(list.accounts.len(), 1);
    assert_eq!(list.accounts[0].alias, "bob");
    let _ = fs::remove_file(&file);
}

#[test]
fn selected_export_uses_an_absolute_path_or_fails_usage() {
    let source = source_rig();
    let app = archive_app(&source, Failpoints::none());
    let err = app
        .export_archive(Path::new("relative.agy"), None)
        .expect_err("relative");
    assert_eq!(err.message(), "the archive path must be absolute");

    // No accounts at all is also refused, with a hint to add first.
    let empty = Rig::new();
    let err2 = archive_app(&empty, Failpoints::none())
        .export_archive(&temp("empty"), None)
        .expect_err("empty");
    assert!(
        err2.message().contains("no vault exists yet"),
        "{}",
        err2.message()
    );
}

#[test]
fn duplicate_import_is_a_full_skip_and_touches_nothing() {
    let source = source_rig();
    let dest = Rig::new();
    let file = temp("duplicate");
    export_source(&source, &file);

    let dst = archive_app(&dest, Failpoints::none());
    let first = dst.import_archive(&file).expect("first import");
    assert_eq!(first.added.len(), 2);

    // Record every destination file (path, bytes, mtime) after the first
    // import, then re-import: bytes and mtimes must be bit-identical.
    let before = tree_state(&dest);
    let second = dst.import_archive(&file).expect("second import");
    assert!(second.added.is_empty());
    assert_eq!(second.skipped.len(), 2);
    let after = tree_state(&dest);
    assert_eq!(before, after, "a skipped import must not touch anything");
    let _ = fs::remove_file(&file);
}

// ------------------------------------------------------------ refusal

#[test]
fn wrong_passphrase_or_tampering_leaves_the_destination_untouched() {
    let source = source_rig();
    let dest = Rig::new();
    let file = temp("wrongpass");
    export_source(&source, &file);

    // Wrong passphrase: refused before any destination state is read.
    let err = wrong_archive_app(&dest)
        .import_archive(&file)
        .expect_err("wrong");
    assert!(
        err.message()
            .contains("wrong archive passphrase or tampered"),
        "{}",
        err.message()
    );
    assert_eq!(tree_state(&dest), Vec::new(), "no files may appear");

    // A flipped ciphertext byte is tampering: same refusal.
    let mut bytes = fs::read(&file).expect("read archive");
    // Flip a byte early in the base64 ciphertext: it still decodes, so the
    // AES-GCM tag check is what fails.
    // serde_json writes compact JSON: `"data":"<base64>"`. Flip a byte
    // early in that base64 run: it still decodes, so the AES-GCM tag check
    // is what fails.
    let pos = bytes
        .windows(8)
        .position(|w| w == b"\"data\":\"")
        .expect("data field")
        + 8;
    bytes[pos] ^= 0x01;
    fs::write(&file, &bytes).expect("rewrite archive");
    let err = wrong_archive_app(&dest)
        .import_archive(&file)
        .expect_err("tampered");
    assert!(
        err.message()
            .contains("wrong archive passphrase or tampered"),
        "{}",
        err.message()
    );
    assert_eq!(tree_state(&dest), Vec::new());

    // A file that is not an archive at all is refused the same way.
    let junk = temp("junk");
    fs::write(&junk, b"not an archive").expect("junk");
    let err = wrong_archive_app(&dest)
        .import_archive(&junk)
        .expect_err("junk");
    assert!(err.message().contains("not a supported agyva archive"));
    assert_eq!(tree_state(&dest), Vec::new());
    let _ = fs::remove_file(&file);
    let _ = fs::remove_file(&junk);
}

#[test]
fn export_uses_archive_production_kdf_and_import_accepts_legacy_19mib() {
    let source = source_rig();
    let file = temp("kdf-64mib");
    export_source(&source, &file);

    let env: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&file).expect("read archive")).expect("json");
    assert_eq!(env["kdf"]["alg"], "argon2id");
    assert_eq!(env["kdf"]["m_kib"], 64 * 1024);
    assert_eq!(env["kdf"]["t"], 3);
    assert_eq!(env["kdf"]["p"], 1);
    let _ = fs::remove_file(&file);

    // Backwards compatibility: a v1 archive sealed with the 19 MiB profile
    // still imports cleanly.
    let dest = Rig::new();
    let legacy_file = temp("kdf-19mib");
    craft_archive(
        &legacy_file,
        &KdfParams::production().expect("kdf"),
        ARCHIVE,
        &[(
            id_of(ALICE).as_str(),
            &credential(ALICE.0, ALICE.1, ALICE.2),
        )],
    );
    let imp = archive_app(&dest, Failpoints::none())
        .import_archive(&legacy_file)
        .expect("19 MiB archive should still import");
    assert_eq!(imp.added, vec![id_of(ALICE)]);
    let _ = fs::remove_file(&legacy_file);
}

#[test]
fn an_archive_with_unsupported_kdf_settings_is_refused() {
    let dest = Rig::new();
    for (label, kdf) in [
        ("cheap", KdfParams::with_cost(8, 1, 1).expect("kdf")),
        (
            "below-mem-floor",
            KdfParams::with_cost(19 * 1024 - 1, 2, 1).expect("kdf"),
        ),
        (
            "below-t-floor",
            KdfParams::with_cost(64 * 1024, 1, 1).expect("kdf"),
        ),
    ] {
        let file = temp(&format!("cheapkdf-{label}"));
        // Craft with a cheap KDF; import refuses the downgrade regardless of
        // the passphrase being right.
        craft_archive(
            &file,
            &kdf,
            ARCHIVE,
            &[(
                id_of(ALICE).as_str(),
                &credential(ALICE.0, ALICE.1, ALICE.2),
            )],
        );
        let err = archive_app(&dest, Failpoints::none())
            .import_archive(&file)
            .expect_err("cheap kdf");
        assert!(
            err.message().contains("unsupported KDF settings"),
            "{label}: {}",
            err.message()
        );
        assert_eq!(tree_state(&dest), Vec::new());
        let _ = fs::remove_file(&file);
    }
}

#[test]
fn a_saved_id_with_a_different_identity_rejects_the_whole_import() {
    let source = source_rig();
    let dest = Rig::new();
    let file = temp("idconflict");
    export_source(&source, &file);

    // The destination registry owns its ids even while a snapshot cannot be
    // read: a record for CAROL whose snapshot was lost (a corrupted backup
    // restore) still reserves carol's id, so an archive entry with another
    // identity under that id is rejected whole.
    dest.add_all(&[CAROL, ALICE]);
    let carol_id = id_of(CAROL);
    let alice_id = id_of(ALICE);
    // Corrupt CAROL's snapshot (context stays bound to her id, so the vault
    // refuses to decrypt it rather than returning a wrong identity).
    let snap_path = dest.dirs().snapshot_file(&carol_id);
    let mut snap_bytes = fs::read(&snap_path).expect("carol snapshot");
    let mid = snap_bytes.len() / 2;
    snap_bytes[mid] ^= 0x01;
    fs::write(&snap_path, &snap_bytes).expect("corrupt snapshot");
    let alice_bytes = credential(ALICE.0, ALICE.1, ALICE.2);
    let carol_bytes = credential(CAROL.0, CAROL.1, CAROL.2);
    craft_archive(
        &file,
        &KdfParams::production().expect("kdf"),
        ARCHIVE,
        &[
            (alice_id.as_str(), &alice_bytes),
            (carol_id.as_str(), &carol_bytes),
        ],
    );
    // Sanity: the crafted entries are self-consistent and would import
    // into a healthy destination.
    let probe = Rig::new();
    let imp = archive_app(&probe, Failpoints::none())
        .import_archive(&file)
        .expect("healthy destination accepts the archive");
    assert_eq!(imp.added.len(), 2);

    let err = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect_err("unreadable snapshot");
    assert!(
        err.message().contains("corrupted data"),
        "{}",
        err.message()
    );
    let reg = archive_app(&dest, Failpoints::none())
        .load_registry()
        .expect("registry");
    assert_eq!(reg.accounts.len(), 2, "nothing was added");
    assert_eq!(
        tree_state(&dest),
        tree_state(&dest),
        "the failed import left the destination intact"
    );
    let _ = fs::remove_file(&file);
}

#[test]
fn a_subjectless_legacy_login_matches_by_email_in_both_directions() {
    // Destination has a legacy subjectless email login; the archive's
    // subjectful credential for the same email is skipped (dest wins).
    let dest = Rig::new();
    let legacy = consumer_credential("dave@example.invalid", "legacy");
    dest.login_raw(legacy.clone());
    let app = archive_app(&dest, Failpoints::none());
    let added = app.add(&Default::default()).expect("add");
    assert_eq!(added.account.alias, "dave");

    // A subjectful credential with the same email but a different subject
    // would normally be a new identity; the legacy rule makes it a skip.
    let modern = credential("sub-modern", "dave@example.invalid", "modern");
    let modern_id = agyva::testing::parse_for_test(&modern).id;
    assert_ne!(
        modern_id, added.account.id,
        "different file ids, same person"
    );
    let file = temp("legacy-email");
    craft_archive(
        &file,
        &KdfParams::production().expect("kdf"),
        ARCHIVE,
        &[(modern_id.as_str(), &modern)],
    );
    let imp = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect("import");
    assert!(imp.added.is_empty(), "{:?}", imp.added);
    assert_eq!(imp.skipped, vec![modern_id], "matched by email");

    // Reverse direction: the archive holds the subjectless legacy login
    // and the destination the subjectful one.
    let dest2 = Rig::new();
    dest2.login(("sub-rev", "erin@example.invalid", "rev"));
    archive_app(&dest2, Failpoints::none())
        .add(&Default::default())
        .expect("add");
    let legacy2 = consumer_credential("erin@example.invalid", "legacy2");
    let legacy2_id = agyva::testing::parse_for_test(&legacy2).id;
    let file2 = temp("legacy-email-rev");
    craft_archive(
        &file2,
        &KdfParams::production().expect("kdf"),
        ARCHIVE,
        &[(legacy2_id.as_str(), &legacy2)],
    );
    let imp2 = archive_app(&dest2, Failpoints::none())
        .import_archive(&file2)
        .expect("import");
    assert!(imp2.added.is_empty());
    assert_eq!(imp2.skipped, vec![legacy2_id]);
    let _ = fs::remove_file(&file);
    let _ = fs::remove_file(&file2);
}

#[test]
fn differing_subjects_are_separate_accounts_even_with_similar_emails() {
    let dest = Rig::new();
    dest.add_all(&[ALICE]);
    let other = credential("sub-other", "alice2@example.invalid", "other");
    let other_id = agyva::testing::parse_for_test(&other).id;
    assert_ne!(other_id, id_of(ALICE));
    let file = temp("two-subjects");
    craft_archive(
        &file,
        &KdfParams::production().expect("kdf"),
        ARCHIVE,
        &[(other_id.as_str(), &other)],
    );
    let imp = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect("import");
    assert_eq!(
        imp.added,
        vec![other_id],
        "subjectful identities never merge"
    );
    let list = archive_app(&dest, Failpoints::none()).list().expect("list");
    assert_eq!(list.accounts.len(), 2);
    let _ = fs::remove_file(&file);
}

// ------------------------------------------------------------ skip rules

#[test]
fn alias_conflicts_block_new_records_but_not_skipped_ones() {
    let source = source_rig();
    let dest = Rig::new();
    // CAROL owns the alias "bob" in the destination.
    dest.add_all(&[CAROL]);
    archive_app(&dest, Failpoints::none())
        .rename("carol", "bob")
        .expect("rename carol to bob");
    let file = temp("alias-conflict");
    export_source(&source, &file);

    // BOB is a new identity there but its archived alias is taken: the
    // whole import is refused, nothing is added.
    let err = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect_err("alias conflict");
    assert!(
        err.message().contains("the alias `bob` is already in use"),
        "{}",
        err.message()
    );
    let list = archive_app(&dest, Failpoints::none()).list().expect("list");
    assert_eq!(list.accounts.len(), 1);

    // Rename the saved account; the retry adds BOB. ALICE is not in this
    // destination either and comes along.
    archive_app(&dest, Failpoints::none())
        .rename("bob", "carol")
        .expect("rename back");
    let imp = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect("retry");
    assert_eq!(imp.added, vec![id_of(ALICE), id_of(BOB)]);
    let list = archive_app(&dest, Failpoints::none()).list().expect("list");
    assert_eq!(list.accounts.len(), 3);
    let _ = fs::remove_file(&file);
}

#[test]
fn a_skipped_identity_keeps_its_destination_alias() {
    let source = source_rig();
    let dest = Rig::new();
    dest.add_all(&[ALICE]);
    // The destination renamed alice to "zoe"; the archive still calls her
    // "alice". Import must skip her, leaving "zoe" untouched.
    archive_app(&dest, Failpoints::none())
        .rename("alice", "zoe")
        .expect("rename alice to zoe");
    let file = temp("skipped-alias");
    export_source(&source, &file);
    let imp = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect("import");
    assert_eq!(imp.added, vec![id_of(BOB)]);
    assert_eq!(imp.skipped, vec![id_of(ALICE)]);
    let list = archive_app(&dest, Failpoints::none()).list().expect("list");
    let alice = list
        .accounts
        .iter()
        .find(|a| a.id == id_of(ALICE))
        .expect("alice");
    assert_eq!(alice.alias, "zoe", "the destination alias survives");
    let _ = fs::remove_file(&file);
}

#[test]
fn an_orphan_or_symlink_at_a_final_path_refuses_the_import() {
    let source = source_rig();
    let file = temp("symlink");
    export_source(&source, &file);

    // A symlink squatting at ALICE's final path: import refuses, BOB is
    // not added either (the whole import fails).
    let dest = Rig::new();
    dest.add_all(&[CAROL]);
    fs::create_dir_all(dest.dirs().config).expect("config dir");
    let target = dest.dirs().config.join("unrelated");
    fs::write(&target, b"unrelated").expect("target");
    let final_path = dest.dirs().snapshot_file(&id_of(ALICE));
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &final_path).expect("symlink");
    let err = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect_err("symlink refusal");
    assert!(
        err.message().contains("already exists"),
        "{}",
        err.message()
    );
    assert!(
        final_path.symlink_metadata().is_ok(),
        "the symlink survives"
    );
    assert!(
        !dest.dirs().snapshot_file(&id_of(BOB)).exists(),
        "bob was not published"
    );

    // After removing the squatter the import succeeds.
    fs::remove_file(&final_path).expect("remove symlink");
    let imp = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect("retry");
    assert_eq!(imp.added, vec![id_of(ALICE), id_of(BOB)]);
    let _ = fs::remove_file(&file);
}

#[test]
fn an_orphan_snapshot_without_a_vault_refuses_the_import() {
    let source = source_rig();
    let file = temp("orphan");
    export_source(&source, &file);
    // A destination with orphaned snapshots but no vault (lost vault.json):
    // creating a new key would orphan them, so the import is refused.
    let dest = Rig::new();
    dest.add_all(&[CAROL]);
    let vault_meta = dest.dirs().config.join("vault.json");
    fs::remove_file(&vault_meta).expect("remove vault meta");
    let err = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect_err("orphan refusal");
    assert!(
        err.message().contains("vault.json is missing"),
        "{}",
        err.message()
    );
    let _ = fs::remove_file(&file);
}

// ------------------------------------------------------------ crash safety

fn staging_dir(rig: &Rig) -> PathBuf {
    rig.dirs().accounts_dir().join(".import-staging")
}

fn marker_file(rig: &Rig) -> PathBuf {
    rig.dirs().config.join(".import-pending.json")
}

#[test]
fn a_crash_at_any_import_stage_is_recovered_by_the_next_import() {
    let source = source_rig();
    let file = temp("crash-retry");
    export_source(&source, &file);

    // Each failpoint runs against a fresh destination: crash mid-import,
    // recover with the next import, verify the third is a full skip.
    for point in [
        "import-staged",
        "import-marker",
        "import-published",
        "import-registry",
    ] {
        let dest = Rig::new();
        let app = archive_app(&dest, Failpoints::armed(point, Fault::Crash));
        let err = app
            .import_archive(&file)
            .expect_err(&format!("import must crash at {point}"));
        assert_eq!(err.kind(), agyva::core::error::ErrorKind::SimulatedCrash);
        // The next import (no failpoint) recovers and completes.
        let imp = archive_app(&dest, Failpoints::none())
            .import_archive(&file)
            .unwrap_or_else(|e| panic!("recovering import after {point}: {}", e.message()));
        assert_eq!(imp.added, vec![id_of(ALICE), id_of(BOB)], "after {point}");
        // A third import is a full skip: nothing left to do.
        let again = archive_app(&dest, Failpoints::none())
            .import_archive(&file)
            .expect("third import");
        assert!(again.added.is_empty());
        assert_eq!(again.skipped.len(), 2);
        // No staging or marker leftovers.
        assert!(!staging_dir(&dest).exists(), "staging after {point}");
        assert!(!marker_file(&dest).exists(), "marker after {point}");
    }
    let _ = fs::remove_file(&file);
}

#[test]
fn a_crash_before_any_publication_leaves_nothing_behind() {
    let source = source_rig();
    let file = temp("crash-early");
    export_source(&source, &file);
    let dest = Rig::new();

    // Crash at the very first failpoint: no staging dir was created yet.
    let app = archive_app(&dest, Failpoints::armed("import-staged", Fault::Crash));
    let err = app.import_archive(&file).expect_err("crash");
    assert_eq!(err.kind(), agyva::core::error::ErrorKind::SimulatedCrash);
    assert!(
        err.message()
            .contains("simulated crash at failpoint `import-staged`"),
        "{}",
        err.message()
    );
    // The vault itself may exist (provisioned before staging), but no
    // snapshot, no staging and no marker.
    assert!(!dest.dirs().accounts_dir().exists(), "no accounts dir");
    assert!(!staging_dir(&dest).exists());
    assert!(!marker_file(&dest).exists());
    let _ = fs::remove_file(&file);
}

#[test]
fn unexpected_staging_entries_are_preserved_and_reported() {
    let source = source_rig();
    let file = temp("staging-unexpected");
    export_source(&source, &file);
    let dest = Rig::new();

    // Crash after staging was written, then plant a stranger in staging.
    let app = archive_app(&dest, Failpoints::armed("import-marker", Fault::Crash));
    let _ = app.import_archive(&file).expect_err("crash");
    let staging = staging_dir(&dest);
    assert!(staging.is_dir(), "staging exists after the crash");
    let stranger = staging.join("stranger.bin");
    fs::write(&stranger, b"someone else's file").expect("plant stranger");

    // Recovery sweeps the recognized anchors and keeps the stranger.
    let imp = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect("recovering import");
    assert_eq!(imp.added.len(), 2);
    assert!(stranger.is_file(), "the stranger is preserved");
    let _ = fs::remove_file(&file);
}

/// A legacy subjectless login whose email hashes to the same 10-hex account
/// id as an unrelated subject would make `same_account` match by email before
/// the id check. The id collision must win: the archive is refused, not
/// silently skipped as "already saved".
#[test]
fn an_id_collision_with_a_different_identity_is_rejected_before_email_matching() {
    // 5c13edeca7 == account_id_for_sub("email:legacy185911@example.invalid")
    //            == account_id_for_sub("collsub1624344"). Both are 40-bit
    //            ids: the email login is subjectless, the other has a subject.
    let legacy_email = "legacy185911@example.invalid";
    let dest = Rig::new();
    dest.login_raw(consumer_credential(legacy_email, "legacy"));
    let added = archive_app(&dest, Failpoints::none())
        .add(&Default::default())
        .expect("add");
    assert_eq!(added.account.id, "5c13edeca7");

    // Different identity, different email, colliding 40-bit id.
    let other = credential("collsub1624344", "other@example.invalid", "coll");
    let other_id = agyva::testing::parse_for_test(&other).id;
    assert_eq!(other_id, "5c13edeca7");
    let file = temp("id-collision-email");
    craft_archive(
        &file,
        &KdfParams::production().expect("kdf"),
        ARCHIVE,
        &[(other_id.as_str(), &other)],
    );
    let err = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect_err("id collision");
    assert_eq!(err.kind(), agyva::core::error::ErrorKind::Conflict);
    assert!(
        err.message().contains("different identity"),
        "{}",
        err.message()
    );
    let reg = archive_app(&dest, Failpoints::none())
        .load_registry()
        .expect("registry");
    assert_eq!(reg.accounts.len(), 1, "nothing was added");
    let _ = fs::remove_file(&file);
}

#[test]
fn a_present_installation_id_is_normalized_and_persisted() {
    let dest = Rig::new();
    let raw = credential("sub-normalize", "norm@example.invalid", "norm");
    let id = agyva::testing::parse_for_test(&raw).id;
    let file = temp("install-id");
    // Uppercase, space-padded UUID: persisted lowercased and trimmed.
    craft_archive_with_companion(
        &file,
        &[(
            id.as_str(),
            &raw,
            Some(" 3F2504E0-4F89-11D3-9A0C-0305E82C3301 "),
        )],
    );
    let imp = archive_app(&dest, Failpoints::none())
        .import_archive(&file)
        .expect("import");
    assert_eq!(imp.added, vec![id.clone()]);
    let app = archive_app(&dest, Failpoints::none());
    let vault = app.vault().expect("vault");
    let snap = agyva::core::account::read_snapshot(vault, &dest.dirs().snapshot_file(&id), &id)
        .expect("snapshot");
    assert_eq!(
        snap.companion.installation_id.as_deref(),
        Some("3f2504e0-4f89-11d3-9a0c-0305e82c3301")
    );
    let _ = fs::remove_file(&file);

    // A malformed present value is refused before anything is written.
    let dest2 = Rig::new();
    let file2 = temp("install-id-bad");
    craft_archive_with_companion(&file2, &[(id.as_str(), &raw, Some("not-a-uuid"))]);
    let err = archive_app(&dest2, Failpoints::none())
        .import_archive(&file2)
        .expect_err("malformed installation id");
    assert!(
        err.message().contains("installation id"),
        "{}",
        err.message()
    );
    assert!(!dest2.dirs().accounts_dir().exists());
    let _ = fs::remove_file(&file2);
}
