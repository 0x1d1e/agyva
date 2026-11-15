mod common;

use agyva::core::checkpoint::CheckpointStore;
use agyva::core::error::{ErrorKind, exit};
use agyva::core::failpoint::{Failpoints, Fault};
use agyva::core::recover::{RecoverMode, RecoverOutcome};
use agyva::storage::lock::StateLock;
use common::*;

/// Switch-path failpoints, in execution order.
const SWITCH_STEPS: &[&str] = &[
    "after-checkpoint",
    "after-prepared",
    "after-native-write",
    "after-companion",
    "after-credential-written",
    "verify",
    "after-verified",
    "after-registry",
];

/// alice live and active, bob saved; returns the rig.
fn rig_alice_active() -> Rig {
    let rig = Rig::new();
    rig.add_all(&[BOB, ALICE]);
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
    rig
}

fn assert_consistent(rig: &Rig, expect_live: &str) {
    let app = rig.app();
    assert_eq!(
        rig.live_id(),
        Some(id_of(if expect_live == "alice" { ALICE } else { BOB }))
    );
    let reg = app.load_registry().unwrap();
    assert_eq!(
        reg.active.as_deref(),
        rig.live_id().as_deref(),
        "registry agrees with the live login"
    );
    assert!(
        app.pending_journal().unwrap().is_none(),
        "no journal left behind"
    );
}

#[test]
fn crash_at_every_step_is_recoverable_and_consistent() {
    for step in SWITCH_STEPS {
        let rig = rig_alice_active();
        let crashing = rig.app_with(Failpoints::armed(step, Fault::Crash));
        let err = crashing.switch("bob", false).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::SimulatedCrash, "{step}");
        assert_eq!(err.exit_code(), exit::SIMULATED_CRASH);

        let app = rig.app();
        let outcome = app.recover(RecoverMode::Auto).unwrap();
        match *step {
            "after-checkpoint" => {
                // Crashed before the journal existed: nothing changed.
                assert_eq!(outcome, RecoverOutcome::NothingToDo);
                assert_consistent(&rig, "alice");
            }
            "after-verified" => {
                assert!(
                    matches!(outcome, RecoverOutcome::RolledForward { .. }),
                    "{step}: {outcome:?}"
                );
                assert_consistent(&rig, "bob");
            }
            "after-registry" => {
                // Registry was committed; only cleanup was missing.
                assert!(
                    matches!(
                        outcome,
                        RecoverOutcome::NothingToDo | RecoverOutcome::RolledForward { .. }
                    ),
                    "{step}: {outcome:?}"
                );
                assert_consistent(&rig, "bob");
            }
            _ => {
                assert!(
                    matches!(outcome, RecoverOutcome::RolledBack { .. }),
                    "{step}: {outcome:?}"
                );
                assert_consistent(&rig, "alice");
            }
        }
        // A second recover is a no-op, and switching works again.
        assert_eq!(
            app.recover(RecoverMode::Auto).unwrap(),
            RecoverOutcome::NothingToDo,
            "{step}"
        );
        app.switch("alice", false).ok();
        app.switch("bob", false).unwrap();
        assert_eq!(
            rig.live_id(),
            Some(id_of(BOB)),
            "{step}: usable after recovery"
        );
    }
}

#[test]
fn explicit_rollback_undoes_even_a_verified_switch() {
    let rig = rig_alice_active();
    let _ = rig
        .app_with(Failpoints::armed("after-verified", Fault::Crash))
        .switch("bob", false);
    let outcome = rig.app().recover(RecoverMode::Rollback).unwrap();
    assert!(matches!(outcome, RecoverOutcome::RolledBack { .. }));
    assert_consistent(&rig, "alice");
}

#[test]
fn error_at_every_step_rolls_back_automatically() {
    for step in SWITCH_STEPS {
        let rig = rig_alice_active();
        let failing = rig.app_with(Failpoints::armed(step, Fault::Error));
        let err = failing.switch("bob", false).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Backend, "{step}");
        assert_consistent(&rig, "alice");
    }
}

