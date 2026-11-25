//! Claude Code HTTP hooks: the route, the hook set and the payload mapping.
//!
//! A payload is parsed into an allowlisted struct: unknown fields (tool
//! input and output, assistant messages, error text) are skipped by the
//! parser and never kept, and the prompt is reduced to
//! [`TextMetrics`] inside the deserializer, so its text never outlives
//! parsing. Each hook maps to at most one Traction Event
//! (`docs/protocol.md` section 1, "Claude Code hooks").

use std::path::PathBuf;

use serde::{Deserialize, Deserializer};
use traction_core::{
    AttributionSource, EndReason, EventType, HarnessId, MAX_IDENTIFIER_LEN, MetricsOnly,
    NetworkClass, SCHEMA_VERSION, TractionEvent,
};

use crate::content_metrics::{self, TextMetrics};
use crate::event_id::fresh_event_id;

/// The bus route Claude Code's HTTP hooks post to.
pub const HOOK_PATH: &str = "/v1/hooks/claude-code";

/// Hook bodies above this many bytes are refused with `413` (hook payloads
/// embed whole tool outputs, so the `/v1/event` cap is far too small).
pub const MAX_HOOK_BODY_BYTES: usize = 8 * 1024 * 1024;

/// A Claude Code hook Traction installs. The set is closed: `WorktreeCreate`
/// is deliberately absent (hooking it replaces Git worktree creation and
/// breaks `claude --worktree`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hook {
    SessionStart,
    UserPromptSubmit,
    PermissionRequest,
    PreToolUse,
    PostToolUse,
    PostToolUseFailure,
    SubagentStart,
    SubagentStop,
    CwdChanged,
    DirectoryAdded,
    Stop,
    StopFailure,
    SessionEnd,
}

impl Hook {
    /// The installed hook set, in the order the spec lists it.
    pub const ALL: [Hook; 13] = [
        Hook::SessionStart,
        Hook::UserPromptSubmit,
        Hook::PermissionRequest,
        Hook::PreToolUse,
        Hook::PostToolUse,
        Hook::PostToolUseFailure,
        Hook::SubagentStart,
        Hook::SubagentStop,
        Hook::CwdChanged,
        Hook::DirectoryAdded,
        Hook::Stop,
        Hook::StopFailure,
        Hook::SessionEnd,
    ];

    /// The `hook_event_name` (and `settings.json` key) of this hook.
    pub const fn name(self) -> &'static str {
        match self {
            Hook::SessionStart => "SessionStart",
            Hook::UserPromptSubmit => "UserPromptSubmit",
            Hook::PermissionRequest => "PermissionRequest",
            Hook::PreToolUse => "PreToolUse",
            Hook::PostToolUse => "PostToolUse",
            Hook::PostToolUseFailure => "PostToolUseFailure",
            Hook::SubagentStart => "SubagentStart",
            Hook::SubagentStop => "SubagentStop",
            Hook::CwdChanged => "CwdChanged",
            Hook::DirectoryAdded => "DirectoryAdded",
            Hook::Stop => "Stop",
            Hook::StopFailure => "StopFailure",
            Hook::SessionEnd => "SessionEnd",
        }
    }

    /// The hook named `name`, if it is in the set.
    pub fn from_name(name: &str) -> Option<Hook> {
        Hook::ALL.into_iter().find(|hook| hook.name() == name)
    }

    /// Whether Claude Code filters this hook by tool name, so the installer
    /// puts it in a matcher group matching every tool.
    pub const fn is_tool_hook(self) -> bool {
        matches!(
            self,
            Hook::PermissionRequest
                | Hook::PreToolUse
                | Hook::PostToolUse
                | Hook::PostToolUseFailure
        )
    }
}

/// What one hook payload asks of the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookAction {
    /// Nothing to store: unusable payload or a hook outside the mapping.
    Nothing,
    /// Route and store this event.
    Event(Box<TractionEvent>),
    /// `DirectoryAdded`: register this folder as a Project.
    AddDirectory(PathBuf),
}

/// The only payload fields the daemon reads. Everything else is skipped.
#[derive(Deserialize)]
struct Payload {
    session_id: String,
    hook_event_name: String,
    #[serde(default)]
    cwd: Option<String>,
    /// Reduced to metrics while parsing; the text is never kept.
    #[serde(default, deserialize_with = "measured")]
    prompt: Option<TextMetrics>,
    #[serde(default)]
    tool_name: Option<String>,
    /// `SessionStart` only.
    #[serde(default)]
    model: Option<String>,
    /// `CwdChanged` only.
    #[serde(default)]
    new_cwd: Option<String>,
    /// `DirectoryAdded` only.
    #[serde(default)]
    directory: Option<String>,
}

