//! Pairing: enrolling a device (`traction login`).

use serde::{Deserialize, Serialize};

use crate::sync::SignedBuildStatement;

/// `POST /v1/pairings`: the device announces its Device Signing Key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingStartRequest {
    /// The Ed25519 public key, 64 lowercase hex digits.
    pub public_key: String,
    /// The `key_id` of a key this machine was paired with before, when it
    /// is re-pairing a Successor Chain. The approving user must own it.
    pub previous_key_id: Option<String>,
    /// A label the user will see on the approval page (a host name).
    pub device_label: String,
    /// The CI-signed build statement compiled into the daemon, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<SignedBuildStatement>,
}

/// The server's answer: show `pair_code`, open `login_url`, then poll.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingStartResponse {
    /// Short code the approval page shows, so the user can match it.
    pub pair_code: String,
    /// Secret only the polling device knows (64 hex digits).
    pub poll_secret: String,
    /// `<server>/auth/login?pair=<pair_code>`.
    pub login_url: String,
    pub expires_in_sec: u32,
}

/// `POST /v1/pairings/poll`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingPollRequest {
    pub pair_code: String,
    pub poll_secret: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairingStatus {
    Pending,
    Approved,
    Expired,
}

/// The device-token exchange: `device_token` is present exactly once, on
/// the first poll after approval (32 random bytes, 64 hex digits).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PairingPollResponse {
    pub status: PairingStatus,
    pub device_token: Option<String>,
    pub device_id: Option<String>,
}