#[test]
fn rollback_and_recovery_are_idempotent_after_a_crash_midway() {
    for (point, first_crash, end) in [
        ("rollback-after-native", "after-companion", "alice"),
        ("recover-after-native", "after-verified", "bob"),
    ] {
        let rig = rig_alice_active();
        let _ = rig
            .app_with(Failpoints::armed(first_crash, Fault::Crash))
            .switch("bob", false);
        let err = rig
            .app_with(Failpoints::armed(point, Fault::Crash))
            .recover(RecoverMode::Auto)
            .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::SimulatedCrash, "{point}");
        assert!(
            rig.app().pending_journal().unwrap().is_some(),
            "{point}: journal survives"
        );
        rig.app().recover(RecoverMode::Auto).unwrap();
        assert_consistent(&rig, end);
    }
}

#[test]
fn mutating_commands_refuse_while_recovery_is_pending() {
    let rig = rig_alice_active();
    let _ = rig
        .app_with(Failpoints::armed("after-native-write", Fault::Crash))
        .switch("bob", false);
    let app = rig.app();

    for err in [
        app.switch("bob", false).unwrap_err(),
        app.rename("bob", "robert").unwrap_err(),
        app.remove("bob", false).unwrap_err(),
        app.add(&Default::default()).unwrap_err(),
    ] {
        assert_eq!(err.kind(), ErrorKind::RecoveryRequired);
        assert_eq!(err.exit_code(), exit::RECOVERY_REQUIRED);
        assert!(err.hint_text().is_some_and(|h| h.contains("agyva recover")));
    }
    // Read-only views still work and report the problem.
    assert!(app.status().unwrap().recovery_pending.is_some());
    let doctor = app.doctor();
    assert!(doctor.recovery_pending);
    assert!(!doctor.healthy);
    assert!(app.list().is_ok());
}

#[test]
fn native_write_failure_leaves_previous_login_and_is_recoverable() {
    let rig = rig_alice_active();
    let app = rig.app();
    rig.native.set_fail_write(true);
    let err = app.switch("bob", false).unwrap_err();
    assert_ne!(err.kind(), ErrorKind::SimulatedCrash);
    rig.native.set_fail_write(false);
    // Whether rollback finished on its own or was left for `recover`, the
    // end state must be consistent and healable.
    app.recover(RecoverMode::Auto).unwrap();
    assert_consistent(&rig, "alice");
    app.switch("bob", false).unwrap();
}

#[test]
fn lock_contention_reports_another_agyva() {
    let rig = rig_alice_active();
    let app = rig.app();
    let _held =
        StateLock::acquire(&rig.dirs().lock_file(), std::time::Duration::from_secs(1)).unwrap();
    let err = app.switch("bob", false).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Locked);
    assert!(
        err.message().contains("another agyva is running"),
        "{}",
        err.message()
    );
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
}

#[test]
fn checkpoints_are_pruned_to_five() {
    let rig = rig_alice_active();
    let app = rig.app();
    for _ in 0..5 {
        app.switch("bob", false).unwrap();
        app.switch("alice", false).unwrap();
    }
    let store = CheckpointStore::new(rig.dirs().checkpoint_dir());
    assert_eq!(store.list().unwrap().len(), 5);
}

#[test]
fn switching_to_the_live_account_is_a_noop() {
    let rig = rig_alice_active();
    let out = rig.app().switch("alice", false).unwrap();
    assert!(out.already_active);
    assert_consistent(&rig, "alice");
}

#[test]
fn switch_warns_to_restart_agy() {
    let rig = rig_alice_active();
    let out = rig.app().switch("bob", false).unwrap();
    assert!(out.warnings.iter().any(|w| w.contains("Restart agy")));
}

// ---------------------------------------------------------------- round 3

