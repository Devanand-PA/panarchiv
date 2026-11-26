//! Encryption at rest.
//!
//! Every file the archive owns (the manifest, the document blobs and the
//! thumbnails) is encrypted with XChaCha20-Poly1305 before it touches the
//! disk. The master key lives in `key.bin` next to the archive, created
//! with owner-only permissions (0600) so other users on the machine cannot
//! read it.
//!
//! The key file may optionally be *wrapped* with a user passphrase: the
//! master key is then encrypted with a key derived from that passphrase via
//! Argon2id and the plaintext master key never touches the disk. Archives
//! that are not wrapped still benefit from at-rest encryption against
//! casual inspection, backups and stolen disks.
//!
//! File formats (all binary, little-endian where numbers are involved):
//!
//! ```text
//! key.bin
//!   magic   "PARCHIV-KEY1"   (12 bytes)
//!   flags   u8               (0 = raw key, 1 = passphrase wrapped)
//!   raw key:
//!     master key              (32 bytes)
//!   wrapped key:
//!     salt                    (16 bytes)
//!     argon2 memory cost      u32
//!     argon2 time cost        u32
//!     argon2 parallelism      u32
//!     nonce                   (24 bytes)
//!     wrapped master key      (32 + 16 bytes: ciphertext || tag)
//!
//! encrypted payload (manifest.json, blobs/<hash>, thumbs/<hash>.png)
//!   magic   "PANC1"          (5 bytes)
//!   nonce   (24 bytes)
//!   ciphertext || Poly1305 tag
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use rand::rngs::OsRng;
use rand::RngCore;
use sha2::{Digest, Sha256};

use crate::error::{msg, Result};

const KEY_MAGIC: &[u8; 12] = b"PARCHIV-KEY1";
const KEY_FLAG_RAW: u8 = 0;
const KEY_FLAG_WRAPPED: u8 = 1;

const PAYLOAD_MAGIC: &[u8; 5] = b"PANC1";
const NONCE_LEN: usize = 24;
const KEY_LEN: usize = 32;

/// Argon2id parameters. The defaults are deliberately modest so unlocking
/// stays fast on the low-power devices this app also runs on.
const ARGON2_M_COST: u32 = 19 * 1024; // 19 MiB
const ARGON2_T_COST: u32 = 2;
const ARGON2_P_COST: u32 = 1;

/// Errors surfaced to the user when the key file cannot be used.
#[derive(Debug)]
pub enum KeyError {
    /// The key file exists but is protected by a passphrase we don't have.
    PassphraseRequired,
    /// The supplied passphrase did not unwrap the key.
    WrongPassphrase,
    /// The key file is present but malformed.
    Corrupt,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            KeyError::PassphraseRequired => write!(
                f,
                "this archive is protected by a passphrase; enter it to continue"
            ),
            KeyError::WrongPassphrase => write!(f, "incorrect passphrase"),
            KeyError::Corrupt => write!(f, "the archive key file is corrupt"),
        }
    }
}

/// The archive's encryption state.
pub struct Crypto {
    cipher: XChaCha20Poly1305,
    /// The master key, kept in memory so the user can add or remove a
    /// passphrase later without re-encrypting the whole archive.
    master: [u8; KEY_LEN],
    /// True when the master key is itself protected by a passphrase.
    wrapped: bool,
}

impl Crypto {
    /// Load the key for `root`, creating one if the archive is new.
    ///
    /// * `passphrase` — `None` opens (or creates) an unprotected key file.
    ///   `Some(p)` creates a passphrase-wrapped key file for a new archive,
    ///   or unwraps an existing one.
    pub fn load_or_create(
        root: &Path,
        passphrase: Option<&str>,
    ) -> std::result::Result<Self, KeyError> {
        let path = key_path(root);
        if path.exists() {
            Self::load(&path, passphrase)
        } else {
            Self::create(&path, passphrase)
        }
    }

