//! The per-device secret in `<data dir>/device.secret`: 32 raw CSPRNG bytes
//! that key `dir:` Project IDs (`docs/protocol.md` section 2a), so a `dir:`
//! ID cannot be confirmed from a path alone.
//!
//! Created atomically with mode `0600` on Unix (Windows relies on the
//! user-profile ACL), the same pattern as the bus token, and reused across
//! restarts. Unlike the token, a malformed file is never regenerated: a new
//! secret would silently give every `dir:` Project a new ID, so startup fails
//! instead and leaves the file for the user to inspect. The value is never
//! logged, formatted or sent anywhere.

use std::fs;
use std::io;
use std::path::Path;

use traction_core::DEVICE_SECRET_LEN;

use crate::fsutil::write_atomically;

pub const DEVICE_SECRET_FILE: &str = "device.secret";

/// The device secret. Deliberately not `Debug`, `Display` or `Clone`.
pub struct DeviceSecret([u8; DEVICE_SECRET_LEN]);

impl DeviceSecret {
    /// Reads the secret file, or creates it when missing. A file of the wrong
    /// length is an `InvalidData` error.
    pub fn load_or_create(data_dir: &Path) -> io::Result<Self> {
        match fs::read(data_dir.join(DEVICE_SECRET_FILE)) {
            Ok(bytes) => <[u8; DEVICE_SECRET_LEN]>::try_from(bytes.as_slice())
                .map(Self)
                .map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "device.secret is not 32 bytes; refusing to replace it",
                    )
                }),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let mut raw = [0u8; DEVICE_SECRET_LEN];
                getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
                write_atomically(data_dir, DEVICE_SECRET_FILE, &raw)?;
                Ok(Self(raw))
            }
            Err(error) => Err(error),
        }
    }

    /// The raw bytes, for `traction_core::dir_project_id`.
    pub fn bytes(&self) -> &[u8; DEVICE_SECRET_LEN] {
        &self.0
    }
}