use agyva::core::checkpoint::Checkpoint;
use agyva::core::registry::Registry;
use agyva::storage::journal::Phase;
use std::sync::Arc;

fn pinned(rig: &Rig) -> Vec<String> {
    CheckpointStore::new(rig.dirs().checkpoint_dir())
        .list()
        .unwrap()
        .into_iter()
        .filter(|n| CheckpointStore::is_pinned(n))
        .collect()
}

/// Decrypt a checkpoint by file name (`<time>-<txn>[.keep].ckpt`).
fn read_checkpoint(rig: &Rig, name: &str) -> Checkpoint {
    let txn = name
        .rsplit_once('-')
        .and_then(|(_, rest)| rest.split('.').next())
        .expect("txn id in name");
    let app = rig.app();
    CheckpointStore::new(rig.dirs().checkpoint_dir())
        .read(app.vault().unwrap(), name, txn)
        .unwrap()
}

fn raw_of(who: (&str, &str, &str)) -> Vec<u8> {
    agyva::testing::credential(who.0, who.1, who.2)
}

/// Fix 1: forcing a switch over a login agyva has never saved keeps an
/// encrypted copy that pruning never deletes, and says where it is.
#[test]
fn force_switch_over_unsaved_login_keeps_a_pinned_checkpoint() {
    let rig = rig_alice_active();
    rig.login(CAROL); // not saved in agyva
    let app = rig.app();
    assert_eq!(
        app.switch("bob", false).unwrap_err().kind(),
        ErrorKind::Refused
    );
    assert!(pinned(&rig).is_empty(), "a refused switch writes nothing");

    let out = app.switch("bob", true).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
    let kept = pinned(&rig);
    assert_eq!(kept.len(), 1, "{kept:?}");
    let text = out.warnings.join("\n");
    assert!(text.contains("NOT pruned automatically"), "{text}");
    assert!(text.contains(&kept[0]), "names the file: {text}");
    let saved = read_checkpoint(&rig, &kept[0]).native.expect("carol kept");
    assert_eq!(saved.as_bytes(), raw_of(CAROL).as_slice());

    // Many later switches prune ordinary checkpoints, never the pinned one.
    for _ in 0..8 {
        app.switch("alice", false).unwrap();
        app.switch("bob", false).unwrap();
    }
    assert_eq!(pinned(&rig), kept, "pinned checkpoint survives pruning");
    let ordinary = CheckpointStore::new(rig.dirs().checkpoint_dir())
        .list()
        .unwrap()
        .len()
        - 1;
    assert_eq!(ordinary, 5);
}

/// Fix 1: if the forced switch fails and rolls back, the login is back and
/// the now redundant pinned copy is gone.
#[test]
fn failed_force_switch_restores_the_login_and_drops_the_pinned_copy() {
    let rig = rig_alice_active();
    rig.login(CAROL);
    let failing = rig.app_with(Failpoints::armed("verify", Fault::Error));
    let err = failing.switch("bob", true).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Backend);
    assert_eq!(rig.native_text(), String::from_utf8(raw_of(CAROL)).unwrap());
    assert!(pinned(&rig).is_empty());
    assert!(rig.app().pending_journal().unwrap().is_none());
}

/// Fix 1 + review 7: a crash mid forced-switch leaves the pinned copy for
/// `recover`, but once recovery has rolled back (the unsaved login is live
/// again) the copy is redundant and goes.
#[test]
fn a_successful_recovery_rollback_drops_the_redundant_pinned_copy() {
    let rig = rig_alice_active();
    rig.login(CAROL);
    let _ = rig
        .app_with(Failpoints::armed("after-native-write", Fault::Crash))
        .switch("bob", true);
    assert_eq!(pinned(&rig).len(), 1, "kept until recovery runs");
    let outcome = rig.app().recover(RecoverMode::Auto).unwrap();
    assert!(
        matches!(outcome, RecoverOutcome::RolledBack { .. }),
        "{outcome:?}"
    );
    assert_eq!(rig.native_text(), String::from_utf8(raw_of(CAROL)).unwrap());
    assert!(
        pinned(&rig).is_empty(),
        "the login is live; the copy is not"
    );
    assert_eq!(rig.live_id(), Some(id_of(CAROL)));
}

