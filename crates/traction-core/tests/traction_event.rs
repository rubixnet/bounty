//! Seam 1: the `traction-core` public API for Traction Events.
//!
//! Every test goes through `TractionEvent::from_json` (the single gate an
//! Adapter's payload passes through) or the public types and `validate`.

use serde_json::{Value, json};
use traction_core::{
    EndReason, EventError, EventType, HarnessId, MetricsOnly, NetworkClass, TractionEvent,
    ValidationError,
};

/// The `docs/protocol.md` section 2 example Traction Event, with each enum
/// slot filled by one concrete protocol value.
fn protocol_example() -> Value {
    json!({
        "schema_version": 1,
        "event_id": "019a7c2e-8b4d-7000-8000-000000000001",
        "timestamp_ms": 1795432800000u64,
        "harness_id": "claude-code",
        "session_id": "string",
        "parent_orchestrator_id": null,
        "event_type": "TURN_END",
        "end_reason": "completed",
        "cwd": "/abs/path/to/working/dir",
        "project_id": "git:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "model_id": "optional-model-string",
        "network_class": "cloud_api",
        "metrics_only": {
            "prompt_char_len": 142,
            "repo_symbol_hits": 3,
            "has_stacktrace": true,
            "tool_name": "Bash",
            "exit_code": 1,
            "diff_lines_delta": 24
        }
    })
}

fn parse(value: &Value) -> Result<TractionEvent, EventError> {
    TractionEvent::from_json(value.to_string().as_bytes())
}

/// Serialize a parsed Traction Event back to a JSON value.
fn reserialize(event: &TractionEvent) -> Value {
    serde_json::to_value(event).expect("a Traction Event always serializes")
}

#[test]
fn protocol_example_traction_event_round_trips_losslessly() {
    let wire = protocol_example();
    let event = parse(&wire).expect("the protocol example is a valid Traction Event");
    assert_eq!(reserialize(&event), wire);
}

/// The protocol example with `field` (a top-level key) replaced by `value`.
fn with(field: &str, value: Value) -> Value {
    let mut wire = protocol_example();
    wire[field] = value;
    wire
}

/// The protocol example with `field` inside `metrics_only` replaced by `value`.
fn with_metric(field: &str, value: Value) -> Value {
    let mut wire = protocol_example();
    wire["metrics_only"][field] = value;
    wire
}

/// A non-`TURN_END` Traction Event of `event_type` (so no `end_reason`).
fn non_turn_end(event_type: &str) -> Value {
    let mut wire = with("event_type", json!(event_type));
    wire["end_reason"] = Value::Null;
    wire
}

fn assert_malformed(wire: &Value) {
    assert_eq!(parse(wire), Err(EventError::Malformed));
}

#[test]
fn every_harness_id_round_trips_with_its_protocol_spelling() {
    let spellings = [
        (HarnessId::ClaudeCode, "claude-code"),
        (HarnessId::OpenCode, "opencode"),
        (HarnessId::CodexCli, "codex-cli"),
        (HarnessId::CodexDesktop, "codex-desktop"),
    ];
    assert_eq!(spellings.len(), HarnessId::ALL.len());
    for (harness, spelling) in spellings {
        let wire = with("harness_id", json!(spelling));
        let event = parse(&wire).expect("every protocol Harness is accepted");
        assert_eq!(event.harness_id, harness);
        assert_eq!(reserialize(&event), wire);
    }
}

#[test]
fn every_event_type_round_trips_with_its_protocol_spelling() {
    let spellings = [
        (EventType::SessionStart, "SESSION_START"),
        (EventType::PromptSubmit, "PROMPT_SUBMIT"),
        (EventType::PermissionRequest, "PERMISSION_REQUEST"),
        (EventType::ToolPre, "TOOL_PRE"),
        (EventType::ToolPost, "TOOL_POST"),
        (EventType::ToolFail, "TOOL_FAIL"),
        (EventType::SubagentStart, "SUBAGENT_START"),
        (EventType::SubagentStop, "SUBAGENT_STOP"),
        (EventType::CwdChange, "CWD_CHANGE"),
        (EventType::TurnEnd, "TURN_END"),
        (EventType::StopFailure, "STOP_FAILURE"),
        (EventType::SessionEnd, "SESSION_END"),
        (EventType::FileMutation, "FILE_MUTATION"),
    ];
    assert_eq!(spellings.len(), EventType::ALL.len());
    for (event_type, spelling) in spellings {
        let wire = if event_type == EventType::TurnEnd {
            protocol_example()
        } else {
            non_turn_end(spelling)
        };
        let event = parse(&wire).expect("every protocol event type is accepted");
        assert_eq!(event.event_type, event_type);
        assert_eq!(reserialize(&event), wire);
    }
}

