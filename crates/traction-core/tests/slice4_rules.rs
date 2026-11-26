//! Seam: the two Slice 4 rules the persona red-team forced, through
//! `work_buckets`: a Harness edit anchors credit only as strongly as the
//! prompt behind it, and a tier with no input counts proves presence from
//! the OS idle timer at a reduced credit.

mod support;

use support::*;
use traction_core::{
    AppCategory, PresenceMinute, RollupInput, TRACTION_RULES_1 as RULES, TelemetryTier,
    work_buckets,
};

fn run(events: &[traction_core::TractionEvent], presence: &[PresenceMinute]) -> (u32, u32) {
    let b = work_buckets(
        &RULES,
        &RollupInput {
            events,
            presence,
            commits: &[],
            baseline: &[],
            from_ms: T0,
            to_ms: T0 + 4 * MIN,
        },
    );
    (
        b.iter().map(|b| b.human_active_sec).sum(),
        b.iter().map(|b| b.struggle_credit_sec).sum(),
    )
}

fn minutes(tier: TelemetryTier, idle: u32) -> Vec<PresenceMinute> {
    let mut rng = Rng::new(1);
    (0..4)
        .map(|m| {
            let full = presence_minute(
                &mut rng,
                T0 + m * MIN,
                Input::Human { median_ms: 900.0 },
                [AppCategory::Ide; 6],
            );
            let mut d = degrade(&full, tier);
            if tier != TelemetryTier::Full {
                d.idle_sec = Some(idle);
            }
            d
        })
        .collect()
}

fn agent_edit(chars: u64, hits: u64) -> Vec<traction_core::TractionEvent> {
    let mut st = Stream::default();
    st.prompt(T0 + 1_000, chars, hits, false);
    st.mutation(T0 + 30_000, false, 20);
    st.turn_end(T0 + 60_000);
    st.events
}

#[test]
fn an_agent_edit_after_a_specific_prompt_anchors_credit() {
    let (human, credit) = run(&agent_edit(120, 4), &minutes(TelemetryTier::Full, 0));
    assert_eq!(human, 240);
    assert_eq!(credit, 240);
}

#[test]
fn an_agent_edit_after_a_terse_prompt_anchors_nothing() {
    let (human, credit) = run(&agent_edit(8, 0), &minutes(TelemetryTier::Full, 0));
    assert_eq!(human, 240);
    assert_eq!(credit, 0);
}

#[test]
fn a_hand_edit_anchors_in_full_regardless_of_prompts() {
    let mut st = Stream::default();
    st.prompt(T0 + 1_000, 8, 0, false);
    st.mutation(T0 + 1_000, true, 5);
    let (_, credit) = run(&st.events, &minutes(TelemetryTier::Full, 0));
    assert_eq!(credit, 240);
}

#[test]
fn a_reduced_tier_minute_with_a_busy_idle_timer_is_present_at_reduced_credit() {
    let mut st = Stream::default();
    st.mutation(T0 + 1_000, true, 5);
    let (human, credit) = run(&st.events, &minutes(TelemetryTier::Reduced, 0));
    assert_eq!(human, 240);
    assert_eq!(credit, 240 * RULES.degraded_tier_credit_milli / 1000);
}

#[test]
fn a_reduced_tier_minute_past_the_idle_limit_is_not_present() {
    let mut st = Stream::default();
    st.mutation(T0 + 1_000, true, 5);
    let busy = RULES.degraded_idle_max_sec;
    let (human, _) = run(&st.events, &minutes(TelemetryTier::Reduced, busy + 1));
    assert_eq!(human, 0);
}
