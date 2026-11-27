//! Bundle submission (`traction submit`): `POST /v1/bundles`, a `PUT` to the
//! returned URL, then `POST /v1/bundles/{id}/complete`.

use serde::{Deserialize, Serialize};

/// `POST /v1/bundles`: announces the bundle about to be uploaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleStartRequest {
    /// The Project the bundle is for (the manifest must say the same).
    pub project_id: String,
    /// The exact byte length of the bundle; the presigned URL accepts
    /// exactly this many bytes.
    pub size_bytes: u64,
}

/// The server's answer: `PUT` the bytes to `put_url` before it expires.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleStartResponse {
    pub bundle_id: String,
    pub put_url: String,
    pub expires_at_ms: u64,
}

/// `POST /v1/bundles/{id}/complete`. The server does not tell the submitter
/// the colour; reviewers read the stored decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleCompleteResponse {
    pub bundle_id: String,
    /// `submitted`.
    pub status: String,
}