#[test]
fn every_network_class_and_end_reason_round_trips_with_its_protocol_spelling() {
    for (class, spelling) in [
        (NetworkClass::CloudApi, "cloud_api"),
        (NetworkClass::LocalModel, "local_model"),
        (NetworkClass::Manual, "manual"),
    ] {
        let wire = with("network_class", json!(spelling));
        let event = parse(&wire).expect("every protocol network class is accepted");
        assert_eq!(event.network_class, class);
        assert_eq!(reserialize(&event), wire);
    }
    for (reason, spelling) in [
        (EndReason::Completed, "completed"),
        (EndReason::Aborted, "aborted"),
    ] {
        let wire = with("end_reason", json!(spelling));
        let event = parse(&wire).expect("every protocol end reason is accepted");
        assert_eq!(event.end_reason, Some(reason));
        assert_eq!(reserialize(&event), wire);
    }
}

#[test]
fn optional_fields_and_metrics_may_all_be_null() {
    let mut wire = non_turn_end("SESSION_START");
    wire["parent_orchestrator_id"] = json!("orchestrator-1");
    wire["model_id"] = Value::Null;
    wire["metrics_only"] = json!({
        "prompt_char_len": null,
        "repo_symbol_hits": null,
        "has_stacktrace": null,
        "tool_name": null,
        "exit_code": null,
        "diff_lines_delta": null
    });
    let event = parse(&wire).expect("a Traction Event with no metrics is valid");
    assert_eq!(event.metrics_only, MetricsOnly::default());
    assert_eq!(reserialize(&event), wire);
}

#[test]
fn rejects_unknown_top_level_field_carrying_raw_prompt() {
    assert_malformed(&with("prompt", json!("SENTINEL")));
}

#[test]
fn rejects_unknown_metrics_only_field_carrying_raw_stdout() {
    assert_malformed(&with_metric("stdout", json!("SENTINEL")));
}

#[test]
fn rejects_wrong_enum_spellings() {
    assert_malformed(&with("harness_id", json!("Claude-Code")));
    assert_malformed(&with("harness_id", json!("cursor")));
    assert_malformed(&with("event_type", json!("turn_end")));
    assert_malformed(&with("network_class", json!("cloud-api")));
    assert_malformed(&with("end_reason", json!("Completed")));
}

#[test]
fn rejects_floats_where_integers_are_required() {
    assert_malformed(&with("timestamp_ms", json!(1795432800000.5)));
    assert_malformed(&with("schema_version", json!(1.0)));
    assert_malformed(&with_metric("prompt_char_len", json!(142.0)));
    assert_malformed(&with_metric("repo_symbol_hits", json!(3.5)));
    assert_malformed(&with_metric("exit_code", json!(1.0)));
    assert_malformed(&with_metric("diff_lines_delta", json!(-2.5)));
}

#[test]
fn rejects_payloads_that_are_not_json() {
    assert_eq!(
        TractionEvent::from_json(b"prompt=SENTINEL"),
        Err(EventError::Malformed)
    );
}

