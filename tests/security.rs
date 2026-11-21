mod common;

use std::os::unix::fs::PermissionsExt;

use agyva::agy::credentials::RawCredential;
use agyva::config::VaultKeyMode;
use agyva::core::account::read_snapshot;
use agyva::core::error::ErrorKind;
use agyva::core::failpoint::{Failpoints, Fault};
use agyva::storage::keyring::{SecretStore, StoreErrorKind};
use agyva::storage::vault::{Vault, VaultKey, open, seal};
use agyva::storage::vault_key::{KEY_SERVICE, KEY_USER};
use agyva::testing::{credential, secrets_for};
use common::*;

fn all_secret_strings() -> Vec<String> {
    ["alice", "bob"]
        .iter()
        .flat_map(|t| secrets_for(t))
        .collect()
}

#[test]
fn json_views_and_errors_never_contain_secrets_and_carry_schema_version() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    app.switch("alice", false).unwrap();

    let mut outputs = vec![
        serde_json::to_string(&app.list().unwrap()).unwrap(),
        serde_json::to_string(&app.current().unwrap()).unwrap(),
        serde_json::to_string(&app.status().unwrap()).unwrap(),
        serde_json::to_string(&app.doctor()).unwrap(),
        format!("{:?}", app.list().unwrap()),
        format!("{:?}", app.status().unwrap()),
    ];
    for v in &outputs[..3] {
        let parsed: serde_json::Value = serde_json::from_str(v).unwrap();
        assert!(
            parsed.get("schema_version").is_some(),
            "schema_version missing in {v}"
        );
    }
    let doctor: serde_json::Value = serde_json::from_str(&outputs[3]).unwrap();
    assert!(doctor.get("schema_version").is_some());

    let _ = rig
        .app_with(Failpoints::armed("after-native-write", Fault::Crash))
        .switch("bob", false);
    let app = rig.app();
    outputs.push(format!("{:?}", app.switch("bob", false).unwrap_err()));
    outputs.push(format!("{:?}", app.pending_journal().unwrap()));
    outputs.push(serde_json::to_string(&app.status().unwrap()).unwrap());
    outputs.push(serde_json::to_string(&app.doctor()).unwrap());

    for out in &outputs {
        for secret in all_secret_strings() {
            assert!(!out.contains(&secret), "leaked `{secret}` in {out}");
        }
    }
}

#[test]
fn debug_output_of_secret_types_is_redacted() {
    let raw = RawCredential::new(credential("sub-x", "x@example.invalid", "xx"));
    let shown = format!("{raw:?}");
    assert!(!shown.contains("access-xx") && !shown.contains("refresh-xx"));
    let key = VaultKey::from_bytes([0x5a; 32]);
    let shown = format!("{key:?}");
    assert!(!shown.contains("5a5a") && !shown.contains("90"), "{shown}");
}

#[test]
fn nothing_on_disk_is_plaintext_and_the_key_is_not_stored() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    rig.app().switch("alice", false).unwrap();
    rig.app().switch("bob", false).unwrap(); // leaves checkpoints holding old logins

    let stored = rig
        .keyring
        .get(KEY_SERVICE, KEY_USER)
        .unwrap()
        .expect("key in keyring");
    let stored_text = String::from_utf8_lossy(&stored).into_owned();
    assert!(!stored_text.is_empty());

    let mut snaps = 0;
    for path in rig.all_files() {
        let bytes = std::fs::read(&path).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        for secret in all_secret_strings() {
            assert!(
                !text.contains(&secret),
                "{} holds `{secret}`",
                path.display()
            );
        }
        assert!(
            !text.contains(&stored_text),
            "{} holds the vault key",
            path.display()
        );
        if path.extension().is_some_and(|e| e == "snap" || e == "ckpt") {
            assert_eq!(&bytes[..4], b"AGYV", "{} is an envelope", path.display());
            snaps += 1;
        }
        assert!(
            !path.to_string_lossy().ends_with(".bak"),
            "no backup files: {}",
            path.display()
        );
    }
    assert!(snaps >= 3);
}

#[test]
fn files_are_private() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    rig.app().switch("alice", false).unwrap();
    for path in rig.all_files() {
        if path.starts_with(rig.agy_home()) {
            continue;
        }
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{}", path.display());
        let mut dir = path.parent();
        while let Some(d) = dir.filter(|d| d.starts_with(rig.tmp.path()) && *d != rig.tmp.path()) {
            let mode = std::fs::metadata(d).unwrap().permissions().mode() & 0o777;
            // The test root itself is created by the harness; agyva's own dirs are private.
            if d.parent() != Some(rig.tmp.path())
                || d.ends_with("config")
                || d.ends_with("state")
                || d.ends_with("cache")
            {
                assert_eq!(mode, 0o700, "{}", d.display());
            }
            dir = d.parent();
        }
    }
}

