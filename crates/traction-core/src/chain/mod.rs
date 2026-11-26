//! The Step 3 tamper-evident chain: domain-separated hashing, Ed25519 Chain
//! Links sealed from a 32-byte seed, and a pure multi-chain verifier.
//!
//! Pure on purpose: no I/O, no clock, no randomness, no OS dependency. The
//! daemon seals with it; the `verify` CLI and the Step 4 server verify with
//! the same code. Every hash is `SHA-256(tag || canonical_json(value))` and
//! every signature input starts with its own tag, so no byte string is valid
//! in two domains.

mod bundle;
mod error;
mod hex;
mod keys;
mod manifest;
mod rows;
mod types;
mod verify;

pub use bundle::{
    AuditBundle, BUNDLE_SCHEMA, BundleBuildError, BundleChain, BundleError, BundleInput,
    BundlePolicy, DeviceRowsSection, Manifest, ManifestBody, ManifestTip, ProjectRowsSection,
    build_bundle, verify_bundle,
};
pub use error::{ChainError, PayloadPart, SealError};
pub use keys::{DeviceSigningKey, VerifyingKeyHex};
pub use manifest::{manifest_digest, sign_manifest, verify_manifest};
pub use rows::{ProjectWindowRows, RowScope, RowSet, RowsError};
pub use types::{
    BuildError, ChainLink, FRAME_DIGEST_NONE, GENESIS_PREV_HASH, KeyId, LinkHeader, LinkPayload,
    ProjectDigest, Supersedes, device_rows_digest,
};
pub use verify::{
    ChainReport, ChainSummary, KeyDiscontinuity, KeyedChain, VerifyPolicy, verify_chain,
    verify_chains,
};

use serde::Serialize;
use sha2::{Digest, Sha256};

/// Domain tag: the 32-byte Ed25519 public key hashed into a [`KeyId`].
pub const TAG_KEY_ID: &[u8] = b"traction/v1/key-id\0";
/// Domain tag: one Project's rows in one window.
pub const TAG_ROWS: &[u8] = b"traction/v1/rows\0";
/// Domain tag: the window's Activity Rollup (or `null`).
pub const TAG_DEVICE_ROWS: &[u8] = b"traction/v1/device-rows\0";
/// Domain tag: the Link Payload.
pub const TAG_PAYLOAD: &[u8] = b"traction/v1/payload\0";
/// Domain tag: the Link Header (gives the `entry_hash`).
pub const TAG_LINK: &[u8] = b"traction/v1/link\0";
/// Domain tag: what a Chain Link signature covers (`entry_hash`).
pub const TAG_SIG_LINK: &[u8] = b"traction/v1/sig/link\0";
/// Domain tag: the Audit Bundle manifest body.
pub const TAG_MANIFEST: &[u8] = b"traction/v1/manifest\0";
/// Domain tag: what a manifest signature covers (the manifest digest).
pub const TAG_SIG_MANIFEST: &[u8] = b"traction/v1/sig/manifest\0";
/// Domain tag reserved for the snapshot-frame digest (Step 3b).
pub const TAG_FRAMES: &[u8] = b"traction/v1/frames\0";

/// `SHA-256(tag || bytes)`.
pub(crate) fn tagged_digest(tag: &[u8], bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(tag);
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Canonical JSON bytes of a chain type. Chain types have no float fields,
/// so the canonicalizer cannot fail.
pub(crate) fn canon<T: Serialize>(value: &T) -> Vec<u8> {
    let value = serde_json::to_value(value).expect("chain types serialize to JSON values");
    crate::canonical_json(&value).expect("chain types have no float fields")
}

/// `hex(SHA-256(tag || canonical_json(value)))`.
pub(crate) fn digest_hex<T: Serialize>(tag: &[u8], value: &T) -> String {
    hex::encode(&tagged_digest(tag, &canon(value)))
}
