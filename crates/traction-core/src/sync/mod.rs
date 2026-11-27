//! The wire contract between a daemon and `traction-server` (`docs/protocol.md`
//! section 3): Pairing, device-token exchange and the per-device heartbeat
//! (`POST /v1/anchors`). Every request and response of the daemon, the real
//! server and the cross-platform test servers is one of these types.
//!
//! All types reject unknown fields and hold no floats. Reason codes are
//! stable, value-free strings: they never echo what a client sent.

mod heartbeat;
mod overlap;
mod pairing;
mod timeline;
mod validate;

pub use heartbeat::{
    AnchorStatus, ChainTip, ErrorBody, HeartbeatRequest, HeartbeatResponse, ItemKind, ItemRef,
    ProjectMeta, RejectedItem, SyncMode,
};
pub use overlap::{MINUTE_CAP_SEC, MergedMinute, MinuteShare, merge_minute};
pub use pairing::{
    PairingPollRequest, PairingPollResponse, PairingStartRequest, PairingStartResponse,
    PairingStatus,
};
pub use timeline::{MAX_MARK_COUNT, TimelineMark, timeline_marks, validate_timeline_mark};
pub use validate::{
    IngestError, validate_project_id, validate_rollup, validate_ruleset,
    validate_window_not_after_tip, validate_work_bucket,
};

/// Codes carried as the `error` of an [`ErrorBody`] or as a
/// [`RejectedItem::code`] that the daemon treats specially.
pub mod codes {
    /// A window older than the server's `MAX_BACKLOG_AGE`: terminal, the
    /// daemon moves its cursor past it.
    pub const WINDOW_TOO_OLD: &str = "window_too_old";
    /// A tip window after `server_received_at + MAX_FUTURE_SKEW`.
    pub const FUTURE_WINDOW: &str = "future_window";
    /// The same `(key_id, seq)` already anchored with a different `entry_hash`.
    pub const ANCHOR_CONFLICT: &str = "anchor_conflict";
    /// The tip's key is not pinned for the calling device.
    pub const KEY_NOT_PINNED: &str = "key_not_pinned";
}
