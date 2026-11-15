//! Encrypted blob storage.
//!
//! Envelope layout (all fields fixed-width, then ciphertext):
//!
//! ```text
//! magic "AGYV" | version u8 | alg u8 | nonce[12] | ciphertext || tag
//! ```
//!
//! The cipher is AES-256-GCM with a fresh random 96-bit nonce per seal. The
//! associated data binds the envelope version, algorithm and a caller-chosen
//! context string (for snapshots: `snapshot:<account id>`), so a blob cannot
//! be moved to another account or file role without failing authentication.

use std::fmt;
use std::path::Path;

use aes_gcm::aead::{Aead, Payload};
use aes_gcm::{Aes256Gcm, Key, KeyInit, Nonce};
use zeroize::{Zeroize, Zeroizing};

use super::fsutil;
use crate::core::error::{Error, Result};

pub const MAGIC: &[u8; 4] = b"AGYV";
pub const ENVELOPE_VERSION: u8 = 1;
pub const ALG_AES256_GCM: u8 = 1;
const NONCE_LEN: usize = 12;
const HEADER_LEN: usize = 4 + 1 + 1 + NONCE_LEN;
const TAG_LEN: usize = 16;

/// 256-bit vault key. Zeroed on drop; `Debug` never prints key material.
#[derive(Clone)]
pub struct VaultKey(Zeroizing<[u8; 32]>);

impl VaultKey {
    pub fn generate() -> Result<Self> {
        let mut bytes = Zeroizing::new([0u8; 32]);
        getrandom::fill(bytes.as_mut_slice())
            .map_err(|e| Error::vault(format!("system randomness unavailable: {e}")))?;
        Ok(Self(bytes))
    }

    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(Zeroizing::new(bytes))
    }

    pub(crate) fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Debug for VaultKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("VaultKey(<redacted>)")
    }
}

fn aad(version: u8, alg: u8, context: &str) -> Vec<u8> {
    let mut a = Vec::with_capacity(16 + context.len());
    a.extend_from_slice(b"agyva-vault\0");
    a.push(version);
    a.push(alg);
    a.extend_from_slice(context.as_bytes());
    a
}

/// Encrypt `plaintext` under `key`, binding `context`.
pub fn seal(key: &VaultKey, context: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key.as_bytes()));
    let mut nonce_bytes = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce_bytes)
        .map_err(|e| Error::vault(format!("system randomness unavailable: {e}")))?;
    let ct = cipher
        .encrypt(
            &Nonce::from(nonce_bytes),
            Payload {
                msg: plaintext,
                aad: &aad(ENVELOPE_VERSION, ALG_AES256_GCM, context),
            },
        )
        .map_err(|_| Error::vault("encryption failed"))?;
    let mut out = Vec::with_capacity(HEADER_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.push(ENVELOPE_VERSION);
    out.push(ALG_AES256_GCM);
    out.extend_from_slice(&nonce_bytes);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt an envelope produced by [`seal`]. Any structural problem,
/// unsupported version, wrong key, wrong context or tampering is an error.
pub fn open(key: &VaultKey, context: &str, envelope: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
    if envelope.len() < HEADER_LEN + TAG_LEN || &envelope[..4] != MAGIC {
        return Err(Error::format(
            "not an agyva vault file (bad magic or truncated)",
        ));
    }
    let (version, alg) = (envelope[4], envelope[5]);
    if version != ENVELOPE_VERSION || alg != ALG_AES256_GCM {
        return Err(Error::format(format!(
            "unsupported vault envelope (version {version}, algorithm {alg})"
        ))
        .hint("this file was written by a newer agyva; upgrade agyva"));
    }
    let nonce = &envelope[6..HEADER_LEN];
    let cipher = Aes256Gcm::new(&Key::<Aes256Gcm>::from(*key.as_bytes()));
    cipher
        .decrypt(
            &Nonce::try_from(nonce).map_err(|_| Error::format("bad nonce length"))?,
            Payload { msg: &envelope[HEADER_LEN..], aad: &aad(version, alg, context) },
        )
        .map(Zeroizing::new)
        .map_err(|_| {
            Error::vault("decryption failed: wrong key, wrong file, or corrupted data")
                .hint("the file may be damaged, or the vault key/passphrase differs from the one that created it")
        })
}

/// A vault bound to a key. Methods read and write encrypted files.
#[derive(Debug, Clone)]
pub struct Vault {
    key: VaultKey,
}

impl Vault {
    pub fn new(key: VaultKey) -> Self {
        Self { key }
    }

    pub fn seal(&self, context: &str, plaintext: &[u8]) -> Result<Vec<u8>> {
        seal(&self.key, context, plaintext)
    }

    pub fn open(&self, context: &str, envelope: &[u8]) -> Result<Zeroizing<Vec<u8>>> {
        open(&self.key, context, envelope)
    }

    /// Encrypt and atomically write `plaintext` to `path` (mode 0600).
    pub fn write_file(&self, path: &Path, context: &str, plaintext: &[u8]) -> Result<()> {
        let sealed = self.seal(context, plaintext)?;
        if let Some(dir) = path.parent() {
            fsutil::create_private_dir(dir)
                .map_err(|e| Error::io(format!("creating {}", dir.display()), e))?;
        }
        fsutil::atomic_write(path, &sealed)
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))
    }

    /// Read and decrypt `path`; `Ok(None)` when it does not exist.
    pub fn read_file(&self, path: &Path, context: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
        let Some(bytes) = fsutil::read_optional(path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
        else {
            return Ok(None);
        };
        self.open(context, &bytes).map(Some)
    }
}

impl Drop for VaultKey {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_failures() {
        let k1 = VaultKey::generate().unwrap();
        let k2 = VaultKey::generate().unwrap();
        let sealed = seal(&k1, "snapshot:abc", b"secret-bytes").unwrap();
        assert_eq!(
            &**open(&k1, "snapshot:abc", &sealed).unwrap(),
            b"secret-bytes"
        );
        assert!(open(&k2, "snapshot:abc", &sealed).is_err(), "wrong key");
        assert!(
            open(&k1, "snapshot:other", &sealed).is_err(),
            "wrong context"
        );
        let mut bad = sealed.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(
            open(&k1, "snapshot:abc", &bad).is_err(),
            "tampered ciphertext"
        );
        let mut bad = sealed.clone();
        bad[4] = 9;
        assert!(open(&k1, "snapshot:abc", &bad).is_err(), "unknown version");
        assert!(
            open(&k1, "snapshot:abc", &sealed[..10]).is_err(),
            "truncated"
        );
    }

    #[test]
    fn nonces_are_unique() {
        let k = VaultKey::generate().unwrap();
        let a = seal(&k, "c", b"x").unwrap();
        let b = seal(&k, "c", b"x").unwrap();
        assert_ne!(a[6..18], b[6..18]);
    }

    #[test]
    fn key_debug_is_redacted() {
        let k = VaultKey::from_bytes([7u8; 32]);
        assert_eq!(format!("{k:?}"), "VaultKey(<redacted>)");
    }
}
