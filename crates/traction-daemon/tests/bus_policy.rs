//! Seam 2: Local Ingestion Bus policy on top of `traction-core` validation.
//! Only the daemon's own file watcher produces `FILE_MUTATION`s, and the
//! reserved Attribution Sources stay closed to external callers in v1.

mod common;

use common::{TestDaemon, stored_event_count, valid_event_value};

/// A `FILE_MUTATION` that `traction-core` accepts: manual work seen by the
/// file watcher.
fn manual_file_mutation(n: u32) -> serde_json::Value {
    let mut event = valid_event_value(n);
    event["event_type"] = serde_json::json!("FILE_MUTATION");
    event["end_reason"] = serde_json::Value::Null;
    event["harness_id"] = serde_json::Value::Null;
    event["session_id"] = serde_json::Value::Null;
    event["model_id"] = serde_json::Value::Null;
    event["network_class"] = serde_json::json!("manual");
    event["attribution_source"] = serde_json::json!("file_watcher");
    event["metrics_only"] = serde_json::json!({ "mutation_kind": "modified" });
    event
}

#[test]
fn an_externally_sent_file_mutation_is_refused_and_not_stored() {
    let mut daemon = TestDaemon::start();
    let caller = daemon.caller();
    let manual = manual_file_mutation(1);
    let mut attributed = manual_file_mutation(2);
    attributed["harness_id"] = serde_json::json!("claude-code");
    attributed["session_id"] = serde_json::json!("session-1");
    attributed["network_class"] = serde_json::json!("cloud_api");

    for event in [manual, attributed] {
        let response = caller.post_event(&event.to_string());
        assert_eq!(response.status, 400, "{event}");
        assert_eq!(response.body, r#"{"error":"external_file_mutation"}"#);
    }

    assert_eq!(stored_event_count(&daemon.shutdown_and_open_db()), 0);
}

#[test]
fn reserved_attribution_sources_are_refused_and_not_stored() {
    let mut daemon = TestDaemon::start();
    let caller = daemon.caller();

    for (n, source) in (1..).zip(["browser_extension", "editor_extension", "manual_pick"]) {
        let mut event = valid_event_value(n);
        event["attribution_source"] = serde_json::json!(source);
        let response = caller.post_event(&event.to_string());
        assert_eq!(response.status, 400, "{source}");
        assert_eq!(
            response.body, r#"{"error":"reserved_attribution_source"}"#,
            "{source}"
        );
    }

    assert_eq!(stored_event_count(&daemon.shutdown_and_open_db()), 0);
}

#[test]
fn file_watcher_on_any_other_event_is_refused() {
    let mut daemon = TestDaemon::start();
    let mut event = valid_event_value(1);
    event["attribution_source"] = serde_json::json!("file_watcher");

    let response = daemon.caller().post_event(&event.to_string());

    assert_eq!(response.status, 400);
    assert_eq!(response.body, r#"{"error":"unexpected_file_watcher"}"#);
    assert_eq!(stored_event_count(&daemon.shutdown_and_open_db()), 0);
}

#[test]
fn every_open_harness_source_is_accepted() {
    let mut daemon = TestDaemon::start();
    let caller = daemon.caller();
    let sources = [
        "harness_hook",
        "harness_plugin",
        "harness_log",
        "harness_db_poll",
    ];

    for (n, source) in (1..).zip(sources) {
        let mut event = valid_event_value(n);
        event["attribution_source"] = serde_json::json!(source);
        let response = caller.post_event(&event.to_string());
        assert_eq!(response.status, 202, "{source}: {}", response.body);
    }

    assert_eq!(stored_event_count(&daemon.shutdown_and_open_db()), 4);
}

#[cfg(unix)]
#[test]
fn the_unix_socket_applies_the_same_policy() {
    let mut daemon = TestDaemon::start();

    let response = daemon
        .socket_caller()
        .post_event(&manual_file_mutation(1).to_string());

    assert_eq!(response.status, 400);
    assert_eq!(response.body, r#"{"error":"external_file_mutation"}"#);
    assert_eq!(stored_event_count(&daemon.shutdown_and_open_db()), 0);
}
