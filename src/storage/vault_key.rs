//! Provisioning of the vault key.
//!
//! Two modes, recorded in `vault.json` (public metadata, no secrets):
//!
//! * `keyring`: a random 32-byte key stored in the OS keyring
//!   (`agyva` / `vault-key`).
//! * `passphrase`: the key is derived with Argon2id from a passphrase taken
//!   from the TTY or `AGYVA_PASSPHRASE` (never argv). The salt lives in
//!   `vault.json`.
//!
//! There is deliberately no plaintext key file.

use std::io::IsTerminal;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use super::fsutil;
use super::keyring::{SecretStore, SetOptions};
use super::vault::{VaultKey, open, seal};
use crate::config::VaultKeyMode;
use crate::core::error::{Error, Result};
use crate::paths::Dirs;

pub const KEY_SERVICE: &str = "agyva";
pub const KEY_USER: &str = "vault-key";
pub const ENV_PASSPHRASE: &str = "AGYVA_PASSPHRASE";
/// Passphrase for import/export archives. Separate from the vault
/// passphrase: an archive must never unlock the vault or vice versa.
pub const ENV_ARCHIVE_PASSPHRASE: &str = "AGYVA_ARCHIVE_PASSPHRASE";
const META_VERSION: u32 = 1;
const VERIFIER_CONTEXT: &str = "vault-verifier";
const VERIFIER_PLAINTEXT: &[u8] = b"agyva vault ok";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    pub alg: String,
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
    pub salt: String,
}

/// Sane bounds for KDF parameters read from `vault.json`. The file is
/// unauthenticated, so an edited (or corrupt) value must not be able to make
/// agyva allocate gigabytes or spin for minutes before the verifier can fail.
pub const KDF_M_KIB: std::ops::RangeInclusive<u32> = 8..=1_048_576;
pub const KDF_T: std::ops::RangeInclusive<u32> = 1..=16;
pub const KDF_P: std::ops::RangeInclusive<u32> = 1..=16;
pub const KDF_SALT_BYTES: std::ops::RangeInclusive<usize> = 8..=64;
/// Ceiling for memory cost x passes (`m_kib * t`), in KiB-passes. Each bound
/// above is sane on its own, but the product is what bounds the work: 1 GiB
/// with 16 passes would spin for a very long time before failing.
pub const KDF_MAX_COMBINED_COST: u64 = 1_048_576 * 4;
/// Shortest passphrase accepted when a vault is *created*.
pub const MIN_PASSPHRASE_CHARS: usize = 8;

impl KdfParams {
    /// Reject out-of-range parameters as a corrupt `vault.json`.
    pub fn validate(&self) -> Result<()> {
        let corrupt = |what: &str| {
            Error::format(format!("vault.json is corrupt: KDF {what} is out of range"))
                .hint("restore vault.json from a backup")
        };
        if self.alg != "argon2id" {
            return Err(Error::format(format!("unsupported KDF `{}`", self.alg)));
        }
        if !KDF_M_KIB.contains(&self.m_kib) {
            return Err(corrupt("memory cost"));
        }
        if !KDF_T.contains(&self.t) {
            return Err(corrupt("time cost"));
        }
        if !KDF_P.contains(&self.p) {
            return Err(corrupt("parallelism"));
        }
        let combined = u64::from(self.m_kib) * u64::from(self.t);
        if combined > KDF_MAX_COMBINED_COST {
            return Err(corrupt("memory x time cost"));
        }
        let salt = B64
            .decode(&self.salt)
            .map_err(|_| Error::format("vault.json: salt is not valid base64"))?;
        if !KDF_SALT_BYTES.contains(&salt.len()) {
            return Err(corrupt("salt length"));
        }
        Ok(())
    }

    /// OWASP-recommended Argon2id floor (19 MiB, t=2, p=1) with a fresh salt.
    pub fn production() -> Result<Self> {
        Self::with_cost(19 * 1024, 2, 1)
    }

