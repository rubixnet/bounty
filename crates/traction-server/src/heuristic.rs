//! `HeuristicEngine`: the deterministic, zero-network default
//! [`DecisionEngine`] (spec S3a, Q21 and Q32). It is a pure function of the
//! [`TriageInput`] and the `[heuristic]` thresholds; it opens no socket and
//! reads no clock.
//!
//! Rules (colour is the worst one that fires):
//! - **Red**: `injected_hid_detected` Human Active Time at or above
//!   `red_injected_share` of the submitted Human Active Time; `late_anchor`
//!   windows at or above `red_late_share` of the submitted realtime windows.
//!   (The deterministic failures are Red before any engine runs; the engine
//!   also returns Red for one, defensively.) **The injection rule can only
//!   trigger today on Linux X11**, the one presence backend with an
//!   injection signal (`docs/protocol.md` 2b).
//! - **Yellow**: any `offline_buffered` window; `clock_skew`; a key
//!   discontinuity; `self_built`; Project minutes shared with another device
//!   at or above `multi_device_overlap_share`; a run of at least
//!   `long_run_minutes` of busy 15-minute slots (at least 5 minutes of Human
//!   Active Time each) whose Timeline Marks and commits show no mutation, no
//!   commit and no turn; Credited Time at or below `yellow_credit_ratio` of
//!   Human Active Time; and, so that a small amount of a Red signal is still
//!   seen by a human, any `late_anchor` window or injected minute below the
//!   Red share, and an unanchored chain tail.
//! - **Green**: none of the above. Green is a fast-track human confirmation,
//!   never an automatic approval.
//! - **Telemetry Tier never changes the colour.** `tier_limited_share` is an
//!   informational reason code only (macOS and Windows ship `reduced`,
//!   Wayland ships `none`; a tier rule would keep those users out of Green).

use crate::config::Heuristic;
use crate::decision::{
    BoxFuture, DecisionEngine, EngineError, SLOT_MS, Triage, TriageInput, Verdict, reasons,
};

/// Version of the rule set; bump when a rule changes.
pub const RUBRIC_VERSION: &str = "heuristic/1";
pub const ENGINE_ID: &str = "heuristic";

/// A slot counts toward a "long run" when it holds at least this much Human
/// Active Time (a third of the slot).
pub const BUSY_SLOT_MIN_HUMAN_SEC: u32 = 300;

#[derive(Debug, Clone)]
pub struct HeuristicEngine {
    config: Heuristic,
}

impl HeuristicEngine {
    pub fn new(config: Heuristic) -> Self {
        Self { config }
    }

