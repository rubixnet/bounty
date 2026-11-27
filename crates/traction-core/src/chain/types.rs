//! The Chain Link format. No float fields; unknown fields are rejected so a
//! payload's canonical bytes can never be read as a header.

use serde::{Deserialize, Serialize};

use super::{ProjectWindowRows, TAG_DEVICE_ROWS, TAG_PAYLOAD, digest_hex};
use crate::ActivityRollup;

/// The `prev_hash` of a genesis link (`seq == 0`), and of no other link.
pub const GENESIS_PREV_HASH: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// The `frame_digest` of a window with no Frames (Visual Proof Mode off, or
/// nothing captured), and of every 3a payload.
pub const FRAME_DIGEST_NONE: &str =
    "0000000000000000000000000000000000000000000000000000000000000000";

/// `"ed25519:" || hex(SHA-256(TAG_KEY_ID || public_key)[..16])`. Derived from
/// a public key, never chosen.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct KeyId(String);

impl KeyId {
    pub(crate) fn from_string(text: String) -> Self {
        Self(text)
    }

    /// The Key ID text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for KeyId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One Project's entry in a Link Payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectDigest {
    pub project_id: String,
    pub rows_digest: String,
}

/// The previous chain's tip, named by a successor chain's genesis link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Supersedes {
    pub key_id: KeyId,
    pub seq: u64,
    pub entry_hash: String,
}

/// What one sealed window commits to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkPayload {
    pub window_start_ms: u64,
    pub ruleset_id: String,
    pub device_rows_digest: String,
    /// Sorted by `project_id`, unique.
    pub projects: Vec<ProjectDigest>,
    /// [`super::frame_digest`] of the window's Frame Records.
    pub frame_digest: String,
    /// `Some` only on the genesis link of a successor chain.
    pub supersedes: Option<Supersedes>,
}

/// A payload could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// Two entries named the same Project.
    DuplicateProject(String),
}

impl std::fmt::Display for BuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("duplicate_project")
    }
}

impl std::error::Error for BuildError {}

/// `SHA-256(TAG_DEVICE_ROWS || canonical_json(rollup or null))`, in hex.
pub fn device_rows_digest(rollup: Option<&ActivityRollup>) -> String {
    digest_hex(TAG_DEVICE_ROWS, &rollup)
}

impl LinkPayload {
    /// Build a payload for one window with no Frames
    /// ([`FRAME_DIGEST_NONE`]); see [`LinkPayload::with_frames`].
    pub fn new(
        window_start_ms: u64,
        ruleset_id: &str,
        device_rows: Option<&ActivityRollup>,
        projects: &[ProjectWindowRows],
        supersedes: Option<Supersedes>,
    ) -> Result<Self, BuildError> {
        let mut entries: Vec<ProjectDigest> = projects
            .iter()
            .map(|p| ProjectDigest {
                project_id: p.project_id.clone(),
                rows_digest: p.rows_digest(),
            })
            .collect();
        entries.sort_by(|a, b| a.project_id.cmp(&b.project_id));
        if let Some(pair) = entries
            .windows(2)
            .find(|w| w[0].project_id == w[1].project_id)
        {
            return Err(BuildError::DuplicateProject(pair[0].project_id.clone()));
        }
        Ok(Self {
            window_start_ms,
            ruleset_id: ruleset_id.to_owned(),
            device_rows_digest: device_rows_digest(device_rows),
            projects: entries,
            frame_digest: FRAME_DIGEST_NONE.to_owned(),
            supersedes,
        })
    }

    /// The same payload committing to the window's Frame Records.
    pub fn with_frames(mut self, frames: &[super::FrameRecord]) -> Self {
        self.frame_digest = super::frame_digest(frames);
        self
    }

    /// `SHA-256(TAG_PAYLOAD || canonical_json(self))`, in hex.
    pub fn digest(&self) -> String {
        digest_hex(TAG_PAYLOAD, self)
    }
}

/// The signed header of a Chain Link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkHeader {
    pub schema_version: u32,
    pub seq: u64,
    pub prev_hash: String,
    pub payload_digest: String,
    pub key_id: KeyId,
    pub ruleset_id: String,
    pub window_start_ms: u64,
    pub sealed_at_ms: u64,
    pub clock_regressed: bool,
}

/// One signed, hash-linked record of a sealed window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainLink {
    pub header: LinkHeader,
    pub payload: LinkPayload,
    /// `SHA-256(TAG_LINK || canonical_json(header))`, hex.
    pub entry_hash: String,
    /// Ed25519 over `TAG_SIG_LINK || entry_hash`, 64 bytes of hex.
    pub signature: String,
}