    /// Load an existing key file.
    pub fn load(path: &Path, passphrase: Option<&str>) -> std::result::Result<Self, KeyError> {
        let mut data = Vec::new();
        File::open(path)
            .and_then(|mut f| f.read_to_end(&mut data))
            .map_err(|_| KeyError::Corrupt)?;
        if data.len() < KEY_MAGIC.len() + 1 || &data[..KEY_MAGIC.len()] != KEY_MAGIC {
            return Err(KeyError::Corrupt);
        }
        let flags = data[KEY_MAGIC.len()];
        let body = &data[KEY_MAGIC.len() + 1..];

        let master = match flags {
            KEY_FLAG_RAW => {
                if body.len() < KEY_LEN {
                    return Err(KeyError::Corrupt);
                }
                body[..KEY_LEN].to_vec()
            }
            KEY_FLAG_WRAPPED => {
                if body.len() < 16 + 12 + NONCE_LEN + KEY_LEN + 16 {
                    return Err(KeyError::Corrupt);
                }
                let salt = &body[..16];
                let m_cost = u32::from_le_bytes(body[16..20].try_into().unwrap());
                let t_cost = u32::from_le_bytes(body[20..24].try_into().unwrap());
                let p_cost = u32::from_le_bytes(body[24..28].try_into().unwrap());
                let nonce = &body[28..28 + NONCE_LEN];
                let wrapped = &body[28 + NONCE_LEN..];

                let passphrase = passphrase.ok_or(KeyError::PassphraseRequired)?;
                let derived = derive_key(passphrase, salt, m_cost, t_cost, p_cost);
                let cipher = XChaCha20Poly1305::new(Key::from_slice(&derived));
                cipher
                    .decrypt(XNonce::from_slice(nonce), wrapped)
                    .map_err(|_| KeyError::WrongPassphrase)?
            }
            _ => return Err(KeyError::Corrupt),
        };

        if master.len() != KEY_LEN {
            return Err(KeyError::Corrupt);
        }
        let mut key_bytes = [0u8; KEY_LEN];
        key_bytes.copy_from_slice(&master);
        Ok(Self {
            cipher: XChaCha20Poly1305::new(Key::from_slice(&key_bytes)),
            master: key_bytes,
            wrapped: flags == KEY_FLAG_WRAPPED,
        })
    }

    /// Create a fresh key file.
    fn create(path: &Path, passphrase: Option<&str>) -> std::result::Result<Self, KeyError> {
        let mut master = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut master);

        let out = serialize_key(&master, passphrase);

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|_| KeyError::Corrupt)?;
        }
        write_private(path, &out).map_err(|_| KeyError::Corrupt)?;

        Ok(Self {
            cipher: XChaCha20Poly1305::new(Key::from_slice(&master)),
            master,
            wrapped: passphrase.is_some(),
        })
    }

    /// True when the master key is protected by a passphrase.
    pub fn is_wrapped(&self) -> bool {
        self.wrapped
    }

    /// Protect (or unprotect) the archive key with a passphrase.
    ///
    /// The master key itself never changes, so this is instantaneous: no
    /// document has to be re-encrypted. `None` removes the passphrase,
    /// leaving the key file readable only by the current user.
    pub fn set_passphrase(
        &mut self,
        path: &Path,
        passphrase: Option<&str>,
    ) -> std::result::Result<(), KeyError> {
        let out = serialize_key(&self.master, passphrase);
        write_private(path, &out).map_err(|_| KeyError::Corrupt)?;
        self.wrapped = passphrase.is_some();
        Ok(())
    }

    /// Encrypt `plaintext`, returning a self-describing payload.
    pub fn encrypt(&self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut nonce = [0u8; NONCE_LEN];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = self
            .cipher
            .encrypt(XNonce::from_slice(&nonce), plaintext)
            .map_err(|_| msg("encryption failed"))?;

        let mut out = Vec::with_capacity(PAYLOAD_MAGIC.len() + NONCE_LEN + ciphertext.len());
        out.extend_from_slice(PAYLOAD_MAGIC);
        out.extend_from_slice(&nonce);
        out.extend_from_slice(&ciphertext);
        Ok(out)
    }

    /// Decrypt a payload produced by [`Crypto::encrypt`].
    pub fn decrypt(&self, payload: &[u8]) -> Result<Vec<u8>> {
        if payload.len() < PAYLOAD_MAGIC.len() + NONCE_LEN + 16 {
            return Err(msg("encrypted payload is truncated"));
        }
        if &payload[..PAYLOAD_MAGIC.len()] != PAYLOAD_MAGIC {
            return Err(msg("payload is not a panarchiv encrypted file"));
        }
        let nonce = &payload[PAYLOAD_MAGIC.len()..PAYLOAD_MAGIC.len() + NONCE_LEN];
        let ciphertext = &payload[PAYLOAD_MAGIC.len() + NONCE_LEN..];
        self.cipher
            .decrypt(XNonce::from_slice(nonce), ciphertext)
            .map_err(|_| msg("could not decrypt: wrong key or corrupted data"))
    }

    /// Decrypt `data` if it carries the encrypted-payload marker, otherwise
    /// return it unchanged. This is what makes upgrades transparent: an
    /// archive written before encryption was introduced still opens, and is
    /// rewritten encrypted the next time it is saved.
    pub fn decrypt_or_plain(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() >= PAYLOAD_MAGIC.len() && &data[..PAYLOAD_MAGIC.len()] == PAYLOAD_MAGIC {
            self.decrypt(data)
        } else {
            Ok(data.to_vec())
        }
    }
}

