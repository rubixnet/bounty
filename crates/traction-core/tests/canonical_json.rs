//! Seam 1: canonical JSON through the `traction-core` public API.
//!
//! Canonical JSON is the only serializer later hashed by the Step 3
//! hash-chain and re-derived by the Step 4 server, so every assertion here is
//! on exact bytes.

use serde_json::{Value, json};
use traction_core::TractionEvent;

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

fn parse(value: &Value) -> TractionEvent {
    TractionEvent::from_json(value.to_string().as_bytes()).expect("a valid Traction Event")
}

/// The exact, checked-in canonical bytes of the protocol example.
const PROTOCOL_EXAMPLE_CANONICAL: &str = concat!(
    r#"{"cwd":"/abs/path/to/working/dir","#,
    r#""end_reason":"completed","#,
    r#""event_id":"019a7c2e-8b4d-7000-8000-000000000001","#,
    r#""event_type":"TURN_END","#,
    r#""harness_id":"claude-code","#,
    r#""metrics_only":{"diff_lines_delta":24,"exit_code":1,"has_stacktrace":true,"#,
    r#""prompt_char_len":142,"repo_symbol_hits":3,"tool_name":"Bash"},"#,
    r#""model_id":"optional-model-string","#,
    r#""network_class":"cloud_api","#,
    r#""parent_orchestrator_id":null,"#,
    r#""project_id":"git:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","#,
    r#""schema_version":1,"#,
    r#""session_id":"string","#,
    r#""timestamp_ms":1795432800000}"#,
);

#[test]
fn protocol_example_has_golden_canonical_bytes() {
    let event = parse(&protocol_example());
    assert_eq!(
        String::from_utf8(event.to_canonical_json()).unwrap(),
        PROTOCOL_EXAMPLE_CANONICAL
    );
}