/// Fix 1: a roll-*forward* did not restore the unsaved login, so its pinned
/// copy is the only one there is and must stay.
#[test]
fn a_recovery_roll_forward_keeps_the_pinned_copy_of_an_unsaved_login() {
    let rig = rig_alice_active();
    rig.login(CAROL);
    let _ = rig
        .app_with(Failpoints::armed("after-verified", Fault::Crash))
        .switch("bob", true);
    let outcome = rig.app().recover(RecoverMode::Auto).unwrap();
    assert!(
        matches!(outcome, RecoverOutcome::RolledForward { .. }),
        "{outcome:?}"
    );
    let kept = pinned(&rig);
    assert_eq!(kept.len(), 1, "{kept:?}");
    let saved = read_checkpoint(&rig, &kept[0]).native.expect("carol kept");
    assert_eq!(saved.as_bytes(), raw_of(CAROL).as_slice());
}

/// Fix 2: rollback must not overwrite a login that is neither the
/// checkpointed one nor the target's.
#[test]
fn recover_refuses_to_overwrite_an_unrelated_login_unless_forced() {
    let rig = rig_alice_active();
    let _ = rig
        .app_with(Failpoints::armed("after-native-write", Fault::Crash))
        .switch("bob", false);
    rig.login(CAROL); // somebody logged in again after the crash
    let app = rig.app();
    let before = CheckpointStore::new(rig.dirs().checkpoint_dir())
        .list()
        .unwrap();

    let err = app.recover(RecoverMode::Auto).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Refused);
    assert!(
        err.hint_text()
            .is_some_and(|h| h.contains("recover --force"))
    );
    assert_eq!(rig.native_text(), String::from_utf8(raw_of(CAROL)).unwrap());
    assert!(app.pending_journal().unwrap().is_some(), "still pending");
    assert_eq!(
        CheckpointStore::new(rig.dirs().checkpoint_dir())
            .list()
            .unwrap(),
        before,
        "a refusal writes nothing"
    );

    let outcome = app.recover_with(RecoverMode::Auto, true).unwrap();
    let RecoverOutcome::RolledBack {
        kept_checkpoint: Some(name),
        ..
    } = outcome
    else {
        panic!("expected a kept checkpoint: {outcome:?}");
    };
    assert!(CheckpointStore::is_pinned(&name));
    assert_eq!(
        read_checkpoint(&rig, &name).native.unwrap().as_bytes(),
        raw_of(CAROL).as_slice()
    );
    assert_consistent(&rig, "alice");
}

/// Fix 3: a login that changes between the first read and the write stops the
/// switch with nothing written and nothing rolled back over the new login.
#[test]
fn login_changed_just_before_the_write_aborts_cleanly() {
    // Read #1 is the initial read; #2 and #3 are the two re-checks, #3 being
    // the one right before the write. A change landing after #3 returned is
    // the unavoidable check-to-write window and is documented, not tested.
    for after_read in [1usize, 2] {
        let rig = rig_alice_active();
        let flaky = rig.flaky();
        let newer = raw_of(("sub-alice", "alice@example.invalid", "alice-refreshed"));
        flaky.replace_after_read(after_read, newer.clone());
        let app = rig.app_with_backend(Arc::new(flaky.clone()), Failpoints::none());

        let err = app.switch("bob", false).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ConcurrentChange, "read {after_read}");
        assert!(
            err.hint_text().is_some_and(|h| h.contains("again")),
            "tells the user to retry"
        );
        assert_eq!(flaky.writes(), 0, "read {after_read}: nothing was written");
        assert_eq!(
            rig.native_text().as_bytes(),
            newer.as_slice(),
            "newer login kept"
        );
        assert!(rig.app().pending_journal().unwrap().is_none());
        assert_eq!(
            rig.app().load_registry().unwrap().active.as_deref(),
            Some(id_of(ALICE).as_str())
        );
        // Retrying works.
        rig.app().switch("bob", false).unwrap();
        assert_eq!(rig.live_id(), Some(id_of(BOB)));
    }
}

