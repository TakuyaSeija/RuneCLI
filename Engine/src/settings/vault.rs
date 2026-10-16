//! At-rest encryption for credentials, with the master key stored in ~/.rune/master.key.
//!
//! Rune's API keys used to sit in `~/.rune/secrets.json` as plaintext JSON — readable by
//! anything that could open the file. This vault moves them into `secrets.vault` as opaque
//! binary blobs: each payload is AES-256-GCM-encrypted with a per-record random nonce,
//! and the 32-byte master key lives in `master.key` (mode 0600), never in plaintext JSON.
//!
//! Threat model, stated honestly: this stops a plaintext leak of `secrets.json` — a backup,
//! an accidental `cat`, a stolen disk image, or unprivileged access by another user/service
//! on the host (guarded by 0600 mode).
//!
//! No home-grown crypto: `aes-gcm` for the cipher, OS entropy for the key.

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;

use crate::error::{Error, Result};

/// Master key filename stored inside Rune's home directory.
const KEY_FILE: &str = "master.key";

/// AES-256 key length in bytes, and GCM standard nonce length.
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

/// The shortest blob [`Vault::seal`] writes: a nonce and a tag around nothing.
pub const SEALED_MIN: usize = NONCE_LEN + 16;

/// A vault encrypts and decrypts credential payloads against the local master key.
/// Construct it once and share it — the key is fetched on construction and held in memory
/// (zeroized on drop by aes-gcm's Key type).
#[derive(Clone)]
pub struct Vault {
    cipher: Aes256Gcm,
}

impl std::fmt::Debug for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never write the key into a debug string — a Debug derive on the cipher
        // would not compile anyway, and leaking key material into logs is the exact
        // failure this type exists to prevent.
        f.debug_struct("Vault").finish_non_exhaustive()
    }
}

impl Vault {
    /// The vault of `home`'s existing master key; `None` when there is none.
    /// Never creates one: a store whose key is missing must say so, not be
    /// paired with a new key that can never open it (owner, 2026-10-16).
    pub fn open_existing(home: &Path) -> Result<Option<Vault>> {
        let path = home.join(KEY_FILE);
        let key = match std::fs::read(&path) {
            Ok(key) => key,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        if key.len() != KEY_LEN {
            return Err(Error::invalid(format!(
                "master key file '{}' is corrupt (expected {} bytes, got {})",
                path.display(),
                KEY_LEN,
                key.len()
            )));
        }
        Ok(Some(Self::with(&key)?))
    }

    /// The vault of `home`'s master key, made now if there is none — only for
    /// writing a store where none exists yet.
    pub fn open_or_create(home: &Path) -> Result<Vault> {
        Self::with(&master_key(home)?)
    }

    fn with(key: &[u8]) -> Result<Vault> {
        let cipher = Aes256Gcm::new_from_slice(key)
            .map_err(|_| Error::invalid("master key has the wrong length"))?;
        Ok(Vault { cipher })
    }

    /// Encrypt `plaintext`, returning the stored blob: `nonce ‖ ciphertext ‖ tag`.
    /// A fresh random nonce per call — reusing a GCM nonce is the one catastrophic
    /// mistake, so the nonce is never caller-supplied.
    pub fn seal(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce_bytes = [0u8; NONCE_LEN];
        rand::rngs::OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from_slice(&nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(
                nonce,
                Payload {
                    msg: plaintext,
                    aad: b"rune-secret",
                },
            )
            .map_err(|_| Error::invalid("encryption failed"))?;
        let mut blob = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        blob.extend_from_slice(&nonce_bytes);
        blob.extend_from_slice(&ciphertext);
        Ok(blob)
    }

    /// Decrypt a blob produced by [`Vault::seal`]. A wrong key or a tampered blob fails
    /// the GCM tag check and returns a named error — never partial or guessed plaintext.
    pub fn open_sealed(&self, blob: &[u8]) -> Result<Vec<u8>> {
        if blob.len() < NONCE_LEN + 16 {
            return Err(Error::invalid("sealed payload is too short to be valid"));
        }
        let (nonce_bytes, ciphertext) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::from_slice(nonce_bytes);
        self.cipher
            .decrypt(nonce, Payload { msg: ciphertext, aad: b"rune-secret" })
            .map_err(|_| Error::invalid("could not decrypt the stored credential — wrong key, or the data was tampered with"))
    }
}

/// Fetch the master key from `home/master.key`, creating a random 32-byte key (mode 0600) on first use.
fn master_key(home: &Path) -> Result<Vec<u8>> {
    let path = home.join(KEY_FILE);
    if path.exists() {
        let bytes = std::fs::read(&path)?;
        if bytes.len() != KEY_LEN {
            return Err(Error::invalid(format!(
                "master key file '{}' is corrupt (expected {} bytes, got {})",
                path.display(),
                KEY_LEN,
                bytes.len()
            )));
        }
        return Ok(bytes);
    }

    let mut key = vec![0u8; KEY_LEN];
    rand::rngs::OsRng.fill_bytes(&mut key);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    super::secrets::atomic_write(&path, &key)?;
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_vault() -> Vault {
        let key = [7u8; KEY_LEN];
        Vault {
            cipher: Aes256Gcm::new_from_slice(&key).unwrap(),
        }
    }

    #[test]
    fn a_sealed_payload_round_trips_back_to_the_same_bytes() {
        let v = fixed_vault();
        let secret = b"sk-test-0123456789abcdef";
        let blob = v.seal(secret).unwrap();
        assert_ne!(
            &blob[NONCE_LEN..],
            secret,
            "the stored form is not the plaintext"
        );
        assert_eq!(v.open_sealed(&blob).unwrap(), secret);
    }

    #[test]
    fn two_seals_of_the_same_payload_differ_because_the_nonce_differs() {
        let v = fixed_vault();
        let a = v.seal(b"same").unwrap();
        let b = v.seal(b"same").unwrap();
        assert_ne!(a, b, "a fresh nonce per seal means no two blobs match");
        assert_eq!(v.open_sealed(&a).unwrap(), v.open_sealed(&b).unwrap());
    }

    #[test]
    fn a_tampered_blob_is_refused_not_decrypted() {
        let v = fixed_vault();
        let mut blob = v.seal(b"sk-real-key").unwrap();
        let last = blob.len() - 1;
        blob[last] ^= 0x01;
        assert!(
            v.open_sealed(&blob).is_err(),
            "the GCM tag must catch the flip"
        );
    }

    #[test]
    fn a_wrong_key_cannot_open_a_blob() {
        let a = fixed_vault();
        let b = Vault {
            cipher: Aes256Gcm::new_from_slice(&[9u8; KEY_LEN]).unwrap(),
        };
        let blob = a.seal(b"sk-real-key").unwrap();
        assert!(b.open_sealed(&blob).is_err());
    }

    #[test]
    fn master_key_generation_and_reload() {
        let dir = std::env::temp_dir().join(format!("rune-vault-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let v1 = Vault::open_or_create(&dir).expect("vault 1 open");
        let blob = v1.seal(b"secret").expect("seal");

        let v2 = Vault::open_or_create(&dir).expect("vault 2 open");
        let plain = v2.open_sealed(&blob).expect("decrypt with reloaded key");
        assert_eq!(plain, b"secret");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
