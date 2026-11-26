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
    // Slots Slice 3 (struggle signals and credit) fills.
    /// Outcome Anchor window, either side of a minute.
    pub anchor_window_ms: u64,
    /// Unanchored `browser`/`other` presence decays to 0 over this long.
    pub drift_decay_ms: u64,
    /// Days of history before comfort-delta may reduce a weight.
    pub baseline_min_days: u32,
    /// Lowest comfort weight (per mille); it never reaches zero.
    pub comfort_floor_milli: u32,
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
};
