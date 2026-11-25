//! The per-user bus token in `<data dir>/daemon.token`: 32 CSPRNG bytes,
//! lowercase hex, created atomically with mode `0600` on Unix (Windows relies
//! on the user-profile ACL), reused across restarts and compared in constant
//! time. The value is never logged or formatted.

use std::fs;
use std::io;
use std::path::Path;

use subtle::ConstantTimeEq;

use crate::fsutil::write_atomically;
use crate::log;

pub const TOKEN_FILE: &str = "daemon.token";
const TOKEN_BYTES: usize = 32;
const TOKEN_HEX_LEN: usize = TOKEN_BYTES * 2;

/// The bearer token every bus request must carry. Deliberately not `Debug`.
pub struct BusToken([u8; TOKEN_HEX_LEN]);

impl BusToken {
    /// Reads the token file, or (re)generates it when missing or malformed.
    pub fn load_or_create(data_dir: &Path) -> io::Result<Self> {
        let path = data_dir.join(TOKEN_FILE);
        match fs::read(&path) {
            Ok(bytes) => {
                if let Some(token) = Self::parse(&bytes) {
                    return Ok(token);
                }
                log::warn("daemon.token is malformed; generating a new token");
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let token = Self::generate()?;
        write_atomically(data_dir, TOKEN_FILE, &token.0)?;
        Ok(token)
    }

    /// Constant-time check of a presented bearer value.
    pub fn matches(&self, presented: &[u8]) -> bool {
        // Length is public (always 64), so the early length check leaks nothing.
        presented.len() == TOKEN_HEX_LEN && bool::from(self.0.ct_eq(presented))
    }

    /// The token's text, for the one place it must be copied: the Windows
    /// per-user `TRACTION_TOKEN` variable the installer sets.
    #[cfg(windows)]
    pub(crate) fn reveal(&self) -> &[u8] {
        &self.0
    }

    fn parse(bytes: &[u8]) -> Option<Self> {
        let bytes = bytes.trim_ascii();
        let token: [u8; TOKEN_HEX_LEN] = bytes.try_into().ok()?;
        token
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            .then_some(Self(token))
    }

    fn generate() -> io::Result<Self> {
        let mut raw = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut raw).map_err(|e| io::Error::other(e.to_string()))?;
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut hex = [0u8; TOKEN_HEX_LEN];
        for (i, byte) in raw.iter().enumerate() {
            hex[2 * i] = HEX[usize::from(byte >> 4)];
            hex[2 * i + 1] = HEX[usize::from(byte & 0x0f)];
        }
        Ok(Self(hex))
    }
}
