mod common;

use agyva::core::add::AddOptions;
use agyva::core::error::ErrorKind;
use common::*;

#[test]
fn add_list_use_previous_rename_remove() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();

    let list = app.list().unwrap();
    assert_eq!(list.accounts.len(), 2);
    assert_eq!(list.accounts[0].alias, "alice");
    assert_eq!(list.accounts[1].alias, "bob");
    assert!(list.accounts[1].active, "last added login is live");

    app.switch("alice", false).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
    app.switch("-", false).unwrap();
    assert_eq!(
        rig.live_id(),
        Some(id_of(BOB)),
        "`use -` returns to previous"
    );
    app.switch("-", false).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(ALICE)), "`use -` toggles");

    app.switch("2", false).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "1-based number");
    app.switch("alice@example.invalid", false).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(ALICE)), "exact email");
    app.switch(&id_of(BOB)[..4], false).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(BOB)), "id prefix");

    let renamed = app.rename("alice", "work").unwrap();
    assert_eq!(renamed.alias, "work");
    assert_eq!(app.resolve("work").unwrap().id, id_of(ALICE));

    let err = app.remove("bob", false).unwrap_err();
    assert_eq!(
        err.kind(),
        ErrorKind::Refused,
        "active account needs --force"
    );
    app.remove("work", false).unwrap();
    assert_eq!(app.list().unwrap().accounts.len(), 1);
    assert_eq!(
        rig.live_id(),
        Some(id_of(BOB)),
        "remove never touches the native login"
    );
}

#[test]
fn force_remove_active_keeps_native_login() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    app.remove("alice", true).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
    assert!(app.list().unwrap().accounts.is_empty());
}

#[test]
fn duplicate_sub_is_refused_or_refreshed() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    let err = app.add(&AddOptions::default()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Conflict);
    assert!(err.hint_text().is_some_and(|h| h.contains("--refresh")));

    rig.login(("sub-alice", "alice@example.invalid", "alice-refreshed"));
    let out = app
        .add(&AddOptions {
            alias: None,
            refresh: true,
        })
        .unwrap();
    assert!(out.refreshed);
    assert_eq!(app.list().unwrap().accounts.len(), 1);
    // Switching away and back yields the refreshed token.
    rig.login(BOB);
    app.add(&AddOptions::default()).unwrap();
    app.switch("alice", false).unwrap();
    let live = rig.native_text();
    assert!(
        live.contains("access-alice-refreshed"),
        "refreshed credential was saved"
    );
}

#[test]
fn default_alias_is_deduplicated_and_ambiguity_lists_candidates() {
    let rig = Rig::new();
    let app = rig.app();
    rig.login(("sub-1", "sam@one.invalid", "s1"));
    app.add(&AddOptions::default()).unwrap();
    rig.login(("sub-2", "sam@two.invalid", "s2"));
    let out = app.add(&AddOptions::default()).unwrap();
    assert_eq!(out.account.alias, "sam-2");

    rig.login(("sub-3", "pat@one.invalid", "p1"));
    app.add(&AddOptions {
        alias: Some("pat".into()),
        refresh: false,
    })
    .unwrap();
    let err = app.add(&AddOptions {
        alias: Some("pat".into()),
        refresh: false,
    });
    assert!(err.is_err(), "alias collisions are refused");
}

#[test]
fn unknown_selector_and_bad_alias_are_clean_errors() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    let app = rig.app();
    assert_eq!(
        app.switch("nobody", false).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        app.switch("9", false).unwrap_err().kind(),
        ErrorKind::NotFound
    );
    assert_eq!(
        app.rename("alice", "bad alias!").unwrap_err().kind(),
        ErrorKind::Usage
    );
    let first = rig.app();
    assert_eq!(
        first.switch("-", false).unwrap_err().kind(),
        ErrorKind::NotFound,
        "no previous account yet"
    );
}

#[test]
fn unmanaged_live_login_is_not_overwritten_without_force() {
    let rig = Rig::new();
    rig.add_all(&[ALICE]);
    rig.login(CAROL); // not saved in agyva
    let app = rig.app();
    let err = app.switch("alice", false).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Refused);
    assert_eq!(rig.live_id(), Some(id_of(CAROL)), "carol's login is intact");
    app.switch("alice", true).unwrap();
    assert_eq!(rig.live_id(), Some(id_of(ALICE)));
}

