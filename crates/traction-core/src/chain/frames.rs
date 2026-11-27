//! Frames (Step 3b, spec Q8): the signed Frame Record and the two hashes the
//! chain commits to.
//!
//! ```text
//! frame_hash   = SHA-256("traction/v1/frame\0"  || webp_bytes || salt)
//! frame_digest = SHA-256("traction/v1/frames\0" || canonical_json([FrameRecord...] sorted by frame_id))
//!              = 32 zero bytes (FRAME_DIGEST_NONE) when the window has no Frames
//! ```
//!
//! A Frame Record holds ids, hashes, timestamps and a closed trigger enum,
//! never text. Redaction and expiry happen after sealing, so they are not in
//! the record.

use serde::{Deserialize, Serialize};

use super::{FRAME_DIGEST_NONE, TAG_FRAMES, digest_hex, hex, tagged_digest};

/// Domain tag: one Frame's WebP bytes and salt (deviation 2).
pub const TAG_FRAME: &[u8] = b"traction/v1/frame\0";

/// Salt length drawn per Frame.
pub const FRAME_SALT_LEN: usize = 16;

/// What caused a Frame. A closed set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameTrigger {
    Scheduled,
    TurnStart,
    ToolFail,
    FocusShift,
}

impl FrameTrigger {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            FrameTrigger::Scheduled => "scheduled",
            FrameTrigger::TurnStart => "turn_start",
            FrameTrigger::ToolFail => "tool_fail",
            FrameTrigger::FocusShift => "focus_shift",
        }
    }

    /// Parses a wire name.
    pub fn parse(text: &str) -> Option<Self> {
        [
            FrameTrigger::Scheduled,
            FrameTrigger::TurnStart,
            FrameTrigger::ToolFail,
            FrameTrigger::FocusShift,
        ]
        .into_iter()
        .find(|t| t.as_str() == text)
    }
}

/// The signed part of one Frame.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrameRecord {
    /// `"<captured_at_ms>-<2-digit counter>"` (see [`frame_id`]).
    pub frame_id: String,
    /// [`frame_hash`] in lowercase hex.
    pub frame_hash: String,
    pub captured_at_ms: u64,
    pub trigger: FrameTrigger,
    /// The schedule slot a scheduled Frame was taken for; `None` for event
    /// triggers.
    pub scheduled_at_ms: Option<u64>,
    /// The Capture Nonce the schedule came from; `None` for local jitter.
    pub nonce_id: Option<String>,
}

/// Why a Frame Record is malformed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameRecordError {
    /// `frame_id` does not follow the grammar or name `captured_at_ms`.
    FrameId,
    /// `frame_hash` is not 64 lowercase hex characters.
    FrameHash,
    /// `nonce_id` is empty, too long or not `[A-Za-z0-9_-]`.
    NonceId,
}

impl std::fmt::Display for FrameRecordError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            FrameRecordError::FrameId => "malformed_frame_id",
            FrameRecordError::FrameHash => "malformed_frame_hash",
            FrameRecordError::NonceId => "malformed_nonce_id",
        })
    }
}

impl std::error::Error for FrameRecordError {}

/// The id of the `counter`-th Frame (0–99) captured at `captured_at_ms`.
pub fn frame_id(captured_at_ms: u64, counter: u8) -> Option<String> {
    (counter < 100).then(|| format!("{captured_at_ms}-{counter:02}"))
}

/// Splits a well-formed `frame_id` into `(captured_at_ms, counter)`. The
/// grammar is `[1-9][0-9]*-[0-9]{2}` (or `0-NN`), so every id has exactly
/// one spelling and is a safe file name.
pub fn parse_frame_id(id: &str) -> Option<(u64, u8)> {
    let (ms, counter) = id.split_once('-')?;
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(ms) || (ms.len() > 1 && ms.starts_with('0')) || ms.len() > 20 {
        return None;
    }
    if counter.len() != 2 || !digits(counter) {
        return None;
    }
    Some((ms.parse().ok()?, counter.parse().ok()?))
}