    pub fn with_cost(m_kib: u32, t: u32, p: u32) -> Result<Self> {
        let mut salt = [0u8; 16];
        getrandom::fill(&mut salt)
            .map_err(|e| Error::vault(format!("system randomness unavailable: {e}")))?;
        Ok(Self {
            alg: "argon2id".into(),
            m_kib,
            t,
            p,
            salt: B64.encode(salt),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VaultMeta {
    pub schema_version: u32,
    /// `"keyring"` or `"passphrase"`.
    pub mode: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kdf: Option<KdfParams>,
    /// Encrypted known plaintext, used to detect a wrong key early.
    pub verifier: String,
}

/// Source of passphrases.
pub trait PassphraseSource: Send + Sync {
    fn passphrase(&self, confirm: bool) -> Result<SecretString>;
}

/// `AGYVA_PASSPHRASE`, else an interactive no-echo prompt on the TTY.
pub struct EnvOrTty {
    env_passphrase: Option<SecretString>,
    /// Prompt label; also names the environment variable in errors.
    env_name: &'static str,
    label: &'static str,
}

impl EnvOrTty {
    /// `env_passphrase`: the value of `AGYVA_PASSPHRASE`, already taken out
    /// of the environment map.
    pub fn new(env_passphrase: Option<SecretString>) -> Self {
        Self {
            env_passphrase,
            env_name: ENV_PASSPHRASE,
            label: "Vault passphrase: ",
        }
    }

    /// Like [`EnvOrTty::new`] with the archive prompt and variable name.
    pub fn for_archives(env_passphrase: Option<SecretString>) -> Self {
        Self {
            env_passphrase,
            env_name: ENV_ARCHIVE_PASSPHRASE,
            label: "Archive passphrase: ",
        }
    }
}

impl PassphraseSource for EnvOrTty {
    fn passphrase(&self, confirm: bool) -> Result<SecretString> {
        if let Some(p) = &self.env_passphrase {
            return Ok(p.clone());
        }
        if !std::io::stdin().is_terminal() {
            return Err(Error::vault(format!(
                "a passphrase is required but no terminal is available (set {})",
                self.env_name
            ))
            .hint(format!("run interactively, or set {}", self.env_name)));
        }
        let first = rpassword::prompt_password(self.label)
            .map_err(|e| Error::io("reading passphrase", e))?;
        let first = SecretString::from(first);
        if first.expose_secret().is_empty() {
            return Err(Error::vault("empty passphrase refused"));
        }
        if confirm {
            let again = SecretString::from(
                rpassword::prompt_password("Repeat passphrase: ")
                    .map_err(|e| Error::io("reading passphrase", e))?,
            );
            if again.expose_secret() != first.expose_secret() {
                return Err(Error::vault("passphrases do not match"));
            }
        }
        Ok(first)
    }
}

/// Fixed passphrase (tests).
pub struct StaticPassphrase(pub String);

impl PassphraseSource for StaticPassphrase {
    fn passphrase(&self, _confirm: bool) -> Result<SecretString> {
        Ok(SecretString::from(self.0.clone()))
    }
}

pub fn derive_key(passphrase: &SecretString, kdf: &KdfParams) -> Result<VaultKey> {
    use argon2::{Algorithm, Argon2, Params, Version};
    kdf.validate()?;
    let salt = B64
        .decode(&kdf.salt)
        .map_err(|_| Error::format("vault.json: salt is not valid base64"))?;
    let params = Params::new(kdf.m_kib, kdf.t, kdf.p, Some(32))
        .map_err(|e| Error::format(format!("vault.json: invalid KDF parameters: {e}")))?;
    let mut out = Zeroizing::new([0u8; 32]);
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(
            passphrase.expose_secret().as_bytes(),
            &salt,
            out.as_mut_slice(),
        )
        .map_err(|e| Error::vault(format!("key derivation failed: {e}")))?;
    Ok(VaultKey::from_bytes(*out))
}

/// Opens (or creates) the vault key.
pub struct KeyProvider {
    dirs: Dirs,
    store: Arc<dyn SecretStore>,
    passphrases: Arc<dyn PassphraseSource>,
    preference: VaultKeyMode,
    kdf: Option<KdfParams>,
}

impl KeyProvider {
    pub fn new(
        dirs: Dirs,
        store: Arc<dyn SecretStore>,
        passphrases: Arc<dyn PassphraseSource>,
        preference: VaultKeyMode,
    ) -> Self {
        Self {
            dirs,
            store,
            passphrases,
            preference,
            kdf: None,
        }
    }

    /// Override KDF cost used when *creating* a passphrase vault (tests).
    #[must_use]
    pub fn with_kdf(mut self, kdf: KdfParams) -> Self {
        self.kdf = Some(kdf);
        self
    }

    pub fn load_meta(&self) -> Result<Option<VaultMeta>> {
        let path = self.dirs.vault_meta_file();
        let Some(bytes) = fsutil::read_optional(&path)
            .map_err(|e| Error::io(format!("reading {}", path.display()), e))?
        else {
            return Ok(None);
        };
        let meta: VaultMeta = serde_json::from_slice(&bytes)
            .map_err(|e| Error::format(format!("{} is corrupt: {e}", path.display())))?;
        if meta.schema_version != META_VERSION {
            return Err(Error::format(format!(
                "vault.json schema {} is not supported (expected {META_VERSION})",
                meta.schema_version
            ))
            .hint("upgrade agyva"));
        }
        if let Some(kdf) = &meta.kdf {
            kdf.validate()?;
        }
        Ok(Some(meta))
    }

    /// Unlock the existing vault. Fails if none exists.
    pub fn unlock(&self) -> Result<VaultKey> {
        let meta = self.load_meta()?.ok_or_else(|| {
            Error::refused("no vault exists yet").hint("run `agyva add` to save your first account")
        })?;
        let key = match meta.mode.as_str() {
            "keyring" => self.keyring_key()?,
            "passphrase" => {
                let kdf = meta.kdf.as_ref().ok_or_else(|| {
                    Error::format("vault.json: passphrase vault without KDF parameters")
                })?;
                derive_key(&self.passphrases.passphrase(false)?, kdf)?
            }
            other => {
                return Err(Error::format(format!(
                    "vault.json: unknown key mode `{other}`"
                )));
            }
        };
        let verifier = B64
            .decode(&meta.verifier)
            .map_err(|_| Error::format("vault.json: verifier is not valid base64"))?;
        match open(&key, VERIFIER_CONTEXT, &verifier) {
            Ok(_) => Ok(key),
            Err(_) if meta.mode == "passphrase" => Err(Error::vault("wrong passphrase")),
            Err(_) => Err(
                Error::vault("the keyring vault key does not match this vault")
                    .hint("the key was replaced or the vault files came from another machine"),
            ),
        }
    }

    /// Unlock, creating the vault on first use.
    pub fn unlock_or_create(&self) -> Result<VaultKey> {
        if self.load_meta()?.is_some() {
            return self.unlock();
        }
        let orphans = fsutil::list_with_extension(&self.dirs.accounts_dir(), "snap")
            .map_err(|e| Error::io("scanning saved accounts", e))?;
        if !orphans.is_empty() {
            return Err(
                Error::vault("saved accounts exist but vault.json is missing")
                    .hint("restore vault.json from a backup; creating a new key would orphan them"),
            );
        }
        fsutil::create_private_dir(&self.dirs.config)
            .map_err(|e| Error::io("creating config directory", e))?;
        match self.preference {
            VaultKeyMode::Keyring => self.create_keyring(),
            VaultKeyMode::Passphrase => self.create_passphrase(),
            VaultKeyMode::Auto => match self.create_keyring() {
                Ok(k) => Ok(k),
                Err(e) => {
                    tracing::info!(reason = %e.message(), "keyring unavailable, using passphrase vault");
                    eprintln!(
                        "note: OS keyring unavailable ({}); using a passphrase-protected vault",
                        e.message()
                    );
                    self.create_passphrase()
                }
            },
        }
    }

    fn keyring_key(&self) -> Result<VaultKey> {
        let raw = self.store.get(KEY_SERVICE, KEY_USER)?.ok_or_else(|| {
            Error::vault("the vault key is missing from the OS keyring")
                .hint("saved snapshots cannot be decrypted without it; restore the keyring entry `agyva`/`vault-key`")
        })?;
        let text = std::str::from_utf8(&raw)
            .map_err(|_| Error::vault("vault key in keyring is malformed"))?;
        let bytes = Zeroizing::new(
            B64.decode(text.trim())
                .map_err(|_| Error::vault("vault key in keyring is malformed"))?,
        );
        let arr: [u8; 32] = bytes
            .as_slice()
            .try_into()
            .map_err(|_| Error::vault("vault key in keyring has the wrong length"))?;
        Ok(VaultKey::from_bytes(arr))
    }

    fn write_meta(&self, meta: &VaultMeta) -> Result<()> {
        let json = serde_json::to_vec_pretty(meta).map_err(|e| Error::format(e.to_string()))?;
        let path = self.dirs.vault_meta_file();
        fsutil::atomic_write(&path, &json)
            .map_err(|e| Error::io(format!("writing {}", path.display()), e))
    }

    fn create_keyring(&self) -> Result<VaultKey> {
        let key = VaultKey::generate()?;
        let encoded = Zeroizing::new(B64.encode(key.as_bytes()));
        self.store.set(
            KEY_SERVICE,
            KEY_USER,
            encoded.as_bytes(),
            &SetOptions {
                label: Some("agyva vault key".into()),
                xdg_schema: Some("org.freedesktop.Secret.Generic".into()),
            },
        )?;
        let back = self.keyring_key()?;
        if back.as_bytes() != key.as_bytes() {
            return Err(Error::vault(
                "keyring read-back mismatch while creating the vault key",
            ));
        }
        let verifier = B64.encode(seal(&key, VERIFIER_CONTEXT, VERIFIER_PLAINTEXT)?);
        self.write_meta(&VaultMeta {
            schema_version: META_VERSION,
            mode: "keyring".into(),
            kdf: None,
            verifier,
        })?;
        Ok(key)
    }

    fn create_passphrase(&self) -> Result<VaultKey> {
        let kdf = match &self.kdf {
            Some(k) => k.clone(),
            None => KdfParams::production()?,
        };
        let passphrase = self.passphrases.passphrase(true)?;
        if passphrase.expose_secret().chars().count() < MIN_PASSPHRASE_CHARS {
            return Err(Error::vault(format!(
                "the passphrase is too short (minimum {MIN_PASSPHRASE_CHARS} characters)"
            ))
            .hint("choose a longer passphrase; a passphrase vault is only as strong as it is"));
        }
        let key = derive_key(&passphrase, &kdf)?;
        let verifier = B64.encode(seal(&key, VERIFIER_CONTEXT, VERIFIER_PLAINTEXT)?);
        self.write_meta(&VaultMeta {
            schema_version: META_VERSION,
            mode: "passphrase".into(),
            kdf: Some(kdf),
            verifier,
        })?;
        Ok(key)
    }
}