/// Path of the key file inside `root`.
pub fn key_path(root: &Path) -> PathBuf {
    root.join("key.bin")
}

/// Serialise a master key into the `key.bin` format.
///
/// With no passphrase the key is stored raw (the file's 0600 permissions
/// are the only protection). With a passphrase the key is wrapped with
/// XChaCha20-Poly1305 using an Argon2id-derived key.
fn serialize_key(master: &[u8; KEY_LEN], passphrase: Option<&str>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(KEY_MAGIC);

    match passphrase {
        None => {
            out.push(KEY_FLAG_RAW);
            out.extend_from_slice(master);
        }
        Some(passphrase) => {
            let mut salt = [0u8; 16];
            OsRng.fill_bytes(&mut salt);
            let derived = derive_key(
                passphrase,
                &salt,
                ARGON2_M_COST,
                ARGON2_T_COST,
                ARGON2_P_COST,
            );
            let cipher = XChaCha20Poly1305::new(Key::from_slice(&derived));
            let mut nonce = [0u8; NONCE_LEN];
            OsRng.fill_bytes(&mut nonce);
            let wrapped = cipher
                .encrypt(XNonce::from_slice(&nonce), master.as_slice())
                .expect("encrypting a 32-byte key cannot fail");

            out.push(KEY_FLAG_WRAPPED);
            out.extend_from_slice(&salt);
            out.extend_from_slice(&ARGON2_M_COST.to_le_bytes());
            out.extend_from_slice(&ARGON2_T_COST.to_le_bytes());
            out.extend_from_slice(&ARGON2_P_COST.to_le_bytes());
            out.extend_from_slice(&nonce);
            out.extend_from_slice(&wrapped);
        }
    }

    out
}

/// True when `data` looks like an encrypted panarchiv payload.
pub fn is_encrypted(data: &[u8]) -> bool {
    data.len() >= PAYLOAD_MAGIC.len() && &data[..PAYLOAD_MAGIC.len()] == PAYLOAD_MAGIC
}

/// Derive a 32-byte key from a passphrase using Argon2id.
fn derive_key(
    passphrase: &str,
    salt: &[u8],
    m_cost: u32,
    t_cost: u32,
    p_cost: u32,
) -> [u8; KEY_LEN] {
    let params = Params::new(m_cost, t_cost, p_cost, None).unwrap_or_default();
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut key = [0u8; KEY_LEN];
    // A failure here can only mean the parameters are unusable; the
    // defaults are valid, so fall back to a plain hash rather than panicking.
    if argon
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .is_err()
    {
        let mut hasher = Sha256::new();
        hasher.update(salt);
        hasher.update(passphrase.as_bytes());
        key = hasher.finalize().into();
    }
    key
}

/// Write a file that only the current user can read.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = file.set_permissions(fs::Permissions::from_mode(0o600));
    }
    file.write_all(bytes)?;
    file.flush()
}

// ---------------------------------------------------------------------------
// Standalone passphrase encryption
//
// Used for files that leave the archive — bundles the user hands to
// somebody else. The archive's own key cannot be used for that, because
// the recipient does not have it.
// ---------------------------------------------------------------------------

/// Marker for a passphrase-encrypted payload (bundles).
const STANDALONE_MAGIC: &[u8; 7] = b"PBNDL1\0";

/// Encrypt `plaintext` with a key derived from `passphrase`.
///
/// The output is self-describing:
///
/// ```text
/// magic   "PBNDL1\0"  (7 bytes)
/// salt    (16 bytes)
/// m_cost  u32, t_cost u32, p_cost u32
/// nonce   (24 bytes)
/// ciphertext || Poly1305 tag
/// ```
pub fn encrypt_with_passphrase(plaintext: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    let mut salt = [0u8; 16];
    OsRng.fill_bytes(&mut salt);
    let derived = derive_key(
        passphrase,
        &salt,
        ARGON2_M_COST,
        ARGON2_T_COST,
        ARGON2_P_COST,
    );
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&derived));
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext)
        .map_err(|_| msg("encryption failed"))?;

    let mut out =
        Vec::with_capacity(STANDALONE_MAGIC.len() + 16 + 12 + NONCE_LEN + ciphertext.len());
    out.extend_from_slice(STANDALONE_MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&ARGON2_M_COST.to_le_bytes());
    out.extend_from_slice(&ARGON2_T_COST.to_le_bytes());
    out.extend_from_slice(&ARGON2_P_COST.to_le_bytes());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ciphertext);
    Ok(out)
}

