//! Seam: the `PresenceMinute` wire type through the `traction-core` API.

use serde_json::{Value, json};
use traction_core::{AppCategory, CategorySec, PresenceMinute, TelemetryTier, canonical_json};

fn full_minute() -> Value {
    json!({
        "minute_start_ms": 1795432800000u64,
        "key_events": 40,
        "pointer_events": 120,
        "injected_events": 0,
        "active_subslots": 5,
        "cadence_entropy_milli": 2750,
        "category_sec": {"ide": 30, "terminal": 20, "browser": 10, "ai_desktop": 0, "other": 0},
        "category_switches": 3,
        "idle_sec": 0,
        "telemetry_tier": "full"
    })
}

#[test]
fn a_full_minute_round_trips_through_canonical_json() {
    let minute: PresenceMinute = serde_json::from_value(full_minute()).unwrap();
    assert_eq!(minute.telemetry_tier, TelemetryTier::Full);
    assert_eq!(minute.category_sec.unwrap().ide, 30);
    let value = serde_json::to_value(&minute).unwrap();
    assert_eq!(value, full_minute());
    let bytes = canonical_json(&value).unwrap();
    let text = String::from_utf8(bytes).unwrap();
    assert!(text.starts_with(r#"{"active_subslots":5,"cadence_entropy_milli":2750,"#));
    let back: PresenceMinute = serde_json::from_slice(text.as_bytes()).unwrap();
    assert_eq!(back, minute);
}

#[test]
fn fields_a_tier_cannot_fill_serialize_as_null_not_zero() {
    let minute = PresenceMinute {
        minute_start_ms: 60_000,
        key_events: None,
        pointer_events: None,
        injected_events: None,
        active_subslots: None,
        cadence_entropy_milli: None,
        category_sec: None,
        category_switches: None,
        idle_sec: None,
        telemetry_tier: TelemetryTier::None,
    };
    let value = serde_json::to_value(&minute).unwrap();
    for key in [
        "key_events",
        "pointer_events",
        "injected_events",
        "active_subslots",
        "cadence_entropy_milli",
        "category_sec",
        "category_switches",
        "idle_sec",
    ] {
        assert_eq!(value[key], Value::Null, "{key}");
    }
    assert_eq!(value["telemetry_tier"], "none");
}

#[test]
fn unknown_fields_and_unknown_enum_values_are_rejected() {
    let mut extra = full_minute();
    extra["window_title"] = json!("secret");
    assert!(serde_json::from_value::<PresenceMinute>(extra).is_err());

    let mut nested = full_minute();
    nested["category_sec"]["games"] = json!(1);
    assert!(serde_json::from_value::<PresenceMinute>(nested).is_err());

    let mut tier = full_minute();
    tier["telemetry_tier"] = json!("total");
    assert!(serde_json::from_value::<PresenceMinute>(tier).is_err());
}

#[test]
fn app_categories_are_a_closed_snake_case_set() {
    let names: Vec<String> = AppCategory::ALL
        .iter()
        .map(|c| {
            serde_json::to_value(c)
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(names, ["ide", "terminal", "browser", "ai_desktop", "other"]);
    assert!(serde_json::from_value::<AppCategory>(json!("games")).is_err());
    let mut sec = CategorySec::default();
    sec.add(AppCategory::AiDesktop, 7);
    assert_eq!(sec.ai_desktop, 7);
    assert_eq!(sec.total(), 7);
}

#[test]
fn active_subslots_above_six_are_invalid() {
    let mut value = full_minute();
    value["active_subslots"] = json!(7);
    let minute: PresenceMinute = serde_json::from_value(value).unwrap();
    assert!(minute.validate().is_err());
    let ok: PresenceMinute = serde_json::from_value(full_minute()).unwrap();
    assert!(ok.validate().is_ok());
}
