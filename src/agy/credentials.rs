//! agy's live credential: shape validation, identity extraction and the
//! backends that hold it.
//!
//! The credential document (OBSERVED-LOCALLY on agy 1.5.9, consumer, Linux
//! keyring) is JSON with exactly three top-level keys:
//! `{"token":{"access_token","token_type","refresh_token","expiry",
//!   "expires_in"},"auth_method":"consumer","id_token_email":"<email>"}`.
//! `id_token` is optional and omitted on consumer logins; agy's binary accepts
//! either an `id_token` or an `id_token_email`. GCP/enterprise shapes are
//! UNVERIFIED. The document is always handled as opaque bytes and written back
//! verbatim; parsing only extracts non-secret facts.

use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD as B64, URL_SAFE_NO_PAD};
use chrono::{DateTime, Utc};
use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::core::error::{Error, ErrorKind, Result};
use crate::storage::fsutil;
use crate::storage::keyring::{SecretStore, SetOptions, StoreError, StoreErrorKind};

pub const SERVICE: &str = "gemini";
pub const USER: &str = "antigravity";
pub const ITEM_LABEL: &str = "Password for 'antigravity' on 'gemini'";
pub const XDG_SCHEMA: &str = "org.freedesktop.Secret.Generic";
const MAX_CREDENTIAL_BYTES: usize = 256 * 1024;

/// Opaque credential bytes. Zeroed on drop; `Debug` is redacted.
#[derive(Clone, PartialEq, Eq)]
pub struct RawCredential(Zeroizing<Vec<u8>>);

impl RawCredential {
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for RawCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RawCredential(<redacted, {} bytes>)", self.0.len())
    }
}

impl Serialize for RawCredential {
    fn serialize<S: Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&B64.encode(&*self.0))
    }
}

impl<'de> Deserialize<'de> for RawCredential {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let text = Zeroizing::new(String::deserialize(d)?);
        let bytes = B64.decode(text.as_bytes()).map_err(D::Error::custom)?;
        Ok(Self::new(bytes))
    }
}

/// Non-secret facts extracted from a credential document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialInfo {
    /// Stable OIDC subject, when the credential carries one.
    pub sub: Option<String>,
    pub email: Option<String>,
    /// Normalized identity used for `account_id` and dedupe. Private so the
    /// namespaced inputs cannot be mistaken for a subject or email.
    identity: Identity,
    pub auth_method: String,
    pub expiry: Option<DateTime<Utc>>,
    pub has_refresh_token: bool,
    pub project_id: Option<String>,
    pub region: Option<String>,
}

/// What the credential is identified by, in precedence order.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Identity {
    /// A usable `id_token` `sub` (hashed as-is, so ids do not change).
    Subject(String),
    /// `email:<lowercase>` from a JWT email claim or `id_token_email`.
    Email(String),
    /// `refresh:<sha256 hex>` when nothing else identifies the login.
    Refresh(String),
}

impl Identity {
    fn input(&self) -> &str {
        match self {
            Self::Subject(s) => s,
            Self::Email(e) | Self::Refresh(e) => e,
        }
    }
}

impl CredentialInfo {
    pub fn account_id(&self) -> String {
        account_id_for_sub(self.identity.input())
    }

    /// Normalized email for dedupe, when the credential has one.
    pub fn normalized_email(&self) -> Option<String> {
        self.email.as_deref().map(str::to_lowercase)
    }

    pub fn is_gcp(&self) -> bool {
        self.auth_method == "gcp" || self.project_id.is_some()
    }
}

/// Account ID: first 10 hex characters of SHA-256(sub).
pub fn account_id_for_sub(sub: &str) -> String {
    let digest = Sha256::digest(sub.as_bytes());
    hex::encode(digest)[..10].to_owned()
}

fn incompatible(why: &str) -> Error {
    Error::new(
        ErrorKind::Incompatible,
        format!("the credential does not look like an agy 1.5.x login: {why}"),
    )
    .hint("agyva only handles formats it understands; try `agyva doctor`, or re-login with `agy`")
}