    /// The synchronous core of [`DecisionEngine::triage`].
    pub fn evaluate(&self, input: &TriageInput) -> Triage {
        let c = &self.config;
        let flags = &input.flags;
        let mut red = Vec::new();
        let mut yellow = Vec::new();
        let mut info = Vec::new();

        if let Some(failure) = &input.verification.failure {
            red.push(reasons::DETERMINISTIC_FAILURE.to_owned());
            red.push(failure.clone());
        }

        let human: u64 = input.human_active_sec();
        let injected: u64 = input
            .slots
            .iter()
            .map(|s| u64::from(s.injected_human_active_sec))
            .sum();
        if injected > 0 {
            if at_least(injected, human, c.red_injected_share) {
                red.push(reasons::INJECTED_HID_DETECTED.to_owned());
            } else {
                yellow.push(reasons::INJECTED_HID_DETECTED.to_owned());
            }
        }

        if flags.late_anchor_windows > 0 {
            if at_least(
                u64::from(flags.late_anchor_windows),
                u64::from(flags.realtime_windows),
                c.red_late_share,
            ) {
                red.push(reasons::LATE_ANCHOR.to_owned());
            } else {
                yellow.push(reasons::LATE_ANCHOR.to_owned());
            }
        }

        if flags.offline_buffered_windows > 0 {
            yellow.push(reasons::OFFLINE_BUFFERED.to_owned());
        }
        if flags.clock_skew {
            yellow.push(reasons::CLOCK_SKEW.to_owned());
        }
        if input.verification.key_discontinuities > 0 {
            yellow.push(reasons::KEY_DISCONTINUITY.to_owned());
        }
        if flags.self_built {
            yellow.push(reasons::SELF_BUILT.to_owned());
        }
        if flags.multi_device_overlap_minutes > 0
            && at_least(
                u64::from(flags.multi_device_overlap_minutes),
                u64::from(flags.active_minutes),
                c.multi_device_overlap_share,
            )
        {
            yellow.push(reasons::MULTI_DEVICE_OVERLAP.to_owned());
        }
        if has_long_run(input, c.long_run_minutes) {
            yellow.push(reasons::LONG_RUN_WITHOUT_MARKS.to_owned());
        }
        let credit: u64 = input
            .slots
            .iter()
            .map(|s| u64::from(s.struggle_credit_sec))
            .sum();
        if human > 0 && at_most(credit, human, c.yellow_credit_ratio) {
            yellow.push(reasons::LOW_CREDIT_RATIO.to_owned());
        }
        if flags.unanchored_tail_links > 0 {
            yellow.push(reasons::UNANCHORED_TAIL.to_owned());
        }

        if input.slots.iter().any(|s| s.tier_limited_minutes > 0) {
            info.push(reasons::TIER_LIMITED_SHARE.to_owned());
        }

        let verdict = if !red.is_empty() {
            Verdict::Red
        } else if !yellow.is_empty() {
            Verdict::Yellow
        } else {
            Verdict::Green
        };
        let mut all = red;
        all.extend(yellow);
        all.extend(info);
        Triage {
            verdict,
            reasons: all,
            engine_id: ENGINE_ID.into(),
            rubric_version: RUBRIC_VERSION.into(),
        }
    }
}

/// `part / whole >= share`, in integers where possible (`whole` of 0 never
/// reaches a share).
fn at_least(part: u64, whole: u64, share: f64) -> bool {
    whole > 0 && (part as f64) >= (whole as f64) * share
}

fn at_most(part: u64, whole: u64, share: f64) -> bool {
    whole > 0 && (part as f64) <= (whole as f64) * share
}

/// Whether `long_run_minutes` or more consecutive 15-minute slots are busy
/// yet show no marks. Missing slots break a run.
fn has_long_run(input: &TriageInput, long_run_minutes: u32) -> bool {
    let needed = u64::from(long_run_minutes).div_ceil(15).max(1);
    let mut run = 0u64;
    let mut previous: Option<u64> = None;
    let mut slots: Vec<_> = input.slots.iter().collect();
    slots.sort_by_key(|s| s.slot_start_ms);
    for slot in slots {
        let adjacent = previous.is_some_and(|p| p + SLOT_MS == slot.slot_start_ms);
        let busy = slot.human_active_sec >= BUSY_SLOT_MIN_HUMAN_SEC && !slot.has_marks();
        run = match (busy, adjacent) {
            (true, true) => run + 1,
            (true, false) => 1,
            (false, _) => 0,
        };
        previous = Some(slot.slot_start_ms);
        if run >= needed {
            return true;
        }
    }
    false
}

