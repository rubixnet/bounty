//! Shared Traction types: the Traction Event wire schema, canonical JSON and
//! hash-chain math, used by the daemon now and by the server from Step 4.
//!
//! This crate must stay free of OS, I/O and async dependencies.

mod canonical;
mod chain;
mod event;
mod presence;
mod project_id;
mod rollup;
mod ruleset;
mod struggle;
pub mod sync;
mod url_policy;
mod validate;

pub use canonical::{CanonicalJsonError, canonical_json};
pub use chain::{
    AuditBundle, BUNDLE_SCHEMA, BuildError, BundleBuildError, BundleChain, BundleError,
    BundleInput, BundlePolicy, ChainError, ChainLink, ChainReport, ChainSummary, DeviceRowsSection,
    DeviceSigningKey, FRAME_DIGEST_NONE, GENESIS_PREV_HASH, KeyDiscontinuity, KeyId, KeyedChain,
    LinkHeader, LinkPayload, Manifest, ManifestBody, ManifestTip, PayloadPart, ProjectDigest,
    ProjectRowsSection, ProjectWindowRows, RowScope, RowSet, RowsError, SealError, Supersedes,
    TAG_DEVICE_ROWS, TAG_FRAMES, TAG_KEY_ID, TAG_LINK, TAG_MANIFEST, TAG_PAYLOAD, TAG_ROWS,
    TAG_SIG_LINK, TAG_SIG_MANIFEST, VerifyPolicy, VerifyingKeyHex, build_bundle,
    device_rows_digest, manifest_digest, sign_manifest, verify_bundle, verify_chain, verify_chains,
    verify_manifest,
};
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
pub use ruleset::{MINUTE_MS, PERSONA_MARKS_1, PersonaMarks, Ruleset, TRACTION_RULES_1};
pub use struggle::{BASELINE_BINS, BaselineDay, StruggleAxes, fold_baseline};
pub use url_policy::{same_origin, secure_http_url};
pub use validate::{
    EventError, MAX_CWD_LEN, MAX_IDENTIFIER_LEN, MAX_SESSION_ID_LEN, ValidationError, uuid_v7,
};
