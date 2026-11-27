//! Read APIs and account deletion (spec S2, Q19/Q20).
//!
//! - `GET /v1/me`: the caller, its config roles and its devices (with the
//!   `clock_skew` flag).
//! - `DELETE /v1/me`: hard-deletes every row the user owns.
//! - `GET /v1/users/{user_id}/projects`: the Project list with merged
//!   totals. Anyone gets `project_name` and totals; the owner, reviewers and
//!   admins also get paths, `os_username`, the Project ID and `linked_repo`.
//! - `GET /v1/users/{user_id}/projects/{project_id}/timeline`: one Project's
//!   Work Buckets, Activity Rollups, commit oids and Timeline Marks lined up
//!   per minute (the Audit Strip's data source). Owner, reviewers and admins
//!   only. Every mark is labelled `unsigned`: context, never proof.
//!
//! Human Active Time and Struggle Credit are always the merged figures
//! (`crate::merge`); stored rows are untouched.
//!
//! Reviewer and admin access today needs a device token of a user whose `sub`
//! is listed under `[roles]`; browser sessions arrive with the website (5a).

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use traction_core::MINUTE_MS;

use crate::app::{AppState, authenticate};
use crate::error::ApiError;
use crate::merge::{ShareRow, merge_rows, project_totals};

/// The longest timeline range one request may ask for.
pub const MAX_TIMELINE_SPAN_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// An authenticated caller and the roles the config grants it.
struct Viewer {
    user_id: String,
    reviewer: bool,
    admin: bool,
}

impl Viewer {
    /// The owner always sees their own data; reviewers and admins see
    /// everyone's private Project fields.
    fn sees_private_of(&self, owner: &str) -> bool {
        self.user_id == owner || self.reviewer || self.admin
    }
}

async fn viewer(state: &AppState, headers: &HeaderMap) -> Result<Viewer, ApiError> {
    let device = authenticate(state, headers).await?;
    let sub: String = sqlx::query_scalar("SELECT sub FROM users WHERE id = $1")
        .bind(&device.user_id)
        .fetch_one(&state.pool)
        .await?;
    let roles = &state.config.roles;
    Ok(Viewer {
        user_id: device.user_id,
        reviewer: roles.reviewers.contains(&sub),
        admin: roles.admins.contains(&sub),
    })
}

pub async fn get_me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let caller = viewer(&state, &headers).await?;
    let display_name: String = sqlx::query_scalar("SELECT display_name FROM users WHERE id = $1")
        .bind(&caller.user_id)
        .fetch_one(&state.pool)
        .await?;
    let devices: Vec<(String, String, bool, Option<i64>, bool)> = sqlx::query_as(
        "SELECT id, label, revoked_at IS NOT NULL, skew_ms, clock_skew
         FROM devices WHERE user_id = $1 ORDER BY created_at, id",
    )
    .bind(&caller.user_id)
    .fetch_all(&state.pool)
    .await?;
    let mut roles = Vec::new();
    if caller.reviewer {
        roles.push("reviewer");
    }
    if caller.admin {
        roles.push("admin");
    }
    Ok(Json(json!({
        "user_id": caller.user_id,
        "display_name": display_name,
        "roles": roles,
        "devices": devices.into_iter().map(|(id, label, revoked, skew_ms, clock_skew)| json!({
            "device_id": id,
            "label": label,
            "revoked": revoked,
            "skew_ms": skew_ms,
            "clock_skew": clock_skew,
        })).collect::<Vec<_>>(),
    })))
}

