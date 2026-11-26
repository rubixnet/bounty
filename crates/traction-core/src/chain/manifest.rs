//! Audit Bundle manifest digest and signature helpers (pure; the bundle
//! itself is built by the daemon).

use serde_json::Value;

use crate::CanonicalJsonError;

use super::{
    ChainError, DeviceSigningKey, TAG_MANIFEST, TAG_SIG_MANIFEST, VerifyingKeyHex, hex,
    tagged_digest,
};

fn digest_bytes(body: &Value) -> Result<[u8; 32], CanonicalJsonError> {
    Ok(tagged_digest(TAG_MANIFEST, &crate::canonical_json(body)?))
}

/// `SHA-256(TAG_MANIFEST || canonical_json(body))`, in hex.
pub fn manifest_digest(body: &Value) -> Result<String, CanonicalJsonError> {
    Ok(hex::encode(&digest_bytes(body)?))
}

/// Sign a manifest body: Ed25519 over `TAG_SIG_MANIFEST || manifest_digest`.
/// Returns the signature as 64 bytes of lowercase hex.
pub fn sign_manifest(key: &DeviceSigningKey, body: &Value) -> Result<String, CanonicalJsonError> {
    Ok(key.sign_tagged(TAG_SIG_MANIFEST, &digest_bytes(body)?))
}

/// Verify a manifest signature strictly.
pub fn verify_manifest(
    key: &VerifyingKeyHex,
    body: &Value,
    signature: &str,
) -> Result<(), ChainError> {
    let digest = digest_bytes(body).map_err(|_| ChainError::ManifestSignatureInvalid)?;
    if key.verify_tagged(TAG_SIG_MANIFEST, &digest, signature) {
        Ok(())
    } else {
        Err(ChainError::ManifestSignatureInvalid)
    }
}
