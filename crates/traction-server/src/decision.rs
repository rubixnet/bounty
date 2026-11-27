//! The `DecisionEngine` seam (spec S3a, Q14). A submitted Audit Bundle is
//! verified deterministically first; only a bundle that survives is shown to
//! an engine, as an anonymized [`TriageInput`]. An engine answers with a
//! [`Triage`]: `Green | Yellow | Red`, reason codes, its own id and rubric
//! version. There is **no ban variant**: Fraud Squad decides, never an engine.
//!
//! `TriageInput` holds an opaque per-submission reference, 15-minute
//! aggregates of the submitted Project's buckets, rollups, Timeline Marks and
//! commit counts, the verification summary and the anchor flags. It never
//! holds a user, a path, a username or a Project name (a test asserts this on
//! the serialized input).

use std::future::Future;
use std::pin::Pin;

use serde::{Deserialize, Serialize};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Width of one aggregate slot.
pub const SLOT_MS: u64 = 15 * 60_000;

/// Reason codes. Colour-changing codes and informational ones share this
/// namespace; the engine's docs say which is which.
pub mod reasons {
    /// Red: the bundle failed a deterministic check (the code of the failure
    /// is stored beside it).
    pub const DETERMINISTIC_FAILURE: &str = "deterministic_failure";
    pub const INJECTED_HID_DETECTED: &str = "injected_hid_detected";
    pub const LATE_ANCHOR: &str = "late_anchor";
    pub const OFFLINE_BUFFERED: &str = "offline_buffered";
    pub const CLOCK_SKEW: &str = "clock_skew";
    pub const KEY_DISCONTINUITY: &str = "key_discontinuity";
    pub const SELF_BUILT: &str = "self_built";
    pub const MULTI_DEVICE_OVERLAP: &str = "multi_device_overlap";
    pub const LONG_RUN_WITHOUT_MARKS: &str = "long_run_without_marks";
    pub const LOW_CREDIT_RATIO: &str = "low_credit_ratio";
    pub const UNANCHORED_TAIL: &str = "unanchored_tail";
    /// Informational only: never changes the colour (Q32).
    pub const TIER_LIMITED_SHARE: &str = "tier_limited_share";
    /// Yellow: the engine itself failed; a human looks.
    pub const ENGINE_ERROR: &str = "engine_error";
    /// Informational (S4): Jev was not used and `HeuristicEngine` decided,
    /// because the monthly spend limit would be crossed.
    pub const JEV_FALLBACK_SPEND_LIMIT: &str = "jev_fallback_spend_limit";
    /// Informational (S4): Jev answered with an error or a malformed reply.
    pub const JEV_FALLBACK_ERROR: &str = "jev_fallback_error";
    /// Informational (S4): Jev did not answer in time.
    pub const JEV_FALLBACK_TIMEOUT: &str = "jev_fallback_timeout";
    /// Informational (S4): a frame was under 280 px on a side and not sent.
    pub const FRAME_TOO_SMALL_SKIPPED: &str = "frame_too_small_skipped";
    /// Informational (S4): a frame could not be decoded and was not sent.
    pub const FRAME_UNDECODABLE: &str = "frame_undecodable";
    /// Yellow (S4): the share of skipped frames reached
    /// `[jev].frame_skip_yellow_share`.
    pub const FRAME_SKIP_SHARE: &str = "frame_skip_share";
}

/// Why the bundle is being triaged; images go to Jev only for `Submitted`
/// or `Flagged` bundles (spec S4).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewContext {
    #[default]
    Routine,
    Submitted,
    Flagged,
}

impl ReviewContext {
    pub fn allows_images(self) -> bool {
        matches!(self, ReviewContext::Submitted | ReviewContext::Flagged)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Green,
    Yellow,
    Red,
}

impl Verdict {
    pub fn as_str(self) -> &'static str {
        match self {
            Verdict::Green => "green",
            Verdict::Yellow => "yellow",
            Verdict::Red => "red",
        }
    }
}

/// An engine's decision. Deliberately has no ban variant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Triage {
    pub verdict: Verdict,
    pub reasons: Vec<String>,
    pub engine_id: String,
    pub rubric_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineError(pub String);

impl std::fmt::Display for EngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EngineError {}

/// One 15-minute aggregate of the submitted Project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slot {
    pub slot_start_ms: u64,
    /// Work Bucket minutes in the slot.
    pub minutes: u32,
    pub human_active_sec: u32,
    pub agent_compute_sec: u32,
    pub struggle_credit_sec: u32,
    /// Human Active Time of buckets flagged `injected_hid_detected`.
    pub injected_human_active_sec: u32,
    /// Buckets whose Telemetry Tier limited the credit.
    pub tier_limited_minutes: u32,
    pub commits_human: u32,
    pub commits_agent: u32,
    /// Sums of the unsigned Timeline Marks.
    pub mutations_manual: u32,
    pub mutations_harness: u32,
    pub turns_started: u32,
    pub tool_failures: u32,
    /// Commit oids the GitHub client stored for the slot.
    pub commit_oids: u32,
    /// Activity Rollup windows in the slot.
    pub rollup_windows: u32,
    pub switch_rate_milli_sum: u64,
}

impl Slot {
    /// Whether the unsigned marks and commits show any work at all.
    pub fn has_marks(&self) -> bool {
        self.mutations_manual
            + self.mutations_harness
            + self.turns_started
            + self.commits_human
            + self.commits_agent
            + self.commit_oids
            > 0
    }
}

/// What the deterministic verification found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerificationSummary {
    /// The stable code of the first deterministic failure; `None` when the
    /// bundle verified, matched every Anchor and equalled the live rows.
    pub failure: Option<String>,
    pub chains: u32,
    pub links: u32,
    pub key_pinned: bool,
    pub key_discontinuities: u32,
    pub clock_regressions: u32,
}

