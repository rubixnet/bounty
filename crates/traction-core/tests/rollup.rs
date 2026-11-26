//! Seam: the pure rollup (`rollup`, `work_buckets`, `activity_rollups`)
//! through the `traction-core` API. No I/O, no clock.

use traction_core::{
    AttributionSource, CategorySec, CommitObservation, EndReason, EventType, HarnessId,
    MetricsOnly, NetworkClass, PresenceMinute, RollupInput, SCHEMA_VERSION, TRACTION_RULES_1,
    TelemetryTier, TractionEvent, WorkBucket, activity_rollups, canonical_json, rollup,
    work_buckets,
};

const MIN: u64 = 60_000;
/// A minute-aligned origin so second counts are exact.
const T0: u64 = 1_800_000_000_000 - 1_800_000_000_000 % (2 * MIN);

fn ev(n: u32, ts: u64, kind: EventType, session: &str, project: &str) -> TractionEvent {
    TractionEvent {
        schema_version: SCHEMA_VERSION,
        event_id: format!("ev-{n:06}"),
        timestamp_ms: ts,
        harness_id: Some(HarnessId::ClaudeCode),
        session_id: Some(session.to_owned()),
        parent_orchestrator_id: None,
        event_type: kind,
        end_reason: (kind == EventType::TurnEnd).then_some(EndReason::Completed),
        cwd: None,
        project_id: Some(project.to_owned()),
        model_id: None,
        network_class: NetworkClass::CloudApi,
        attribution_source: AttributionSource::HarnessHook,
        metrics_only: MetricsOnly::default(),
    }
}

fn pm(minute: u64, subslots: u8, tier: TelemetryTier) -> PresenceMinute {
    let full = tier == TelemetryTier::Full;
    PresenceMinute {
        minute_start_ms: minute,
        key_events: full.then_some(if subslots > 0 { 30 } else { 0 }),
        pointer_events: full.then_some(if subslots > 0 { 30 } else { 0 }),
        injected_events: full.then_some(0),
        active_subslots: Some(subslots),
        cadence_entropy_milli: full.then_some(2500),
        category_sec: Some(CategorySec {
            ide: 60,
            ..CategorySec::default()
        }),
        category_switches: Some(0),
        idle_sec: Some(if subslots == 0 { 60 } else { 0 }),
        telemetry_tier: tier,
    }
}

fn input<'a>(
    events: &'a [TractionEvent],
    presence: &'a [PresenceMinute],
    commits: &'a [CommitObservation],
    from: u64,
    to: u64,
) -> RollupInput<'a> {
    RollupInput {
        events,
        presence,
        commits,
        baseline: &[],
        from_ms: from,
        to_ms: to,
    }
}

fn buckets(
    events: &[TractionEvent],
    presence: &[PresenceMinute],
    commits: &[CommitObservation],
    from: u64,
    to: u64,
) -> Vec<WorkBucket> {
    work_buckets(
        &TRACTION_RULES_1,
        &input(events, presence, commits, from, to),
    )
}

fn sum<F: Fn(&WorkBucket) -> u32>(bs: &[WorkBucket], minute: u64, f: F) -> u32 {
    bs.iter()
        .filter(|b| b.minute_start_ms == minute)
        .map(f)
        .sum()
}

#[test]
fn gate_simulation_ten_idle_minutes_is_all_agent_and_mostly_unverified() {
    let events = [
        ev(1, T0, EventType::PromptSubmit, "s1", "p"),
        ev(2, T0 + 10 * MIN, EventType::TurnEnd, "s1", "p"),
    ];
    let presence: Vec<_> = (0..10)
        .map(|i| pm(T0 + i * MIN, 0, TelemetryTier::Reduced))
        .collect();
    let bs = buckets(&events, &presence, &[], T0, T0 + 10 * MIN);
    assert_eq!(bs.iter().map(|b| b.agent_compute_sec).sum::<u32>(), 600);
    assert_eq!(bs.iter().map(|b| b.human_active_sec).sum::<u32>(), 0);
    assert_eq!(bs.iter().map(|b| b.idle_autonomous_sec).sum::<u32>(), 510);
    assert!(bs.iter().all(|b| b.ruleset_id == "traction-rules/1"));
    assert!(bs.iter().all(|b| b.project_id == "p"));
}