#[test]
fn rejects_duplicate_keys() {
    let wire = protocol_example().to_string();
    let duplicated = wire.replacen('{', r#"{"session_id":"first","#, 1);
    assert_eq!(
        TractionEvent::from_json(duplicated.as_bytes()),
        Err(EventError::Malformed)
    );
}

#[test]
fn rejection_codes_never_echo_the_payload() {
    let error = parse(&with("prompt", json!("SENTINEL"))).unwrap_err();
    assert!(!error.to_string().contains("SENTINEL"));
    assert!(!format!("{error:?}").contains("SENTINEL"));
}

fn assert_invalid(wire: &Value, expected: ValidationError) {
    assert_eq!(parse(wire), Err(EventError::Invalid(expected)));
}

#[test]
fn rejects_turn_end_without_end_reason() {
    assert_invalid(
        &with("end_reason", Value::Null),
        ValidationError::MissingEndReason,
    );
}

#[test]
fn rejects_end_reason_on_every_non_turn_end_event() {
    for event_type in EventType::ALL {
        if event_type == EventType::TurnEnd {
            continue;
        }
        let spelling = serde_json::to_value(event_type).unwrap();
        assert_invalid(
            &with("event_type", spelling),
            ValidationError::UnexpectedEndReason,
        );
    }
}

#[test]
fn rejects_unsupported_schema_version() {
    assert_invalid(
        &with("schema_version", json!(2)),
        ValidationError::UnsupportedSchemaVersion,
    );
    assert_invalid(
        &with("schema_version", json!(0)),
        ValidationError::UnsupportedSchemaVersion,
    );
}

#[test]
fn accepts_uuid_event_ids_in_either_case() {
    assert!(
        parse(&with(
            "event_id",
            json!("019A7C2E-8B4D-7000-8000-00000000000F")
        ))
        .is_ok()
    );
    assert!(
        parse(&with(
            "event_id",
            json!("6f1c1a2b-3c4d-4e5f-8a9b-0c1d2e3f4a5b")
        ))
        .is_ok()
    );
}

#[test]
fn rejects_malformed_event_ids() {
    for bad in [
        "",
        "not-a-uuid",
        "019a7c2e8b4d70008000000000000001",
        "019a7c2e-8b4d-7000-8000-00000000000",
        "019a7c2e-8b4d-7000-8000-0000000000011",
        "019a7c2e-8b4d-7000-8000-00000000000g",
        "{019a7c2e-8b4d-7000-8000-000000000001}",
        "019a7c2e_8b4d_7000_8000_000000000001",
    ] {
        assert_invalid(
            &with("event_id", json!(bad)),
            ValidationError::InvalidEventId,
        );
    }
}

#[test]
fn accepts_both_project_id_forms() {
    let hex = "a".repeat(64);
    assert!(parse(&with("project_id", json!(format!("git:{hex}")))).is_ok());
    assert!(parse(&with("project_id", json!(format!("dir:{hex}")))).is_ok());
}

#[test]
fn rejects_malformed_project_ids() {
    let hex = "0123456789abcdef".repeat(4);
    for bad in [
        String::new(),
        hex.clone(),
        format!("svn:{hex}"),
        format!("git:{}", &hex[..63]),
        format!("git:{hex}0"),
        format!("git:{}", hex.to_uppercase()),
        format!("dir:{}g", &hex[..63]),
        format!("GIT:{hex}"),
        format!("git: {}", &hex[..63]),
        "dir:/home/teen/my-game".to_owned(),
    ] {
        assert_invalid(
            &with("project_id", json!(bad)),
            ValidationError::InvalidProjectId,
        );
    }
}

#[test]
fn accepts_absolute_cwds_on_every_os() {
    for good in [
        "/",
        "/home/teen/code/my game",
        "C:\\Users\\teen\\code\\my-game",
        "d:/code/my-game",
        "\\\\server\\share\\my-game",
    ] {
        assert!(parse(&with("cwd", json!(good))).is_ok(), "{good}");
    }
}

#[test]
fn rejects_relative_cwds() {
    for bad in [
        "",
        "code/my-game",
        "./my-game",
        "~/code/my-game",
        "C:my-game",
        "\\code",
    ] {
        assert_invalid(&with("cwd", json!(bad)), ValidationError::InvalidCwd);
    }
}

#[test]
fn rejects_over_long_or_control_character_cwds() {
    let long = format!("/{}", "a".repeat(4096));
    assert_invalid(&with("cwd", json!(long)), ValidationError::InvalidCwd);
    assert_invalid(
        &with("cwd", json!("/home/teen\nSENTINEL")),
        ValidationError::InvalidCwd,
    );
}

#[test]
fn accepts_realistic_session_ids() {
    for good in [
        "5f3c8e2a-1b2c-4d3e-9f4a-0b1c2d3e4f5a",
        "ses_7a8b9c0d1e2f3a4b5c6d7e8f",
        "rollout-2026-11-25T10-00-00",
    ] {
        assert!(parse(&with("session_id", json!(good))).is_ok(), "{good}");
    }
}

#[test]
fn rejects_unbounded_or_unsafe_session_ids() {
    for bad in [
        String::new(),
        "s".repeat(129),
        "fix the bug in main.rs".to_owned(),
        "session\nSENTINEL".to_owned(),
        "../../etc/passwd".to_owned(),
    ] {
        assert_invalid(
            &with("session_id", json!(bad)),
            ValidationError::InvalidSessionId,
        );
    }
}

#[test]
fn rejects_unbounded_or_unsafe_parent_orchestrator_ids() {
    for bad in [String::new(), "p".repeat(129), "please refactor".to_owned()] {
        assert_invalid(
            &with("parent_orchestrator_id", json!(bad)),
            ValidationError::InvalidParentOrchestratorId,
        );
    }
}

#[test]
fn accepts_realistic_tool_names_and_model_ids() {
    for tool in ["Bash", "mcp__github__create_issue", "apply_patch", "read"] {
        assert!(
            parse(&with_metric("tool_name", json!(tool))).is_ok(),
            "{tool}"
        );
    }
    for model in [
        "claude-opus-4-1-20250805",
        "anthropic/claude-sonnet-4.5",
        "ollama/qwen2.5-coder:7b",
        "gpt-5-codex",
    ] {
        assert!(parse(&with("model_id", json!(model))).is_ok(), "{model}");
    }
}

#[test]
fn rejects_over_long_tool_names() {
    assert_invalid(
        &with_metric("tool_name", json!("t".repeat(65))),
        ValidationError::InvalidToolName,
    );
}

#[test]
fn rejects_whitespace_laden_or_unsafe_tool_names() {
    for bad in [
        "",
        "Bash ",
        "rm -rf /",
        "Bash\tSENTINEL",
        "Bash\nSENTINEL",
        "tool\"x",
    ] {
        assert_invalid(
            &with_metric("tool_name", json!(bad)),
            ValidationError::InvalidToolName,
        );
    }
}

#[test]
fn rejects_over_long_model_ids() {
    assert_invalid(
        &with("model_id", json!("m".repeat(65))),
        ValidationError::InvalidModelId,
    );
}

#[test]
fn rejects_whitespace_laden_or_unsafe_model_ids() {
    for bad in [
        "",
        " claude",
        "claude  opus",
        "claude\nSENTINEL",
        "model@host",
    ] {
        assert_invalid(
            &with("model_id", json!(bad)),
            ValidationError::InvalidModelId,
        );
    }
}
