//! Typed-column storage of Work Buckets and Activity Rollups. A row converts
//! back to exactly the struct that was stored, so the later submission
//! cross-check is plain struct equality. Runtime `sqlx::query` only.

use sqlx::{FromRow, PgConnection};
use traction_core::{ActivityRollup, CategoryShareMilli, StruggleAxes, TelemetryTier, WorkBucket};

/// A value Postgres' `BIGINT` cannot hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OutOfRange;

fn to_i64(value: u64) -> Result<i64, OutOfRange> {
    i64::try_from(value).map_err(|_| OutOfRange)
}

fn to_u64(value: i64) -> Result<u64, OutOfRange> {
    u64::try_from(value).map_err(|_| OutOfRange)
}

fn to_u32(value: i64) -> Result<u32, OutOfRange> {
    u32::try_from(value).map_err(|_| OutOfRange)
}

fn tier_text(tier: TelemetryTier) -> &'static str {
    match tier {
        TelemetryTier::Full => "full",
        TelemetryTier::Reduced => "reduced",
        TelemetryTier::IdleOnly => "idle_only",
        TelemetryTier::None => "none",
    }
}

fn tier_from(text: &str) -> Result<TelemetryTier, OutOfRange> {
    match text {
        "full" => Ok(TelemetryTier::Full),
        "reduced" => Ok(TelemetryTier::Reduced),
        "idle_only" => Ok(TelemetryTier::IdleOnly),
        "none" => Ok(TelemetryTier::None),
        _ => Err(OutOfRange),
    }
}

#[derive(FromRow)]
pub struct BucketRow {
    project_id: String,
    minute_start_ms: i64,
    ruleset_id: String,
    agent_compute_sec: i64,
    idle_autonomous_sec: i64,
    human_active_sec: i64,
    permission_wait_ms: i64,
    commits_human: i64,
    commits_agent: i64,
    injected_hid_detected: bool,
    auto_approver_suspected: bool,
    telemetry_tier: String,
    tier_limited: bool,
    axis_steering: i64,
    axis_specificity: i64,
    axis_friction: i64,
    axis_comfort_delta: i64,
    verified_struggle: i64,
    struggle_credit_sec: i64,
}

impl BucketRow {
    pub fn into_bucket(self) -> Result<WorkBucket, OutOfRange> {
        Ok(WorkBucket {
            project_id: self.project_id,
            minute_start_ms: to_u64(self.minute_start_ms)?,
            ruleset_id: self.ruleset_id,
            agent_compute_sec: to_u32(self.agent_compute_sec)?,
            idle_autonomous_sec: to_u32(self.idle_autonomous_sec)?,
            human_active_sec: to_u32(self.human_active_sec)?,
            permission_wait_ms: to_u64(self.permission_wait_ms)?,
            commits_human: to_u32(self.commits_human)?,
            commits_agent: to_u32(self.commits_agent)?,
            injected_hid_detected: self.injected_hid_detected,
            auto_approver_suspected: self.auto_approver_suspected,
            telemetry_tier: tier_from(&self.telemetry_tier)?,
            tier_limited: self.tier_limited,
            struggle_axes: StruggleAxes {
                steering: to_u32(self.axis_steering)?,
                specificity: to_u32(self.axis_specificity)?,
                friction: to_u32(self.axis_friction)?,
                comfort_delta: to_u32(self.axis_comfort_delta)?,
            },
            verified_struggle: to_u32(self.verified_struggle)?,
            struggle_credit_sec: to_u32(self.struggle_credit_sec)?,
        })
    }
}

#[derive(Debug)]
pub enum StoreError {
    OutOfRange,
    Database(sqlx::Error),
}

impl From<sqlx::Error> for StoreError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<OutOfRange> for StoreError {
    fn from(_: OutOfRange) -> Self {
        Self::OutOfRange
    }
}

/// Stores one bucket; `false` when that `(device, project, minute, ruleset)`
/// was already stored (stored rows never change).
pub async fn insert_bucket(
    conn: &mut PgConnection,
    user_id: &str,
    device_id: &str,
    b: &WorkBucket,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "INSERT INTO work_buckets (user_id, device_id, project_id, minute_start_ms, ruleset_id,
            agent_compute_sec, idle_autonomous_sec, human_active_sec, permission_wait_ms,
            commits_human, commits_agent, injected_hid_detected, auto_approver_suspected,
            telemetry_tier, tier_limited, axis_steering, axis_specificity, axis_friction,
            axis_comfort_delta, verified_struggle, struggle_credit_sec)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .bind(device_id)
    .bind(&b.project_id)
    .bind(to_i64(b.minute_start_ms)?)
    .bind(&b.ruleset_id)
    .bind(i64::from(b.agent_compute_sec))
    .bind(i64::from(b.idle_autonomous_sec))
    .bind(i64::from(b.human_active_sec))
    .bind(to_i64(b.permission_wait_ms)?)
    .bind(i64::from(b.commits_human))
    .bind(i64::from(b.commits_agent))
    .bind(b.injected_hid_detected)
    .bind(b.auto_approver_suspected)
    .bind(tier_text(b.telemetry_tier))
    .bind(b.tier_limited)
    .bind(i64::from(b.struggle_axes.steering))
    .bind(i64::from(b.struggle_axes.specificity))
    .bind(i64::from(b.struggle_axes.friction))
    .bind(i64::from(b.struggle_axes.comfort_delta))
    .bind(i64::from(b.verified_struggle))
    .bind(i64::from(b.struggle_credit_sec))
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