#[test]
fn verified_presence_needs_three_real_subslots_and_a_work_category() {
    let mut other_only = pm(T0 + 2 * MIN, 5, TelemetryTier::Full);
    other_only.category_sec = Some(CategorySec {
        other: 60,
        ..CategorySec::default()
    });
    let presence = [
        pm(T0, 3, TelemetryTier::Full),
        pm(T0 + MIN, 2, TelemetryTier::Full),
        other_only,
    ];
    let events = [ev(1, T0 - MIN, EventType::ToolPost, "s1", "p")];
    let bs = buckets(&events, &presence, &[], T0, T0 + 3 * MIN);
    assert_eq!(sum(&bs, T0, |b| b.human_active_sec), 60);
    assert_eq!(sum(&bs, T0 + MIN, |b| b.human_active_sec), 0);
    assert_eq!(sum(&bs, T0 + 2 * MIN, |b| b.human_active_sec), 0);
}

#[test]
fn category_clause_is_skipped_on_idle_only_and_the_minute_is_tier_limited() {
    let mut idle_only = pm(T0, 4, TelemetryTier::IdleOnly);
    idle_only.category_sec = None;
    idle_only.category_switches = None;
    let events = [ev(1, T0 - MIN, EventType::ToolPost, "s1", "p")];
    let bs = buckets(&events, &[idle_only], &[], T0, T0 + MIN);
    assert_eq!(bs[0].human_active_sec, 60);
    assert!(bs[0].tier_limited);
    assert_eq!(bs[0].telemetry_tier, TelemetryTier::IdleOnly);
}

#[test]
fn injected_ratio_flags_the_minute_and_it_earns_nothing() {
    let mut m = pm(T0, 6, TelemetryTier::Full);
    m.injected_events = Some(60); // 60 of 120 events
    let events = [
        ev(1, T0 - MIN, EventType::ToolPost, "s1", "p"),
        ev(2, T0, EventType::PromptSubmit, "s1", "p"),
        ev(3, T0 + 5 * MIN, EventType::TurnEnd, "s1", "p"),
    ];
    let bs = buckets(&events, &[m], &[], T0, T0 + 5 * MIN);
    let first = bs.iter().find(|b| b.minute_start_ms == T0).unwrap();
    assert!(first.injected_hid_detected);
    assert_eq!(first.human_active_sec, 0);
    // Not an anchor either: all but the 90 s grace is unverified, and the
    // flagged minute itself gave no presence.
    assert_eq!(bs.iter().map(|b| b.idle_autonomous_sec).sum::<u32>(), 300);
}

#[test]
fn the_injected_ratio_is_injected_over_all_key_and_pointer_events() {
    // key and pointer counts include the injected events, so 20 of 40 is
    // 500 per mille: at the flag.
    let mut m = pm(T0, 6, TelemetryTier::Full);
    m.key_events = Some(20);
    m.pointer_events = Some(20);
    m.injected_events = Some(20);
    let bs = buckets(
        &[ev(1, T0, EventType::PromptSubmit, "s1", "p")],
        &[m.clone()],
        &[],
        T0,
        T0 + MIN,
    );
    assert!(bs[0].injected_hid_detected);
    // One fewer injected event is under the flag.
    m.injected_events = Some(19);
    let bs = buckets(
        &[ev(1, T0, EventType::PromptSubmit, "s1", "p")],
        &[m.clone()],
        &[],
        T0,
        T0 + MIN,
    );
    assert!(!bs[0].injected_hid_detected);
    let r = activity_rollups(&TRACTION_RULES_1, &[m], T0, T0 + 2 * MIN);
    assert_eq!(r[0].injected_ratio_milli, Some(475));
}

#[test]
fn robotic_cadence_flags_the_minute() {
    let mut m = pm(T0, 6, TelemetryTier::Full);
    m.cadence_entropy_milli = Some(100);
    let events = [
        ev(1, T0 - MIN, EventType::ToolPost, "s1", "p"),
        ev(2, T0 - 1000, EventType::PromptSubmit, "s1", "p"),
        ev(3, T0 + 9 * MIN, EventType::TurnEnd, "s1", "p"),
    ];
    let presence: Vec<_> = std::iter::once(m)
        .chain((1..9).map(|i| pm(T0 + i * MIN, 0, TelemetryTier::Full)))
        .collect();
    let bs = buckets(&events, &presence, &[], T0, T0 + 9 * MIN);
    let first = bs.iter().find(|b| b.minute_start_ms == T0).unwrap();
    assert!(first.injected_hid_detected);
    assert_eq!(first.human_active_sec, 0);
    assert!(
        bs.iter().map(|b| b.idle_autonomous_sec).sum::<u32>() > 400,
        "robotic minutes route agent time to unverified"
    );
}

