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
/// serialize as explicit `null`; on the wire an optional field may also be
/// left out.
///
/// `harness_id` and `session_id` are absent only on a manual `FILE_MUTATION`
/// (work the file watcher saw outside every Harness window); `validate`
/// enforces when each may be missing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TractionEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub timestamp_ms: u64,
    pub harness_id: Option<HarnessId>,
    pub session_id: Option<String>,
    pub parent_orchestrator_id: Option<String>,
    pub event_type: EventType,
    pub end_reason: Option<EndReason>,
    pub cwd: Option<String>,
    pub project_id: Option<String>,
    pub model_id: Option<String>,
    pub network_class: NetworkClass,
    pub attribution_source: AttributionSource,
    pub metrics_only: MetricsOnly,
}

/// Which kind of source observed a Traction Event.
///
/// A closed set: the first five are today's sources; `browser_extension`,
/// `editor_extension` and `manual_pick` are reserved so that adding those
/// sources needs no schema migration. Core accepts every value; the Local
/// Ingestion Bus decides which ones an external caller may send.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttributionSource {
    /// A Harness HTTP hook (Claude Code).
    HarnessHook,
    /// An in-process Harness plugin (OpenCode).
    HarnessPlugin,
    /// A tailed Harness log (Codex rollout files).
    HarnessLog,
    /// A read-only poll of a Harness database (OpenCode fallback).
    HarnessDbPoll,
    /// The daemon's own file watcher. Valid only on `FILE_MUTATION`.
    FileWatcher,
    /// Reserved: a browser extension.
    BrowserExtension,
    /// Reserved: an editor extension.
    EditorExtension,
    /// Reserved: a later user-driven attribution.
    ManualPick,
}

impl AttributionSource {
    /// Every attribution source, in protocol order.
    pub const ALL: [AttributionSource; 8] = [
        AttributionSource::HarnessHook,
        AttributionSource::HarnessPlugin,
        AttributionSource::HarnessLog,
        AttributionSource::HarnessDbPoll,
        AttributionSource::FileWatcher,
        AttributionSource::BrowserExtension,
        AttributionSource::EditorExtension,
        AttributionSource::ManualPick,
    ];

    /// The exact wire spelling (the same string serde writes).
    pub const fn as_str(self) -> &'static str {
        match self {
            AttributionSource::HarnessHook => "harness_hook",
            AttributionSource::HarnessPlugin => "harness_plugin",
            AttributionSource::HarnessLog => "harness_log",
            AttributionSource::HarnessDbPoll => "harness_db_poll",
            AttributionSource::FileWatcher => "file_watcher",
            AttributionSource::BrowserExtension => "browser_extension",
            AttributionSource::EditorExtension => "editor_extension",
            AttributionSource::ManualPick => "manual_pick",
        }
    }
}

/// What happened to a file in a `FILE_MUTATION`. The file's path and name are
/// never carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MutationKind {
    Created,
    Modified,
    Removed,
    Renamed,
}

impl MutationKind {
    /// Every mutation kind, in protocol order.
    pub const ALL: [MutationKind; 4] = [
        MutationKind::Created,
        MutationKind::Modified,
        MutationKind::Removed,
        MutationKind::Renamed,
    ];

    /// The exact wire spelling (the same string serde writes).
    pub const fn as_str(self) -> &'static str {
        match self {
            MutationKind::Created => "created",
            MutationKind::Modified => "modified",
            MutationKind::Removed => "removed",
            MutationKind::Renamed => "renamed",
        }
    }
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

    /// The exact wire spelling (the same string serde writes).
    pub const fn as_str(self) -> &'static str {
        match self {
            HarnessId::ClaudeCode => "claude-code",
            HarnessId::OpenCode => "opencode",
            HarnessId::CodexCli => "codex-cli",
            HarnessId::CodexDesktop => "codex-desktop",
        }
    }
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

    /// The exact wire spelling (the same string serde writes).
    pub const fn as_str(self) -> &'static str {
        match self {
            EventType::SessionStart => "SESSION_START",
            EventType::PromptSubmit => "PROMPT_SUBMIT",
            EventType::PermissionRequest => "PERMISSION_REQUEST",
            EventType::ToolPre => "TOOL_PRE",
            EventType::ToolPost => "TOOL_POST",
            EventType::ToolFail => "TOOL_FAIL",
            EventType::SubagentStart => "SUBAGENT_START",
            EventType::SubagentStop => "SUBAGENT_STOP",
            EventType::CwdChange => "CWD_CHANGE",
            EventType::TurnEnd => "TURN_END",
            EventType::StopFailure => "STOP_FAILURE",
            EventType::SessionEnd => "SESSION_END",
            EventType::FileMutation => "FILE_MUTATION",
        }
    }
}

/// How a `TURN_END` finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EndReason {
    Completed,
    Aborted,
}

impl EndReason {
    /// Every end reason, in protocol order.
    pub const ALL: [EndReason; 2] = [EndReason::Completed, EndReason::Aborted];

    /// The exact wire spelling (the same string serde writes).
    pub const fn as_str(self) -> &'static str {
        match self {
            EndReason::Completed => "completed",
            EndReason::Aborted => "aborted",
        }
    }
}

/// Where the model behind the Harness runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NetworkClass {
    CloudApi,
    LocalModel,
    Manual,
}

impl NetworkClass {
    /// Every network class, in protocol order.
    pub const ALL: [NetworkClass; 3] = [
        NetworkClass::CloudApi,
        NetworkClass::LocalModel,
        NetworkClass::Manual,
    ];

    /// The exact wire spelling (the same string serde writes).
    pub const fn as_str(self) -> &'static str {
        match self {
            NetworkClass::CloudApi => "cloud_api",
            NetworkClass::LocalModel => "local_model",
            NetworkClass::Manual => "manual",
        }
    }
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
    /// Required on, and only on, a `FILE_MUTATION`.
    pub mutation_kind: Option<MutationKind>,
}
