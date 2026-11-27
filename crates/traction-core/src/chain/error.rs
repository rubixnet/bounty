//! Typed, deterministic chain errors. They carry sequence numbers, key IDs,
//! window starts and Project IDs only, never row values.

use super::KeyId;

/// Which part of a link's payload failed to recompute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PayloadPart {
    /// The payload as a whole (or its window and ruleset against the header).
    Payload,
    /// The window's Activity Rollup.
    DeviceRows,
    /// One Project's rows in the window.
    Project(String),
    /// The window's Frame Records (`frame_digest`).
    Frames,
}

/// Why a chain, a bundle manifest or a key failed verification.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ChainError {
    /// `schema_version` is not 1.
    UnsupportedSchema { seq: u64 },
    /// The first link is not a genesis link (earlier links were cut off).
    TruncatedGenesis { first_seq: u64 },
    /// A sequence number repeats or goes backwards.
    DuplicateSeq { seq: u64 },
    /// A sequence number was skipped.
    SeqGap { expected: u64, found: u64 },
    /// The link names a key that is not the chain's (or not pinned).
    UnknownKey { seq: u64, key_id: KeyId },
    /// A public key is malformed, off-curve or of small order.
    WeakKey,
    /// A digest the link signs does not recompute from the rows.
    PayloadHashMismatch { seq: u64, part: PayloadPart },
    /// `entry_hash` does not match the header.
    EntryHashMismatch { seq: u64 },
    /// The Ed25519 signature does not verify (strictly).
    InvalidSignature { seq: u64 },
    /// `prev_hash` is not the previous link's `entry_hash`.
    BrokenPrevHash { seq: u64 },
    /// The window does not advance, or is not aligned to the window length.
    WindowOrder { seq: u64 },
    /// A row lies in a window no Chain Link covers.
    UnanchoredRow { window_start_ms: u64 },
    /// The chain tip is not the expected one (truncation or extension).
    TipMismatch { expected_seq: u64, found_seq: u64 },
    /// A chain starts before the previous chain ended.
    ChainOverlap { key_id: KeyId },
    /// Two chains share one key.
    DuplicateChain { key_id: KeyId },
    /// A successor chain does not commit to the previous chain's tip.
    SupersedesMismatch { key_id: KeyId },
    /// A bundle manifest signature does not verify.
    ManifestSignatureInvalid,
    /// A bundle manifest does not list the chains it ships.
    ManifestChainsMismatch,
}

impl ChainError {
    /// A stable reason code.
    pub fn code(&self) -> &'static str {
        match self {
            ChainError::UnsupportedSchema { .. } => "unsupported_schema",
            ChainError::TruncatedGenesis { .. } => "truncated_genesis",
            ChainError::DuplicateSeq { .. } => "duplicate_seq",
            ChainError::SeqGap { .. } => "seq_gap",
            ChainError::UnknownKey { .. } => "unknown_key",
            ChainError::WeakKey => "weak_key",
            ChainError::PayloadHashMismatch { .. } => "payload_hash_mismatch",
            ChainError::EntryHashMismatch { .. } => "entry_hash_mismatch",
            ChainError::InvalidSignature { .. } => "invalid_signature",
            ChainError::BrokenPrevHash { .. } => "broken_prev_hash",
            ChainError::WindowOrder { .. } => "window_order",
            ChainError::UnanchoredRow { .. } => "unanchored_row",
            ChainError::TipMismatch { .. } => "tip_mismatch",
            ChainError::ChainOverlap { .. } => "chain_overlap",
            ChainError::DuplicateChain { .. } => "duplicate_chain",
            ChainError::SupersedesMismatch { .. } => "supersedes_mismatch",
            ChainError::ManifestSignatureInvalid => "manifest_signature_invalid",
            ChainError::ManifestChainsMismatch => "manifest_chains_mismatch",
        }
    }
}

impl ChainError {
    /// The sequence number the error points at, when it has one.
    pub fn seq(&self) -> Option<u64> {
        match self {
            ChainError::UnsupportedSchema { seq }
            | ChainError::DuplicateSeq { seq }
            | ChainError::UnknownKey { seq, .. }
            | ChainError::PayloadHashMismatch { seq, .. }
            | ChainError::EntryHashMismatch { seq }
            | ChainError::InvalidSignature { seq }
            | ChainError::BrokenPrevHash { seq }
            | ChainError::WindowOrder { seq } => Some(*seq),
            ChainError::TruncatedGenesis { first_seq } => Some(*first_seq),
            ChainError::SeqGap { found, .. } => Some(*found),
            ChainError::TipMismatch { found_seq, .. } => Some(*found_seq),
            _ => None,
        }
    }

    /// The key the error names, when it names one.
    pub fn key_id(&self) -> Option<&KeyId> {
        match self {
            ChainError::UnknownKey { key_id, .. }
            | ChainError::ChainOverlap { key_id }
            | ChainError::DuplicateChain { key_id }
            | ChainError::SupersedesMismatch { key_id } => Some(key_id),
            _ => None,
        }
    }
}

impl std::fmt::Display for ChainError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for ChainError {}

/// Why a Chain Link could not be sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealError {
    /// The window does not start after the previous link's window.
    WindowOrder,
    /// `supersedes` is only allowed on a genesis link.
    SupersedesOnNonGenesis,
    /// The previous header was signed by another key.
    ForeignPrev,
    /// The previous header's sequence number cannot be incremented.
    SeqOverflow,
}

impl SealError {
    /// A stable reason code.
    pub fn code(self) -> &'static str {
        match self {
            SealError::WindowOrder => "seal_window_order",
            SealError::SupersedesOnNonGenesis => "seal_supersedes_on_non_genesis",
            SealError::ForeignPrev => "seal_foreign_prev",
            SealError::SeqOverflow => "seal_seq_overflow",
        }
    }
}

impl std::fmt::Display for SealError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for SealError {}
