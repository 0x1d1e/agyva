mod common;

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

use agyva::core::checkpoint::{Checkpoint, CheckpointStore};
use agyva::core::endpoints::{self, EndpointPatch, NewEndpoint};
use agyva::core::error::ErrorKind;
use agyva::core::failpoint::{Failpoints, Fault};
use agyva::storage::lock::StateLock;
use common::*;
use zeroize::Zeroizing;

#[test]
fn checkpoint_list_meta_restore_force_and_prune() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    app.switch("alice", false).unwrap();
    app.switch("bob", false).unwrap();

    // Force-switch over an unsaved login (CAROL) creates a pinned checkpoint.
    rig.login(CAROL);
    let sw = app.switch("alice", true).unwrap();
    assert!(sw.warnings.iter().any(|w| w.contains(".keep.ckpt")));

    // Add a malformed .ckpt file and a path-unsafe dotfile; verify safe handling.
    let ckpt_dir = rig.dirs().checkpoint_dir();
    std::fs::write(ckpt_dir.join("weirdname.ckpt"), b"bad").unwrap();
    std::fs::write(ckpt_dir.join(".hidden.ckpt"), b"bad").unwrap();

    let metas = app.list_checkpoints().unwrap();
    assert!(metas.iter().any(|m| m.pinned && m.valid));
    let weird = metas.iter().find(|m| m.name == "weirdname.ckpt").unwrap();
    assert!(!weird.valid);
    assert!(!metas.iter().any(|m| m.name == ".hidden.ckpt"));

    let pinned_name = metas
        .iter()
        .find(|m| m.pinned && m.valid)
        .unwrap()
        .name
        .clone();

    // Live login is ALICE, checkpoint has CAROL: refused without --force.
    let err = app.restore_checkpoint(&pinned_name, false).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Refused);

    // With --force: creates a pinned backup of ALICE, restores CAROL.
    let res = app.restore_checkpoint(&pinned_name, true).unwrap();
    assert_eq!(res.restored, pinned_name);
    let backup_name = res.kept_checkpoint.expect("pinned backup of ALICE");
    assert!(CheckpointStore::is_pinned(&backup_name));
    assert_eq!(rig.live_id(), Some(id_of(CAROL)));

    // Pending journal blocks checkpoint restore with RecoveryRequired (exit 3).
    let _ = rig
        .app_with(Failpoints::armed("after-native-write", Fault::Crash))
        .switch("bob", true);
    let err = app.restore_checkpoint(&backup_name, true).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::RecoveryRequired);
    app.recover(agyva::core::recover::RecoverMode::Auto)
        .unwrap();

    // Prune non-pinned keeps pinned intact; include_pinned = true removes pinned.
    // Clean weirdname.ckpt first so prune operates cleanly.
    app.delete_checkpoint("weirdname.ckpt").unwrap();
    let prune1 = app.prune_checkpoints(false).unwrap();
    assert!(prune1.pinned_remaining >= 1);
    let prune2 = app.prune_checkpoints(true).unwrap();
    assert_eq!(prune2.pinned_remaining, 0);
}

#[test]
fn doctor_fix_fixes_permissions_and_dead_lockfiles_only() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();

    // Widen permissions on config dir & registry file and leave a dead-PID lockfile.
    #[cfg(unix)]
    {
        std::fs::set_permissions(&rig.dirs().config, std::fs::Permissions::from_mode(0o755))
            .unwrap();
        std::fs::set_permissions(
            rig.dirs().registry_file(),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
    }
    let cache_lock = rig.dirs().cache.join("quota.lock");
    std::fs::create_dir_all(&rig.dirs().cache).unwrap();
    std::fs::write(&cache_lock, "999999999").unwrap();

    // Hold state lock live: doctor_fix must NOT delete a contended lockfile.
    let _live_lock =
        StateLock::acquire(&rig.dirs().lock_file(), std::time::Duration::from_secs(1)).unwrap();

    let fixed = app.doctor_fix().unwrap();
    assert!(!cache_lock.exists(), "dead uncontended lockfile removed");
    assert!(
        rig.dirs().lock_file().exists(),
        "live contended lockfile preserved"
    );
    assert!(!fixed.actions.is_empty());
    #[cfg(unix)]
    {
        assert_eq!(
            std::fs::metadata(&rig.dirs().config)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(rig.dirs().registry_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn update_endpoint_propagates_corrupt_secret_when_snapshot_exists() {
    let rig = Rig::new();
    let app = rig.app();
    let mut ep_spec = NewEndpoint::default();
    ep_spec.alias = "ep1".into();
    ep_spec.base_url = "https://api.example.com/v1".into();
    ep_spec.api_key = Zeroizing::new("sk-initial".into());
    let ep = app.add_endpoint(ep_spec).unwrap();

    // Corrupt the .ep.snap file on disk.
    let snap_path = endpoints::secret_file(&rig.dirs(), &ep.id);
    assert!(snap_path.exists());
    std::fs::write(&snap_path, b"corrupted-ciphertext").unwrap();

    // Updating a secret field must propagate the read error instead of silently resetting.
    let mut patch1 = EndpointPatch::default();
    patch1.api_key = Some(Zeroizing::new("sk-new".into()));
    let err = app.update_endpoint("ep1", patch1).unwrap_err();
    assert_ne!(err.kind(), ErrorKind::NotFound);

    // If snapshot does not exist at all, update_endpoint may initialize from default.
    std::fs::remove_file(&snap_path).unwrap();
    let mut patch2 = EndpointPatch::default();
    patch2.api_key = Some(Zeroizing::new("sk-recovered".into()));
    app.update_endpoint("ep1", patch2).unwrap();
    let _ = Checkpoint::new("0123456789ab", None, Default::default(), false, None, None);
}