#[test]
fn outgoing_tokens_are_saved_back_on_switch() {
    let rig = Rig::new();
    rig.add_all(&[ALICE, BOB]);
    let app = rig.app();
    app.switch("alice", false).unwrap();
    // agy refreshes alice's token while running.
    rig.login(("sub-alice", "alice@example.invalid", "alice-rotated"));
    app.switch("bob", false).unwrap();
    app.switch("alice", false).unwrap();
    assert!(rig.native_text().contains("access-alice-rotated"));
}

#[test]
fn policy_notice_is_shown_once() {
    let rig = Rig::new();
    rig.login(ALICE);
    let app = rig.app();
    let first = app.add(&AddOptions::default()).unwrap();
    assert!(first.policy_notice.is_some());
    rig.login(BOB);
    let second = rig.app().add(&AddOptions::default()).unwrap();
    assert!(
        second.policy_notice.is_none(),
        "acknowledged state persists in config.toml"
    );
}

#[test]
fn ambiguous_id_prefix_lists_candidates() {
    use agyva::agy::credentials::account_id_for_sub;
    // Find two synthetic subs whose ids share a leading hex letter.
    let mut seen: std::collections::HashMap<char, String> = Default::default();
    let mut pair = None;
    for n in 0..500 {
        let sub = format!("sub-amb-{n}");
        let c = account_id_for_sub(&sub).chars().next().unwrap_or('0');
        if !c.is_ascii_lowercase() {
            continue;
        }
        if let Some(prev) = seen.insert(c, sub.clone()) {
            pair = Some((prev, sub, c));
            break;
        }
    }
    let (s1, s2, c) = pair.expect("a colliding pair exists within 500 subs");
    let rig = Rig::new();
    let app = rig.app();
    for (i, sub) in [&s1, &s2].into_iter().enumerate() {
        rig.login((
            sub.as_str(),
            &format!("amb{i}@example.invalid"),
            &format!("amb{i}"),
        ));
        app.add(&AddOptions::default()).unwrap();
    }
    let err = app.resolve(&c.to_string()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Ambiguous);
    let text = format!("{} {}", err.message(), err.hint_text().unwrap_or_default());
    assert!(
        text.contains("amb0") && text.contains("amb1"),
        "candidates listed: {text}"
    );
}

#[test]
fn an_email_only_login_adds_and_dedupes_case_insensitively() {
    let rig = Rig::new();
    let app = rig.app();
    rig.login_raw(agyva::testing::consumer_credential(
        "First.Last@Example.Invalid",
        "e1",
    ));
    let out = app.add(&AddOptions::default()).unwrap();
    assert_eq!(
        out.account.email.as_deref(),
        Some("First.Last@Example.Invalid")
    );

    // The same email with different case is the same account.
    rig.login_raw(agyva::testing::consumer_credential(
        "first.last@example.invalid",
        "e2",
    ));
    let err = app.add(&AddOptions::default()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Conflict);
    assert!(err.hint_text().is_some_and(|h| h.contains("--refresh")));

    let refreshed = app
        .add(&AddOptions {
            alias: None,
            refresh: true,
        })
        .unwrap();
    assert!(refreshed.refreshed);
    assert_eq!(app.list().unwrap().accounts.len(), 1);
    assert!(rig.native_text().contains("access-e2"));
}

#[test]
fn an_email_only_login_dedupes_against_an_account_saved_with_a_subject() {
    let rig = Rig::new();
    let app = rig.app();
    rig.login(ALICE);
    app.add(&AddOptions::default()).unwrap();
    // Same email, different case, no subject: still the saved account.
    rig.login_raw(agyva::testing::consumer_credential(
        "ALICE@example.invalid",
        "a2",
    ));
    let err = app.add(&AddOptions::default()).unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Conflict);
    assert_eq!(app.list().unwrap().accounts.len(), 1);
}

#[test]
fn a_refresh_token_fingerprint_is_a_stable_duplicate_key() {
    let rig = Rig::new();
    let app = rig.app();
    rig.login_raw(agyva::testing::unidentified_credential("f1"));
    let first = app.add(&AddOptions::default()).unwrap();
    assert_eq!(first.account.email, None);
    let again = app.add(&AddOptions::default()).unwrap_err();
    assert_eq!(again.kind(), ErrorKind::Conflict);

    // A fresh app instance derives the same id from the same credential.
    let listed = rig.app().list().unwrap();
    assert_eq!(listed.accounts.len(), 1);
    assert_eq!(listed.accounts[0].id, first.account.id);
}
