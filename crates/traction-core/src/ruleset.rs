//! The Ruleset: every Step 2 scoring constant in one place, under an explicit
//! ID (`docs/protocol.md` section 3). Changing any number means a new ID.
//!
//! Nothing else in the workspace defines these numbers. Durations are plain
//! integers (milliseconds, seconds or per-mille) so the rollup stays
//! float-free and byte-exact across machines.

/// One versioned set of scoring constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ruleset {
    /// Recorded on every Work Bucket and Activity Rollup.
    pub ruleset_id: &'static str,
    /// Agent time more than this many seconds past the last verified human
    /// presence is Unverified Autonomous Time.
    pub unverified_after_sec: u32,
    /// Sub-slots (of 6) with real input a minute needs to count as present.
    pub min_active_subslots: u8,
    /// A minute whose injected share of input events is at or above this
    /// (per mille) is flagged `injected_hid_detected`.
    pub injected_ratio_flag_milli: u32,
    /// A cadence entropy below this (bits x 1000) is robotic. The sensor
    /// reports `null` below `min_cadence_samples`, so "enough samples" is
    /// already part of the value.
    pub robotic_entropy_max_milli: u32,
    /// Longest human-decision latency a permission prompt can earn.
    pub permission_latency_cap_ms: u64,
    /// An approval faster than this is machine speed.
    pub permission_latency_floor_ms: u64,
    /// Fast approvals in one minute bucket that set `auto_approver_suspected`.
    pub auto_approver_min_fast: u32,
    /// How far back a present minute looks for the Project it belongs to.
    pub presence_project_lookback_ms: u64,
    /// A commit with no open turn and no Harness event in this long before
    /// it is human.
    pub agent_quiet_ms: u64,
    /// A commit's committer time must be this close to its observation.
    pub commit_freshness_ms: u64,
    /// A minute is finalized once it is this much older than now.
    pub bucket_settle_ms: u64,
    /// An open turn with no event for this long closes at its last event.
    pub stale_turn_timeout_ms: u64,
    /// Activity Rollup window; matches the 2-minute Anchor cadence.
    pub activity_rollup_ms: u64,
    /// An OS idle timer at or above this counts the sub-slot as idle.
    pub idle_threshold_sec: u32,
    /// Fewer real inter-event intervals than this and cadence entropy is
    /// `null`.
    pub min_cadence_samples: u32,
    // Struggle signals and credit (Slice 3).
    /// Outcome Anchor window, either side of a minute.
    pub anchor_window_ms: u64,
    /// Unanchored `browser`/`other` presence decays to 0 over this long.
    pub drift_decay_ms: u64,
    /// Days of history before comfort-delta may reduce a weight.
    pub baseline_min_days: u32,
    /// Lowest comfort weight (per mille); it never reaches zero.
    pub comfort_floor_milli: u32,
    /// Days of per-axis histograms the local baseline keeps.
    pub baseline_window_days: u32,
    /// A minute with specificity below this (per mille) counts as
    /// low-specificity for comfort-delta.
    pub comfort_low_specificity_milli: u32,
    /// Steering looks for interleaved switching this close to an anchor.
    pub steering_window_ms: u64,
    /// `verified_struggle` is the weighted mean of the four axes; these
    /// weights (per mille) sum to 1000.
    pub weight_steering_milli: u32,
    pub weight_specificity_milli: u32,
    pub weight_friction_milli: u32,
    pub weight_comfort_milli: u32,
    /// A `verified_struggle` at or above this (per mille) is "high".
    pub verified_struggle_bar_milli: u32,
    /// Friction of a minute holding Manual Work that follows Harness churn.
    pub friction_revert_milli: u32,
    /// Manual Work this soon after a Harness `FILE_MUTATION` in the same
    /// Project counts as reverting churn.
    pub churn_revert_window_ms: u64,
    /// Specificity per distinct `repo_symbol_hits` (per mille).
    pub specificity_symbol_milli: u32,
    /// Specificity added by a pasted stack trace (per mille).
    pub specificity_stacktrace_milli: u32,
    /// A prompt shorter than this many characters is terse.
    pub terse_prompt_chars: u64,
    /// An agent turn changing at least this many lines is large.
    pub large_turn_diff_lines: u64,
    /// Share of its specificity (per mille) a terse prompt keeps when its
    /// turn is large.
    pub terse_big_turn_keep_milli: u32,
    /// Most entries one Project's file-name index holds.
    pub index_max_entries: u32,
    /// Most Projects with an index at once (least recently used goes).
    pub index_max_projects: u32,
    /// Longer file or folder names are not indexed.
    pub index_max_name_bytes: u32,
    /// Most distinct candidate words kept from one prompt while matching.
    pub index_max_prompt_candidates: u32,
    // Tier-limited presence and credit (Slice 4).
    /// On a tier that reports no input counts, a minute is present when the
    /// OS idle timer was past `idle_threshold_sec` for at most this many
    /// seconds of it.
    pub degraded_idle_max_sec: u32,
    /// Credit kept (per mille) on a `reduced`, `idle_only` or `none` minute:
    /// it weighs less as evidence for the user, never against.
    pub degraded_tier_credit_milli: u32,
}

