//! `POST /v1/anchors`: one heartbeat per device. The server assigns a
//! monotonic `server_received_at`, stores the Anchor once (a different
//! `entry_hash` for the same `(device, key_id, seq)` is `409`, and the stored
//! Anchor never changes), validates every item with the core `validate_*`
//! functions and stores the accepted ones.
//!
//! The server never runs chain verification here (`docs/adr/0004`).

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use sqlx::PgConnection;
use traction_core::sync::{
    AnchorStatus, HeartbeatRequest, HeartbeatResponse, ItemKind, ItemRef, ProjectMeta,
    RejectedItem, codes, validate_project_id, validate_rollup, validate_window_not_after_tip,
    validate_work_bucket,
};
use traction_core::{ActivityRollup, TRACTION_RULES_1, VerifyingKeyHex, WorkBucket};

use crate::app::{AppState, authenticate};
use crate::config::AnchorPolicy;
use crate::error::ApiError;
use crate::fanout::{LiveEvent, Totals};
use crate::partition::{self, Month};
use crate::store::{self, StoreError};

/// The result of checking a heartbeat's items against policy.
#[derive(Debug, Default, PartialEq)]
pub struct Classified {
    pub buckets: Vec<WorkBucket>,
    pub rollups: Vec<ActivityRollup>,
    pub rejected: Vec<RejectedItem>,
}

/// Why a whole heartbeat is refused before any item is looked at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TipError {
    Misaligned,
    FutureWindow,
    WindowTooOld,
}

impl TipError {
    pub fn code(self) -> &'static str {
        match self {
            TipError::Misaligned => "misaligned_window",
            TipError::FutureWindow => codes::FUTURE_WINDOW,
            TipError::WindowTooOld => codes::WINDOW_TOO_OLD,
        }
    }
}

/// `422 future_window` / `422 window_too_old` on the tip's window.
pub fn check_tip(tip_window_ms: u64, now_ms: u64, policy: &AnchorPolicy) -> Result<(), TipError> {
    if !tip_window_ms.is_multiple_of(TRACTION_RULES_1.activity_rollup_ms) {
        return Err(TipError::Misaligned);
    }
    if tip_window_ms > now_ms.saturating_add(policy.max_future_skew_ms) {
        return Err(TipError::FutureWindow);
    }
    if tip_window_ms < now_ms.saturating_sub(policy.max_backlog_age_ms()) {
        return Err(TipError::WindowTooOld);
    }
    Ok(())
}

fn too_old(ms: u64, now_ms: u64, policy: &AnchorPolicy) -> bool {
    ms < now_ms.saturating_sub(policy.max_backlog_age_ms())
}

/// Validates every bucket and rollup. Rejections carry value-free codes.
pub fn classify(request: &HeartbeatRequest, now_ms: u64, policy: &AnchorPolicy) -> Classified {
    let tip = request.tip.window_start_ms;
    let mut out = Classified::default();
    for bucket in &request.work_buckets {
        let item = ItemRef {
            kind: ItemKind::WorkBucket,
            start_ms: bucket.minute_start_ms,
            project_id: Some(bucket.project_id.clone()),
        };
        let verdict = validate_work_bucket(bucket)
            .and_then(|()| validate_window_not_after_tip(bucket.minute_start_ms, tip))
            .map_err(|e| e.code())
            .and_then(|()| {
                if too_old(bucket.minute_start_ms, now_ms, policy) {
                    Err(codes::WINDOW_TOO_OLD)
                } else if i64::try_from(bucket.minute_start_ms).is_err()
                    || i64::try_from(bucket.permission_wait_ms).is_err()
                {
                    Err("value_out_of_range")
                } else {
                    Ok(())
                }
            });
        match verdict {
            Ok(()) => out.buckets.push(bucket.clone()),
            Err(code) => out.rejected.push(RejectedItem {
                item,
                code: code.into(),
            }),
        }
    }
    for rollup in &request.activity_rollups {
        let item = ItemRef {
            kind: ItemKind::ActivityRollup,
            start_ms: rollup.window_start_ms,
            project_id: None,
        };
        let verdict = validate_rollup(rollup)
            .and_then(|()| validate_window_not_after_tip(rollup.window_start_ms, tip))
            .map_err(|e| e.code())
            .and_then(|()| {
                if too_old(rollup.window_start_ms, now_ms, policy) {
                    Err(codes::WINDOW_TOO_OLD)
                } else if i64::try_from(rollup.window_start_ms).is_err() {
                    Err("value_out_of_range")
                } else {
                    Ok(())
                }
            });
        match verdict {
            Ok(()) => out.rollups.push(rollup.clone()),
            Err(code) => out.rejected.push(RejectedItem {
                item,
                code: code.into(),
            }),
        }
    }
    out
}