#[test]
fn a_human_minute_keeps_following_agent_time_verified_for_ninety_seconds() {
    // Verified minute T0, agent runs T0 .. T0 + 4 min.
    let events = [
        ev(1, T0, EventType::PromptSubmit, "s1", "p"),
        ev(2, T0 + 4 * MIN, EventType::TurnEnd, "s1", "p"),
    ];
    let presence = [pm(T0, 6, TelemetryTier::Full)];
    let bs = buckets(&events, &presence, &[], T0, T0 + 4 * MIN);
    // Present through T0 + 60 s, plus 90 s: seconds 150..240 are unverified.
    assert_eq!(bs.iter().map(|b| b.agent_compute_sec).sum::<u32>(), 240);
    assert_eq!(bs.iter().map(|b| b.idle_autonomous_sec).sum::<u32>(), 90);
}

#[test]
fn agent_time_is_the_union_of_parallel_sessions_in_a_project() {
    let events = [
        ev(1, T0, EventType::PromptSubmit, "s1", "p"),
        ev(2, T0 + MIN, EventType::PromptSubmit, "s2", "p"),
        ev(3, T0 + 2 * MIN, EventType::TurnEnd, "s1", "p"),
        ev(4, T0 + 3 * MIN, EventType::TurnEnd, "s2", "p"),
        ev(5, T0 + 90_000, EventType::SubagentStart, "s1", "p"),
    ];
    let bs = buckets(&events, &[], &[], T0, T0 + 4 * MIN);
    assert_eq!(bs.iter().map(|b| b.agent_compute_sec).sum::<u32>(), 180);
    assert!(bs.iter().all(|b| b.agent_compute_sec <= 60));
}

#[test]
fn stop_failure_and_session_end_close_the_session_at_their_timestamp() {
    for closer in [EventType::StopFailure, EventType::SessionEnd] {
        let events = [
            ev(1, T0, EventType::PromptSubmit, "s1", "p"),
            ev(2, T0 + 30_000, closer, "s1", "p"),
        ];
        let bs = buckets(&events, &[], &[], T0, T0 + 5 * MIN);
        assert_eq!(
            bs.iter().map(|b| b.agent_compute_sec).sum::<u32>(),
            30,
            "{closer:?}"
        );
    }
}

#[test]
fn a_turn_left_open_by_a_crash_closes_at_its_last_event() {
    let events = [
        ev(1, T0, EventType::PromptSubmit, "s1", "p"),
        ev(2, T0 + 2 * MIN, EventType::ToolPost, "s1", "p"),
    ];
    let stale_end = T0 + 2 * MIN + TRACTION_RULES_1.stale_turn_timeout_ms + MIN;
    let bs = buckets(&events, &[], &[], T0, stale_end + MIN);
    assert_eq!(bs.iter().map(|b| b.agent_compute_sec).sum::<u32>(), 120);
}

#[test]
fn two_projects_never_exceed_wall_clock_in_a_minute() {
    let events = [
        ev(1, T0 - MIN, EventType::ToolPost, "s1", "a"),
        ev(2, T0 - 30_000, EventType::ToolPost, "s2", "b"),
        ev(3, T0 - 5_000, EventType::PromptSubmit, "s1", "a"),
        ev(4, T0, EventType::PromptSubmit, "s2", "b"),
        ev(5, T0 + 2 * MIN, EventType::TurnEnd, "s1", "a"),
        ev(6, T0 + 2 * MIN, EventType::TurnEnd, "s2", "b"),
    ];
    let presence: Vec<_> = (0..2)
        .map(|i| pm(T0 + i * MIN, 6, TelemetryTier::Full))
        .collect();
    let bs = buckets(&events, &presence, &[], T0, T0 + 2 * MIN);
    for m in [T0, T0 + MIN] {
        assert!(sum(&bs, m, |b| b.human_active_sec) <= 60, "human {m}");
        assert!(sum(&bs, m, |b| b.agent_compute_sec) <= 60, "agent {m}");
        assert_eq!(sum(&bs, m, |b| b.agent_compute_sec), 60);
    }
    // Presence goes to the project with the most recent event ("b").
    let human_b: u32 = bs
        .iter()
        .filter(|b| b.project_id == "b")
        .map(|b| b.human_active_sec)
        .sum();
    assert_eq!(human_b, 120);
    assert_eq!(
        bs.iter()
            .filter(|b| b.project_id == "a")
            .map(|b| b.human_active_sec)
            .sum::<u32>(),
        0
    );
}