/// Fix 4: a timed-out keyring write may or may not have happened. No rollback
/// is guessed, the journal stays, and `recover` decides from the real state.
#[test]
fn keyring_write_timeout_is_an_unknown_outcome_resolved_by_recover() {
    for applied in [false, true] {
        let rig = rig_alice_active();
        let flaky = rig.flaky();
        flaky.timeout_on_write(1, applied);
        let app = rig.app_with_backend(Arc::new(flaky.clone()), Failpoints::none());

        let err = app.switch("bob", false).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::RecoveryRequired, "applied={applied}");
        assert_eq!(err.exit_code(), exit::RECOVERY_REQUIRED);
        let hint = err.hint_text().unwrap_or_default();
        assert!(
            hint.contains("unlock") && hint.contains("agyva recover"),
            "{hint}"
        );
        assert_eq!(flaky.writes(), 1, "no rollback write was attempted");
        // Native is exactly what the (possibly applied) write left.
        let expect = if applied { BOB } else { ALICE };
        assert_eq!(rig.live_id(), Some(id_of(expect)), "applied={applied}");
        let journal = rig.app().pending_journal().unwrap().expect("journal kept");
        assert_eq!(journal.phase, Phase::RecoveryRequired);

        // Mutating commands are blocked until recovered.
        assert_eq!(
            rig.app().switch("bob", false).unwrap_err().kind(),
            ErrorKind::RecoveryRequired
        );
        let outcome = rig.app().recover(RecoverMode::Auto).unwrap();
        assert!(
            matches!(outcome, RecoverOutcome::RolledBack { .. }),
            "{outcome:?}"
        );
        assert_consistent(&rig, "alice");
    }
}

/// Fix 4: the same holds when the timeout hits the automatic rollback's write.
#[test]
fn keyring_timeout_during_rollback_leaves_recovery_required() {
    let rig = rig_alice_active();
    let flaky = rig.flaky();
    // write #1 = switch write, #2 = rollback write.
    flaky.timeout_on_write(2, false);
    let app = rig.app_with_backend(
        Arc::new(flaky.clone()),
        Failpoints::armed("after-companion", Fault::Error),
    );
    let err = app.switch("bob", false).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::RecoveryRequired);
    assert_eq!(
        rig.app().pending_journal().unwrap().map(|j| j.phase),
        Some(Phase::RecoveryRequired)
    );
    // Native still holds bob (the rollback write was not applied); recover fixes it.
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
    rig.app().recover(RecoverMode::Auto).unwrap();
    assert_consistent(&rig, "alice");
}

/// Fix 7: failing to tidy up after a committed switch is a warning.
#[test]
fn post_commit_cleanup_failure_is_a_warning_not_an_error() {
    let rig = rig_alice_active();
    let app = rig.app_with(Failpoints::armed("post-commit-cleanup", Fault::Error));
    let out = app.switch("bob", false).unwrap();
    assert!(
        out.warnings.iter().any(|w| w.contains("cleanup failed")),
        "{:?}",
        out.warnings
    );
    assert_eq!(rig.live_id(), Some(id_of(BOB)));
    let reg: Registry = rig.app().load_registry().unwrap();
    assert_eq!(reg.active.as_deref(), Some(id_of(BOB).as_str()));
    // The leftover journal is already committed: the next command tidies it.
    assert!(rig.app().pending_journal().unwrap().is_none());
    rig.app().switch("alice", false).unwrap();
    assert_consistent(&rig, "alice");
}