/// Months an accepted heartbeat needs partitions for.
fn months_needed(classified: &Classified, received_ms: u64) -> Vec<Month> {
    let mut months = vec![Month::of_ms(received_ms)];
    months.extend(
        classified
            .buckets
            .iter()
            .map(|b| Month::of_ms(b.minute_start_ms)),
    );
    months.extend(
        classified
            .rollups
            .iter()
            .map(|r| Month::of_ms(r.window_start_ms)),
    );
    months.sort();
    months.dedup();
    months
}

/// Creates (once per process) any partition a heartbeat needs, on demand.
async fn ensure_partitions(state: &AppState, months: &[Month]) -> Result<(), ApiError> {
    let missing: Vec<Month> = {
        let known = state.known_months.lock().unwrap_or_else(|e| e.into_inner());
        months
            .iter()
            .copied()
            .filter(|m| !known.contains(m))
            .collect()
    };
    if missing.is_empty() {
        return Ok(());
    }
    partition::ensure_months(&state.pool, &missing).await?;
    state
        .known_months
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(missing);
    Ok(())
}

fn store_error(error: StoreError) -> ApiError {
    match error {
        StoreError::Database(e) => ApiError::database(&e),
        StoreError::OutOfRange => ApiError::unprocessable("value_out_of_range"),
    }
}

async fn upsert_project(
    conn: &mut PgConnection,
    user_id: &str,
    meta: &ProjectMeta,
    now_ms: u64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO projects (user_id, project_id, project_name, root_path, home_relative_path,
                               os_username, updated_at_ms)
         VALUES ($1,$2,$3,$4,$5,$6,$7)
         ON CONFLICT (user_id, project_id) DO UPDATE SET
            project_name = EXCLUDED.project_name, root_path = EXCLUDED.root_path,
            home_relative_path = EXCLUDED.home_relative_path,
            os_username = EXCLUDED.os_username, updated_at_ms = EXCLUDED.updated_at_ms",
    )
    .bind(user_id)
    .bind(&meta.project_id)
    .bind(&meta.project_name)
    .bind(&meta.project_root_path)
    .bind(&meta.project_home_relative_path)
    .bind(&meta.os_username)
    .bind(i64::try_from(now_ms).unwrap_or(i64::MAX))
    .execute(conn)
    .await?;
    Ok(())
}

pub async fn user_totals(state: &AppState, user_id: &str) -> Result<Totals, sqlx::Error> {
    let (human, credit, agent): (i64, i64, i64) = sqlx::query_as(
        "SELECT COALESCE(SUM(human_active_sec), 0)::bigint,
                COALESCE(SUM(struggle_credit_sec), 0)::bigint,
                COALESCE(SUM(agent_compute_sec), 0)::bigint
         FROM work_buckets WHERE user_id = $1",
    )
    .bind(user_id)
    .fetch_one(&state.pool)
    .await?;
    let to_u64 = |v: i64| u64::try_from(v).unwrap_or(0);
    Ok(Totals {
        human_active_sec: to_u64(human),
        struggle_credit_sec: to_u64(credit),
        agent_compute_sec: to_u64(agent),
    })
}