fn non_empty_str<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// Strictly validate the credential document and extract its identity.
/// Never echoes any part of the document in errors.
pub fn parse_credential(raw: &[u8]) -> Result<CredentialInfo> {
    if raw.is_empty() || raw.len() > MAX_CREDENTIAL_BYTES {
        return Err(incompatible("unexpected size"));
    }
    let doc: serde_json::Value =
        serde_json::from_slice(raw).map_err(|_| incompatible("not valid JSON"))?;
    if !doc.is_object() {
        return Err(incompatible("not a JSON object"));
    }
    let token = doc
        .get("token")
        .filter(|t| t.is_object())
        .ok_or_else(|| incompatible("missing `token` object"))?;
    let access = non_empty_str(token, "access_token");
    let refresh = non_empty_str(token, "refresh_token");
    if access.is_none() && refresh.is_none() {
        return Err(incompatible(
            "`token` has neither access_token nor refresh_token",
        ));
    }
    // `id_token` is optional, and a malformed one is not fatal: agy itself
    // treats it as optional and consumer logins usually omit it. Only a
    // decodable token contributes claims, and `id_token_email` is never handed
    // to the JWT decoder (it is a plain email, dots included).
    let claims = non_empty_str(&doc, "id_token").and_then(|t| decode_jwt_claims(t).ok());
    let sub = claims
        .as_ref()
        .and_then(|c| non_empty_str(c, "sub"))
        .filter(|s| s.len() <= 256)
        .map(str::to_owned);
    let jwt_email = claims
        .as_ref()
        .and_then(|c| non_empty_str(c, "email"))
        .and_then(valid_email);
    let plain_email = non_empty_str(&doc, "id_token_email").and_then(valid_email);
    let email = jwt_email.or(plain_email);
    let identity = if let Some(sub) = &sub {
        Identity::Subject(sub.clone())
    } else if let Some(email) = &email {
        Identity::Email(format!("email:{}", email.to_lowercase()))
    } else if let Some(refresh) = refresh {
        Identity::Refresh(format!(
            "refresh:{}",
            hex::encode(Sha256::digest(refresh.as_bytes()))
        ))
    } else {
        return Err(incompatible(
            "it has no id_token subject or email, no valid id_token_email, and no refresh_token to identify it",
        ));
    };
    // Expiry is informational. The observed agy format (RFC3339 with nine
    // fractional digits and a numeric offset) parses; anything else is simply
    // unknown and must not make the credential unusable.
    let expiry = non_empty_str(token, "expiry")
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|d| d.with_timezone(&Utc));
    Ok(CredentialInfo {
        sub,
        email,
        identity,
        auth_method: non_empty_str(&doc, "auth_method")
            .unwrap_or("unknown")
            .to_owned(),
        expiry,
        has_refresh_token: refresh.is_some(),
        project_id: non_empty_str(&doc, "project_id").map(str::to_owned),
        region: non_empty_str(&doc, "region").map(str::to_owned),
    })
}

/// A plain email: trimmed, one `@`, non-empty local and domain, no control
/// characters. Returns it unchanged (identity and dedupe lowercase it).
fn valid_email(s: &str) -> Option<String> {
    let s = s.trim();
    if s.chars().any(char::is_control) {
        return None;
    }
    let mut parts = s.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return None;
    };
    (!local.is_empty() && !domain.is_empty()).then(|| s.to_owned())
}

/// Decode the payload of a JWT *without verifying the signature*. The token
/// is read back from local storage, not received from a peer; the result is
/// only used as an identity label.
fn decode_jwt_claims(jwt: &str) -> Result<serde_json::Value> {
    let mut parts = jwt.split('.');
    let (_header, Some(payload)) = (parts.next(), parts.next()) else {
        return Err(incompatible("id_token is not a JWT"));
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .map_err(|_| incompatible("id_token payload is not base64url"))?;
    let claims: serde_json::Value =
        serde_json::from_slice(&bytes).map_err(|_| incompatible("id_token payload is not JSON"))?;
    if claims.is_object() {
        Ok(claims)
    } else {
        Err(incompatible("id_token payload is not an object"))
    }
}

// ---------------------------------------------------------------- backends

pub trait NativeBackend: Send + Sync + fmt::Debug {
    fn name(&self) -> &'static str;
    fn read(&self) -> Result<Option<RawCredential>>;
    fn write(&self, raw: &[u8]) -> Result<()>;
    /// Remove the credential. Missing is not an error.
    fn delete(&self) -> Result<()>;
}

/// A timed-out write/delete may have been applied: callers must not assume
/// either outcome (see `Error::outcome_unknown`).
fn unknown_if_timeout(e: StoreError) -> Error {
    if e.kind == StoreErrorKind::Timeout {
        Error::outcome_unknown(
            "the OS keyring did not confirm the change to agy's login; it may or may not have been applied",
        )
    } else {
        e.into()
    }
}

/// Linux Secret Service item `service=gemini, username=antigravity`.
#[derive(Debug, Clone)]
pub struct SecretServiceBackend {
    store: Arc<dyn SecretStore>,
}

impl SecretServiceBackend {
    pub fn new(store: Arc<dyn SecretStore>) -> Self {
        Self { store }
    }
}

impl NativeBackend for SecretServiceBackend {
    fn name(&self) -> &'static str {
        "secret-service"
    }

    fn read(&self) -> Result<Option<RawCredential>> {
        Ok(self
            .store
            .get(SERVICE, USER)?
            .map(|b| RawCredential::new(b.to_vec())))
    }

    fn write(&self, raw: &[u8]) -> Result<()> {
        // UNVERIFIED against real agy: attributes of a newly created item
        // (service, username, label, xdg:schema) mirror go-keyring's output.
        self.store
            .set(
                SERVICE,
                USER,
                raw,
                &SetOptions {
                    label: Some(ITEM_LABEL.into()),
                    xdg_schema: Some(XDG_SCHEMA.into()),
                },
            )
            .map_err(unknown_if_timeout)
    }

    fn delete(&self) -> Result<()> {
        self.store
            .delete(SERVICE, USER)
            .map(|_| ())
            .map_err(unknown_if_timeout)
    }
}