fn measured<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<TextMetrics>, D::Error> {
    let text: Option<std::borrow::Cow<'de, str>> = Option::deserialize(deserializer)?;
    Ok(text.map(|text| content_metrics::measure(&text)))
}

/// Maps one raw hook body, received at `received_ms`, to what the daemon
/// does with it. Never fails: anything unusable is [`HookAction::Nothing`].
pub fn map_hook(body: &[u8], received_ms: u64) -> HookAction {
    let Ok(payload) = serde_json::from_slice::<Payload>(body) else {
        return HookAction::Nothing;
    };
    let Some(hook) = Hook::from_name(&payload.hook_event_name) else {
        return HookAction::Nothing;
    };
    let (event_type, end_reason) = match hook {
        Hook::DirectoryAdded => {
            return payload.directory.map_or(HookAction::Nothing, |dir| {
                HookAction::AddDirectory(PathBuf::from(dir))
            });
        }
        Hook::SessionStart => (EventType::SessionStart, None),
        Hook::UserPromptSubmit => (EventType::PromptSubmit, None),
        Hook::PermissionRequest => (EventType::PermissionRequest, None),
        Hook::PreToolUse => (EventType::ToolPre, None),
        Hook::PostToolUse => (EventType::ToolPost, None),
        Hook::PostToolUseFailure => (EventType::ToolFail, None),
        Hook::SubagentStart => (EventType::SubagentStart, None),
        Hook::SubagentStop => (EventType::SubagentStop, None),
        Hook::CwdChanged => (EventType::CwdChange, None),
        Hook::Stop => (EventType::TurnEnd, Some(EndReason::Completed)),
        Hook::StopFailure => (EventType::StopFailure, None),
        Hook::SessionEnd => (EventType::SessionEnd, None),
    };
    let cwd = match hook {
        Hook::CwdChanged => payload.new_cwd.or(payload.cwd),
        _ => payload.cwd,
    };
    let prompt = payload.prompt.filter(|_| hook == Hook::UserPromptSubmit);
    let tool_name = payload
        .tool_name
        .filter(|_| hook.is_tool_hook())
        .and_then(capped_identifier);
    let model_id = payload
        .model
        .filter(|_| hook == Hook::SessionStart)
        .and_then(|model| (model.len() <= MAX_IDENTIFIER_LEN).then_some(model))
        .and_then(capped_identifier);
    let event = TractionEvent {
        schema_version: SCHEMA_VERSION,
        event_id: fresh_event_id(received_ms),
        timestamp_ms: received_ms,
        harness_id: Some(HarnessId::ClaudeCode),
        session_id: Some(payload.session_id),
        parent_orchestrator_id: None,
        event_type,
        end_reason,
        cwd,
        project_id: None,
        model_id,
        network_class: NetworkClass::CloudApi,
        attribution_source: AttributionSource::HarnessHook,
        metrics_only: MetricsOnly {
            prompt_char_len: prompt.map(|m| m.prompt_char_len),
            has_stacktrace: prompt.map(|m| m.has_stacktrace),
            tool_name,
            ..MetricsOnly::default()
        },
    };
    match event.validate() {
        Ok(()) => HookAction::Event(Box::new(event)),
        Err(_) => HookAction::Nothing,
    }
}

/// `value` cut to [`MAX_IDENTIFIER_LEN`] bytes, kept only when it is a safe
/// identifier (letters, digits, `.`, `_`, `-`, `:`, `/`): never prose.
pub(crate) fn capped_identifier(mut value: String) -> Option<String> {
    let safe = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b':' | b'/');
    if value.is_empty() || !value.bytes().all(safe) {
        return None;
    }
    // All ASCII, so any byte index is a char boundary.
    value.truncate(MAX_IDENTIFIER_LEN);
    Some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_set_never_contains_worktree_create_and_names_round_trip() {
        assert_eq!(Hook::from_name("WorktreeCreate"), None);
        for hook in Hook::ALL {
            assert_eq!(Hook::from_name(hook.name()), Some(hook));
        }
    }
}
