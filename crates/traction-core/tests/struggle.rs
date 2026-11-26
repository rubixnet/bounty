//! Seam: struggle axes, Outcome Anchors, credit and the comfort baseline
//! through the `traction-core` API (`work_buckets`, `fold_baseline`). No
//! I/O, no clock.

use traction_core::{
    AttributionSource, BASELINE_BINS, BaselineDay, CategorySec, EndReason, EventType, HarnessId,
    MetricsOnly, MutationKind, NetworkClass, PresenceMinute, RollupInput, SCHEMA_VERSION,
    TRACTION_RULES_1, TelemetryTier, TractionEvent, WorkBucket, fold_baseline, work_buckets,
};

const MIN: u64 = 60_000;
const DAY: u64 = 86_400_000;
const T0: u64 = 1_800_000_000_000 - 1_800_000_000_000 % (2 * MIN);
const P: &str = "git:proj";

struct Ev {
    n: u32,
    ts: u64,
    kind: EventType,
    session: Option<&'static str>,
    metrics: MetricsOnly,
}

fn ev(n: u32, ts: u64, kind: EventType) -> Ev {
    Ev {
        n,
        ts,
        kind,
        session: Some("s1"),
        metrics: MetricsOnly::default(),
    }
}

impl Ev {
    fn build(self) -> TractionEvent {
        let manual = self.session.is_none();
        TractionEvent {
            schema_version: SCHEMA_VERSION,
            event_id: format!("ev-{:06}", self.n),
            timestamp_ms: self.ts,
            harness_id: (!manual).then_some(HarnessId::ClaudeCode),
            session_id: self.session.map(str::to_owned),
            parent_orchestrator_id: None,
            event_type: self.kind,
            end_reason: (self.kind == EventType::TurnEnd).then_some(EndReason::Completed),
            cwd: None,
            project_id: Some(P.to_owned()),
            model_id: None,
            network_class: if manual {
                NetworkClass::Manual
            } else {
                NetworkClass::CloudApi
            },
            attribution_source: if manual {
                AttributionSource::FileWatcher
            } else {
                AttributionSource::HarnessHook
            },
            metrics_only: self.metrics,
        }
    }
}

fn turn_end(n: u32, ts: u64) -> TractionEvent {
    ev(n, ts, EventType::TurnEnd).build()
}

fn prompt(n: u32, ts: u64, chars: u64, hits: u64, trace: bool) -> TractionEvent {
    let mut e = ev(n, ts, EventType::PromptSubmit);
    e.metrics = MetricsOnly {
        prompt_char_len: Some(chars),
        repo_symbol_hits: Some(hits),
        has_stacktrace: Some(trace),
        ..MetricsOnly::default()
    };
    e.build()
}

fn tool(n: u32, ts: u64, kind: EventType, name: &str) -> TractionEvent {
    let mut e = ev(n, ts, kind);
    e.metrics.tool_name = Some(name.to_owned());
    e.build()
}

fn mutation(n: u32, ts: u64, manual: bool, diff: i64) -> TractionEvent {
    let mut e = ev(n, ts, EventType::FileMutation);
    if manual {
        e.session = None;
    }
    e.metrics.mutation_kind = Some(MutationKind::Modified);
    e.metrics.diff_lines_delta = Some(diff);
    e.build()
}

/// A verified minute with the given foreground seconds; ide plus terminal
/// counts as two switches.
fn present(minute: u64, ide: u32, terminal: u32, browser: u32) -> PresenceMinute {
    PresenceMinute {
        minute_start_ms: minute,
        key_events: Some(30),
        pointer_events: Some(30),
        injected_events: Some(0),
        active_subslots: Some(4),
        cadence_entropy_milli: Some(2500),
        category_sec: Some(CategorySec {
            ide,
            terminal,
            browser,
            ..CategorySec::default()
        }),
        category_switches: Some(if ide > 0 && terminal > 0 { 2 } else { 0 }),
        idle_sec: Some(0),
        telemetry_tier: TelemetryTier::Full,
    }
}