#[test]
fn tampered_swapped_or_wrong_key_snapshots_fail_closed() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    let (a, b) = (id_of(ALICE), id_of(BOB));
    let (pa, pb) = (rig.dirs().snapshot_file(&a), rig.dirs().snapshot_file(&b));

    // Bit flip.
    let original = std::fs::read(&pa).unwrap();
    let mut flipped = original.clone();
    let last = flipped.len() - 1;
    flipped[last] ^= 1;
    std::fs::write(&pa, &flipped).unwrap();
    assert!(
        app.switch("alice", false).is_err(),
        "tampered snapshot must not be applied"
    );
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
    assert!(!rig.app().doctor().healthy);

    // Header version byte changed.
    let mut bad_version = original.clone();
    bad_version[4] = 9;
    std::fs::write(&pa, &bad_version).unwrap();
    assert!(rig.app().switch("alice", false).is_err());

    // Valid ciphertext moved to another account's slot: AAD must reject it.
    std::fs::write(&pa, &original).unwrap();
    std::fs::copy(&pa, &pb).unwrap();
    assert!(rig.app().switch("bob", false).is_err() || rig.live_id() == Some(id_of(BOB)));
    let vault = rig.app().vault().unwrap().clone();
    assert!(
        read_snapshot(&vault, &pb, &b).is_err(),
        "context binds the account id"
    );
    assert!(read_snapshot(&vault, &pa, &a).is_ok());

    // Wrong key.
    let other = Vault::new(VaultKey::from_bytes([1; 32]));
    assert!(read_snapshot(&other, &pa, &a).is_err());
}

#[test]
fn envelope_binds_context_and_never_repeats_nonces() {
    let key = VaultKey::from_bytes([3; 32]);
    let one = seal(&key, "snapshot:a", b"payload").unwrap();
    let two = seal(&key, "snapshot:a", b"payload").unwrap();
    assert_ne!(one, two, "random nonce per seal");
    assert!(open(&key, "snapshot:a", &one).is_ok());
    assert!(open(&key, "snapshot:b", &one).is_err());
}

#[test]
fn passphrase_vault_works_without_a_keyring_and_stores_no_key() {
    let rig = Rig::new();
    rig.keyring.set_failure(Some(StoreErrorKind::Unavailable));
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    app.switch("alice", false).unwrap();

    let meta = std::fs::read_to_string(rig.dirs().vault_meta_file()).unwrap();
    assert!(meta.contains("argon2id"));
    assert!(!meta.contains("synthetic-passphrase"));
    for path in rig.all_files() {
        let text = String::from_utf8_lossy(&std::fs::read(&path).unwrap()).into_owned();
        assert!(!text.contains("synthetic-passphrase"), "{}", path.display());
    }

    // A forced keyring mode with no keyring is a clear error, not a silent downgrade.
    let fresh = Rig::new();
    fresh.keyring.set_failure(Some(StoreErrorKind::Unavailable));
    fresh.login(ALICE);
    let strict = fresh.app().with_vault_mode(VaultKeyMode::Keyring);
    assert!(strict.add(&Default::default()).is_err());
    assert!(
        !fresh.dirs().vault_meta_file().exists(),
        "no vault created on failure"
    );
}

#[test]
fn wrong_passphrase_is_rejected() {
    use agyva::storage::vault_key::StaticPassphrase;
    use std::sync::Arc;
    let rig = Rig::new();
    rig.keyring.set_failure(Some(StoreErrorKind::Unavailable));
    rig.add_all(&[ALICE]);
    let wrong = rig
        .app()
        .with_passphrases(Arc::new(StaticPassphrase("not-the-passphrase".into())));
    let err = wrong
        .list()
        .and_then(|_| wrong.switch("alice", false))
        .unwrap_err();
    assert!(
        matches!(err.kind(), ErrorKind::Vault | ErrorKind::Refused),
        "{err:?}"
    );
}

