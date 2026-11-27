//! Seam: the `traction_core::sync` wire types. Every request and response
//! round-trips through JSON, and an unknown field is refused at every level.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use traction_core::sync::{
    AnchorStatus, ChainTip, ErrorBody, HeartbeatRequest, HeartbeatResponse, ItemKind, ItemRef,
    PairingPollRequest, PairingPollResponse, PairingStartRequest, PairingStartResponse,
    PairingStatus, ProjectMeta, RejectedItem, SyncMode, TimelineMark,
};
use traction_core::{
    ActivityRollup, DeviceSigningKey, HarnessId, StruggleAxes, TelemetryTier, WorkBucket,
    canonical_json,
};
use zeroize::Zeroizing;

fn project() -> String {
    format!("git:{}", "b".repeat(64))
}

fn bucket() -> WorkBucket {
    WorkBucket {
        project_id: project(),
        minute_start_ms: 1_800_000_000_000 / 60_000 * 60_000,
        ruleset_id: "traction-rules/1".to_owned(),
        agent_compute_sec: 30,
        idle_autonomous_sec: 10,
        human_active_sec: 40,
        permission_wait_ms: 1234,
        commits_human: 1,
        commits_agent: 0,
        injected_hid_detected: false,
        auto_approver_suspected: false,
        telemetry_tier: TelemetryTier::Reduced,
        tier_limited: true,
        struggle_axes: StruggleAxes {
            steering: 1,
            specificity: 2,
            friction: 3,
            comfort_delta: 4,
        },
        verified_struggle: 500,
        struggle_credit_sec: 20,
    }
}

fn rollup() -> ActivityRollup {
    ActivityRollup {
        window_start_ms: 1_800_000_000_000 / 120_000 * 120_000,
        ruleset_id: "traction-rules/1".to_owned(),
        minutes_observed: 2,
        category_share_milli: Default::default(),
        switch_rate_milli: 5,
        mean_cadence_entropy_milli: Some(7),
        injected_ratio_milli: None,
        telemetry_tier: TelemetryTier::Full,
    }
}

fn heartbeat() -> HeartbeatRequest {
    let key = DeviceSigningKey::from_seed(Zeroizing::new([9u8; 32])).unwrap();
    HeartbeatRequest {
        tip: ChainTip {
            key_id: key.key_id(),
            seq: 4,
            entry_hash: "c".repeat(64),
            window_start_ms: 1_800_000_000_000 / 120_000 * 120_000,
        },
        sync_mode: SyncMode::OfflineBuffered,
        sent_at_ms: 1_800_000_100_000,
        work_buckets: vec![bucket()],
        activity_rollups: vec![rollup()],
        projects: vec![ProjectMeta {
            project_id: project(),
            project_name: "demo".into(),
            project_root_path: "/home/u/demo".into(),
            project_home_relative_path: Some("~/demo".into()),
            os_username: "u".into(),
        }],
        timeline_marks: vec![TimelineMark {
            project_id: project(),
            minute_start_ms: 1_800_000_000_000 / 60_000 * 60_000,
            mutations_manual: 1,
            mutations_harness: 2,
            turns_started: 3,
            tool_failures: 0,
            harnesses: vec![HarnessId::ClaudeCode],
        }],
    }
}

fn round_trips<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(value: &T) {
    let text = serde_json::to_string(value).unwrap();
    assert_eq!(&serde_json::from_str::<T>(&text).unwrap(), value);
    // No floats anywhere on the wire.
    assert!(!contains_float(&serde_json::to_value(value).unwrap()));
}

fn contains_float(value: &Value) -> bool {
    match value {
        Value::Number(n) => n.is_f64(),
        Value::Array(items) => items.iter().any(contains_float),
        Value::Object(map) => map.values().any(contains_float),
        _ => false,
    }
}

/// Adds `"zz_extra": 1` to the object at each path and asserts the document
/// no longer parses.
fn rejects_extra_at<T: Serialize + DeserializeOwned>(value: &T, paths: &[&[&str]]) {
    for path in paths {
        let mut doc = serde_json::to_value(value).unwrap();
        let mut node = &mut doc;
        for step in *path {
            node = match step.parse::<usize>() {
                Ok(i) => &mut node[i],
                Err(_) => &mut node[*step],
            };
        }
        node.as_object_mut()
            .unwrap_or_else(|| panic!("{path:?} is not an object"))
            .insert("zz_extra".into(), json!(1));
        assert!(
            serde_json::from_value::<T>(doc).is_err(),
            "an unknown field at {path:?} was accepted"
        );
    }
}

#[test]
fn the_heartbeat_round_trips_and_refuses_unknown_fields_at_every_level() {
    let request = heartbeat();
    round_trips(&request);
    rejects_extra_at(
        &request,
        &[
            &[],
            &["tip"],
            &["work_buckets", "0"],
            &["work_buckets", "0", "struggle_axes"],
            &["activity_rollups", "0"],
            &["activity_rollups", "0", "category_share_milli"],
            &["projects", "0"],
            &["timeline_marks", "0"],
        ],
    );
}

#[test]
fn the_heartbeat_response_round_trips_and_refuses_unknown_fields() {
    let item = ItemRef {
        kind: ItemKind::WorkBucket,
        start_ms: 60_000,
        project_id: Some(project()),
    };
    let response = HeartbeatResponse {
        anchor: AnchorStatus::Duplicate,
        server_received_at_ms: 1_800_000_000_123,
        accepted: vec![item.clone()],
        rejected: vec![RejectedItem {
            item: ItemRef {
                kind: ItemKind::ActivityRollup,
                start_ms: 120_000,
                project_id: None,
            },
            code: "window_too_old".into(),
        }],
    };
    round_trips(&response);
    rejects_extra_at(
        &response,
        &[
            &[],
            &["accepted", "0"],
            &["rejected", "0"],
            &["rejected", "0", "item"],
        ],
    );
    round_trips(&ErrorBody {
        error: "future_window".into(),
    });
    rejects_extra_at(&ErrorBody { error: "x".into() }, &[&[]]);
}

#[test]
fn the_pairing_messages_round_trip_and_refuse_unknown_fields() {
    let start = PairingStartRequest {
        public_key: "d".repeat(64),
        previous_key_id: Some("ed25519:abc".into()),
        device_label: "laptop".into(),
    };
    round_trips(&start);
    rejects_extra_at(&start, &[&[]]);
    let started = PairingStartResponse {
        pair_code: "ABCD-EFGH".into(),
        poll_secret: "e".repeat(64),
        login_url: "http://x/auth/login?pair=ABCD-EFGH".into(),
        expires_in_sec: 600,
    };
    round_trips(&started);
    rejects_extra_at(&started, &[&[]]);
    let poll = PairingPollRequest {
        pair_code: "ABCD-EFGH".into(),
        poll_secret: "e".repeat(64),
    };
    round_trips(&poll);
    rejects_extra_at(&poll, &[&[]]);
    let polled = PairingPollResponse {
        status: PairingStatus::Approved,
        device_token: Some("f".repeat(64)),
        device_id: Some("id".into()),
    };
    round_trips(&polled);
    rejects_extra_at(&polled, &[&[]]);
}

#[test]
fn enums_use_snake_case_on_the_wire() {
    let value = serde_json::to_value(heartbeat()).unwrap();
    assert_eq!(value["sync_mode"], "offline_buffered");
    assert_eq!(value["work_buckets"][0]["telemetry_tier"], "reduced");
    // The heartbeat is canonical-JSON serializable (used for hashing later).
    assert!(canonical_json(&value).is_ok());
}