/// The first Ruleset.
pub const TRACTION_RULES_1: Ruleset = Ruleset {
    ruleset_id: "traction-rules/1",
    unverified_after_sec: 90,
    min_active_subslots: 3,
    injected_ratio_flag_milli: 500,
    robotic_entropy_max_milli: 500,
    permission_latency_cap_ms: 120_000,
    permission_latency_floor_ms: 300,
    auto_approver_min_fast: 2,
    presence_project_lookback_ms: 5 * 60_000,
    agent_quiet_ms: 60_000,
    commit_freshness_ms: 60_000,
    bucket_settle_ms: 120_000,
    stale_turn_timeout_ms: 10 * 60_000,
    activity_rollup_ms: 120_000,
    idle_threshold_sec: 30,
    min_cadence_samples: 8,
    anchor_window_ms: 5 * 60_000,
    drift_decay_ms: 10 * 60_000,
    baseline_min_days: 3,
    comfort_floor_milli: 500,
    baseline_window_days: 30,
    comfort_low_specificity_milli: 300,
    steering_window_ms: 120_000,
    weight_steering_milli: 150,
    weight_specificity_milli: 250,
    weight_friction_milli: 400,
    weight_comfort_milli: 200,
    verified_struggle_bar_milli: 600,
    friction_revert_milli: 500,
    churn_revert_window_ms: 120_000,
    specificity_symbol_milli: 250,
    specificity_stacktrace_milli: 400,
    terse_prompt_chars: 40,
    large_turn_diff_lines: 30,
    terse_big_turn_keep_milli: 250,
    index_max_entries: 4096,
    index_max_projects: 16,
    index_max_name_bytes: 64,
    index_max_prompt_candidates: 512,
    degraded_idle_max_sec: 10,
    degraded_tier_credit_milli: 600,
};

/// Persona red-team pass marks (Step 2 Slice 4), kept beside the Ruleset they
/// tune. Each is `struggle_credit_sec` per mille of a 60-minute wall clock:
/// the honest debugger must reach its floor, every bot must stay at or
/// below its ceiling, on every Telemetry Tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersonaMarks {
    /// Honest debugger on a `full` tier.
    pub honest_min_milli: u32,
    /// Honest debugger on `reduced` and `idle_only`.
    pub honest_degraded_min_milli: u32,
    /// 59 s key-tapper.
    pub key_tapper_max_milli: u32,
    /// Trivial-prompt spammer.
    pub spammer_max_milli: u32,
    /// Steer-style rhythm faker with real input.
    pub steer_faker_max_milli: u32,
    /// Steer-style faker with injected input.
    pub steer_injected_max_milli: u32,
}

/// The marks that go with `traction-rules/1`.
pub const PERSONA_MARKS_1: PersonaMarks = PersonaMarks {
    honest_min_milli: 600,
    honest_degraded_min_milli: 400,
    key_tapper_max_milli: 20,
    spammer_max_milli: 50,
    steer_faker_max_milli: 20,
    steer_injected_max_milli: 0,
};