/// `DELETE /v1/me`: removes every row the user owns (devices, keys, buckets,
/// rollups, Anchors, marks, commits, Projects, the account). R2 objects join
/// this in S3a, when `BlobStore` exists.
pub async fn delete_me(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    let caller = authenticate(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    for statement in [
        "DELETE FROM anchors WHERE device_id IN (SELECT id FROM devices WHERE user_id = $1)",
        "DELETE FROM work_buckets WHERE user_id = $1",
        "DELETE FROM activity_rollups WHERE user_id = $1",
        "DELETE FROM timeline_marks WHERE user_id = $1",
        "DELETE FROM project_commits WHERE user_id = $1",
        "DELETE FROM projects WHERE user_id = $1",
        // Cascades to devices, device_keys and pairings.
        "DELETE FROM users WHERE id = $1",
    ] {
        sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(&caller.user_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(sqlx::FromRow)]
struct ProjectRow {
    project_id: String,
    project_name: String,
    root_path: String,
    home_relative_path: Option<String>,
    os_username: String,
    linked_repo: Option<String>,
}

/// A caller is optional here: no `Authorization` header is the public view,
/// but a bad token is still `401`.
async fn optional_viewer(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<Option<Viewer>, ApiError> {
    if headers.get(AUTHORIZATION).is_none() {
        return Ok(None);
    }
    viewer(state, headers).await.map(Some)
}

pub async fn list_projects(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(user_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let caller = optional_viewer(&state, &headers).await?;
    let private = caller.as_ref().is_some_and(|c| c.sees_private_of(&user_id));
    let rows: Vec<ProjectRow> = sqlx::query_as(
        "SELECT project_id, project_name, root_path, home_relative_path, os_username, linked_repo
         FROM projects WHERE user_id = $1 ORDER BY project_name, project_id",
    )
    .bind(&user_id)
    .fetch_all(&state.pool)
    .await?;
    let totals = project_totals(&state.pool, &user_id).await?;
    let projects: Vec<Value> = rows
        .into_iter()
        .map(|row| {
            let t = totals.get(&row.project_id).copied().unwrap_or_default();
            let mut project = json!({
                "project_name": row.project_name,
                "human_active_sec": t.human_active_sec,
                "struggle_credit_sec": t.struggle_credit_sec,
                "agent_compute_sec": t.agent_compute_sec,
            });
            if private && let Some(object) = project.as_object_mut() {
                object.insert("project_id".into(), json!(row.project_id));
                object.insert("project_root_path".into(), json!(row.root_path));
                object.insert(
                    "project_home_relative_path".into(),
                    json!(row.home_relative_path),
                );
                object.insert("os_username".into(), json!(row.os_username));
                object.insert("linked_repo".into(), json!(row.linked_repo));
            }
            project
        })
        .collect();
    Ok(Json(json!({
        "visibility": if private { "private" } else { "public" },
        "projects": projects,
    })))
}

#[derive(Deserialize)]
pub struct RangeQuery {
    from_ms: u64,
    to_ms: u64,
}

#[derive(sqlx::FromRow)]
struct BucketDetail {
    device_id: String,
    minute_start_ms: i64,
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
    verified_struggle: i64,
    struggle_credit_sec: i64,
    late_anchor: bool,
    offline_buffered: bool,
}

#[derive(sqlx::FromRow)]
struct RollupDetail {
    device_id: String,
    window_start_ms: i64,
    minutes_observed: i64,
    share_ide: i64,
    share_terminal: i64,
    share_browser: i64,
    share_ai_desktop: i64,
    share_other: i64,
    switch_rate_milli: i64,
    telemetry_tier: String,
}

#[derive(sqlx::FromRow)]
struct MarkDetail {
    device_id: String,
    minute_start_ms: i64,
    mutations_manual: i64,
    mutations_harness: i64,
    turns_started: i64,
    tool_failures: i64,
    harnesses: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct CommitDetail {
    oid: String,
    authored_at_ms: i64,
    committed_at_ms: i64,
    author_match: bool,
}

#[derive(Default)]
struct MinuteEntry {
    buckets: Vec<Value>,
    rollups: Vec<Value>,
    commits: Vec<Value>,
    marks: Vec<Value>,
    multi_device_overlap: bool,
    late_anchor: bool,
    offline_buffered: bool,
}

fn minute_of(ms: i64) -> i64 {
    let minute = i64::try_from(MINUTE_MS).unwrap_or(60_000);
    ms.div_euclid(minute) * minute
}

pub async fn project_timeline(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path((user_id, project_id)): Path<(String, String)>,
    Query(range): Query<RangeQuery>,
) -> Result<Json<Value>, ApiError> {
    let caller = viewer(&state, &headers).await?;
    if !caller.sees_private_of(&user_id) {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "forbidden"));
    }
    if range.from_ms >= range.to_ms || range.to_ms - range.from_ms > MAX_TIMELINE_SPAN_MS {
        return Err(ApiError::unprocessable("bad_range"));
    }
    let (from, to) = (
        i64::try_from(range.from_ms).map_err(|_| ApiError::unprocessable("bad_range"))?,
        i64::try_from(range.to_ms).map_err(|_| ApiError::unprocessable("bad_range"))?,
    );
    let from = minute_of(from);

    // Merge over every Project of the user: the cap is per user and minute.
    let shares: Vec<ShareRow> = sqlx::query_as(
        "SELECT device_id, project_id, minute_start_ms, human_active_sec::bigint,
                struggle_credit_sec::bigint
         FROM work_buckets
         WHERE user_id = $1 AND minute_start_ms >= $2 AND minute_start_ms < $3",
    )
    .bind(&user_id)
    .bind(from)
    .bind(to)
    .fetch_all(&state.pool)
    .await?;
    let merged = merge_rows(&shares);

    let buckets: Vec<BucketDetail> = sqlx::query_as(
        "SELECT device_id, minute_start_ms, agent_compute_sec, idle_autonomous_sec,
                human_active_sec, permission_wait_ms, commits_human, commits_agent,
                injected_hid_detected, auto_approver_suspected, telemetry_tier, tier_limited,
                verified_struggle, struggle_credit_sec, late_anchor, offline_buffered
         FROM work_buckets
         WHERE user_id = $1 AND project_id = $2 AND minute_start_ms >= $3 AND minute_start_ms < $4
         ORDER BY minute_start_ms, device_id",
    )
    .bind(&user_id)
    .bind(&project_id)
    .bind(from)
    .bind(to)
    .fetch_all(&state.pool)
    .await?;
    let marks: Vec<MarkDetail> = sqlx::query_as(
        "SELECT device_id, minute_start_ms, mutations_manual, mutations_harness, turns_started,
                tool_failures, harnesses
         FROM timeline_marks
         WHERE user_id = $1 AND project_id = $2 AND minute_start_ms >= $3 AND minute_start_ms < $4
         ORDER BY minute_start_ms, device_id",
    )
    .bind(&user_id)
    .bind(&project_id)
    .bind(from)
    .bind(to)
    .fetch_all(&state.pool)
    .await?;
    let commits: Vec<CommitDetail> = sqlx::query_as(
        "SELECT oid, authored_at_ms, committed_at_ms, author_match FROM project_commits
         WHERE user_id = $1 AND project_id = $2 AND committed_at_ms >= $3 AND committed_at_ms < $4
         ORDER BY committed_at_ms, oid",
    )
    .bind(&user_id)
    .bind(&project_id)
    .bind(from)
    .bind(to)
    .fetch_all(&state.pool)
    .await?;
    let rollups: Vec<RollupDetail> = sqlx::query_as(
        "SELECT device_id, window_start_ms, minutes_observed, share_ide, share_terminal,
                share_browser, share_ai_desktop, share_other, switch_rate_milli, telemetry_tier
         FROM activity_rollups
         WHERE user_id = $1 AND window_start_ms >= $2 AND window_start_ms < $3
         ORDER BY window_start_ms, device_id",
    )
    .bind(&user_id)
    .bind(from - 2 * i64::try_from(MINUTE_MS).unwrap_or(60_000))
    .bind(to)
    .fetch_all(&state.pool)
    .await?;

    let mut minutes: BTreeMap<i64, MinuteEntry> = BTreeMap::new();
    for b in buckets {
        let key = (b.device_id.clone(), project_id.clone(), b.minute_start_ms);
        let merged_share = merged.get(&key).copied();
        let entry = minutes.entry(b.minute_start_ms).or_default();
        let (human, credit, overlap) =
            merged_share.map_or((b.human_active_sec, b.struggle_credit_sec, false), |m| {
                (
                    i64::from(m.human_active_sec),
                    i64::from(m.struggle_credit_sec),
                    m.multi_device_overlap,
                )
            });
        entry.multi_device_overlap |= overlap;
        entry.late_anchor |= b.late_anchor;
        entry.offline_buffered |= b.offline_buffered;
        entry.buckets.push(json!({
            "device_id": b.device_id,
            "human_active_sec": human,
            "struggle_credit_sec": credit,
            "human_active_sec_stored": b.human_active_sec,
            "agent_compute_sec": b.agent_compute_sec,
            "idle_autonomous_sec": b.idle_autonomous_sec,
            "permission_wait_ms": b.permission_wait_ms,
            "commits_human": b.commits_human,
            "commits_agent": b.commits_agent,
            "injected_hid_detected": b.injected_hid_detected,
            "auto_approver_suspected": b.auto_approver_suspected,
            "telemetry_tier": b.telemetry_tier,
            "tier_limited": b.tier_limited,
            "verified_struggle": b.verified_struggle,
            "late_anchor": b.late_anchor,
            "offline_buffered": b.offline_buffered,
        }));
    }
    for m in marks {
        minutes
            .entry(m.minute_start_ms)
            .or_default()
            .marks
            .push(json!({
                "device_id": m.device_id,
                "integrity": "unsigned",
                "mutations_manual": m.mutations_manual,
                "mutations_harness": m.mutations_harness,
                "turns_started": m.turns_started,
                "tool_failures": m.tool_failures,
                "harnesses": m.harnesses,
            }));
    }
    for c in commits {
        minutes
            .entry(minute_of(c.committed_at_ms))
            .or_default()
            .commits
            .push(json!({
                "oid": c.oid,
                "authored_at_ms": c.authored_at_ms,
                "committed_at_ms": c.committed_at_ms,
                "author_match": c.author_match,
            }));
    }
    // A rollup (device x 120 s window) attaches to the minutes of its window
    // that already have Project data.
    for r in rollups {
        let value = json!({
            "device_id": r.device_id,
            "window_start_ms": r.window_start_ms,
            "minutes_observed": r.minutes_observed,
            "category_share_milli": {
                "ide": r.share_ide, "terminal": r.share_terminal, "browser": r.share_browser,
                "ai_desktop": r.share_ai_desktop, "other": r.share_other,
            },
            "switch_rate_milli": r.switch_rate_milli,
            "telemetry_tier": r.telemetry_tier,
        });
        for minute in [r.window_start_ms, r.window_start_ms + 60_000] {
            if let Some(entry) = minutes.get_mut(&minute) {
                entry.rollups.push(value.clone());
            }
        }
    }
    let minutes: Vec<Value> = minutes
        .into_iter()
        .filter(|(minute, _)| *minute >= from && *minute < to)
        .map(|(minute, e)| {
            let mut flags = Vec::new();
            if e.multi_device_overlap {
                flags.push("multi_device_overlap");
            }
            if e.late_anchor {
                flags.push("late_anchor");
            }
            if e.offline_buffered {
                flags.push("offline_buffered");
            }
            json!({
                "minute_start_ms": minute,
                "flags": flags,
                "work_buckets": e.buckets,
                "activity_rollups": e.rollups,
                "commits": e.commits,
                "timeline_marks": e.marks,
            })
        })
        .collect();
    Ok(Json(json!({
        "project_id": project_id,
        "from_ms": from,
        "to_ms": to,
        "timeline_marks_integrity": "unsigned",
        "minutes": minutes,
    })))
}
