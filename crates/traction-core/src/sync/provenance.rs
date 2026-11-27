//! Soft build provenance (spec S3b, Q18). Release CI signs
//! `{version, git_commit, target_triple}` with a CI Ed25519 key and the
//! signed statement is compiled into the daemon. The server checks it against
//! the CI public keys in its config and tags the device `ci_signed` or
//! `self_built`.
//!
//! This is a tag, not proof: the statement can be copied verbatim into a
//! fork's binary. It only lets the `HeuristicEngine` send a self-built
//! submission to a standard spot-check (Yellow), never to Red.

use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{DeviceSigningKey, VerifyingKeyHex, canonical_json};

/// Domain tag of the signature, so a build signature can never verify as a
/// chain link or a bundle manifest.
const TAG_SIG_BUILD: &[u8] = b"traction-build-statement/1\0";

/// Longest accepted field, so a statement stays a small, bounded value.
const MAX_FIELD_LEN: usize = 128;

/// How the server classifies a device's build.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BuildTag {
    CiSigned,
    SelfBuilt,
}

impl BuildTag {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CiSigned => "ci_signed",
            Self::SelfBuilt => "self_built",
        }
    }
}

/// A build statement and the CI signature over it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedBuildStatement {
    pub version: String,
    pub git_commit: String,
    pub target_triple: String,
    /// 64-byte Ed25519 signature, lowercase hex.
    pub signature: String,
}

fn field_ok(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_FIELD_LEN
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b'+'))
}

fn digest(version: &str, git_commit: &str, target_triple: &str) -> Option<[u8; 32]> {
    if !(field_ok(version) && field_ok(git_commit) && field_ok(target_triple)) {
        return None;
    }
    let body = json!({
        "version": version,
        "git_commit": git_commit,
        "target_triple": target_triple,
    });
    let bytes = canonical_json(&body).ok()?;
    Some(Sha256::digest(bytes).into())
}

impl SignedBuildStatement {
    /// Signs a statement with the CI key (the release workflow's job).
    /// `None` when a field is empty, too long or has characters outside
    /// `[A-Za-z0-9._+-]`.
    pub fn sign(
        ci_key: &DeviceSigningKey,
        version: &str,
        git_commit: &str,
        target_triple: &str,
    ) -> Option<Self> {
        let digest = digest(version, git_commit, target_triple)?;
        Some(Self {
            version: version.to_owned(),
            git_commit: git_commit.to_owned(),
            target_triple: target_triple.to_owned(),
            signature: ci_key.sign_tagged(TAG_SIG_BUILD, &digest),
        })
    }

    /// Whether any of `ci_keys` signed exactly this statement.
    pub fn verifies_against(&self, ci_keys: &[VerifyingKeyHex]) -> bool {
        let Some(digest) = digest(&self.version, &self.git_commit, &self.target_triple) else {
            return false;
        };
        ci_keys
            .iter()
            .any(|key| key.verify_tagged(TAG_SIG_BUILD, &digest, &self.signature))
    }
}

/// The tag for a device that sent `statement` (or nothing).
pub fn tag_build(statement: Option<&SignedBuildStatement>, ci_keys: &[VerifyingKeyHex]) -> BuildTag {
    match statement {
        Some(s) if s.verifies_against(ci_keys) => BuildTag::CiSigned,
        _ => BuildTag::SelfBuilt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroize::Zeroizing;

    fn key(byte: u8) -> DeviceSigningKey {
        DeviceSigningKey::from_seed(Zeroizing::new([byte; 32])).unwrap()
    }

    fn signed(ci: &DeviceSigningKey) -> SignedBuildStatement {
        SignedBuildStatement::sign(ci, "0.4.0", &"a".repeat(40), "x86_64-unknown-linux-gnu").unwrap()
    }

    #[test]
    fn a_statement_signed_by_a_configured_ci_key_is_ci_signed() {
        let ci = key(7);
        let keys = [key(9).verifying_key().clone(), ci.verifying_key().clone()];
        assert_eq!(tag_build(Some(&signed(&ci)), &keys), BuildTag::CiSigned);
    }

    #[test]
    fn no_statement_an_unknown_key_or_no_configured_keys_is_self_built() {
        let ci = key(7);
        let statement = signed(&ci);
        assert_eq!(tag_build(None, &[ci.verifying_key().clone()]), BuildTag::SelfBuilt);
        assert_eq!(
            tag_build(Some(&statement), &[key(8).verifying_key().clone()]),
            BuildTag::SelfBuilt
        );
        assert_eq!(tag_build(Some(&statement), &[]), BuildTag::SelfBuilt);
    }

    #[test]
    fn changing_any_signed_field_or_the_signature_breaks_the_tag() {
        let ci = key(7);
        let keys = [ci.verifying_key().clone()];
        let good = signed(&ci);
        for tweak in [
            |s: &mut SignedBuildStatement| s.version = "0.4.1".into(),
            |s: &mut SignedBuildStatement| s.git_commit = "b".repeat(40),
            |s: &mut SignedBuildStatement| s.target_triple = "aarch64-apple-darwin".into(),
            |s: &mut SignedBuildStatement| s.signature = "0".repeat(128),
            |s: &mut SignedBuildStatement| s.signature = "zz".into(),
        ] {
            let mut bad = good.clone();
            tweak(&mut bad);
            assert_eq!(tag_build(Some(&bad), &keys), BuildTag::SelfBuilt);
        }
    }

    #[test]
    fn a_device_signing_key_signature_over_the_same_bytes_does_not_verify_as_a_build() {
        // Domain separation: the tag is part of the signed input.
        let ci = key(7);
        let good = signed(&ci);
        let digest = digest(&good.version, &good.git_commit, &good.target_triple).unwrap();
        let other = ci.sign_tagged(b"traction-something-else/1\0", &digest);
        let mut forged = good;
        forged.signature = other;
        assert!(!forged.verifies_against(&[ci.verifying_key().clone()]));
    }

    #[test]
    fn malformed_fields_cannot_be_signed_or_verified() {
        let ci = key(7);
        assert!(SignedBuildStatement::sign(&ci, "", "c", "t").is_none());
        assert!(SignedBuildStatement::sign(&ci, "1\n2", "c", "t").is_none());
        assert!(SignedBuildStatement::sign(&ci, &"v".repeat(129), "c", "t").is_none());
    }

    #[test]
    fn the_wire_form_rejects_unknown_fields() {
        let text = r#"{"version":"1","git_commit":"c","target_triple":"t","signature":"s","x":1}"#;
        assert!(serde_json::from_str::<SignedBuildStatement>(text).is_err());
    }
}