impl DecisionEngine for HeuristicEngine {
    fn engine_id(&self) -> &'static str {
        ENGINE_ID
    }

    fn triage<'a>(&'a self, input: &'a TriageInput) -> BoxFuture<'a, Result<Triage, EngineError>> {
        Box::pin(async move { Ok(self.evaluate(input)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::{AnchorFlags, Slot, VerificationSummary};

    fn engine() -> HeuristicEngine {
        HeuristicEngine::new(Heuristic::default())
    }

    /// A busy slot with marks and a healthy credit: ordinary honest work.
    fn honest_slot(index: u64) -> Slot {
        Slot {
            slot_start_ms: index * SLOT_MS,
            minutes: 15,
            human_active_sec: 600,
            agent_compute_sec: 300,
            struggle_credit_sec: 400,
            mutations_manual: 4,
            turns_started: 2,
            commits_human: 1,
            ..Slot::default()
        }
    }

    fn green_input() -> TriageInput {
        TriageInput {
            submission_ref: "r".into(),
            verification: VerificationSummary {
                chains: 1,
                links: 9,
                key_pinned: true,
                ..VerificationSummary::default()
            },
            flags: AnchorFlags {
                realtime_windows: 20,
                active_minutes: 45,
                ..AnchorFlags::default()
            },
            slots: (0..3).map(honest_slot).collect(),
            review: crate::decision::ReviewContext::Submitted,
            frames: Vec::new(),
        }
    }

    fn verdict_of(input: &TriageInput) -> (Verdict, Vec<String>) {
        let triage = engine().evaluate(input);
        assert_eq!(triage.engine_id, "heuristic");
        assert_eq!(triage.rubric_version, RUBRIC_VERSION);
        (triage.verdict, triage.reasons)
    }

    #[test]
    fn honest_work_is_green_with_no_reasons() {
        assert_eq!(verdict_of(&green_input()), (Verdict::Green, vec![]));
    }

    #[test]
    fn a_deterministic_failure_is_red() {
        let mut input = green_input();
        input.verification.failure = Some("tip_mismatch".into());
        let (verdict, reasons) = verdict_of(&input);
        assert_eq!(verdict, Verdict::Red);
        assert!(reasons.contains(&"tip_mismatch".to_owned()));
    }

    #[test]
    fn injected_input_is_red_at_the_share_and_yellow_below_it() {
        let mut input = green_input();
        // 1800 s of Human Active Time in total; 10% is 180 s.
        input.slots[0].injected_human_active_sec = 180;
        assert_eq!(verdict_of(&input).0, Verdict::Red);
        input.slots[0].injected_human_active_sec = 179;
        let (verdict, reasons) = verdict_of(&input);
        assert_eq!(verdict, Verdict::Yellow);
        assert_eq!(reasons, ["injected_hid_detected"]);
    }

    #[test]
    fn late_anchors_are_red_at_the_share_and_yellow_below_it() {
        let mut input = green_input();
        input.flags.late_anchor_windows = 1; // 1 of 20 realtime windows = 5%
        assert_eq!(
            verdict_of(&input),
            (Verdict::Red, vec!["late_anchor".into()])
        );
        input.flags.realtime_windows = 21;
        assert_eq!(
            verdict_of(&input),
            (Verdict::Yellow, vec!["late_anchor".into()])
        );
    }

    #[test]
    fn each_yellow_signal_alone_makes_yellow() {
        type Edit = fn(&mut TriageInput);
        let cases: [(&str, Edit); 7] = [
            ("offline_buffered", |i| i.flags.offline_buffered_windows = 3),
            ("clock_skew", |i| i.flags.clock_skew = true),
            ("key_discontinuity", |i| {
                i.verification.key_discontinuities = 1
            }),
            ("self_built", |i| i.flags.self_built = true),
            ("multi_device_overlap", |i| {
                i.flags.multi_device_overlap_minutes = 5
            }),
            ("low_credit_ratio", |i| {
                for s in &mut i.slots {
                    s.struggle_credit_sec = 90; // 15% of 600
                }
            }),
            ("unanchored_tail", |i| i.flags.unanchored_tail_links = 2),
        ];
        for (code, edit) in cases {
            let mut input = green_input();
            edit(&mut input);
            let (verdict, reasons) = verdict_of(&input);
            assert_eq!(verdict, Verdict::Yellow, "{code}");
            assert_eq!(reasons, [code], "{code}");
        }
    }

    #[test]
    fn the_overlap_share_and_credit_ratio_boundaries() {
        let mut input = green_input();
        input.flags.multi_device_overlap_minutes = 4; // 4/45 < 10%
        assert_eq!(verdict_of(&input).0, Verdict::Green);
        input.flags.multi_device_overlap_minutes = 5; // 5/45 >= 10%
        assert_eq!(verdict_of(&input).0, Verdict::Yellow);
        let mut input = green_input();
        for s in &mut input.slots {
            s.struggle_credit_sec = 91; // just above 15%
        }
        assert_eq!(verdict_of(&input).0, Verdict::Green);
    }

    #[test]
    fn a_long_run_of_busy_slots_without_marks_is_yellow() {
        let mut input = green_input();
        input.slots = (0..6)
            .map(|i| Slot {
                mutations_manual: 0,
                turns_started: 0,
                commits_human: 0,
                ..honest_slot(i)
            })
            .collect();
        // 6 slots = 90 minutes.
        let (verdict, reasons) = verdict_of(&input);
        assert_eq!(verdict, Verdict::Yellow);
        assert_eq!(reasons, ["long_run_without_marks"]);
        // A gap or a mark in the middle breaks the run.
        input.slots[3].turns_started = 1;
        assert_eq!(verdict_of(&input).0, Verdict::Green);
        input.slots[3].turns_started = 0;
        input.slots[3].slot_start_ms += SLOT_MS;
        assert_eq!(verdict_of(&input).0, Verdict::Green);
    }

    #[test]
    fn a_commit_oid_counts_as_a_mark() {
        let mut input = green_input();
        input.slots = (0..6)
            .map(|i| Slot {
                mutations_manual: 0,
                turns_started: 0,
                commits_human: 0,
                commit_oids: u32::from(i == 2),
                ..honest_slot(i)
            })
            .collect();
        assert_eq!(verdict_of(&input).0, Verdict::Green);
    }

    #[test]
    fn telemetry_tier_never_changes_the_colour() {
        let mut input = green_input();
        for s in &mut input.slots {
            s.tier_limited_minutes = 15;
        }
        let (verdict, reasons) = verdict_of(&input);
        assert_eq!(verdict, Verdict::Green);
        assert_eq!(reasons, ["tier_limited_share"]);
        // And it does not hide or soften a Yellow either.
        input.flags.clock_skew = true;
        let (verdict, reasons) = verdict_of(&input);
        assert_eq!(verdict, Verdict::Yellow);
        assert_eq!(reasons, ["clock_skew", "tier_limited_share"]);
    }

    #[test]
    fn red_lists_the_yellow_reasons_too() {
        let mut input = green_input();
        input.flags.late_anchor_windows = 5;
        input.flags.clock_skew = true;
        let (verdict, reasons) = verdict_of(&input);
        assert_eq!(verdict, Verdict::Red);
        assert_eq!(reasons, ["late_anchor", "clock_skew"]);
    }

    #[test]
    fn no_human_time_means_no_ratio_rules() {
        let mut input = green_input();
        input.slots = vec![];
        input.flags.active_minutes = 0;
        assert_eq!(verdict_of(&input).0, Verdict::Green);
    }

    /// Zero network: the engine completes when polled outside any runtime
    /// (a tokio socket or timer would panic there), and its source names no
    /// networking crate.
    #[test]
    fn the_engine_runs_with_no_network_and_no_runtime() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};
        let engine = engine();
        let input = green_input();
        let mut future = Box::pin(engine.triage(&input));
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(Ok(triage)) => assert_eq!(triage.verdict, Verdict::Green),
            other => panic!("the engine must finish on its first poll: {other:?}"),
        }
        let source = include_str!("heuristic.rs");
        let code = source.split("#[cfg(test)]").next().unwrap();
        for banned in [
            "ureq",
            "std::net",
            "tokio::net",
            "reqwest",
            "hyper",
            "TcpStream",
        ] {
            assert!(!code.contains(banned), "{banned} in the engine");
        }
    }
}
