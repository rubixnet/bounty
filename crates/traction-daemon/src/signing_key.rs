//! The Device Signing Key in `<data dir>/signing.key`: the 32 raw bytes of an
//! Ed25519 seed that signs every Chain Link (`docs/protocol.md`). Distinct
//! from `device.secret` (the `dir:` Project ID key) and the bus token.
//!
//! Created from the OS CSPRNG straight into a zeroizing buffer and written
//! owner-only through [`write_owner_only`] (`0600` on Unix, a one-ACE
//! protected DACL on Windows). No keyring and no Keychain: both can prompt or
//! fail on a headless machine. `signing.pub` beside it holds the public key
//! as lowercase hex for the user to hand to a reviewer; it is not secret and
//! is rewritten from the seed whenever it differs.
//!
//! A malformed file is never replaced (a new key would silently fork the
//! chain's trust), so startup fails and leaves it for the user to inspect.
//! The seed is never logged, formatted or sent anywhere.

use std::fs;
use std::io;
use std::path::Path;

use traction_core::DeviceSigningKey;
use zeroize::Zeroizing;

use crate::fsutil::{write_atomically, write_owner_only};
#[cfg(unix)]
use crate::log;

pub const SIGNING_KEY_FILE: &str = "signing.key";
pub const SIGNING_PUB_FILE: &str = "signing.pub";

/// The 32-byte seed in `bytes` as a key. `suffix` is appended to the error
/// text (the creating path says it refuses to replace the file).
fn parse_seed(bytes: &[u8], suffix: &str) -> io::Result<DeviceSigningKey> {
    let invalid =
        |what: &str| io::Error::new(io::ErrorKind::InvalidData, format!("{what}{suffix}"));
    let seed = <[u8; 32]>::try_from(bytes).map_err(|_| invalid("signing.key is not 32 bytes"))?;
    DeviceSigningKey::from_seed(Zeroizing::new(seed))
        .map_err(|_| invalid("signing.key holds a weak key"))
}

/// Reads the key file without creating, repairing or rewriting anything (the
/// `export-bundle` command must be safe next to a running daemon).
pub fn load_existing(data_dir: &Path) -> io::Result<DeviceSigningKey> {
    let bytes = Zeroizing::new(fs::read(data_dir.join(SIGNING_KEY_FILE))?);
    parse_seed(&bytes, "")
}

/// Reads the key file, or creates it when missing.
///
/// On Unix a loosened mode is tightened back to `0600`; on Windows the
/// protected DACL set at creation is not re-checked or re-tightened on load.
pub fn load_or_create(data_dir: &Path) -> io::Result<DeviceSigningKey> {
    let path = data_dir.join(SIGNING_KEY_FILE);
    let key = match fs::read(&path) {
        Ok(bytes) => {
            let key = parse_seed(&Zeroizing::new(bytes), "; refusing to replace it")?;
            tighten_mode(&path)?;
            key
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut seed = Zeroizing::new([0u8; 32]);
            getrandom::fill(&mut *seed).map_err(|e| io::Error::other(e.to_string()))?;
            let key = parse_seed(&*seed, "")
                .map_err(|_| io::Error::other("generated an unusable signing key"))?;
            write_owner_only(data_dir, SIGNING_KEY_FILE, &*seed)?;
            key
        }
        Err(error) => return Err(error),
    };
    write_public(data_dir, &key)?;
    Ok(key)
}

/// `signing.pub`: the public key in lowercase hex plus a newline.
fn write_public(data_dir: &Path, key: &DeviceSigningKey) -> io::Result<()> {
    let text = format!("{}\n", key.verifying_key().to_hex());
    if fs::read(data_dir.join(SIGNING_PUB_FILE)).is_ok_and(|current| current == text.as_bytes()) {
        return Ok(());
    }
    write_atomically(data_dir, SIGNING_PUB_FILE, text.as_bytes())
}

/// A key file that grants group or other any bit is tightened to `0600`.
#[cfg(unix)]
fn tighten_mode(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let mode = fs::metadata(path)?.permissions().mode();
    if mode & 0o077 != 0 {
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        log::warn("signing.key was readable by other users; tightened it to owner-only (0600)");
    }
    Ok(())
}

#[cfg(not(unix))]
fn tighten_mode(_path: &Path) -> io::Result<()> {
    Ok(())
}
