//! The Traction Event wire type (`docs/protocol.md` section 2).
//!
//! The type has no free-text slot: every string is an enum, an identifier, a
//! path or a hash-derived ID, and unknown fields are rejected at every level.

use serde::{Deserialize, Serialize};

/// The only `schema_version` this crate accepts.
pub const SCHEMA_VERSION: u32 = 1;

/// One normalized Traction Event, as an Adapter sends it to the Local
/// Ingestion Bus.
///
/// Field order matches `docs/protocol.md`. Optional fields that are `None`
/// serialize as explicit `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TractionEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub timestamp_ms: u64,
    pub harness_id: HarnessId,
    pub session_id: String,
    pub parent_orchestrator_id: Option<String>,
    pub event_type: EventType,
    pub end_reason: Option<EndReason>,
    pub cwd: String,
    pub project_id: String,
    pub model_id: Option<String>,
    pub network_class: NetworkClass,
    pub metrics_only: MetricsOnly,
}

/// The Harness that produced a Traction Event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum HarnessId {
    #[serde(rename = "claude-code")]
    ClaudeCode,
    #[serde(rename = "opencode")]
    OpenCode,
    #[serde(rename = "codex-cli")]
    CodexCli,
    #[serde(rename = "codex-desktop")]
    CodexDesktop,
}

impl HarnessId {
    /// Every Harness, in protocol order.
    pub const ALL: [HarnessId; 4] = [
        HarnessId::ClaudeCode,
        HarnessId::OpenCode,
        HarnessId::CodexCli,
        HarnessId::CodexDesktop,
    ];
}

/// What happened in the Harness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum EventType {
    SessionStart,
    PromptSubmit,
    PermissionRequest,
    ToolPre,
    ToolPost,
    ToolFail,
    SubagentStart,
    SubagentStop,
    CwdChange,
    TurnEnd,
    StopFailure,
    SessionEnd,
    FileMutation,
}

impl EventType {
    /// Every event type, in protocol order.
    pub const ALL: [EventType; 13] = [
        EventType::SessionStart,
        EventType::PromptSubmit,
        EventType::PermissionRequest,
        EventType::ToolPre,
        EventType::ToolPost,
        EventType::ToolFail,
        EventType::SubagentStart,
        EventType::SubagentStop,
        EventType::CwdChange,
        EventType::TurnEnd,
        EventType::StopFailure,
        EventType::SessionEnd,
        EventType::FileMutation,
    ];
}

/// How a `TURN_END` finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndReason {
    Completed,
    Aborted,
}

/// Where the model behind the Harness runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkClass {
    CloudApi,
    LocalModel,
    Manual,
}

/// Metrics an Adapter computed in memory from raw content it then discarded.
///
/// Every field is optional because not every event type carries every metric.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsOnly {
    pub prompt_char_len: Option<u64>,
    pub repo_symbol_hits: Option<u64>,
    pub has_stacktrace: Option<bool>,
    pub tool_name: Option<String>,
    pub exit_code: Option<i64>,
    pub diff_lines_delta: Option<i64>,
}
