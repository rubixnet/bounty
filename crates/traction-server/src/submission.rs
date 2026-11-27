//! The submission path (spec S3a, Q3, Q14, Q15):
//!
//! 1. `POST /v1/bundles` returns `{bundle_id, put_url}` (presigned for the
//!    exact length, 15 minutes, 64 MB cap).
//! 2. The CLI `PUT`s the bundle to the blob store (for the local store, to
//!    `PUT /v1/blobs/{key}` on this server).
//! 3. `POST /v1/bundles/{id}/complete` verifies the bundle **deterministically
//!    first**, then asks the `DecisionEngine`:
//!    - `verify_bundle` runs unchanged with the device's pinned keys
//!      (historical ones included), original `project_id`s kept;
//!    - every stored Anchor of the device must appear among the bundle's
//!      links with the same `entry_hash` (a cut tail or a rewrite is Red);
//!    - the bundle's rows must equal, as structs, the buckets synced live;
//!    - windows first anchored late carry `late_anchor` into the input.
//!
//!    Any deterministic failure is stored as **Red** before any engine runs
//!    and no engine can upgrade it. The submitter is told only `submitted`;
//!    reviewers read the stored decision.
//!
//! Rows are compared only below the newest Anchor's window, so a daemon that
//! sealed (but has not yet synced) newer windows between export and submit
//! is not mistaken for tampering; those links are reported as
//! `unanchored_tail_links` (Yellow). Rows older than `MAX_BACKLOG_AGE` were
//! terminally rejected at sync and are protected by the chain and Anchors
//! instead, so they are not compared either.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use traction_core::sync::{
    BundleCompleteResponse, BundleStartRequest, BundleStartResponse, validate_project_id,
};
use traction_core::{
    ActivityRollup, AuditBundle, BundlePolicy, TRACTION_RULES_1, VerifyingKeyHex, WorkBucket,
    verify_bundle,
};

use crate::app::{AppState, Device, authenticate};
use crate::blob::BlobError;
use crate::decision::{AnchorFlags, SLOT_MS, Slot, TriageInput, VerificationSummary, decide};
use crate::error::ApiError;
use crate::merge::{ShareRow, merge_rows};
use crate::store;
use crate::token::random_hex;

/// Pending (unexpired) upload URLs one device may hold at once.
pub const MAX_PENDING_BUNDLES: i64 = 10;

fn blob_key(bundle_id: &str) -> String {
    format!("bundles/{bundle_id}.json")
}

/// `POST /v1/bundles`.
pub async fn start(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<BundleStartRequest>,
) -> Result<Json<BundleStartResponse>, ApiError> {
    let device = authenticate(&state, &headers).await?;
    validate_project_id(&request.project_id)
        .map_err(|_| ApiError::unprocessable("invalid_project_id"))?;
    let now_ms = state.now_ms();
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM bundles
         WHERE device_id = $1 AND status = 'pending' AND expires_at_ms > $2",
    )
    .bind(&device.id)
    .bind(i64::try_from(now_ms).unwrap_or(i64::MAX))
    .fetch_one(&state.pool)
    .await?;
    if pending >= MAX_PENDING_BUNDLES {
        return Err(ApiError::too_many(60));
    }
    let bundle_id = random_hex(16);
    let key = blob_key(&bundle_id);
    let target = state
        .blobs
        .presign_put(&key, request.size_bytes, now_ms)
        .map_err(|e| match e {
            BlobError::BadLength => ApiError::unprocessable("bad_length"),
            _ => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "blob_store"),
        })?;
    sqlx::query(
        "INSERT INTO bundles (id, user_id, device_id, project_id, blob_key, size_bytes,
            created_at_ms, expires_at_ms)
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
    )
    .bind(&bundle_id)
    .bind(&device.user_id)
    .bind(&device.id)
    .bind(&request.project_id)
    .bind(&key)
    .bind(i64::try_from(request.size_bytes).unwrap_or(i64::MAX))
    .bind(i64::try_from(now_ms).unwrap_or(i64::MAX))
    .bind(i64::try_from(target.expires_at_ms).unwrap_or(i64::MAX))
    .execute(&state.pool)
    .await?;
    Ok(Json(BundleStartResponse {
        bundle_id,
        put_url: target.url,
        expires_at_ms: target.expires_at_ms,
    }))
}

#[derive(Deserialize)]
pub struct LocalPutQuery {
    exp: u64,
    len: u64,
    sig: String,
}

