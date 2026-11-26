//! Shared Traction types: the Traction Event wire schema, canonical JSON and
//! hash-chain math, used by the daemon now and by the server from Step 4.
//!
//! This crate must stay free of OS, I/O and async dependencies.

mod canonical;
mod event;
mod presence;
mod project_id;
mod rollup;
mod ruleset;
mod struggle;
mod validate;

pub use canonical::{CanonicalJsonError, canonical_json};
pub use event::{
    AttributionSource, EndReason, EventType, HarnessId, MetricsOnly, MutationKind, NetworkClass,
    SCHEMA_VERSION, TractionEvent,
};
pub use presence::{
    AppCategory, CategorySec, PresenceError, PresenceMinute, SUBSLOT_SEC, SUBSLOTS_PER_MINUTE,
    TelemetryTier,
};
pub use project_id::{DEVICE_SECRET_LEN, dir_project_id, git_project_id, normalize_origin};
pub use rollup::{
    ActivityRollup, CategoryShareMilli, CommitObservation, RollupInput, RollupOutput, WorkBucket,
    activity_rollups, rollup, work_buckets,
};
pub use ruleset::{Ruleset, TRACTION_RULES_1};
pub use struggle::{BASELINE_BINS, BaselineDay, StruggleAxes, fold_baseline};
pub use validate::{
    EventError, MAX_CWD_LEN, MAX_IDENTIFIER_LEN, MAX_SESSION_ID_LEN, ValidationError, uuid_v7,
};
