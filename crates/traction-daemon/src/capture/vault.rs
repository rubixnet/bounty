//! The Vault (spec Q7): `<data-dir>/vault/` holds one encrypted file per
//! Frame plus `vault.key` (32 random bytes, owner-only, created on first
//! enable).
//!
//! `<frame_id>.bin` = `nonce (24) || XChaCha20-Poly1305(salt || webp)` with
//! associated data `frame_id || frame_hash` (the raw 32 bytes), so a file
//! cannot be moved to another Frame's name or record. The key guards copied
//! folders and backups; it does not stop malware running as the same user.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use traction_core::{FRAME_SALT_LEN, parse_frame_id};
use zeroize::Zeroizing;

use crate::fsutil::write_owner_only;

/// The Vault folder under the data folder.
pub const VAULT_DIR: &str = "vault";
/// The Vault key file inside [`VAULT_DIR`].
pub const VAULT_KEY_FILE: &str = "vault.key";
/// The extension of one Frame's encrypted file.
pub const FRAME_FILE_EXTENSION: &str = "bin";
const NONCE_LEN: usize = 24;

/// `<data-dir>/vault`.
pub fn vault_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(VAULT_DIR)
}

/// `<data-dir>/vault/<frame_id>.bin`.
pub fn frame_path(data_dir: &Path, frame_id: &str) -> PathBuf {
    vault_dir(data_dir).join(format!("{frame_id}.{FRAME_FILE_EXTENSION}"))
}

/// The Vault key. Deliberately not `Debug`, `Display` or `Clone`.
pub struct VaultKey(Zeroizing<[u8; 32]>);

impl VaultKey {
    /// Reads `vault.key`; `Ok(None)` when Visual Proof Mode was never enabled.
    pub fn load(data_dir: &Path) -> io::Result<Option<Self>> {
        match fs::read(vault_dir(data_dir).join(VAULT_KEY_FILE)) {
            Ok(bytes) => {
                let bytes = Zeroizing::new(bytes);
                <[u8; 32]>::try_from(bytes.as_slice())
                    .map(|raw| Some(Self(Zeroizing::new(raw))))
                    .map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "vault.key is not 32 bytes; refusing to replace it",
                        )
                    })
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    /// Reads `vault.key`, creating the Vault and a fresh key when missing.
    pub fn load_or_create(data_dir: &Path) -> io::Result<Self> {
        if let Some(key) = Self::load(data_dir)? {
            return Ok(key);
        }
        let dir = vault_dir(data_dir);
        fs::create_dir_all(&dir)?;
        let mut raw = Zeroizing::new([0u8; 32]);
        getrandom::fill(raw.as_mut()).map_err(|e| io::Error::other(e.to_string()))?;
        write_owner_only(&dir, VAULT_KEY_FILE, raw.as_ref())?;
        Ok(Self(raw))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new_from_slice(self.0.as_ref()).expect("a 32-byte key")
    }
}

fn aad(frame_id: &str, frame_hash: &[u8; 32]) -> Vec<u8> {
    let mut aad = frame_id.as_bytes().to_vec();
    aad.extend_from_slice(frame_hash);
    aad
}

/// Encrypts `salt || webp` into `vault/<frame_id>.bin` through a temp file
/// and a rename (owner-only).
pub fn write_frame(
    data_dir: &Path,
    key: &VaultKey,
    frame_id: &str,
    frame_hash: &[u8; 32],
    salt: &[u8; FRAME_SALT_LEN],
    webp: &[u8],
) -> io::Result<()> {
    if parse_frame_id(frame_id).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "malformed frame id",
        ));
    }
    let mut nonce = [0u8; NONCE_LEN];
    getrandom::fill(&mut nonce).map_err(|e| io::Error::other(e.to_string()))?;
    let mut plain = Zeroizing::new(Vec::with_capacity(FRAME_SALT_LEN + webp.len()));
    plain.extend_from_slice(salt);
    plain.extend_from_slice(webp);
    let aad = aad(frame_id, frame_hash);
    let sealed = key
        .cipher()
        .encrypt(
            &XNonce::from(nonce),
            Payload {
                msg: &plain,
                aad: &aad,
            },
        )
        .map_err(|_| io::Error::other("frame encryption failed"))?;
    let mut file = Vec::with_capacity(NONCE_LEN + sealed.len());
    file.extend_from_slice(&nonce);
    file.extend_from_slice(&sealed);
    write_owner_only(
        &vault_dir(data_dir),
        &format!("{frame_id}.{FRAME_FILE_EXTENSION}"),
        &file,
    )
}