#[test]
fn presence_with_no_recent_project_event_goes_to_no_project() {
    let events = [ev(1, T0 - 6 * MIN, EventType::ToolPost, "s1", "p")];
    let bs = buckets(
        &events,
        &[pm(T0, 6, TelemetryTier::Full)],
        &[],
        T0,
        T0 + MIN,
    );
    assert!(bs.iter().all(|b| b.human_active_sec == 0));
}

fn permission_events(request_at: u64, reply_at: u64) -> Vec<TractionEvent> {
    vec![
        ev(1, T0 - MIN, EventType::ToolPost, "s1", "p"),
        ev(2, request_at, EventType::PermissionRequest, "s1", "p"),
        ev(3, reply_at, EventType::ToolPost, "s1", "p"),
    ]
}

#[test]
fn a_permission_wait_with_real_input_earns_human_time_even_below_verified_presence() {
    let events = permission_events(T0 + 10_000, T0 + 25_000);
    let presence = [pm(T0, 1, TelemetryTier::Full)];
    let bs = buckets(&events, &presence, &[], T0, T0 + MIN);
    assert_eq!(bs[0].permission_wait_ms, 15_000);
    assert_eq!(bs[0].human_active_sec, 15);
    assert!(!bs[0].auto_approver_suspected);
}

#[test]
fn a_permission_wait_is_clamped_to_the_cap() {
    let events = permission_events(T0, T0 + 10 * MIN);
    let presence: Vec<_> = (0..10)
        .map(|i| pm(T0 + i * MIN, 1, TelemetryTier::Full))
        .collect();
    let bs = buckets(&events, &presence, &[], T0, T0 + MIN);
    assert_eq!(bs[0].permission_wait_ms, 120_000);
    assert!(bs[0].human_active_sec <= 60);
}

#[test]
fn an_approval_with_no_real_input_earns_nothing_and_is_suspected() {
    let events = permission_events(T0 + 10_000, T0 + 25_000);
    let presence = [pm(T0, 0, TelemetryTier::Full)];
    let bs = buckets(&events, &presence, &[], T0, T0 + MIN);
    assert_eq!(bs[0].human_active_sec, 0);
    assert!(bs[0].auto_approver_suspected);
}

#[test]
fn repeated_machine_speed_approvals_are_suspected_and_earn_nothing() {
    let mut events = vec![ev(1, T0 - MIN, EventType::ToolPost, "s1", "p")];
    for i in 0..3u32 {
        let t = T0 + 5_000 * u64::from(i);
        events.push(ev(10 + i * 2, t, EventType::PermissionRequest, "s1", "p"));
        events.push(ev(11 + i * 2, t + 100, EventType::ToolPost, "s1", "p"));
    }
    let presence = [pm(T0, 1, TelemetryTier::Full)];
    let bs = buckets(&events, &presence, &[], T0, T0 + MIN);
    assert!(bs[0].auto_approver_suspected);
    assert_eq!(bs[0].human_active_sec, 0);
}

fn commit_at(ts: u64) -> CommitObservation {
    CommitObservation {
        project_id: "p".to_owned(),
        commit_oid: "a".repeat(40),
        observed_ms: ts,
    }
}

#[test]
fn a_commit_30_s_after_a_tool_post_is_agent_attributed() {
    let events = [ev(1, T0 + 10_000, EventType::ToolPost, "s1", "p")];
    let bs = buckets(&events, &[], &[commit_at(T0 + 40_000)], T0, T0 + MIN);
    assert_eq!((bs[0].commits_human, bs[0].commits_agent), (0, 1));
}

#[test]
fn a_commit_90_s_after_a_turn_end_is_human() {
    let events = [
        ev(1, T0, EventType::PromptSubmit, "s1", "p"),
        ev(2, T0 + 10_000, EventType::TurnEnd, "s1", "p"),
    ];
    let bs = buckets(&events, &[], &[commit_at(T0 + 100_000)], T0, T0 + 2 * MIN);
    let b = bs.iter().find(|b| b.minute_start_ms == T0 + MIN).unwrap();
    assert_eq!((b.commits_human, b.commits_agent), (1, 0));
}