fn run_at(
    from: u64,
    events: &[TractionEvent],
    presence: &[PresenceMinute],
    baseline: &[BaselineDay],
    minutes: u64,
) -> Vec<WorkBucket> {
    work_buckets(
        &TRACTION_RULES_1,
        &RollupInput {
            events,
            presence,
            commits: &[],
            baseline,
            from_ms: from,
            to_ms: from + minutes * MIN,
        },
    )
}

fn run(
    events: &[TractionEvent],
    presence: &[PresenceMinute],
    baseline: &[BaselineDay],
    minutes: u64,
) -> Vec<WorkBucket> {
    run_at(T0, events, presence, baseline, minutes)
}

fn at(buckets: &[WorkBucket], minute: u64) -> &WorkBucket {
    buckets
        .iter()
        .find(|b| b.minute_start_ms == T0 + minute * MIN)
        .unwrap_or_else(|| panic!("no bucket at minute {minute}"))
}

fn sum(buckets: &[WorkBucket], f: fn(&WorkBucket) -> u32) -> u32 {
    buckets.iter().map(f).sum()
}

/// error -> edit -> test-fail -> edit -> test-pass, `diff` lines per edit.
fn debugging_loop(diff: i64) -> (Vec<TractionEvent>, Vec<PresenceMinute>) {
    let events = vec![
        prompt(1, T0 + 5_000, 220, 3, true),
        tool(2, T0 + 20_000, EventType::ToolFail, "Bash"),
        mutation(3, T0 + 40_000, false, diff),
        tool(4, T0 + 90_000, EventType::ToolFail, "Bash"),
        mutation(5, T0 + 110_000, false, diff),
        tool(6, T0 + 150_000, EventType::ToolPost, "Bash"),
        turn_end(7, T0 + 170_000),
    ];
    let presence = (0..4).map(|i| present(T0 + i * MIN, 30, 30, 0)).collect();
    (events, presence)
}

#[test]
fn a_fail_edit_fail_edit_pass_loop_scores_high_and_size_is_ignored() {
    let (small, presence) = debugging_loop(1);
    let (large, _) = debugging_loop(300);
    let a = run(&small, &presence, &[], 4);
    let b = run(&large, &presence, &[], 4);
    let best = a.iter().map(|x| x.verified_struggle).max().unwrap();
    assert!(
        best >= TRACTION_RULES_1.verified_struggle_bar_milli,
        "{a:#?}"
    );
    // The same loop scores the same whether the edits are 1 or 300 lines.
    assert_eq!(a, b);
    // The loop's minutes carry full friction.
    assert_eq!(at(&a, 1).struggle_axes.friction, 1000);
    // A beginner (no baseline) closing a short loop keeps every second.
    assert!(at(&a, 2).human_active_sec > 0);
    assert_eq!(at(&a, 2).struggle_credit_sec, at(&a, 2).human_active_sec);
}

#[test]
fn credit_never_exceeds_human_active_time() {
    let (events, presence) = debugging_loop(5);
    for b in run(&events, &presence, &[], 4) {
        assert!(b.struggle_credit_sec <= b.human_active_sec, "{b:?}");
    }
}

#[test]
fn a_fix_that_closes_a_different_tool_is_not_a_friction_loop() {
    let (mut events, presence) = debugging_loop(1);
    events[5] = tool(6, T0 + 150_000, EventType::ToolPost, "Read");
    let out = run(&events, &presence, &[], 4);
    assert!(
        out.iter().all(|b| b.struggle_axes.friction == 0),
        "{out:#?}"
    );
}

#[test]
fn present_minutes_with_no_anchor_keep_human_time_but_earn_no_credit() {
    // A prompt and an open turn: no edit, no commit, no loop, no approval.
    let events = vec![
        prompt(1, T0 + 5_000, 12, 0, false),
        turn_end(2, T0 + 8 * MIN),
    ];
    let presence: Vec<_> = (0..5).map(|i| present(T0 + i * MIN, 40, 20, 0)).collect();
    let out = run(&events, &presence, &[], 5);
    assert!(sum(&out, |b| b.human_active_sec) >= 4 * 60, "{out:#?}");
    assert_eq!(sum(&out, |b| b.struggle_credit_sec), 0, "{out:#?}");
}

