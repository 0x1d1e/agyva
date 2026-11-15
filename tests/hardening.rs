//! Input hardening: ids that reach paths, KDF bounds, passphrase handling.
//! Everything runs in a tempdir against in-memory fakes.

mod common;

use std::sync::Arc;

use agyva::agy::credentials::MemoryBackend;
use agyva::config::{Config, VaultKeyMode};
use agyva::core::account::read_snapshot;
use agyva::core::app::App;
use agyva::core::error::ErrorKind;
use agyva::paths::{AgyPaths, Dirs, Env};
use agyva::storage::journal::{Journal, JournalStore};
use agyva::storage::keyring::MemoryKeyring;
use agyva::storage::vault_key::{KdfParams, StaticPassphrase};
use common::*;

const BAD_IDS: &[&str] = &[
    "../../etc/x",
    "..",
    "",
    "ABCDEF0123",
    "abcdef012",
    "abcdef01234",
    "abcdef012g",
    "abc/def012",
];

fn write_registry(rig: &Rig, active: &str, id: &str) {
    let doc = serde_json::json!({
        "schema_version": 1,
        "active": active,
        "previous": null,
        "accounts": [{
            "id": id, "alias": "x", "email": null, "auth_method": "consumer",
            "added_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
            "last_used_at": null
        }]
    });
    let path = rig.dirs().registry_file();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, doc.to_string()).unwrap();
}

/// Fix 8: a hand-edited registry cannot smuggle a path through an id.
#[test]
fn registry_with_a_malformed_account_id_is_rejected() {
    for bad in BAD_IDS {
        let rig = Rig::new();
        write_registry(&rig, bad, bad);
        let err = rig.app().load_registry().unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Format, "{bad:?}");
        assert!(err.message().contains("malformed account id"), "{bad:?}");
    }
    let rig = Rig::new();
    write_registry(&rig, "0123456789", "0123456789");
    assert!(rig.app().load_registry().is_ok(), "a well formed id loads");
}

/// Fix 8: the same for the journal, whose fields name files to read and write.
#[test]
fn journal_with_malformed_ids_or_checkpoint_name_is_rejected() {
    let good = || {
        Journal::new(
            "0123456789ab".into(),
            None,
            "0123456789".into(),
            "20260101T000000.000000Z-0123456789ab.ckpt".into(),
        )
    };
    let cases: Vec<(&str, serde_json::Value)> = vec![
        ("txn_id", "../../../tmp/x".into()),
        ("txn_id", "0123456789".into()),
        ("target", "../../etc/passwd".into()),
        ("from", "ZZZZZZZZZZ".into()),
        ("checkpoint", "../../accounts/0123456789.json.ckpt".into()),
        ("checkpoint", "sub/dir.ckpt".into()),
        ("checkpoint", "notackpt".into()),
    ];
    for (field, value) in cases {
        let rig = Rig::new();
        let store = JournalStore::new(rig.dirs().journal_dir());
        let mut doc = serde_json::to_value(good()).unwrap();
        doc[field] = value.clone();
        std::fs::create_dir_all(store.path().parent().unwrap()).unwrap();
        std::fs::write(store.path(), doc.to_string()).unwrap();
        let err = store.load().unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::RecoveryRequired,
            "{field}={value}: {}",
            err.message()
        );
        assert!(err.message().contains("malformed"), "{field}={value}");
        // The app treats it as a pending recovery problem, never follows it.
        assert!(rig.app().pending_journal().is_err());
    }
    // The untouched journal is accepted.
    let rig = Rig::new();
    let store = JournalStore::new(rig.dirs().journal_dir());
    store.save(&mut good()).unwrap();
    assert!(store.load().unwrap().is_some());
}

/// Fix 8: ids are checked where they become snapshot paths, too.
#[test]
fn snapshot_paths_are_never_built_from_a_malformed_id() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let vault = app.vault().unwrap();
    for bad in BAD_IDS {
        let path = rig.dirs().accounts_dir().join("whatever");
        let err = read_snapshot(vault, &path, bad).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Format, "{bad:?}");
    }
}

fn passphrase_app(rig: &Rig, passphrase: &str) -> App {
    rig.app()
        .with_vault_mode(VaultKeyMode::Passphrase)
        .with_passphrases(Arc::new(StaticPassphrase(passphrase.into())))
}