/// `~/.gemini/antigravity-cli/antigravity-oauth-token`.
#[derive(Debug, Clone)]
pub struct FileBackend {
    path: PathBuf,
}

impl FileBackend {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn exists(&self) -> bool {
        self.path.exists()
    }
}

impl NativeBackend for FileBackend {
    fn name(&self) -> &'static str {
        "file"
    }

    fn read(&self) -> Result<Option<RawCredential>> {
        let bytes = fsutil::read_optional(&self.path)
            .map_err(|e| Error::io(format!("reading {}", self.path.display()), e))?;
        Ok(bytes
            .filter(|b| !b.iter().all(u8::is_ascii_whitespace))
            .map(RawCredential::new))
    }

    fn write(&self, raw: &[u8]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            fsutil::create_dir_with_mode(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        fsutil::atomic_write(&self.path, raw)
            .map_err(|e| Error::io(format!("writing {}", self.path.display()), e))
    }

    fn delete(&self) -> Result<()> {
        fsutil::remove_durable(&self.path)
            .map(|_| ())
            .map_err(|e| Error::io(format!("removing {}", self.path.display()), e))
    }
}

/// In-memory backend for tests, with failure injection.
#[derive(Debug, Clone, Default)]
pub struct MemoryBackend {
    inner: Arc<Mutex<MemoryState>>,
}

#[derive(Debug, Default)]
struct MemoryState {
    value: Option<Vec<u8>>,
    fail_read: bool,
    fail_write: bool,
}

impl MemoryBackend {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_fail_read(&self, fail: bool) {
        if let Ok(mut s) = self.inner.lock() {
            s.fail_read = fail;
        }
    }

    pub fn set_fail_write(&self, fail: bool) {
        if let Ok(mut s) = self.inner.lock() {
            s.fail_write = fail;
        }
    }

    fn state(&self) -> Result<std::sync::MutexGuard<'_, MemoryState>> {
        self.inner
            .lock()
            .map_err(|_| Error::backend("memory backend poisoned"))
    }
}

impl NativeBackend for MemoryBackend {
    fn name(&self) -> &'static str {
        "memory"
    }

    fn read(&self) -> Result<Option<RawCredential>> {
        let s = self.state()?;
        if s.fail_read {
            return Err(Error::backend("injected native read failure"));
        }
        Ok(s.value.clone().map(RawCredential::new))
    }

    fn write(&self, raw: &[u8]) -> Result<()> {
        let mut s = self.state()?;
        if s.fail_write {
            return Err(Error::backend("injected native write failure"));
        }
        s.value = Some(raw.to_vec());
        Ok(())
    }

    fn delete(&self) -> Result<()> {
        self.state()?.value = None;
        Ok(())
    }
}

/// The primary backend plus an optional mirror: when agy's token *file*
/// already exists next to a keyring primary, it is kept in sync (never
/// created).
#[derive(Debug, Clone)]
pub struct NativeAuth {
    primary: Arc<dyn NativeBackend>,
    mirror: Option<FileBackend>,
}

impl NativeAuth {
    pub fn new(primary: Arc<dyn NativeBackend>, mirror: Option<FileBackend>) -> Self {
        Self { primary, mirror }
    }