#[test]
fn a_harness_edit_inside_a_human_prompted_turn_is_an_anchor_but_one_outside_is_not() {
    let inside = vec![
        prompt(1, T0 + 5_000, 90, 1, false),
        mutation(2, T0 + 20_000, false, 4),
    ];
    let presence: Vec<_> = (0..2).map(|i| present(T0 + i * MIN, 40, 20, 0)).collect();
    let out = run(&inside, &presence, &[], 2);
    assert!(sum(&out, |b| b.struggle_credit_sec) > 0, "{out:#?}");
    // Same edit after the turn closed: no anchor.
    let outside = vec![
        prompt(1, T0 + 5_000, 90, 1, false),
        turn_end(2, T0 + 10_000),
        mutation(3, T0 + 20_000, false, 4),
    ];
    let out = run(&outside, &presence, &[], 2);
    assert_eq!(sum(&out, |b| b.struggle_credit_sec), 0, "{out:#?}");
}

#[test]
fn unanchored_browser_minutes_decay_linearly_while_other_categories_stop_at_the_window() {
    // One manual edit; then minutes of pure browsing.
    // A later prompt keeps the minutes assigned to the Project; it is no
    // anchor.
    let events = vec![
        mutation(1, T0 + 10_000, true, 1),
        prompt(2, T0 + 4 * MIN + 10_000, 20, 0, false),
        turn_end(3, T0 + 4 * MIN + 20_000),
    ];
    let browsing: Vec<_> = (0..8).map(|i| present(T0 + i * MIN, 0, 0, 60)).collect();
    let out = run(&events, &browsing, &[], 8);
    let credit = |m| at(&out, m).struggle_credit_sec;
    assert_eq!(credit(0), 60, "the anchor's own minute");
    // 50 s from the anchor, then 110 s: weight is 1 - d / 600 s.
    assert_eq!(credit(1), 54);
    assert_eq!(credit(2), 48);
    assert!(credit(3) > credit(4) && credit(4) > 0);
    assert!(credit(5) > 0, "290 s is inside the 5 minute window");
    assert!(
        credit(6) > 0 && credit(6) < credit(5),
        "drift keeps decaying past the 5 minute window: {out:#?}"
    );
    // The same minutes in the IDE are not discounted inside the window.
    let ide: Vec<_> = (0..8).map(|i| present(T0 + i * MIN, 60, 0, 0)).collect();
    let out = run(&events, &ide, &[], 8);
    assert_eq!(at(&out, 3).struggle_credit_sec, 60);
    assert_eq!(at(&out, 5).struggle_credit_sec, 60);
    assert_eq!(at(&out, 6).struggle_credit_sec, 0);
    // human_active_sec is never reduced.
    assert_eq!(at(&out, 6).human_active_sec, 60);
}

#[test]
fn browser_drift_decays_linearly_over_the_whole_decay_span() {
    // Terse prompts (no anchor) keep every minute assigned to the Project.
    let mut events = vec![mutation(1, T0, true, 1)];
    for (i, m) in [2u64, 5, 8].into_iter().enumerate() {
        let n = 10 + 2 * i as u32;
        events.push(prompt(n, T0 + m * MIN, 20, 0, false));
        events.push(turn_end(n + 1, T0 + m * MIN + 10_000));
    }
    let browsing: Vec<_> = (0..11).map(|i| present(T0 + i * MIN, 0, 0, 60)).collect();
    let out = run(&events, &browsing, &[], 11);
    let credit = |m| at(&out, m).struggle_credit_sec;
    // Weight is 1 - d / 10 min, d = whole minutes from the anchor.
    assert_eq!(credit(3), 42, "3 min: 0.7");
    assert_eq!(credit(5), 30, "5 min: 0.5, no cliff at the anchor window");
    assert_eq!(credit(7), 18, "7 min: 0.3");
    assert_eq!(credit(10), 0, "10 min: 0");
}