#[test]
fn a_commit_during_an_open_turn_is_agent_even_with_no_recent_event() {
    let events = [
        ev(1, T0, EventType::PromptSubmit, "s1", "p"),
        ev(2, T0 + 5 * MIN, EventType::TurnEnd, "s1", "p"),
    ];
    let bs = buckets(&events, &[], &[commit_at(T0 + 3 * MIN)], T0, T0 + 6 * MIN);
    let b = bs
        .iter()
        .find(|b| b.minute_start_ms == T0 + 3 * MIN)
        .unwrap();
    assert_eq!((b.commits_human, b.commits_agent), (0, 1));
}

#[test]
fn a_minute_with_no_presence_row_is_tier_none_and_earns_no_human_time() {
    let events = [
        ev(1, T0, EventType::PromptSubmit, "s1", "p"),
        ev(2, T0 + MIN, EventType::TurnEnd, "s1", "p"),
    ];
    let bs = buckets(&events, &[], &[], T0, T0 + MIN);
    assert_eq!(bs[0].telemetry_tier, TelemetryTier::None);
    assert_eq!(bs[0].human_active_sec, 0);
    assert!(!bs[0].injected_hid_detected);
}

#[test]
fn shuffled_inputs_give_byte_identical_canonical_json() {
    let events = vec![
        ev(1, T0 - MIN, EventType::ToolPost, "s1", "a"),
        ev(2, T0, EventType::PromptSubmit, "s1", "a"),
        ev(3, T0 + 30_000, EventType::PermissionRequest, "s1", "a"),
        ev(4, T0 + 40_000, EventType::ToolPost, "s1", "a"),
        ev(5, T0, EventType::PromptSubmit, "s2", "b"),
        ev(6, T0 + 3 * MIN, EventType::TurnEnd, "s2", "b"),
        ev(7, T0 + 4 * MIN, EventType::TurnEnd, "s1", "a"),
    ];
    let presence: Vec<_> = (0..4)
        .map(|i| pm(T0 + i * MIN, (i * 2) as u8, TelemetryTier::Full))
        .collect();
    let commits = vec![commit_at(T0 + 3 * MIN + 5_000)];
    let render = |e: &[TractionEvent], p: &[PresenceMinute]| {
        let out = rollup(&TRACTION_RULES_1, &input(e, p, &commits, T0, T0 + 4 * MIN));
        canonical_json(&serde_json::to_value(&out).unwrap()).unwrap()
    };
    let baseline = render(&events, &presence);
    let mut e2 = events.clone();
    e2.reverse();
    e2.swap(1, 4);
    let mut p2 = presence.clone();
    p2.reverse();
    assert_eq!(render(&e2, &p2), baseline);
    assert!(!baseline.is_empty());
}

#[test]
fn activity_rollup_summarizes_a_two_minute_window() {
    let mut a = pm(T0, 6, TelemetryTier::Full);
    a.category_sec = Some(CategorySec {
        ide: 30,
        terminal: 30,
        ..CategorySec::default()
    });
    a.category_switches = Some(2);
    a.cadence_entropy_milli = Some(2000);
    a.injected_events = Some(6); // 6 of 60
    let mut b = pm(T0 + MIN, 4, TelemetryTier::Reduced);
    b.category_switches = Some(4);
    b.cadence_entropy_milli = None;
    b.key_events = None;
    b.pointer_events = None;
    b.injected_events = None;
    let out = activity_rollups(&TRACTION_RULES_1, &[b.clone(), a.clone()], T0, T0 + 2 * MIN);
    assert_eq!(out.len(), 1);
    let r = &out[0];
    assert_eq!(r.window_start_ms, T0);
    assert_eq!(r.minutes_observed, 2);
    assert_eq!(r.ruleset_id, "traction-rules/1");
    assert_eq!(r.telemetry_tier, TelemetryTier::Reduced);
    assert_eq!(r.switch_rate_milli, 3000); // 6 switches over 2 minutes
    assert_eq!(r.mean_cadence_entropy_milli, Some(2000));
    assert_eq!(r.injected_ratio_milli, Some(100)); // 6 of 60
    // ide 30 + 60, terminal 30 of 120 category seconds.
    assert_eq!(r.category_share_milli.ide, 750);
    assert_eq!(r.category_share_milli.terminal, 250);
}

#[test]
fn activity_rollups_only_cover_whole_aligned_windows() {
    let presence: Vec<_> = (0..5)
        .map(|i| pm(T0 + i * MIN, 1, TelemetryTier::Full))
        .collect();
    let out = activity_rollups(&TRACTION_RULES_1, &presence, T0, T0 + 5 * MIN);
    assert_eq!(
        out.iter().map(|r| r.window_start_ms).collect::<Vec<_>>(),
        [T0, T0 + 2 * MIN]
    );
}
