//! Opt-in check against the real OS keyring (Secret Service on Linux).
//!
//! Ignored by default. It runs only with `AGYVA_REAL_KEYRING_TEST=1`, and even
//! then it touches nothing but a throwaway item under a random service name
//! (`agyva-test-<random>`), which it deletes again. It never uses the names
//! agy or agyva use (`gemini`/`antigravity`, `agyva`/`vault-key`).
//!
//!     AGYVA_REAL_KEYRING_TEST=1 cargo test --test real_keyring -- --ignored
//!
//! This is the one place the keyring-backed code path meets a real daemon, so
//! it is also the place to check that a locked keyring prompts and times out
//! as documented (docs/compatibility.md).

use agyva::storage::keyring::{SecretStore, SetOptions, SystemKeyring};

struct Cleanup<'a> {
    store: &'a SystemKeyring,
    service: String,
    user: String,
}

impl Drop for Cleanup<'_> {
    fn drop(&mut self) {
        let _ = self.store.delete(&self.service, &self.user);
    }
}

#[test]
#[ignore = "touches the real OS keyring; set AGYVA_REAL_KEYRING_TEST=1 and use --ignored"]
fn throwaway_item_round_trips_through_the_real_keyring() {
    if std::env::var("AGYVA_REAL_KEYRING_TEST").as_deref() != Ok("1") {
        eprintln!("AGYVA_REAL_KEYRING_TEST=1 not set: skipping");
        return;
    }
    let mut r = [0u8; 8];
    getrandom::fill(&mut r).expect("randomness");
    let service = format!("agyva-test-{}", hex::encode(r));
    let user = "throwaway".to_owned();
    assert!(service.starts_with("agyva-test-") && service.len() > "agyva-test-".len());
    for forbidden in ["gemini", "antigravity", "agyva"] {
        assert_ne!(service, forbidden);
        assert_ne!(user, forbidden);
    }

    let store = SystemKeyring::new();
    let _cleanup = Cleanup {
        store: &store,
        service: service.clone(),
        user: user.clone(),
    };
    assert!(store.get(&service, &user).expect("get").is_none());
    let opts = SetOptions {
        label: Some("agyva test item (safe to delete)".into()),
        xdg_schema: None,
    };
    store
        .set(&service, &user, b"synthetic-secret", &opts)
        .expect("set");
    let back = store.get(&service, &user).expect("get").expect("present");
    assert_eq!(&back[..], b"synthetic-secret");
    store
        .set(&service, &user, b"synthetic-secret-2", &opts)
        .expect("overwrite");
    let back = store.get(&service, &user).expect("get").expect("present");
    assert_eq!(&back[..], b"synthetic-secret-2");
    assert!(store.delete(&service, &user).expect("delete"));
    assert!(store.get(&service, &user).expect("get").is_none());
}