#[test]
fn a_terse_prompt_that_launches_a_big_turn_scores_near_zero_specificity() {
    let terse = vec![
        prompt(1, T0 + 5_000, 8, 0, false),
        mutation(2, T0 + 20_000, false, 400),
        turn_end(3, T0 + 40_000),
    ];
    let named = vec![
        prompt(1, T0 + 5_000, 240, 3, true),
        mutation(2, T0 + 20_000, false, 400),
        turn_end(3, T0 + 40_000),
    ];
    let presence = vec![present(T0, 40, 20, 0)];
    let terse = run(&terse, &presence, &[], 1);
    let named = run(&named, &presence, &[], 1);
    assert!(at(&terse, 0).struggle_axes.specificity <= 50, "{terse:#?}");
    // Naming files and pasting a trace stays high even for a big change.
    assert!(at(&named, 0).struggle_axes.specificity >= 800, "{named:#?}");
}

#[test]
fn manual_edits_right_after_agent_churn_count_as_reverting_it() {
    let events = vec![
        prompt(1, T0 + 5_000, 60, 1, false),
        mutation(2, T0 + 20_000, false, 30),
        turn_end(3, T0 + 30_000),
        mutation(4, T0 + 90_000, true, 20),
    ];
    let presence: Vec<_> = (0..2).map(|i| present(T0 + i * MIN, 40, 20, 0)).collect();
    let out = run(&events, &presence, &[], 2);
    assert_eq!(
        at(&out, 1).struggle_axes.friction,
        TRACTION_RULES_1.friction_revert_milli
    );
    assert_eq!(at(&out, 0).struggle_axes.friction, 0);
}

fn calm_days(days: u64, minutes_per_day: u32) -> Vec<BaselineDay> {
    (0..days)
        .map(|d| {
            let mut day = BaselineDay::new(d);
            for _ in 0..minutes_per_day {
                day.record(0, 0);
            }
            day
        })
        .collect()
}

fn one_calm_minute(baseline: &[BaselineDay]) -> WorkBucket {
    let events = vec![mutation(1, T0 + 10_000, true, 1)];
    let out = run(&events, &[present(T0, 60, 0, 0)], baseline, 1);
    at(&out, 0).clone()
}

#[test]
fn boilerplate_decays_toward_the_floor_but_never_to_zero_and_beginners_keep_full_weight() {
    // No history, or fewer than the minimum days: never reduced.
    assert_eq!(one_calm_minute(&[]).struggle_credit_sec, 60);
    assert_eq!(one_calm_minute(&calm_days(2, 100)).struggle_credit_sec, 60);
    // Thirty identical calm days: the weight is exactly the floor.
    let boring = one_calm_minute(&calm_days(30, 100));
    assert_eq!(boring.struggle_credit_sec, 30);
    assert_eq!(boring.struggle_axes.comfort_delta, 0);
    // Half the history was hard: partly familiar, so in between.
    let mixed: Vec<_> = calm_days(30, 50)
        .into_iter()
        .map(|mut d| {
            for _ in 0..50 {
                d.record(1000, 900);
            }
            d
        })
        .collect();
    let partly = one_calm_minute(&mixed);
    assert!(
        partly.struggle_credit_sec > boring.struggle_credit_sec && partly.struggle_credit_sec < 60,
        "{partly:?}"
    );
}

#[test]
fn a_hard_minute_is_never_discounted_whatever_the_baseline() {
    let (events, presence) = debugging_loop(3);
    let out = run(&events, &presence, &calm_days(30, 100), 4);
    assert_eq!(
        at(&out, 2).struggle_credit_sec,
        at(&out, 2).human_active_sec
    );
}

#[test]
fn the_baseline_is_a_compact_aggregate_pruned_to_thirty_days() {
    let mut days = Vec::new();
    for d in 0..40u64 {
        let minute = d * DAY + T0 % DAY - (T0 % MIN);
        let events = vec![mutation(1, minute + 10_000, true, 1)];
        let buckets = run_at(minute, &events, &[present(minute, 60, 0, 0)], &[], 1);
        fold_baseline(&TRACTION_RULES_1, &mut days, &buckets);
    }
    assert_eq!(days.len(), TRACTION_RULES_1.baseline_window_days as usize);
    assert_eq!(days.last().unwrap().day, 39);
    // Each day is just two small histograms.
    assert_eq!(days[0].friction_hist.iter().sum::<u32>(), 1);
    assert_eq!(days[0].friction_hist.len(), BASELINE_BINS);
}