/// Decrypts one Frame's file into `(salt, webp)`. A wrong key, a moved or
/// edited file, or the wrong `frame_hash` is `InvalidData`.
pub fn read_frame(
    data_dir: &Path,
    key: &VaultKey,
    frame_id: &str,
    frame_hash: &[u8; 32],
) -> io::Result<([u8; FRAME_SALT_LEN], Vec<u8>)> {
    if parse_frame_id(frame_id).is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "malformed frame id",
        ));
    }
    let file = fs::read(frame_path(data_dir, frame_id))?;
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "frame file does not decrypt");
    if file.len() < NONCE_LEN + FRAME_SALT_LEN {
        return Err(invalid());
    }
    let (nonce, sealed) = file.split_at(NONCE_LEN);
    let aad = aad(frame_id, frame_hash);
    let plain = Zeroizing::new(
        key.cipher()
            .decrypt(
                &XNonce::try_from(nonce).map_err(|_| invalid())?,
                Payload {
                    msg: sealed,
                    aad: &aad,
                },
            )
            .map_err(|_| invalid())?,
    );
    let (salt, webp) = plain.split_at(FRAME_SALT_LEN);
    Ok((salt.try_into().map_err(|_| invalid())?, webp.to_vec()))
}

/// Removes every Vault entry that is not a recorded Frame: leftover temp
/// files and `.bin` files `is_recorded` does not know (a killed child, or a
/// write whose record never committed). Returns how many were removed.
pub fn remove_orphans(
    data_dir: &Path,
    mut is_recorded: impl FnMut(&str) -> bool,
) -> io::Result<usize> {
    let entries = match fs::read_dir(vault_dir(data_dir)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut removed = 0;
    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name == VAULT_KEY_FILE || !entry.file_type()?.is_file() {
            continue;
        }
        let keep = name
            .strip_suffix(".bin")
            .is_some_and(|id| parse_frame_id(id).is_some() && is_recorded(id));
        if !keep {
            fs::remove_file(entry.path())?;
            removed += 1;
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips_and_binds_its_id_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let key = VaultKey::load_or_create(dir.path()).unwrap();
        let hash = [7u8; 32];
        let salt = [9u8; FRAME_SALT_LEN];
        write_frame(dir.path(), &key, "1000-00", &hash, &salt, b"RIFFwebp").unwrap();
        let (s, w) = read_frame(dir.path(), &key, "1000-00", &hash).unwrap();
        assert_eq!((s, w.as_slice()), (salt, &b"RIFFwebp"[..]));

        let raw = fs::read(frame_path(dir.path(), "1000-00")).unwrap();
        assert!(!raw.windows(8).any(|w| w == b"RIFFwebp"), "ciphertext only");
        // Wrong hash, moved file, other key: all refuse.
        assert!(read_frame(dir.path(), &key, "1000-00", &[8u8; 32]).is_err());
        fs::copy(
            frame_path(dir.path(), "1000-00"),
            frame_path(dir.path(), "1000-01"),
        )
        .unwrap();
        assert!(read_frame(dir.path(), &key, "1000-01", &hash).is_err());
        let other = tempfile::tempdir().unwrap();
        let other_key = VaultKey::load_or_create(other.path()).unwrap();
        assert!(read_frame(dir.path(), &other_key, "1000-00", &hash).is_err());
        // The key is stable once created.
        let again = VaultKey::load_or_create(dir.path()).unwrap();
        assert!(read_frame(dir.path(), &again, "1000-00", &hash).is_ok());
    }

    #[test]
    fn orphans_are_removed_and_the_key_and_recorded_frames_stay() {
        let dir = tempfile::tempdir().unwrap();
        let key = VaultKey::load_or_create(dir.path()).unwrap();
        for id in ["1-00", "2-00"] {
            write_frame(dir.path(), &key, id, &[0; 32], &[0; 16], b"x").unwrap();
        }
        let vault = vault_dir(dir.path());
        fs::write(vault.join("3-00.bin.traction-tmp-42"), b"partial").unwrap();
        fs::write(vault.join("notes.txt"), b"?").unwrap();
        assert_eq!(remove_orphans(dir.path(), |id| id == "1-00").unwrap(), 3);
        let mut left: Vec<_> = fs::read_dir(&vault)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["1-00.bin", "vault.key"]);
    }

    #[test]
    fn webp_and_the_schedule_hmac_build_here() {
        // Addition 3 smoke: libwebp (via `cc`) encodes lossy; HMAC-SHA256
        // (S2's schedule) computes.
        use hmac::{Hmac, KeyInit as _, Mac};
        let rgb = vec![128u8; 64 * 48 * 3];
        let webp = webp::Encoder::from_rgb(&rgb, 64, 48).encode(70.0);
        assert_eq!(&webp[..4], b"RIFF");
        assert_eq!(&webp[8..12], b"WEBP");
        let mut mac = Hmac::<sha2::Sha256>::new_from_slice(&[0u8; 32]).unwrap();
        mac.update(&0u64.to_be_bytes());
        assert_eq!(mac.finalize().into_bytes().len(), 32);
    }
}
