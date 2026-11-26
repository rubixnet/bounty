//! The single gate every Traction Event passes: deserialize, then validate.
//!
//! Errors never carry input values, so callers can log or return them
//! without echoing raw content.

use std::fmt;

use crate::event::{AttributionSource, EventType, NetworkClass, SCHEMA_VERSION, TractionEvent};

/// Longest accepted `tool_name` or `model_id`, in bytes.
pub const MAX_IDENTIFIER_LEN: usize = 64;
/// Longest accepted `session_id` or `parent_orchestrator_id`, in bytes.
pub const MAX_SESSION_ID_LEN: usize = 128;
/// Longest accepted `cwd`, in bytes.
pub const MAX_CWD_LEN: usize = 4096;
/// Number of hex digits after the `git:` or `dir:` prefix of a `project_id`.
const PROJECT_HASH_HEX_LEN: usize = 64;

/// Why a payload was refused as a Traction Event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventError {
    /// Not JSON, or JSON that does not fit the Traction Event shape (unknown
    /// field, wrong enum spelling, float where an integer is required, ...).
    Malformed,
    /// Well-formed, but breaks a Traction Event rule.
    Invalid(ValidationError),
}

impl EventError {
    /// A short, stable, machine-readable reason code.
    pub fn code(&self) -> &'static str {
        match self {
            EventError::Malformed => "malformed_event",
            EventError::Invalid(error) => error.code(),
        }
    }
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for EventError {}

impl From<ValidationError> for EventError {
    fn from(error: ValidationError) -> Self {
        EventError::Invalid(error)
    }
}

/// A rule a deserialized Traction Event breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {
    /// A `schema_version` other than [`SCHEMA_VERSION`].
    UnsupportedSchemaVersion,
    /// An `event_id` that is not a hyphenated UUID.
    InvalidEventId,
    /// A `session_id` that is empty, too long or has unsafe characters.
    InvalidSessionId,
    /// A `parent_orchestrator_id` that is empty, too long or has unsafe
    /// characters.
    InvalidParentOrchestratorId,
    /// A `TURN_END` without an `end_reason`.
    MissingEndReason,
    /// An `end_reason` on an event type other than `TURN_END`.
    UnexpectedEndReason,
    /// A `cwd` that is not absolute, too long or has control characters.
    InvalidCwd,
    /// A `project_id` that is not `git:<64 hex>` or `dir:<64 hex>`.
    InvalidProjectId,
    /// A `model_id` that is empty, too long or not a safe identifier.
    InvalidModelId,
    /// A `metrics_only.tool_name` that is empty, too long or not a safe
    /// identifier.
    InvalidToolName,
    /// No `harness_id` where one is required: on every event type except
    /// `FILE_MUTATION`, and on a `FILE_MUTATION` that has a `session_id`.
    MissingHarness,
    /// No `session_id` where one is required: on every event type except
    /// `FILE_MUTATION`, and on a `FILE_MUTATION` that has a `harness_id`.
    MissingSession,
    /// A manual `FILE_MUTATION` (no Harness, no session) whose
    /// `network_class` is not `manual`.
    ManualMutationNotManual,
    /// `attribution_source: file_watcher` on an event type other than
    /// `FILE_MUTATION`.
    UnexpectedFileWatcher,
    /// A `FILE_MUTATION` whose `attribution_source` is not `file_watcher`.
    InvalidAttributionSource,
    /// A `FILE_MUTATION` without a `metrics_only.mutation_kind`.
    MissingMutationKind,
    /// A `metrics_only.mutation_kind` on an event type other than
    /// `FILE_MUTATION`.
    UnexpectedMutationKind,
}

