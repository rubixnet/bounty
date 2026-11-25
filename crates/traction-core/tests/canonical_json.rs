//! Seam 1: canonical JSON through the `traction-core` public API.
//!
//! Canonical JSON is the only serializer later hashed by the Step 3
//! hash-chain and re-derived by the Step 4 server, so every assertion here is
//! on exact bytes.

use serde_json::{Value, json};
use traction_core::{
    AttributionSource, CanonicalJsonError, EndReason, EventType, HarnessId, MutationKind,
    TractionEvent,
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
        "attribution_source": "harness_hook",
        "metrics_only": {
            "prompt_char_len": 142,
            "repo_symbol_hits": 3,
            "has_stacktrace": true,
            "tool_name": "Bash",
            "exit_code": 1,
            "diff_lines_delta": 24,
            "mutation_kind": null
        }
    })
}

fn parse(value: &Value) -> TractionEvent {
    TractionEvent::from_json(value.to_string().as_bytes()).expect("a valid Traction Event")
}

/// The exact, checked-in canonical bytes of the protocol example.
const PROTOCOL_EXAMPLE_CANONICAL: &str = concat!(
    r#"{"attribution_source":"harness_hook","#,
    r#""cwd":"/abs/path/to/working/dir","#,
    r#""end_reason":"completed","#,
    r#""event_id":"019a7c2e-8b4d-7000-8000-000000000001","#,
    r#""event_type":"TURN_END","#,
    r#""harness_id":"claude-code","#,
    r#""metrics_only":{"diff_lines_delta":24,"exit_code":1,"has_stacktrace":true,"#,
    r#""mutation_kind":null,"prompt_char_len":142,"repo_symbol_hits":3,"tool_name":"Bash"},"#,
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

#[test]
fn absent_optional_fields_are_explicit_null_with_sorted_nested_keys_and_no_whitespace() {
    let mut wire = protocol_example();
    wire["event_type"] = json!("SESSION_START");
    wire["end_reason"] = Value::Null;
    wire["model_id"] = Value::Null;
    wire["metrics_only"] = json!({});
    let bytes = parse(&wire).to_canonical_json();
    assert_eq!(
        String::from_utf8(bytes).unwrap(),
        concat!(
            r#"{"attribution_source":"harness_hook","#,
            r#""cwd":"/abs/path/to/working/dir","end_reason":null,"#,
            r#""event_id":"019a7c2e-8b4d-7000-8000-000000000001","#,
            r#""event_type":"SESSION_START","harness_id":"claude-code","#,
            r#""metrics_only":{"diff_lines_delta":null,"exit_code":null,"has_stacktrace":null,"#,
            r#""mutation_kind":null,"prompt_char_len":null,"repo_symbol_hits":null,"tool_name":null},"#,
            r#""model_id":null,"network_class":"cloud_api","parent_orchestrator_id":null,"#,
            r#""project_id":"git:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","#,
            r#""schema_version":1,"session_id":"string","timestamp_ms":1795432800000}"#,
        )
    );
}

#[test]
fn nested_keys_sort_by_utf8_bytes_and_strings_escape_minimally() {
    // Uppercase sorts before lowercase and multi-byte UTF-8 after ASCII.
    let value = json!({
        "b": {"z": 1, "a": [{"y": true, "x": null}]},
        "a": "line\nquote\"é",
        "é": -5,
        "B": 0
    });
    assert_eq!(
        String::from_utf8(traction_core::canonical_json(&value).unwrap()).unwrap(),
        r#"{"B":0,"a":"line\nquote\"é","b":{"a":[{"x":null,"y":true}],"z":1},"é":-5}"#
    );
}

#[test]
fn equal_events_built_in_different_field_orders_give_identical_bytes() {
    let forward = protocol_example().to_string();
    let reversed = concat!(
        r#"{"metrics_only":{"diff_lines_delta":24,"exit_code":1,"tool_name":"Bash","#,
        r#""has_stacktrace":true,"repo_symbol_hits":3,"prompt_char_len":142},"#,
        r#""attribution_source":"harness_hook","#,
        r#""network_class":"cloud_api","model_id":"optional-model-string","#,
        r#""project_id":"git:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef","#,
        r#""cwd":"/abs/path/to/working/dir","end_reason":"completed","event_type":"TURN_END","#,
        r#""parent_orchestrator_id":null,"session_id":"string","harness_id":"claude-code","#,
        r#""timestamp_ms":1795432800000,"event_id":"019a7c2e-8b4d-7000-8000-000000000001","#,
        r#""schema_version":1}"#
    );
    let a = TractionEvent::from_json(forward.as_bytes()).unwrap();
    let b = TractionEvent::from_json(reversed.as_bytes()).unwrap();
    assert_eq!(a.to_canonical_json(), b.to_canonical_json());
    assert_eq!(
        String::from_utf8(a.to_canonical_json()).unwrap(),
        PROTOCOL_EXAMPLE_CANONICAL
    );
}

#[test]
fn every_event_type_and_harness_round_trips_through_canonical_json() {
    for event_type in EventType::ALL {
        for harness_id in HarnessId::ALL {
            let mut event = parse(&protocol_example());
            event.event_type = event_type;
            event.harness_id = Some(harness_id);
            event.end_reason = (event_type == EventType::TurnEnd).then_some(EndReason::Aborted);
            if event_type == EventType::FileMutation {
                event.attribution_source = AttributionSource::FileWatcher;
                event.metrics_only.mutation_kind = Some(MutationKind::Renamed);
            }
            event.validate().expect("still a valid Traction Event");
            let bytes = event.to_canonical_json();
            let back = TractionEvent::from_json(&bytes).expect("canonical bytes parse back");
            assert_eq!(back, event);
            assert_eq!(back.to_canonical_json(), bytes);
        }
    }
}

#[test]
fn floats_are_rejected() {
    // Non-finite floats cannot be represented in a `serde_json::Value` at all.
    for value in [
        json!(1.5),
        json!({"a": [0.0]}),
        json!({"x": 1e3}),
        json!(-0.0),
    ] {
        assert_eq!(
            traction_core::canonical_json(&value),
            Err(CanonicalJsonError::Float)
        );
    }
}

#[test]
fn integers_have_no_exponent_or_fraction_at_the_extremes() {
    let value = json!({"max": u64::MAX, "min": i64::MIN, "zero": 0});
    assert_eq!(
        String::from_utf8(traction_core::canonical_json(&value).unwrap()).unwrap(),
        r#"{"max":18446744073709551615,"min":-9223372036854775808,"zero":0}"#
    );
}
