//! Helpers for tests (unit, integration and the scripted E2E). Everything
//! here is synthetic: unsigned JWTs with fake subjects and fake tokens.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

/// Build a synthetic credential document. Token strings embed `tag`, so tests
/// can scan outputs for leaks of `access-<tag>`, `refresh-<tag>` and
/// `idtok-<tag>`.
pub fn credential(sub: &str, email: &str, tag: &str) -> Vec<u8> {
    credential_with(sub, email, tag, "consumer", &[])
}

/// Like [`credential`] with a custom `auth_method` and extra top-level fields.
pub fn credential_with(
    sub: &str,
    email: &str,
    tag: &str,
    auth_method: &str,
    extra: &[(&str, &str)],
) -> Vec<u8> {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"none","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::json!({"iss":"https://accounts.example.invalid","sub":sub,"email":email})
            .to_string(),
    );
    let mut doc = serde_json::json!({
        "token": {
            "access_token": format!("access-{tag}"),
            "token_type": "Bearer",
            "refresh_token": format!("refresh-{tag}"),
            "expiry": "2030-01-01T00:00:00Z",
        },
        "auth_method": auth_method,
        "id_token": format!("{header}.{payload}.idtok-{tag}"),
    });
    if let Some(obj) = doc.as_object_mut() {
        for (k, v) in extra {
            obj.insert((*k).to_owned(), serde_json::Value::String((*v).to_owned()));
        }
    }
    doc.to_string().into_bytes()
}

/// Strings that must never appear in any output for a credential built with `tag`.
pub fn secrets_for(tag: &str) -> [String; 3] {
    [
        format!("access-{tag}"),
        format!("refresh-{tag}"),
        format!("idtok-{tag}"),
    ]
}
