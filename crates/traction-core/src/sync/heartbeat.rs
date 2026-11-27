//! The per-device heartbeat: one `POST /v1/anchors` carries the Chain Tip and
//! the rows sealed since the last acknowledged sequence number.

use serde::{Deserialize, Serialize};

use crate::sync::{SignedBuildStatement, TimelineMark};
use crate::{ActivityRollup, KeyId, WorkBucket};

/// Whether the request was sent close to the windows' end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncMode {
    Realtime,
    OfflineBuffered,
}

/// The Chain Tip being anchored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChainTip {
    pub key_id: KeyId,
    pub seq: u64,
    pub entry_hash: String,
    pub window_start_ms: u64,
}

/// Project metadata (`docs/protocol.md` section 2a). `linked_repo` is set
/// from the website, never by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectMeta {
    pub project_id: String,
    pub project_name: String,
    pub project_root_path: String,
    pub project_home_relative_path: Option<String>,
    pub os_username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatRequest {
    pub tip: ChainTip,
    pub sync_mode: SyncMode,
    pub sent_at_ms: u64,
    pub work_buckets: Vec<WorkBucket>,
    pub activity_rollups: Vec<ActivityRollup>,
    /// Metadata of the Projects named by `work_buckets`.
    pub projects: Vec<ProjectMeta>,
    /// Unsigned Timeline Marks (`docs/adr/0005`); absent from older daemons.
    #[serde(default)]
    pub timeline_marks: Vec<TimelineMark>,
    /// The CI-signed build statement, sent on the first heartbeat after the
    /// daemon starts (so a changed version is always reported). Soft
    /// provenance: see `sync::provenance`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub build: Option<SignedBuildStatement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    WorkBucket,
    ActivityRollup,
    TimelineMark,
}

/// Names one row without echoing its content: the minute or window start,
/// and the Project for a Work Bucket.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ItemRef {
    pub kind: ItemKind,
    pub start_ms: u64,
    pub project_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RejectedItem {
    pub item: ItemRef,
    /// A stable reason code (`IngestError::code`, or `window_too_old`).
    pub code: String,
}

/// Whether the tip created the Anchor or matched one already stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorStatus {
    Stored,
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HeartbeatResponse {
    pub anchor: AnchorStatus,
    pub server_received_at_ms: u64,
    pub accepted: Vec<ItemRef>,
    pub rejected: Vec<RejectedItem>,
}

/// Every non-2xx answer: `{"error": "<code>"}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ErrorBody {
    pub error: String,
}