/// Decrypt a payload produced by [`encrypt_with_passphrase`].
pub fn decrypt_with_passphrase(payload: &[u8], passphrase: &str) -> Result<Vec<u8>> {
    let header = STANDALONE_MAGIC.len() + 16 + 12 + NONCE_LEN;
    if payload.len() < header + 16 {
        return Err(msg("encrypted payload is truncated"));
    }
    if &payload[..STANDALONE_MAGIC.len()] != STANDALONE_MAGIC {
        return Err(msg("payload is not a panarchiv encrypted bundle"));
    }
    let body = &payload[STANDALONE_MAGIC.len()..];
    let salt = &body[..16];
    let m_cost = u32::from_le_bytes(body[16..20].try_into().unwrap());
    let t_cost = u32::from_le_bytes(body[20..24].try_into().unwrap());
    let p_cost = u32::from_le_bytes(body[24..28].try_into().unwrap());
    let nonce = &body[28..28 + NONCE_LEN];
    let ciphertext = &body[28 + NONCE_LEN..];

    let derived = derive_key(passphrase, salt, m_cost, t_cost, p_cost);
    let cipher = XChaCha20Poly1305::new(Key::from_slice(&derived));
    cipher
        .decrypt(XNonce::from_slice(nonce), ciphertext)
        .map_err(|_| msg("could not decrypt: wrong passphrase or corrupted data"))
}

/// True when `data` looks like a passphrase-encrypted payload.
pub fn is_standalone_encrypted(data: &[u8]) -> bool {
    data.len() >= STANDALONE_MAGIC.len() && &data[..STANDALONE_MAGIC.len()] == STANDALONE_MAGIC
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("pa-crypto-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trips_payloads() {
        let dir = tmpdir();
        let crypto = Crypto::load_or_create(&dir, None).unwrap();
        let secret = b"the quick brown fox";
        let enc = crypto.encrypt(secret).unwrap();
        assert!(is_encrypted(&enc));
        assert_eq!(&enc[..PAYLOAD_MAGIC.len()], PAYLOAD_MAGIC);
        // The plaintext must not be visible in the ciphertext.
        assert!(!enc.windows(secret.len()).any(|w| w == secret));
        let dec = crypto.decrypt(&enc).unwrap();
        assert_eq!(dec, secret);
    }

    #[test]
    fn plaintext_is_returned_unchanged() {
        let dir = tmpdir();
        let crypto = Crypto::load_or_create(&dir, None).unwrap();
        let plain = b"legacy manifest";
        assert_eq!(crypto.decrypt_or_plain(plain).unwrap(), plain);
    }

    #[test]
    fn key_survives_reopen_and_is_private() {
        let dir = tmpdir();
        let crypto = Crypto::load_or_create(&dir, None).unwrap();
        let enc = crypto.encrypt(b"secret").unwrap();
        let reopened = Crypto::load_or_create(&dir, None).unwrap();
        assert_eq!(reopened.decrypt(&enc).unwrap(), b"secret");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(key_path(&dir)).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "key file must not be group/world readable"
            );
        }
    }

    #[test]
    fn passphrase_wraps_the_key() {
        let dir = tmpdir();
        let crypto = Crypto::load_or_create(&dir, Some("hunter2")).unwrap();
        assert!(crypto.is_wrapped());
        let enc = crypto.encrypt(b"secret").unwrap();

        // Without the passphrase we cannot even open the archive.
        match Crypto::load_or_create(&dir, None) {
            Err(KeyError::PassphraseRequired) => {}
            other => panic!("expected PassphraseRequired, got {:?}", other.err()),
        }
        // The wrong passphrase is rejected.
        match Crypto::load_or_create(&dir, Some("wrong")) {
            Err(KeyError::WrongPassphrase) => {}
            other => panic!("expected WrongPassphrase, got {:?}", other.err()),
        }
        // The right one works.
        let good = Crypto::load_or_create(&dir, Some("hunter2")).unwrap();
        assert_eq!(good.decrypt(&enc).unwrap(), b"secret");
    }

    #[test]
    fn wrong_key_cannot_decrypt() {
        let a = tmpdir();
        let b = tmpdir();
        let ca = Crypto::load_or_create(&a, None).unwrap();
        let cb = Crypto::load_or_create(&b, None).unwrap();
        let enc = ca.encrypt(b"secret").unwrap();
        assert!(cb.decrypt(&enc).is_err());
    }
}
