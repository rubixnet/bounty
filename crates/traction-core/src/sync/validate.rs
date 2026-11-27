//! The pure ingest checks the server runs on every heartbeat item. Nothing
//! here echoes a value: a failure is one stable code.

use std::fmt;

use crate::validate::is_project_id;
use crate::{ActivityRollup, MINUTE_MS, TRACTION_RULES_1, WorkBucket};

/// Why a Work Bucket or Activity Rollup was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IngestError {
    UnknownRuleset,
    HumanActiveOverMinute,
    AgentComputeOverMinute,
    CreditOverHumanActive,
    IdleOverAgentCompute,
    MisalignedMinute,
    MisalignedWindow,
    WindowAfterTip,
    InvalidProjectId,
    InvalidTimelineMark,
}

impl IngestError {
    /// A short, stable, machine-readable reason code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownRuleset => "unknown_ruleset",
            Self::HumanActiveOverMinute => "human_active_over_minute",
            Self::AgentComputeOverMinute => "agent_compute_over_minute",
            Self::CreditOverHumanActive => "credit_over_human_active",
            Self::IdleOverAgentCompute => "idle_over_agent_compute",
            Self::MisalignedMinute => "misaligned_minute",
            Self::MisalignedWindow => "misaligned_window",
            Self::WindowAfterTip => "window_after_tip",
            Self::InvalidProjectId => "invalid_project_id",
            Self::InvalidTimelineMark => "invalid_timeline_mark",
        }
    }
}

impl fmt::Display for IngestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for IngestError {}

/// Only `TRACTION_RULES_1` is known.
pub fn validate_ruleset(ruleset_id: &str) -> Result<(), IngestError> {
    if ruleset_id == TRACTION_RULES_1.ruleset_id {
        Ok(())
    } else {
        Err(IngestError::UnknownRuleset)
    }
}

/// `git:<64 hex>` or `dir:<64 hex>`.
pub fn validate_project_id(project_id: &str) -> Result<(), IngestError> {
    if is_project_id(project_id) {
        Ok(())
    } else {
        Err(IngestError::InvalidProjectId)
    }
}

/// The window (a multiple of the rollup width) holding `ms`.
fn window_of(ms: u64) -> u64 {
    ms / TRACTION_RULES_1.activity_rollup_ms * TRACTION_RULES_1.activity_rollup_ms
}

/// Every per-bucket ingest check.
pub fn validate_work_bucket(bucket: &WorkBucket) -> Result<(), IngestError> {
    validate_ruleset(&bucket.ruleset_id)?;
    validate_project_id(&bucket.project_id)?;
    if !bucket.minute_start_ms.is_multiple_of(MINUTE_MS) {
        return Err(IngestError::MisalignedMinute);
    }
    if bucket.human_active_sec > 60 {
        return Err(IngestError::HumanActiveOverMinute);
    }
    if bucket.agent_compute_sec > 60 {
        return Err(IngestError::AgentComputeOverMinute);
    }
    if bucket.struggle_credit_sec > bucket.human_active_sec {
        return Err(IngestError::CreditOverHumanActive);
    }
    if bucket.idle_autonomous_sec > bucket.agent_compute_sec {
        return Err(IngestError::IdleOverAgentCompute);
    }
    Ok(())
}

/// Every per-rollup ingest check.
pub fn validate_rollup(rollup: &ActivityRollup) -> Result<(), IngestError> {
    validate_ruleset(&rollup.ruleset_id)?;
    if !rollup
        .window_start_ms
        .is_multiple_of(TRACTION_RULES_1.activity_rollup_ms)
    {
        return Err(IngestError::MisalignedWindow);
    }
    Ok(())
}

/// A row's window must not come after the tip's window. `row_ms` is a
/// minute start or a window start.
pub fn validate_window_not_after_tip(
    row_ms: u64,
    tip_window_start_ms: u64,
) -> Result<(), IngestError> {
    if window_of(row_ms) > tip_window_start_ms {
        Err(IngestError::WindowAfterTip)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StruggleAxes, TelemetryTier};

    fn bucket() -> WorkBucket {
        WorkBucket {
            project_id: format!("dir:{}", "a".repeat(64)),
            minute_start_ms: 1_800_000_000_000 / 60_000 * 60_000,
            ruleset_id: "traction-rules/1".to_owned(),
            agent_compute_sec: 30,
            idle_autonomous_sec: 10,
            human_active_sec: 40,
            permission_wait_ms: 0,
            commits_human: 0,
            commits_agent: 0,
            injected_hid_detected: false,
            auto_approver_suspected: false,
            telemetry_tier: TelemetryTier::Full,
            tier_limited: false,
            struggle_axes: StruggleAxes::default(),
            verified_struggle: 500,
            struggle_credit_sec: 20,
        }
    }

    fn code(b: &WorkBucket) -> Option<&'static str> {
        validate_work_bucket(b).err().map(|e| e.code())
    }

    #[test]
    fn a_consistent_bucket_passes() {
        assert_eq!(code(&bucket()), None);
    }

    #[test]
    fn each_ingest_rule_has_its_stable_code() {
        let mut b = bucket();
        b.ruleset_id = "traction-rules/2".into();
        assert_eq!(code(&b), Some("unknown_ruleset"));
        let mut b = bucket();
        b.human_active_sec = 61;
        assert_eq!(code(&b), Some("human_active_over_minute"));
        let mut b = bucket();
        b.struggle_credit_sec = 41;
        assert_eq!(code(&b), Some("credit_over_human_active"));
        let mut b = bucket();
        b.idle_autonomous_sec = 31;
        assert_eq!(code(&b), Some("idle_over_agent_compute"));
        let mut b = bucket();
        b.minute_start_ms += 1;
        assert_eq!(code(&b), Some("misaligned_minute"));
        let mut b = bucket();
        b.project_id = "dir:xyz".into();
        assert_eq!(code(&b), Some("invalid_project_id"));
    }

    #[test]
    fn rollups_must_be_window_aligned_and_known() {
        let mut r = ActivityRollup {
            window_start_ms: 120_000 * 7,
            ruleset_id: "traction-rules/1".into(),
            minutes_observed: 2,
            category_share_milli: Default::default(),
            switch_rate_milli: 0,
            mean_cadence_entropy_milli: None,
            injected_ratio_milli: None,
            telemetry_tier: TelemetryTier::Full,
        };
        assert_eq!(validate_rollup(&r), Ok(()));
        r.window_start_ms += 60_000;
        assert_eq!(validate_rollup(&r), Err(IngestError::MisalignedWindow));
        r.window_start_ms = 120_000;
        r.ruleset_id = "x".into();
        assert_eq!(validate_rollup(&r), Err(IngestError::UnknownRuleset));
    }

    #[test]
    fn no_row_may_come_after_the_tip_window() {
        // The tip window [240_000, 360_000) holds minutes 240_000 and 300_000.
        assert_eq!(validate_window_not_after_tip(300_000, 240_000), Ok(()));
        assert_eq!(
            validate_window_not_after_tip(360_000, 240_000),
            Err(IngestError::WindowAfterTip)
        );
    }
}
