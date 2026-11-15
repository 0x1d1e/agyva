//! Helpers for tests (unit, integration and the scripted E2E). Everything
//! here is synthetic: unsigned JWTs with fake subjects and fake tokens.
//! [`consumer_credential`] mirrors the shape observed on agy 1.5.9 consumer
//! logins, where `id_token` is omitted and `id_token_email` carries the email.

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

/// The shape observed on agy 1.5.9 (consumer, Linux keyring): exactly three
/// top-level keys, no `id_token`, and an RFC3339 expiry with nine fractional
/// digits and a numeric offset.
pub fn consumer_credential(email: &str, tag: &str) -> Vec<u8> {
    serde_json::json!({
        "token": {
            "access_token": format!("access-{tag}"),
            "token_type": "Bearer",
            "refresh_token": format!("refresh-{tag}"),
            "expiry": "2030-01-01T00:00:00.123456789+01:00",
            "expires_in": 3599,
        },
        "auth_method": "consumer",
        "id_token_email": email,
    })
    .to_string()
    .into_bytes()
}

/// A consumer blob carrying a malformed `id_token` *and* a usable
/// `id_token_email`: parsing must fall back to the email, not refuse.
pub fn malformed_id_token_credential(email: &str, tag: &str) -> Vec<u8> {
    let mut doc: serde_json::Value =
        serde_json::from_slice(&consumer_credential(email, tag)).expect("json");
    doc.as_object_mut()
        .expect("object")
        .insert("id_token".into(), "not-a-jwt".into());
    doc.to_string().into_bytes()
}

/// A consumer blob with neither `id_token` nor `id_token_email`: only the
/// refresh token can identify it.
pub fn unidentified_credential(tag: &str) -> Vec<u8> {
    serde_json::json!({
        "token": {
            "access_token": format!("access-{tag}"),
            "token_type": "Bearer",
            "refresh_token": format!("refresh-{tag}"),
            "expiry": "2030-01-01T00:00:00.123456789+01:00",
            "expires_in": 3599,
        },
        "auth_method": "consumer",
    })
    .to_string()
    .into_bytes()
}

/// Strings that must never appear in any output for a credential built with `tag`.
pub fn secrets_for(tag: &str) -> [String; 3] {
    [
        format!("access-{tag}"),
        format!("refresh-{tag}"),
        format!("idtok-{tag}"),
    ]
}

/// Parse a synthetic credential into the pieces an archive record needs:
/// derived id, alias, email and auth method. Fails the caller loudly for
/// credentials `parse_credential` refuses, so tests never save junk.
pub fn parse_for_test(raw: &[u8]) -> ParsedCredential {
    let info = crate::agy::credentials::parse_credential(raw)
        .unwrap_or_else(|e| panic!("test credential does not parse: {}", e.message()));
    let alias = info
        .email
        .as_deref()
        .and_then(|e| e.split('@').next())
        .unwrap_or("account")
        .to_owned();
    ParsedCredential {
        id: info.account_id(),
        alias,
        email: info.email.clone(),
        auth_method: info.auth_method.clone(),
    }
}

/// The identity facts of a synthetic credential, ready for a
/// `core::registry::AccountRecord` or an archive entry.
pub struct ParsedCredential {
    pub id: String,
    pub alias: String,
    pub email: Option<String>,
    pub auth_method: String,
}