impl ValidationError {
    /// A short, stable, machine-readable reason code.
    pub fn code(&self) -> &'static str {
        match self {
            ValidationError::UnsupportedSchemaVersion => "unsupported_schema_version",
            ValidationError::InvalidEventId => "invalid_event_id",
            ValidationError::InvalidSessionId => "invalid_session_id",
            ValidationError::InvalidParentOrchestratorId => "invalid_parent_orchestrator_id",
            ValidationError::MissingEndReason => "missing_end_reason",
            ValidationError::UnexpectedEndReason => "unexpected_end_reason",
            ValidationError::InvalidCwd => "invalid_cwd",
            ValidationError::InvalidProjectId => "invalid_project_id",
            ValidationError::InvalidModelId => "invalid_model_id",
            ValidationError::InvalidToolName => "invalid_tool_name",
            ValidationError::MissingHarness => "missing_harness",
            ValidationError::MissingSession => "missing_session",
            ValidationError::ManualMutationNotManual => "manual_mutation_not_manual",
            ValidationError::UnexpectedFileWatcher => "unexpected_file_watcher",
            ValidationError::InvalidAttributionSource => "invalid_attribution_source",
            ValidationError::MissingMutationKind => "missing_mutation_kind",
            ValidationError::UnexpectedMutationKind => "unexpected_mutation_kind",
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for ValidationError {}

impl TractionEvent {
    /// Parse and validate a Traction Event from JSON bytes.
    ///
    /// This is the one entry point Adapters' payloads go through.
    pub fn from_json(bytes: &[u8]) -> Result<Self, EventError> {
        let event: TractionEvent =
            serde_json::from_slice(bytes).map_err(|_| EventError::Malformed)?;
        event.validate()?;
        Ok(event)
    }

    /// Check every Traction Event rule that the type alone cannot express.
    ///
    /// Integer-only numbers, closed enums and unknown-field rejection are
    /// enforced by deserialization; this checks the rest.
    pub fn validate(&self) -> Result<(), ValidationError> {
        check(
            self.schema_version == SCHEMA_VERSION,
            ValidationError::UnsupportedSchemaVersion,
        )?;
        check(is_uuid(&self.event_id), ValidationError::InvalidEventId)?;
        if let Some(session_id) = &self.session_id {
            check(is_session_id(session_id), ValidationError::InvalidSessionId)?;
        }
        if let Some(parent) = &self.parent_orchestrator_id {
            check(
                is_session_id(parent),
                ValidationError::InvalidParentOrchestratorId,
            )?;
        }
        match (self.event_type, self.end_reason) {
            (EventType::TurnEnd, None) => return Err(ValidationError::MissingEndReason),
            (EventType::TurnEnd, Some(_)) | (_, None) => {}
            (_, Some(_)) => return Err(ValidationError::UnexpectedEndReason),
        }
        self.validate_attribution()?;
        if let Some(cwd) = &self.cwd {
            check(is_absolute_cwd(cwd), ValidationError::InvalidCwd)?;
        }
        if let Some(project_id) = &self.project_id {
            check(is_project_id(project_id), ValidationError::InvalidProjectId)?;
        }
        if let Some(model_id) = &self.model_id {
            check(is_identifier(model_id), ValidationError::InvalidModelId)?;
        }
        if let Some(tool_name) = &self.metrics_only.tool_name {
            check(is_identifier(tool_name), ValidationError::InvalidToolName)?;
        }
        Ok(())
    }

    /// Harness, session, network class, attribution source and mutation
    /// kind rules. Only a `FILE_MUTATION` may lack a Harness (manual work),
    /// and only the file watcher produces `FILE_MUTATION`s.
    fn validate_attribution(&self) -> Result<(), ValidationError> {
        let is_mutation = self.event_type == EventType::FileMutation;
        let manual = is_mutation && self.harness_id.is_none() && self.session_id.is_none();
        if !manual {
            check(self.harness_id.is_some(), ValidationError::MissingHarness)?;
            check(self.session_id.is_some(), ValidationError::MissingSession)?;
        }
        if manual {
            check(
                self.network_class == NetworkClass::Manual,
                ValidationError::ManualMutationNotManual,
            )?;
        }
        let from_watcher = self.attribution_source == AttributionSource::FileWatcher;
        if is_mutation {
            check(from_watcher, ValidationError::InvalidAttributionSource)?;
            check(
                self.metrics_only.mutation_kind.is_some(),
                ValidationError::MissingMutationKind,
            )?;
        } else {
            check(!from_watcher, ValidationError::UnexpectedFileWatcher)?;
            check(
                self.metrics_only.mutation_kind.is_none(),
                ValidationError::UnexpectedMutationKind,
            )?;
        }
        Ok(())
    }
}

fn check(ok: bool, error: ValidationError) -> Result<(), ValidationError> {
    if ok { Ok(()) } else { Err(error) }
}

/// A hyphenated UUID (`8-4-4-4-12` hex digits, either case).
fn is_uuid(value: &str) -> bool {
    let groups: Vec<&str> = value.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(group, len)| group.len() == len && group.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// A UUIDv7 (RFC 9562) for `unix_ms`: the 48-bit millisecond timestamp,
/// version 7, the variant bits, and `random` in the remaining bits (the
/// caller supplies the randomness; this crate does no I/O). Always passes
/// the `event_id` check.
pub fn uuid_v7(unix_ms: u64, random: [u8; 10]) -> String {
    let mut bytes = [0u8; 16];
    bytes[..6].copy_from_slice(&unix_ms.to_be_bytes()[2..]);
    bytes[6..].copy_from_slice(&random);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// A bounded session-style ID: letters, digits, `.`, `_`, `-`, `:`.
fn is_session_id(value: &str) -> bool {
    (1..=MAX_SESSION_ID_LEN).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':'))
}

/// A short, safe identifier: letters, digits, `.`, `_`, `-`, `:`, `/`.
/// No whitespace is allowed, so it cannot carry prose.
fn is_identifier(value: &str) -> bool {
    (1..=MAX_IDENTIFIER_LEN).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/'))
}

/// An absolute path on any OS the daemon or server runs on: POSIX (`/...`),
/// a Windows drive (`C:\...` or `C:/...`) or UNC (`\\server\share`).
///
/// Checked lexically rather than with `std::path`, so every OS agrees.
fn is_absolute_cwd(value: &str) -> bool {
    let absolute = match value.as_bytes() {
        [b'/', ..] | [b'\\', b'\\', ..] => true,
        [drive, b':', b'\\' | b'/', ..] => drive.is_ascii_alphabetic(),
        _ => false,
    };
    absolute && value.len() <= MAX_CWD_LEN && !value.chars().any(char::is_control)
}

/// `git:<64 lowercase hex>` or `dir:<64 lowercase hex>`.
pub(crate) fn is_project_id(value: &str) -> bool {
    let Some(hash) = value
        .strip_prefix("git:")
        .or_else(|| value.strip_prefix("dir:"))
    else {
        return false;
    };
    hash.len() == PROJECT_HASH_HEX_LEN
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