/// `PUT /v1/blobs/{key}`: the local store's upload endpoint. The signature,
/// the expiry and the exact `Content-Length` are checked before any write.
pub async fn local_put(
    State(state): State<Arc<AppState>>,
    Path(key): Path<String>,
    Query(query): Query<LocalPutQuery>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    let local = state
        .blobs
        .as_local()
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_found"))?;
    local
        .accept_put(
            &key,
            query.exp,
            query.len,
            &query.sig,
            state.now_ms(),
            &body,
        )
        .map_err(|e| match e {
            BlobError::BadSignature | BlobError::BadKey => ApiError::unauthorized(),
            BlobError::Expired => ApiError::new(StatusCode::FORBIDDEN, "expired"),
            BlobError::BadLength | BlobError::LengthMismatch => {
                ApiError::new(StatusCode::BAD_REQUEST, "length_mismatch")
            }
            BlobError::Io(_) => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "blob_store"),
        })?;
    Ok(StatusCode::OK)
}

/// `POST /v1/bundles/{id}/complete`.
pub async fn complete(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(bundle_id): Path<String>,
) -> Result<Json<BundleCompleteResponse>, ApiError> {
    let device = authenticate(&state, &headers).await?;
    let row: Option<(String, String, String, i64)> = sqlx::query_as(
        "SELECT project_id, blob_key, status, size_bytes FROM bundles
         WHERE id = $1 AND device_id = $2",
    )
    .bind(&bundle_id)
    .bind(&device.id)
    .fetch_optional(&state.pool)
    .await?;
    let Some((project_id, key, status, size)) = row else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found"));
    };
    let done = || {
        Json(BundleCompleteResponse {
            bundle_id: bundle_id.clone(),
            status: "submitted".into(),
        })
    };
    if status == "submitted" {
        return Ok(done());
    }
    let bytes = state
        .blobs
        .get(&key)
        .await
        .map_err(|_| ApiError::new(StatusCode::BAD_GATEWAY, "blob_store"))?
        .ok_or_else(|| ApiError::new(StatusCode::CONFLICT, "not_uploaded"))?;
    if i64::try_from(bytes.len()).ok() != Some(size) {
        return Err(ApiError::new(StatusCode::CONFLICT, "size_mismatch"));
    }

    let input = evaluate(&state, &device, &project_id, &bytes).await?;
    let triage = decide(state.engine.as_ref(), &input).await;
    let input_json = serde_json::to_value(&input)
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal"))?;
    let now_ms = i64::try_from(state.now_ms()).unwrap_or(i64::MAX);

    let mut tx = state.pool.begin().await?;
    let moved = sqlx::query(
        "UPDATE bundles SET status = 'submitted', completed_at_ms = $2
         WHERE id = $1 AND status = 'pending'",
    )
    .bind(&bundle_id)
    .bind(now_ms)
    .execute(&mut *tx)
    .await?;
    if moved.rows_affected() == 1 {
        sqlx::query(
            "INSERT INTO triage_decisions (bundle_id, verdict, reasons, engine_id,
                rubric_version, failure, input, decided_at_ms)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8)",
        )
        .bind(&bundle_id)
        .bind(triage.verdict.as_str())
        .bind(&triage.reasons)
        .bind(&triage.engine_id)
        .bind(&triage.rubric_version)
        .bind(&input.verification.failure)
        .bind(input_json)
        .bind(now_ms)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(done())
}