#[test]
fn rekey_passphrase_rotation_reencrypts_snapshots_and_checkpoints_and_scrubs_env() {
    use agyva::agy::compatibility::{Compat, parse_version};
    use agyva::agy::credentials::NativeAuth;
    use agyva::config::Config;
    use agyva::core::app::App;
    use agyva::core::checkpoint::CheckpointStore;
    use agyva::paths::{AgyPaths, Env};
    use agyva::storage::vault_key::{ENV_NEW_PASSPHRASE, KdfParams, StaticPassphrase};
    use std::sync::Arc;

    let rig = Rig::new();
    rig.keyring.set_failure(Some(StoreErrorKind::Unavailable));
    rig.add_all(&[ALICE, BOB]);
    rig.app().switch("alice", false).unwrap();
    rig.app().switch("bob", false).unwrap();

    let ckpts = CheckpointStore::new(rig.dirs().checkpoint_dir())
        .list()
        .unwrap();
    assert!(!ckpts.is_empty(), "switches created checkpoints");

    // Build via App::new with AGYVA_NEW_PASSPHRASE in Env to verify it is
    // immediately scrubbed from `app.env` while still usable for rekey.
    let mut env = Env::from_pairs([
        ("AGYVA_PASSPHRASE", "synthetic-passphrase"),
        (ENV_NEW_PASSPHRASE, "rotated-passphrase-2"),
    ]);
    let app = App::new(
        rig.dirs(),
        AgyPaths {
            home: rig.agy_home(),
        },
        env.clone(),
        Config::default(),
    )
    .with_secrets(Arc::new(rig.keyring.clone()))
    .with_native(NativeAuth::new(Arc::new(rig.native.clone()), None))
    .with_kdf(KdfParams::with_cost(8, 1, 1).unwrap())
    .with_compat(Compat::Supported(parse_version("agy 1.5.9").unwrap()));
    assert!(app.env.get(ENV_NEW_PASSPHRASE).is_none());
    assert!(app.env.get("AGYVA_PASSPHRASE").is_none());
    let _ = env.take(ENV_NEW_PASSPHRASE);

    let outcome = app.rekey(None).unwrap();
    assert_eq!(outcome.mode, "passphrase");
    assert_eq!(outcome.snapshots, 2);
    assert_eq!(outcome.checkpoints, ckpts.len());
    assert!(outcome.warnings.is_empty());

    // Old passphrase no longer unlocks.
    let old_app = rig.app();
    assert_eq!(old_app.vault().unwrap_err().kind(), ErrorKind::Vault);

    // New passphrase unlocks snapshots AND checkpoints.
    let new_app = rig
        .app()
        .with_passphrases(Arc::new(StaticPassphrase("rotated-passphrase-2".into())));
    let new_vault = new_app.vault().unwrap();
    assert!(
        read_snapshot(
            new_vault,
            &rig.dirs().snapshot_file(&id_of(ALICE)),
            &id_of(ALICE)
        )
        .is_ok()
    );
    assert!(
        read_snapshot(
            new_vault,
            &rig.dirs().snapshot_file(&id_of(BOB)),
            &id_of(BOB)
        )
        .is_ok()
    );
    let store = CheckpointStore::new(rig.dirs().checkpoint_dir());
    for name in &ckpts {
        let stem = name
            .strip_suffix(".keep.ckpt")
            .or_else(|| name.strip_suffix(".ckpt"))
            .unwrap();
        let txn_id = stem.rsplit_once('-').unwrap().1;
        assert!(store.read(new_vault, name, txn_id).is_ok(), "{name}");
    }
    new_app.switch("alice", false).unwrap();
}

#[test]
fn rekey_keyring_rotation_invalidates_old_key_and_preserves_vault() {
    use agyva::core::checkpoint::CheckpointStore;

    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    rig.app().switch("alice", false).unwrap();

    let old_raw = rig
        .keyring
        .get(KEY_SERVICE, KEY_USER)
        .unwrap()
        .expect("old keyring entry");
    let old_vault = rig.app().vault().unwrap().clone();

    let outcome = rig.app().rekey(None).unwrap();
    assert_eq!(outcome.mode, "keyring");
    assert_eq!(outcome.snapshots, 2);
    assert!(outcome.checkpoints >= 1);

    let new_raw = rig
        .keyring
        .get(KEY_SERVICE, KEY_USER)
        .unwrap()
        .expect("new keyring entry");
    assert_ne!(old_raw.as_slice(), new_raw.as_slice());

    // Old key fails on re-encrypted snapshots and checkpoints.
    assert!(
        read_snapshot(
            &old_vault,
            &rig.dirs().snapshot_file(&id_of(ALICE)),
            &id_of(ALICE)
        )
        .is_err()
    );
    let store = CheckpointStore::new(rig.dirs().checkpoint_dir());
    for name in store.list().unwrap() {
        let txn_id = name
            .strip_suffix(".ckpt")
            .unwrap()
            .rsplit_once('-')
            .unwrap()
            .1;
        assert!(store.read(&old_vault, &name, txn_id).is_err());
    }

    // New key works seamlessly.
    let fresh_app = rig.app();
    fresh_app.switch("bob", false).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
}