/// A device's stored buckets, oldest first.
pub async fn fetch_buckets(
    conn: &mut PgConnection,
    device_id: &str,
) -> Result<Vec<WorkBucket>, StoreError> {
    let rows: Vec<BucketRow> = sqlx::query_as(
        "SELECT project_id, minute_start_ms, ruleset_id, agent_compute_sec, idle_autonomous_sec,
                human_active_sec, permission_wait_ms, commits_human, commits_agent,
                injected_hid_detected, auto_approver_suspected, telemetry_tier, tier_limited,
                axis_steering, axis_specificity, axis_friction, axis_comfort_delta,
                verified_struggle, struggle_credit_sec
         FROM work_buckets WHERE device_id = $1 ORDER BY minute_start_ms, project_id",
    )
    .bind(device_id)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| r.into_bucket().map_err(StoreError::from))
        .collect()
}

#[derive(FromRow)]
pub struct RollupRow {
    window_start_ms: i64,
    ruleset_id: String,
    minutes_observed: i64,
    share_ide: i64,
    share_terminal: i64,
    share_browser: i64,
    share_ai_desktop: i64,
    share_other: i64,
    switch_rate_milli: i64,
    mean_cadence_entropy_milli: Option<i64>,
    injected_ratio_milli: Option<i64>,
    telemetry_tier: String,
}

impl RollupRow {
    pub fn into_rollup(self) -> Result<ActivityRollup, OutOfRange> {
        Ok(ActivityRollup {
            window_start_ms: to_u64(self.window_start_ms)?,
            ruleset_id: self.ruleset_id,
            minutes_observed: to_u32(self.minutes_observed)?,
            category_share_milli: CategoryShareMilli {
                ide: to_u32(self.share_ide)?,
                terminal: to_u32(self.share_terminal)?,
                browser: to_u32(self.share_browser)?,
                ai_desktop: to_u32(self.share_ai_desktop)?,
                other: to_u32(self.share_other)?,
            },
            switch_rate_milli: to_u32(self.switch_rate_milli)?,
            mean_cadence_entropy_milli: self.mean_cadence_entropy_milli.map(to_u32).transpose()?,
            injected_ratio_milli: self.injected_ratio_milli.map(to_u32).transpose()?,
            telemetry_tier: tier_from(&self.telemetry_tier)?,
        })
    }
}

pub async fn insert_rollup(
    conn: &mut PgConnection,
    user_id: &str,
    device_id: &str,
    r: &ActivityRollup,
) -> Result<bool, StoreError> {
    let result = sqlx::query(
        "INSERT INTO activity_rollups (user_id, device_id, window_start_ms, ruleset_id,
            minutes_observed, share_ide, share_terminal, share_browser, share_ai_desktop,
            share_other, switch_rate_milli, mean_cadence_entropy_milli, injected_ratio_milli,
            telemetry_tier)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)
         ON CONFLICT DO NOTHING",
    )
    .bind(user_id)
    .bind(device_id)
    .bind(to_i64(r.window_start_ms)?)
    .bind(&r.ruleset_id)
    .bind(i64::from(r.minutes_observed))
    .bind(i64::from(r.category_share_milli.ide))
    .bind(i64::from(r.category_share_milli.terminal))
    .bind(i64::from(r.category_share_milli.browser))
    .bind(i64::from(r.category_share_milli.ai_desktop))
    .bind(i64::from(r.category_share_milli.other))
    .bind(i64::from(r.switch_rate_milli))
    .bind(r.mean_cadence_entropy_milli.map(i64::from))
    .bind(r.injected_ratio_milli.map(i64::from))
    .bind(tier_text(r.telemetry_tier))
    .execute(conn)
    .await?;
    Ok(result.rows_affected() == 1)
}

pub async fn fetch_rollups(
    conn: &mut PgConnection,
    device_id: &str,
) -> Result<Vec<ActivityRollup>, StoreError> {
    let rows: Vec<RollupRow> = sqlx::query_as(
        "SELECT window_start_ms, ruleset_id, minutes_observed, share_ide, share_terminal,
                share_browser, share_ai_desktop, share_other, switch_rate_milli,
                mean_cadence_entropy_milli, injected_ratio_milli, telemetry_tier
         FROM activity_rollups WHERE device_id = $1 ORDER BY window_start_ms",
    )
    .bind(device_id)
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|r| r.into_rollup().map_err(StoreError::from))
        .collect()
}