fn failed(code: &str, submission_ref: String) -> TriageInput {
    TriageInput {
        submission_ref,
        verification: VerificationSummary {
            failure: Some(code.to_owned()),
            ..VerificationSummary::default()
        },
        flags: AnchorFlags::default(),
        slots: Vec::new(),
    }
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn sat32(v: u64) -> u32 {
    u32::try_from(v).unwrap_or(u32::MAX)
}

/// Verifies the bundle against the stored Anchors and the live rows and
/// builds the anonymized [`TriageInput`]. A deterministic failure is a
/// `Some(failure)` in the input, never an `Err`.
pub async fn evaluate(
    state: &AppState,
    device: &Device,
    project_id: &str,
    bytes: &[u8],
) -> Result<TriageInput, ApiError> {
    let submission_ref = random_hex(8);
    let width = TRACTION_RULES_1.activity_rollup_ms;

    // 1. The chain, the manifest and the rows' digests: unchanged core code,
    //    pinned to every key the device ever had.
    let key_texts: Vec<String> = sqlx::query_scalar(
        "SELECT public_key FROM device_keys WHERE device_id = $1 ORDER BY pinned_at, key_id",
    )
    .bind(&device.id)
    .fetch_all(&state.pool)
    .await?;
    let keys: Vec<VerifyingKeyHex> = key_texts
        .iter()
        .filter_map(|text| VerifyingKeyHex::parse(text).ok())
        .collect();
    let policy = BundlePolicy {
        trusted_keys: Some(keys),
        expected_tip: None,
    };
    let report = match verify_bundle(bytes, &policy) {
        Ok(report) => report,
        Err(error) => return Ok(failed(error.code(), submission_ref)),
    };
    let Ok(bundle) = serde_json::from_slice::<AuditBundle>(bytes) else {
        return Ok(failed("malformed_bundle", submission_ref));
    };
    if bundle.manifest.body.project_id != project_id {
        return Ok(failed("project_mismatch", submission_ref));
    }

    // 2. Every stored Anchor must be a link of the bundle.
    let anchors: Vec<(String, i64, String, i64)> = sqlx::query_as(
        "SELECT key_id, seq, entry_hash, window_start_ms FROM anchors WHERE device_id = $1",
    )
    .bind(&device.id)
    .fetch_all(&state.pool)
    .await?;
    if anchors.is_empty() {
        return Ok(failed("no_anchors", submission_ref));
    }
    let mut links: HashMap<(String, u64), &str> = HashMap::new();
    for chain in &bundle.chains {
        for link in &chain.links {
            links.insert(
                (link.header.key_id.as_str().to_owned(), link.header.seq),
                link.entry_hash.as_str(),
            );
        }
    }
    let mut boundary: u64 = 0;
    for (key_id, seq, entry_hash, window) in &anchors {
        let found = links.get(&(key_id.clone(), u64::try_from(*seq).unwrap_or(u64::MAX)));
        match found {
            None => return Ok(failed("anchor_missing", submission_ref)),
            Some(hash) if *hash != entry_hash => {
                return Ok(failed("anchor_mismatch", submission_ref));
            }
            Some(_) => {}
        }
        boundary = boundary.max(u64::try_from(*window).unwrap_or(0) + width);
    }
    let unanchored_tail_links = bundle
        .chains
        .iter()
        .flat_map(|c| &c.links)
        .filter(|l| l.header.window_start_ms + width > boundary)
        .count();

    // 3. The bundle's rows equal the rows synced live (as structs).
    let now_ms = state.now_ms();
    let cutoff = now_ms.saturating_sub(state.config.anchor_policy.max_backlog_age_ms());
    let comparable = |minute: u64| minute >= cutoff && minute < boundary;
    let mut conn = state.pool.acquire().await?;
    let live_buckets: Vec<WorkBucket> = store::fetch_buckets(&mut conn, &device.id)
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal"))?
        .into_iter()
        .filter(|b| b.project_id == project_id)
        .collect();
    let live_rollups: Vec<ActivityRollup> = store::fetch_rollups(&mut conn, &device.id)
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal"))?;
    drop(conn);
    let key_of = |b: &WorkBucket| (b.minute_start_ms, b.ruleset_id.clone());
    let mut bundle_buckets: Vec<WorkBucket> = bundle
        .project_rows
        .work_buckets
        .iter()
        .filter(|b| comparable(b.minute_start_ms))
        .cloned()
        .collect();
    bundle_buckets.sort_by_key(key_of);
    let mut live_compared: Vec<WorkBucket> = live_buckets
        .iter()
        .filter(|b| comparable(b.minute_start_ms))
        .cloned()
        .collect();
    live_compared.sort_by_key(key_of);
    let mut bundle_rollups: Vec<ActivityRollup> = bundle
        .device_rows
        .activity_rollups
        .iter()
        .filter(|r| comparable(r.window_start_ms))
        .cloned()
        .collect();
    bundle_rollups.sort_by_key(|r| (r.window_start_ms, r.ruleset_id.clone()));
    let mut live_rollups_compared: Vec<ActivityRollup> = live_rollups
        .into_iter()
        .filter(|r| comparable(r.window_start_ms))
        .collect();
    live_rollups_compared.sort_by_key(|r| (r.window_start_ms, r.ruleset_id.clone()));
    if bundle_buckets != live_compared || bundle_rollups != live_rollups_compared {
        return Ok(failed("rows_mismatch", submission_ref));
    }

    // 4. Timing flags (set when each window was first anchored).
    let timing: Vec<(i64, bool, bool)> = sqlx::query_as(
        "SELECT minute_start_ms / $3 AS window, bool_or(late_anchor), bool_or(offline_buffered)
         FROM work_buckets WHERE device_id = $1 AND project_id = $2 GROUP BY 1",
    )
    .bind(&device.id)
    .bind(project_id)
    .bind(to_i64(width))
    .fetch_all(&state.pool)
    .await?;
    let mut flags = AnchorFlags {
        unanchored_tail_links: sat32(unanchored_tail_links as u64),
        ..AnchorFlags::default()
    };
    for (_, late, offline) in &timing {
        if *offline {
            flags.offline_buffered_windows += 1;
        } else {
            flags.realtime_windows += 1;
            if *late {
                flags.late_anchor_windows += 1;
            }
        }
    }
    flags.clock_skew = sqlx::query_scalar("SELECT clock_skew FROM devices WHERE id = $1")
        .bind(&device.id)
        .fetch_one(&state.pool)
        .await?;

    // 5. Other devices of the user in the Project's active minutes, merged
    //    under the 60-second cap.
    let shares: Vec<ShareRow> = sqlx::query_as(
        "SELECT device_id, project_id, minute_start_ms, human_active_sec::bigint,
                struggle_credit_sec::bigint
         FROM work_buckets
         WHERE user_id = $1 AND minute_start_ms IN (
             SELECT minute_start_ms FROM work_buckets
             WHERE device_id = $2 AND project_id = $3 AND human_active_sec > 0)",
    )
    .bind(&device.user_id)
    .bind(&device.id)
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    let merged = merge_rows(&shares);
    let mut merged_minute: HashMap<u64, (u32, u32)> = HashMap::new();
    for (device_id, project, minute, human, _) in &shares {
        if *device_id == device.id && project == project_id && *human > 0 {
            let minute = u64::try_from(*minute).unwrap_or(0);
            flags.active_minutes += 1;
            let m = merged[&(device_id.clone(), project.clone(), to_i64(minute))];
            if m.multi_device_overlap {
                flags.multi_device_overlap_minutes += 1;
            }
            merged_minute.insert(minute, (m.human_active_sec, m.struggle_credit_sec));
        }
    }

    // 6. 15-minute slots: buckets (merged figures), unsigned marks, commits
    //    and rollups.
    let mut slots: BTreeMap<u64, Slot> = BTreeMap::new();
    let slot_of = |ms: u64| ms / SLOT_MS * SLOT_MS;
    for b in &bundle.project_rows.work_buckets {
        let (human, credit) = merged_minute
            .get(&b.minute_start_ms)
            .copied()
            .unwrap_or((b.human_active_sec, b.struggle_credit_sec));
        let start = slot_of(b.minute_start_ms);
        let slot = slots.entry(start).or_insert_with(|| Slot {
            slot_start_ms: start,
            ..Slot::default()
        });
        slot.minutes += 1;
        slot.human_active_sec += human;
        slot.agent_compute_sec += b.agent_compute_sec;
        slot.struggle_credit_sec += credit;
        if b.injected_hid_detected {
            slot.injected_human_active_sec += human;
        }
        if b.tier_limited {
            slot.tier_limited_minutes += 1;
        }
        slot.commits_human += b.commits_human;
        slot.commits_agent += b.commits_agent;
    }
    let marks: Vec<(i64, i64, i64, i64, i64)> = sqlx::query_as(
        "SELECT minute_start_ms, mutations_manual, mutations_harness, turns_started,
                tool_failures
         FROM timeline_marks WHERE device_id = $1 AND project_id = $2",
    )
    .bind(&device.id)
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    for (minute, manual, harness, turns, failures) in marks {
        if let Some(slot) = slots.get_mut(&slot_of(u64::try_from(minute).unwrap_or(0))) {
            slot.mutations_manual = slot.mutations_manual.saturating_add(sat32(manual as u64));
            slot.mutations_harness = slot.mutations_harness.saturating_add(sat32(harness as u64));
            slot.turns_started = slot.turns_started.saturating_add(sat32(turns as u64));
            slot.tool_failures = slot.tool_failures.saturating_add(sat32(failures as u64));
        }
    }
    let commits: Vec<i64> = sqlx::query_scalar(
        "SELECT committed_at_ms FROM project_commits WHERE user_id = $1 AND project_id = $2",
    )
    .bind(&device.user_id)
    .bind(project_id)
    .fetch_all(&state.pool)
    .await?;
    for at in commits {
        if let Some(slot) = slots.get_mut(&slot_of(u64::try_from(at).unwrap_or(0))) {
            slot.commit_oids += 1;
        }
    }
    for r in &bundle.device_rows.activity_rollups {
        if let Some(slot) = slots.get_mut(&slot_of(r.window_start_ms)) {
            slot.rollup_windows += 1;
            slot.switch_rate_milli_sum += u64::from(r.switch_rate_milli);
        }
    }

    Ok(TriageInput {
        submission_ref,
        verification: VerificationSummary {
            failure: None,
            chains: sat32(report.chains.len() as u64),
            links: sat32(report.chains.iter().map(|c| c.links as u64).sum()),
            key_pinned: report.key_pinned,
            key_discontinuities: sat32(report.key_discontinuities.len() as u64),
            clock_regressions: sat32(report.clock_regressions as u64),
        },
        flags,
        slots: slots.into_values().collect(),
    })
}

/// The blob keys of every bundle of `user_id` (for `DELETE /v1/me`).
pub async fn blob_keys_of_user(
    pool: &sqlx::PgPool,
    user_id: &str,
) -> Result<Vec<String>, sqlx::Error> {
    sqlx::query_scalar("SELECT blob_key FROM bundles WHERE user_id = $1")
        .bind(user_id)
        .fetch_all(pool)
        .await
}