/// Fix 9: KDF parameters from vault.json are clamped; out-of-range means a
/// corrupt vault, reported at once rather than after a huge allocation.
#[test]
fn out_of_range_kdf_parameters_are_corrupt() {
    let rig = Rig::new();
    rig.login(ALICE);
    passphrase_app(&rig, "synthetic-passphrase")
        .add(&Default::default())
        .unwrap();
    let meta_path = rig.dirs().vault_meta_file();
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&meta_path).unwrap()).unwrap();
    assert_eq!(original["mode"], "passphrase");

    let tweaks: Vec<(&str, serde_json::Value)> = vec![
        ("m_kib", 4_000_000_000u64.into()),
        ("m_kib", 0.into()),
        ("t", 0.into()),
        ("t", 100_000.into()),
        ("p", 0.into()),
        ("p", 5_000.into()),
        ("salt", "AAAA".into()), // 3 bytes
    ];
    for (field, value) in tweaks {
        let mut doc = original.clone();
        doc["kdf"][field] = value.clone();
        std::fs::write(&meta_path, doc.to_string()).unwrap();
        let started = std::time::Instant::now();
        let err = passphrase_app(&rig, "synthetic-passphrase")
            .vault()
            .map(|_| ())
            .unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::Format,
            "{field}={value}: {}",
            err.message()
        );
        assert!(
            err.message().contains("out of range"),
            "{field}={value}: {}",
            err.message()
        );
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "{field}={value}: rejected without deriving a key"
        );
    }
    // Each bound can pass on its own while the product is still absurd;
    // the combined cost is capped too.
    let mut doc = original.clone();
    doc["kdf"]["m_kib"] = 1_048_576u64.into();
    doc["kdf"]["t"] = 16u64.into();
    std::fs::write(&meta_path, doc.to_string()).unwrap();
    let started = std::time::Instant::now();
    let err = passphrase_app(&rig, "synthetic-passphrase")
        .vault()
        .map(|_| ())
        .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Format, "{}", err.message());
    assert!(
        err.message().contains("memory x time cost"),
        "{}",
        err.message()
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "rejected without deriving a key"
    );

    // Restored, it unlocks again.
    std::fs::write(&meta_path, original.to_string()).unwrap();
    assert!(passphrase_app(&rig, "synthetic-passphrase").vault().is_ok());
}

/// Fix 9: a new passphrase vault needs at least 8 characters.
#[test]
fn short_passphrases_cannot_create_a_vault() {
    let rig = Rig::new();
    rig.login(ALICE);
    let err = passphrase_app(&rig, "short")
        .add(&Default::default())
        .unwrap_err();
    assert!(err.message().contains("too short"), "{}", err.message());
    assert!(
        !rig.dirs().vault_meta_file().exists(),
        "no vault was created"
    );
    // Seven characters is still too short, eight is fine.
    assert!(
        passphrase_app(&rig, "1234567")
            .add(&Default::default())
            .is_err()
    );
    assert!(
        passphrase_app(&rig, "12345678")
            .add(&Default::default())
            .is_ok()
    );
}

/// Fix 9: AGYVA_PASSPHRASE is moved out of the environment map the moment the
/// context is built, yet still unlocks the vault without a terminal.
#[test]
fn env_passphrase_is_taken_out_of_the_environment_map() {
    let tmp = tempfile::TempDir::new().unwrap();
    let native = MemoryBackend::new();
    use agyva::agy::credentials::{NativeAuth, NativeBackend};
    native
        .write(&agyva::testing::credential(ALICE.0, ALICE.1, ALICE.2))
        .unwrap();
    let make = || {
        App::new(
            Dirs::under(tmp.path()),
            AgyPaths {
                home: tmp.path().join("agyhome"),
            },
            Env::from_pairs([("AGYVA_PASSPHRASE", "synthetic-passphrase")]),
            Config::default(),
        )
        .with_secrets(Arc::new(MemoryKeyring::new()))
        .with_native(NativeAuth::new(Arc::new(native.clone()), None))
        .with_kdf(KdfParams::with_cost(8, 1, 1).unwrap())
        .with_compat(agyva::agy::compatibility::Compat::Supported(
            agyva::agy::compatibility::parse_version("agy 1.5.9").unwrap(),
        ))
        .with_vault_mode(VaultKeyMode::Passphrase)
    };
    let app = make();
    assert!(app.env.get("AGYVA_PASSPHRASE").is_none(), "not kept in env");
    assert!(app.has_env_passphrase());
    app.add(&Default::default()).unwrap();
    // A second context unlocks the existing vault with the same variable.
    assert_eq!(make().list().unwrap().accounts.len(), 1);
}