pub async fn post_anchors(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    let device = authenticate(&state, &headers).await?;
    let now_us = (state.clock)();
    if let Err(retry_after) = state.limiter.check(&device.id, now_us / 1000) {
        return Err(ApiError::too_many(retry_after));
    }
    let request: HeartbeatRequest =
        serde_json::from_slice(&body).map_err(|_| ApiError::bad_request("malformed_request"))?;
    let policy = &state.config.anchor_policy;
    let now_ms = now_us / 1000;
    check_tip(request.tip.window_start_ms, now_ms, policy)
        .map_err(|e| ApiError::unprocessable(e.code()))?;

    let pinned: Option<String> = sqlx::query_scalar(
        "SELECT public_key FROM device_keys WHERE device_id = $1 AND key_id = $2",
    )
    .bind(&device.id)
    .bind(request.tip.key_id.as_str())
    .fetch_optional(&state.pool)
    .await?;
    if !pinned.is_some_and(|key| VerifyingKeyHex::parse(&key).is_ok()) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, codes::KEY_NOT_PINNED));
    }
    if !(request.tip.entry_hash.len() == 64
        && request
            .tip
            .entry_hash
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')))
    {
        return Err(ApiError::unprocessable("invalid_entry_hash"));
    }
    let seq = i64::try_from(request.tip.seq)
        .map_err(|_| ApiError::unprocessable("value_out_of_range"))?;
    let tip_window = i64::try_from(request.tip.window_start_ms).unwrap_or(i64::MAX);

    let classified = classify(&request, now_ms, policy);
    ensure_partitions(&state, &months_needed(&classified, now_ms)).await?;

    let mut tx = state.pool.begin().await?;
    // One heartbeat at a time per device: makes the (device, key_id, seq)
    // check-then-insert atomic and `server_received_at` monotonic.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(&device.id)
        .execute(&mut *tx)
        .await?;
    let existing: Option<String> = sqlx::query_scalar(
        "SELECT entry_hash FROM anchors WHERE device_id = $1 AND key_id = $2 AND seq = $3",
    )
    .bind(&device.id)
    .bind(request.tip.key_id.as_str())
    .bind(seq)
    .fetch_optional(&mut *tx)
    .await?;
    let (status, received_us) = match existing {
        Some(hash) if hash != request.tip.entry_hash => {
            return Err(ApiError::new(StatusCode::CONFLICT, codes::ANCHOR_CONFLICT));
        }
        Some(_) => {
            let us: i64 = sqlx::query_scalar(
                "SELECT server_received_us FROM anchors
                 WHERE device_id = $1 AND key_id = $2 AND seq = $3",
            )
            .bind(&device.id)
            .bind(request.tip.key_id.as_str())
            .bind(seq)
            .fetch_one(&mut *tx)
            .await?;
            (AnchorStatus::Duplicate, u64::try_from(us).unwrap_or(0))
        }
        None => {
            let last: Option<i64> = sqlx::query_scalar(
                "SELECT max(server_received_us) FROM anchors WHERE device_id = $1",
            )
            .bind(&device.id)
            .fetch_one(&mut *tx)
            .await?;
            let received_us = now_us.max(u64::try_from(last.unwrap_or(0)).unwrap_or(0) + 1);
            sqlx::query(
                "INSERT INTO anchors (device_id, key_id, seq, entry_hash, window_start_ms,
                    sync_mode, sent_at_ms, server_received_us, server_received_at)
                 VALUES ($1,$2,$3,$4,$5,$6,$7,$8,
                         'epoch'::timestamptz + ($8::bigint * interval '1 microsecond'))",
            )
            .bind(&device.id)
            .bind(request.tip.key_id.as_str())
            .bind(seq)
            .bind(&request.tip.entry_hash)
            .bind(tip_window)
            .bind(match request.sync_mode {
                traction_core::sync::SyncMode::Realtime => "realtime",
                traction_core::sync::SyncMode::OfflineBuffered => "offline_buffered",
            })
            .bind(i64::try_from(request.sent_at_ms).unwrap_or(i64::MAX))
            .bind(i64::try_from(received_us).unwrap_or(i64::MAX))
            .execute(&mut *tx)
            .await?;
            (AnchorStatus::Stored, received_us)
        }
    };

    let mut accepted = Vec::new();
    for bucket in &classified.buckets {
        store::insert_bucket(&mut tx, &device.user_id, &device.id, bucket)
            .await
            .map_err(store_error)?;
        accepted.push(ItemRef {
            kind: ItemKind::WorkBucket,
            start_ms: bucket.minute_start_ms,
            project_id: Some(bucket.project_id.clone()),
        });
    }
    for rollup in &classified.rollups {
        store::insert_rollup(&mut tx, &device.user_id, &device.id, rollup)
            .await
            .map_err(store_error)?;
        accepted.push(ItemRef {
            kind: ItemKind::ActivityRollup,
            start_ms: rollup.window_start_ms,
            project_id: None,
        });
    }
    for meta in &request.projects {
        if validate_project_id(&meta.project_id).is_ok() {
            upsert_project(&mut tx, &device.user_id, meta, now_ms).await?;
        }
    }
    tx.commit().await?;

    if !classified.buckets.is_empty() {
        let totals = user_totals(&state, &device.user_id).await?;
        state
            .fanout
            .publish(&device.user_id, LiveEvent::Totals(totals));
    }
    Ok(Json(HeartbeatResponse {
        anchor: status,
        server_received_at_ms: received_us / 1000,
        accepted,
        rejected: classified.rejected,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use traction_core::sync::{ChainTip, SyncMode};
    use traction_core::{DeviceSigningKey, StruggleAxes, TelemetryTier};
    use zeroize::Zeroizing;

    const NOW: u64 = 1_795_694_400_000; // 2026-11-26T12:00:00Z, window aligned
    const WINDOW: u64 = 120_000;

    fn bucket(minute: u64) -> WorkBucket {
        WorkBucket {
            project_id: format!("dir:{}", "a".repeat(64)),
            minute_start_ms: minute,
            ruleset_id: "traction-rules/1".into(),
            agent_compute_sec: 10,
            idle_autonomous_sec: 0,
            human_active_sec: 30,
            permission_wait_ms: 0,
            commits_human: 0,
            commits_agent: 0,
            injected_hid_detected: false,
            auto_approver_suspected: false,
            telemetry_tier: TelemetryTier::Full,
            tier_limited: false,
            struggle_axes: StruggleAxes::default(),
            verified_struggle: 0,
            struggle_credit_sec: 10,
        }
    }

    fn request(tip_window: u64, buckets: Vec<WorkBucket>) -> HeartbeatRequest {
        let key = DeviceSigningKey::from_seed(Zeroizing::new([1u8; 32])).unwrap();
        HeartbeatRequest {
            tip: ChainTip {
                key_id: key.key_id(),
                seq: 0,
                entry_hash: "0".repeat(64),
                window_start_ms: tip_window,
            },
            sync_mode: SyncMode::Realtime,
            sent_at_ms: NOW,
            work_buckets: buckets,
            activity_rollups: vec![],
            projects: vec![],
        }
    }

    #[test]
    fn the_tip_window_is_checked_against_the_future_and_the_backlog_age() {
        let policy = AnchorPolicy::default();
        assert_eq!(check_tip(NOW, NOW, &policy), Ok(()));
        // Exactly MAX_FUTURE_SKEW ahead is allowed; one window more is not.
        assert_eq!(check_tip(NOW + WINDOW, NOW, &policy), Ok(()));
        assert_eq!(
            check_tip(NOW + 2 * WINDOW, NOW, &policy),
            Err(TipError::FutureWindow)
        );
        let age = policy.max_backlog_age_ms();
        assert_eq!(check_tip(NOW - age, NOW, &policy), Ok(()));
        assert_eq!(
            check_tip(NOW - age - WINDOW, NOW, &policy),
            Err(TipError::WindowTooOld)
        );
        assert_eq!(check_tip(NOW + 1, NOW, &policy), Err(TipError::Misaligned));
    }

    #[test]
    fn items_are_accepted_or_rejected_with_value_free_codes() {
        let policy = AnchorPolicy::default();
        let mut bad_human = bucket(NOW);
        bad_human.human_active_sec = 61;
        let mut bad_project = bucket(NOW);
        bad_project.project_id = "nope".into();
        let after_tip = bucket(NOW + WINDOW);
        let old = bucket(NOW - policy.max_backlog_age_ms() - 10 * WINDOW);
        let request = request(
            NOW,
            vec![bucket(NOW), bad_human, bad_project, after_tip, old],
        );
        let result = classify(&request, NOW, &policy);
        assert_eq!(result.buckets.len(), 1);
        let codes: Vec<&str> = result.rejected.iter().map(|r| r.code.as_str()).collect();
        assert_eq!(
            codes,
            [
                "human_active_over_minute",
                "invalid_project_id",
                "window_after_tip",
                "window_too_old"
            ]
        );
    }

    #[test]
    fn partitions_are_requested_for_every_month_a_heartbeat_touches() {
        let policy = AnchorPolicy::default();
        let nov_1 = 1_793_491_200_000u64;
        let request = request(nov_1 + WINDOW, vec![bucket(nov_1 - 60_000), bucket(nov_1)]);
        let result = classify(&request, nov_1 + WINDOW, &policy);
        let months = months_needed(&result, nov_1 + WINDOW);
        assert_eq!(
            months.iter().map(|m| (m.year, m.month)).collect::<Vec<_>>(),
            [(2026, 10), (2026, 11)]
        );
    }
}