    pub fn backend_name(&self) -> &'static str {
        self.primary.name()
    }

    pub fn read(&self) -> Result<Option<RawCredential>> {
        self.primary.read()
    }

    fn live_mirror(&self) -> Option<&FileBackend> {
        self.mirror.as_ref().filter(|m| m.exists())
    }

    pub fn write(&self, raw: &[u8]) -> Result<()> {
        self.primary.write(raw)?;
        if let Some(m) = self.live_mirror() {
            m.write(raw)?;
        }
        Ok(())
    }

    pub fn delete(&self) -> Result<()> {
        self.primary.delete()?;
        if let Some(m) = self.live_mirror() {
            m.delete()?;
        }
        Ok(())
    }

    /// Read back (no network) and confirm the store now holds exactly `expected`.
    pub fn verify(&self, expected: &[u8]) -> Result<()> {
        let back = self.primary.read()?;
        if back.as_ref().map(RawCredential::as_bytes) != Some(expected) {
            return Err(Error::backend(
                "read-back verification failed: the native store does not hold the written credential",
            ));
        }
        if let Some(m) = self.live_mirror() {
            let back = m.read()?;
            if back.as_ref().map(RawCredential::as_bytes) != Some(expected) {
                return Err(Error::backend(
                    "read-back verification failed for the token file mirror",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_synthetic_credential() {
        let raw = crate::testing::credential("sub-1", "a@example.com", "t1");
        let info = parse_credential(&raw).unwrap();
        assert_eq!(info.sub.as_deref(), Some("sub-1"));
        assert_eq!(info.email.as_deref(), Some("a@example.com"));
        assert_eq!(info.account_id().len(), 10);
        assert!(info.has_refresh_token);
    }

    #[test]
    fn consumer_blob_without_id_token_identifies_by_email() {
        let raw = crate::testing::consumer_credential("First.Last@Example.Invalid", "c1");
        let info = parse_credential(&raw).unwrap();
        assert_eq!(info.sub, None);
        assert_eq!(info.email.as_deref(), Some("First.Last@Example.Invalid"));
        assert_eq!(info.auth_method, "consumer");
        assert!(info.has_refresh_token);
        assert_eq!(
            info.account_id(),
            account_id_for_sub("email:first.last@example.invalid")
        );
        // The observed expiry format (9 fractional digits, numeric offset)
        // parses; it is metadata, never a reason to refuse.
        assert!(info.expiry.is_some());
    }

    #[test]
    fn an_unparseable_expiry_is_unknown_not_fatal() {
        let mut doc: serde_json::Value = serde_json::from_slice(
            &crate::testing::consumer_credential("e@example.invalid", "x1"),
        )
        .expect("json");
        doc["token"]["expiry"] = "whenever".into();
        let info = parse_credential(doc.to_string().as_bytes()).unwrap();
        assert_eq!(info.expiry, None);
        assert_eq!(info.email.as_deref(), Some("e@example.invalid"));
    }

    #[test]
    fn a_dotted_id_token_email_is_not_decoded_as_a_jwt() {
        let raw = crate::testing::consumer_credential("first.last@example.invalid", "c2");
        assert_eq!(parse_credential(&raw).unwrap().account_id().len(), 10);
    }

    #[test]
    fn malformed_id_token_falls_back_to_email() {
        let raw = crate::testing::malformed_id_token_credential("a.b@example.invalid", "m1");
        let info = parse_credential(&raw).unwrap();
        assert_eq!(info.email.as_deref(), Some("a.b@example.invalid"));
        assert_eq!(
            info.account_id(),
            account_id_for_sub("email:a.b@example.invalid")
        );
    }

    #[test]
    fn without_sub_or_email_the_refresh_token_is_the_identity() {
        let raw = crate::testing::unidentified_credential("r1");
        let a = parse_credential(&raw).unwrap();
        let b = parse_credential(&raw).unwrap();
        assert_eq!(a.sub, None);
        assert_eq!(a.email, None);
        assert_eq!(a.account_id(), b.account_id(), "stable fingerprint");
        assert_eq!(a.account_id().len(), 10);
    }

    #[test]
    fn a_credential_with_nothing_to_identify_it_is_refused() {
        let raw = br#"{"token":{"access_token":"access-x"},"auth_method":"consumer"}"#;
        let err = parse_credential(raw).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::Incompatible);
        assert!(err.hint_text().is_some_and(|h| h.contains("doctor")));
        assert!(!err.message().contains("access-x"));
    }

    #[test]
    fn gcp_shaped_blob_still_parses() {
        let raw = crate::testing::credential_with(
            "sub-gcp",
            "g@example.invalid",
            "g1",
            "gcp",
            &[("project_id", "proj-1"), ("region", "us-central1")],
        );
        let info = parse_credential(&raw).unwrap();
        assert!(info.is_gcp());
        assert_eq!(info.project_id.as_deref(), Some("proj-1"));
        assert_eq!(info.region.as_deref(), Some("us-central1"));
    }

    #[test]
    fn rejects_garbled_shapes_without_echoing_content() {
        for bad in [
            &b""[..],
            b"not json",
            b"[]",
            b"{}",
            br#"{"token":{}}"#,
            br#"{"token":{"access_token":"SECRETVALUE"}}"#,
            br#"{"token":{"access_token":"SECRETVALUE"},"id_token":"nope"}"#,
            br#"{"token":{"access_token":"SECRETVALUE"},"id_token":"a.!!!.c"}"#,
        ] {
            let err = parse_credential(bad).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::Incompatible);
            assert!(!err.message().contains("SECRETVALUE"));
        }
    }

    #[test]
    fn debug_is_redacted() {
        let raw = RawCredential::new(b"TOP-SECRET".to_vec());
        assert!(!format!("{raw:?}").contains("TOP-SECRET"));
    }
}