/// Anchor-derived flags, counted over the submitted Project.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnchorFlags {
    /// Two-minute windows of the Project first anchored `late_anchor`.
    pub late_anchor_windows: u32,
    /// Two-minute windows of the Project first anchored in `realtime` mode.
    pub realtime_windows: u32,
    pub offline_buffered_windows: u32,
    pub clock_skew: bool,
    /// Project minutes with Human Active Time.
    pub active_minutes: u32,
    /// Of those, minutes another device of the user also covered (the
    /// 60-second merge cap applied).
    pub multi_device_overlap_minutes: u32,
    /// From build provenance (S3b); `false` while it is absent.
    pub self_built: bool,
    /// Chain links newer than the newest stored Anchor.
    pub unanchored_tail_links: u32,
}

/// The anonymized input every engine sees.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriageInput {
    /// Random, opaque, per submission; links to nothing.
    pub submission_ref: String,
    pub verification: VerificationSummary,
    pub flags: AnchorFlags,
    pub slots: Vec<Slot>,
    #[serde(default)]
    pub review: ReviewContext,
    /// Sampled WebP frames (visual proof, Step 3b). Never serialized, so
    /// never stored with the decision; until 3b ships frames this is empty
    /// outside tests.
    #[serde(skip)]
    pub frames: Vec<Vec<u8>>,
}

impl TriageInput {
    pub fn human_active_sec(&self) -> u64 {
        self.slots
            .iter()
            .map(|s| u64::from(s.human_active_sec))
            .sum()
    }
}

/// The seam. Engines are swappable behind it: `HeuristicEngine` is the
/// zero-network default, `JevEngine` arrives in S4.
pub trait DecisionEngine: Send + Sync {
    /// Stable id recorded with every decision.
    fn engine_id(&self) -> &'static str;

    fn triage<'a>(&'a self, input: &'a TriageInput) -> BoxFuture<'a, Result<Triage, EngineError>>;
}

/// Decides one submission. A deterministic failure is Red **before** any
/// engine runs and no engine can change it; an engine error is Yellow (a
/// human looks), never Green and never a ban.
pub async fn decide(engine: &dyn DecisionEngine, input: &TriageInput) -> Triage {
    if let Some(failure) = &input.verification.failure {
        return Triage {
            verdict: Verdict::Red,
            reasons: vec![reasons::DETERMINISTIC_FAILURE.into(), failure.clone()],
            engine_id: "deterministic".into(),
            rubric_version: "verification/1".into(),
        };
    }
    match engine.triage(input).await {
        Ok(triage) => triage,
        Err(_) => Triage {
            verdict: Verdict::Yellow,
            reasons: vec![reasons::ENGINE_ERROR.into()],
            engine_id: engine.engine_id().into(),
            rubric_version: "unknown".into(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Always(Verdict);

    impl DecisionEngine for Always {
        fn engine_id(&self) -> &'static str {
            "always"
        }
        fn triage<'a>(&'a self, _: &'a TriageInput) -> BoxFuture<'a, Result<Triage, EngineError>> {
            let verdict = self.0;
            Box::pin(async move {
                Ok(Triage {
                    verdict,
                    reasons: vec![],
                    engine_id: "always".into(),
                    rubric_version: "t".into(),
                })
            })
        }
    }

    struct Broken;

    impl DecisionEngine for Broken {
        fn engine_id(&self) -> &'static str {
            "broken"
        }
        fn triage<'a>(&'a self, _: &'a TriageInput) -> BoxFuture<'a, Result<Triage, EngineError>> {
            Box::pin(async { Err(EngineError("boom".into())) })
        }
    }

    fn input(failure: Option<&str>) -> TriageInput {
        TriageInput {
            submission_ref: "ref".into(),
            verification: VerificationSummary {
                failure: failure.map(str::to_owned),
                ..Default::default()
            },
            flags: AnchorFlags::default(),
            slots: vec![],
            review: ReviewContext::Submitted,
            frames: vec![],
        }
    }

    #[tokio::test]
    async fn a_deterministic_failure_is_red_whatever_the_engine_says() {
        let triage = decide(&Always(Verdict::Green), &input(Some("tip_mismatch"))).await;
        assert_eq!(triage.verdict, Verdict::Red);
        assert_eq!(triage.engine_id, "deterministic");
        assert!(triage.reasons.contains(&"tip_mismatch".to_owned()));
    }

    #[tokio::test]
    async fn without_a_failure_the_engine_decides_and_is_recorded() {
        let triage = decide(&Always(Verdict::Yellow), &input(None)).await;
        assert_eq!(
            (triage.verdict, triage.engine_id.as_str()),
            (Verdict::Yellow, "always")
        );
    }

    #[tokio::test]
    async fn an_engine_error_is_yellow_never_green_and_never_a_ban() {
        let triage = decide(&Broken, &input(None)).await;
        assert_eq!(triage.verdict, Verdict::Yellow);
        assert_eq!(triage.reasons, [reasons::ENGINE_ERROR]);
    }

    #[test]
    fn the_verdict_has_exactly_three_colours() {
        // A ban variant would make this match non-exhaustive.
        for verdict in [Verdict::Green, Verdict::Yellow, Verdict::Red] {
            match verdict {
                Verdict::Green | Verdict::Yellow | Verdict::Red => {}
            }
        }
    }

    #[test]
    fn the_serialized_input_names_no_user_path_username_or_project() {
        let text = serde_json::to_string(&input(None)).unwrap();
        for forbidden in ["user", "path", "name", "project", "root"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
    }
}
