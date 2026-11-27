//! Device tokens and the other random values the server hands out. A device
//! token is 32 CSPRNG bytes (64 hex digits); the server keeps only its
//! SHA-256, so a database leak reveals no usable token.

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const HEX: &[u8; 16] = b"0123456789abcdef";

pub fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(char::from(HEX[usize::from(b >> 4)]));
        out.push(char::from(HEX[usize::from(b & 0x0f)]));
    }
    out
}

/// `len` random bytes as lowercase hex.
pub fn random_hex(len: usize) -> String {
    let mut raw = vec![0u8; len];
    getrandom::fill(&mut raw).expect("the OS random source is available");
    hex_encode(&raw)
}

/// A fresh device token: 64 hex digits.
pub fn new_device_token() -> String {
    random_hex(32)
}

/// What the database stores for a token (or a pairing poll secret).
pub fn hash_secret(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

/// Constant-time equality of two byte strings.
pub fn secure_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

/// The bearer value of an `Authorization: Bearer <token>` header.
pub fn bearer(header: Option<&str>) -> Option<&str> {
    header?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_is_32_random_bytes_in_hex() {
        let a = new_device_token();
        let b = new_device_token();
        assert_eq!(a.len(), 64);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn only_the_sha256_of_a_token_is_stored() {
        // SHA-256("abc"), the published test vector.
        assert_eq!(
            hex_encode(&hash_secret("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_ne!(&hash_secret("abc")[..], b"abc");
    }

    #[test]
    fn bearer_parsing_and_constant_time_compare() {
        assert_eq!(bearer(Some("Bearer abc")), Some("abc"));
        assert_eq!(bearer(Some("Basic abc")), None);
        assert_eq!(bearer(Some("Bearer ")), None);
        assert_eq!(bearer(None), None);
        assert!(secure_eq(b"x", b"x"));
        assert!(!secure_eq(b"x", b"y"));
        assert!(!secure_eq(b"x", b"xx"));
    }
}