impl FrameRecord {
    /// Checks the record's grammar: the id names `captured_at_ms`, the hash
    /// is lowercase hex and any `nonce_id` is a short token.
    pub fn validate(&self) -> Result<(), FrameRecordError> {
        match parse_frame_id(&self.frame_id) {
            Some((ms, _)) if ms == self.captured_at_ms => {}
            _ => return Err(FrameRecordError::FrameId),
        }
        if !is_lower_hex(&self.frame_hash, 64) {
            return Err(FrameRecordError::FrameHash);
        }
        if let Some(nonce) = &self.nonce_id
            && (nonce.is_empty()
                || nonce.len() > 64
                || !nonce
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
        {
            return Err(FrameRecordError::NonceId);
        }
        Ok(())
    }
}

fn is_lower_hex(text: &str, len: usize) -> bool {
    text.len() == len && text.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// `SHA-256(TAG_FRAME || webp || salt)`.
pub fn frame_hash(webp: &[u8], salt: &[u8]) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(webp.len() + salt.len());
    bytes.extend_from_slice(webp);
    bytes.extend_from_slice(salt);
    tagged_digest(TAG_FRAME, &bytes)
}

/// [`frame_hash`] in lowercase hex.
pub fn frame_hash_hex(webp: &[u8], salt: &[u8]) -> String {
    hex::encode(&frame_hash(webp, salt))
}

/// The `frame_digest` of one window's Frame Records (any order): sorted by
/// `frame_id`, or [`FRAME_DIGEST_NONE`] when there are none.
pub fn frame_digest(records: &[FrameRecord]) -> String {
    if records.is_empty() {
        return FRAME_DIGEST_NONE.to_owned();
    }
    let mut sorted: Vec<&FrameRecord> = records.iter().collect();
    sorted.sort_by(|a, b| a.frame_id.cmp(&b.frame_id));
    digest_hex(TAG_FRAMES, &sorted)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ids_have_one_spelling() {
        assert_eq!(
            frame_id(1_700_000_000_123, 7).as_deref(),
            Some("1700000000123-07")
        );
        assert_eq!(frame_id(5, 100), None);
        assert_eq!(
            parse_frame_id("1700000000123-07"),
            Some((1_700_000_000_123, 7))
        );
        assert_eq!(parse_frame_id("0-00"), Some((0, 0)));
        for bad in [
            "",
            "-",
            "1-",
            "-01",
            "01-01",
            "1-1",
            "1-001",
            "1-0a",
            "a-01",
            "1_01",
            "1-01-",
            "../1-01",
            "1-01.bin",
            " 1-01",
            "+1-01",
            "123456789012345678901-01",
        ] {
            assert_eq!(parse_frame_id(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn records_validate_their_grammar() {
        let good = FrameRecord {
            frame_id: "10-00".into(),
            frame_hash: "ab".repeat(32),
            captured_at_ms: 10,
            trigger: FrameTrigger::Scheduled,
            scheduled_at_ms: Some(9),
            nonce_id: None,
        };
        assert_eq!(good.validate(), Ok(()));
        let with = |f: &dyn Fn(&mut FrameRecord)| {
            let mut r = good.clone();
            f(&mut r);
            r.validate()
        };
        assert_eq!(
            with(&|r| r.captured_at_ms = 11),
            Err(FrameRecordError::FrameId)
        );
        assert_eq!(
            with(&|r| r.frame_hash = "AB".repeat(32)),
            Err(FrameRecordError::FrameHash)
        );
        assert_eq!(
            with(&|r| r.frame_hash.pop().map(drop).unwrap()),
            Err(FrameRecordError::FrameHash)
        );
        assert_eq!(
            with(&|r| r.nonce_id = Some("a b".into())),
            Err(FrameRecordError::NonceId)
        );
        assert_eq!(
            with(&|r| r.nonce_id = Some(String::new())),
            Err(FrameRecordError::NonceId)
        );
        assert_eq!(with(&|r| r.nonce_id = Some("n-1_x".into())), Ok(()));
    }

    #[test]
    fn unknown_fields_and_triggers_are_rejected() {
        let text = r#"{"frame_id":"1-00","frame_hash":"00","captured_at_ms":1,"trigger":"scheduled","scheduled_at_ms":null,"nonce_id":null,"title":"x"}"#;
        assert!(serde_json::from_str::<FrameRecord>(text).is_err());
        let text = r#"{"frame_id":"1-00","frame_hash":"00","captured_at_ms":1,"trigger":"other","scheduled_at_ms":null,"nonce_id":null}"#;
        assert!(serde_json::from_str::<FrameRecord>(text).is_err());
        for t in ["scheduled", "turn_start", "tool_fail", "focus_shift"] {
            assert_eq!(FrameTrigger::parse(t).map(FrameTrigger::as_str), Some(t));
        }
        assert_eq!(FrameTrigger::parse("other"), None);
    }
}