#[test]
fn rekey_migrates_between_passphrase_and_keyring() {
    use agyva::storage::vault_key::StaticPassphrase;
    use std::sync::Arc;

    // 1. Passphrase -> Keyring
    let rig = Rig::new();
    rig.keyring.set_failure(Some(StoreErrorKind::Unavailable));
    rig.add_all(&[ALICE, BOB]);
    rig.app().switch("alice", false).unwrap();

    // Re-enable keyring and migrate to keyring mode.
    rig.keyring.set_failure(None);
    let outcome = rig.app().rekey(Some(VaultKeyMode::Keyring)).unwrap();
    assert_eq!(outcome.mode, "keyring");
    assert!(rig.keyring.get(KEY_SERVICE, KEY_USER).unwrap().is_some());
    let meta = std::fs::read_to_string(rig.dirs().vault_meta_file()).unwrap();
    assert!(meta.contains("\"keyring\""));
    assert!(!meta.contains("argon2id"));

    // Unlock works without matching the old passphrase.
    let no_pass = rig
        .app()
        .with_passphrases(Arc::new(StaticPassphrase("wrong".into())));
    no_pass.switch("bob", false).unwrap();

    // 2. Keyring -> Passphrase
    let outcome2 = rig
        .app()
        .with_new_passphrases(Arc::new(StaticPassphrase("migrated-passphrase".into())))
        .rekey(Some(VaultKeyMode::Passphrase))
        .unwrap();
    assert_eq!(outcome2.mode, "passphrase");
    assert!(outcome2.warnings.is_empty());
    assert!(
        rig.keyring.get(KEY_SERVICE, KEY_USER).unwrap().is_none(),
        "old keyring key removed after migration to passphrase"
    );
    let migrated_app = rig
        .app()
        .with_passphrases(Arc::new(StaticPassphrase("migrated-passphrase".into())));
    migrated_app.switch("alice", false).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
}

#[test]
fn rekey_refuses_when_pending_recovery_or_bad_passphrase_and_rolls_back_on_error() {
    use agyva::storage::vault_key::StaticPassphrase;
    use std::sync::Arc;

    // Pending recovery blocks rekey.
    let rig = Rig::new();
    rig.keyring.set_failure(Some(StoreErrorKind::Unavailable));
    rig.add_all(&[ALICE, BOB]);
    let _ = rig
        .app_with(Failpoints::armed("after-native-write", Fault::Crash))
        .switch("alice", false);
    let err = rig
        .app()
        .with_new_passphrases(Arc::new(StaticPassphrase("new-valid-pass".into())))
        .rekey(None)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::RecoveryRequired);
    rig.app()
        .recover(agyva::core::recover::RecoverMode::Auto)
        .unwrap();

    // Short new passphrase leaves vault untouched and old passphrase working.
    let err = rig
        .app()
        .with_new_passphrases(Arc::new(StaticPassphrase("short".into())))
        .rekey(None)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Vault);
    rig.app().switch("bob", false).unwrap();

    // Wrong current passphrase leaves vault untouched.
    let err = rig
        .app()
        .with_passphrases(Arc::new(StaticPassphrase("wrong-current".into())))
        .with_new_passphrases(Arc::new(StaticPassphrase("new-valid-pass".into())))
        .rekey(None)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Vault);
    rig.app().switch("alice", false).unwrap();

    // Simulated error before vault.json commit rolls back snapshots, checkpoints,
    // and keyring key cleanly.
    let kr_rig = Rig::new();
    kr_rig.add_all(&[ALICE, BOB]);
    kr_rig.app().switch("alice", false).unwrap();
    let before_key = kr_rig.keyring.get(KEY_SERVICE, KEY_USER).unwrap().unwrap();
    let err = kr_rig
        .app_with(Failpoints::armed("rekey-before-meta", Fault::Error))
        .rekey(None)
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Backend);
    let after_key = kr_rig.keyring.get(KEY_SERVICE, KEY_USER).unwrap().unwrap();
    assert_eq!(before_key.as_slice(), after_key.as_slice());
    kr_rig.app().switch("bob", false).unwrap();

    // No vault yet is refused cleanly.
    let empty = Rig::new();
    let err = empty.app().rekey(None).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Refused);
}
