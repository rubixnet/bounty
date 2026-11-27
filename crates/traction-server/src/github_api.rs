//! GitHub linking endpoints (spec S3b, Q16). Linking is optional for
//! tracking (ADR 0002); Submission Eligibility is enforced in 5a, not here.
//!
//! - `POST /v1/github/link`: needs a **user-to-server token** that shows push
//!   access on the repo, and the App installed on it. The token is used for
//!   the request and never stored.
//! - `POST /v1/github/projects/{project_id}/sync`: fetches new commits with
//!   an installation token.
//! - `DELETE /v1/github/projects/{project_id}`: unlinks and forgets the
//!   fetched commits.
//!
//! Stored: `oid`, `authored_at`, `committed_at` and the author-match flag in
//! `project_commits`. No messages, no diffs.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::app::{AppState, authenticate};
use crate::error::ApiError;
use crate::github::{CommitRecord, GithubClient, GithubError, valid_repo_name};

/// A re-fetch asks for commits since the last fetch minus this overlap.
const REFETCH_OVERLAP_MS: i64 = 24 * 60 * 60 * 1000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkRequest {
    pub project_id: String,
    /// `owner/name`.
    pub repo: String,
    /// A user-to-server token from the App's OAuth flow.
    pub user_token: String,
}

fn client(state: &AppState) -> Result<Arc<GithubClient>, ApiError> {
    GithubClient::from_config(&state.config.github, |name| std::env::var(name).ok())
        .map(Arc::new)
        .map_err(map_error)
}

fn map_error(error: GithubError) -> ApiError {
    match error {
        GithubError::RateLimited { retry_after_sec } => ApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            code: "github_rate_limited",
            retry_after_sec: Some(retry_after_sec),
        },
        GithubError::NotConfigured => {
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, "github_not_configured")
        }
        GithubError::Unauthorized => ApiError::new(StatusCode::UNAUTHORIZED, "github_unauthorized"),
        GithubError::NotFound => ApiError::new(StatusCode::NOT_FOUND, "github_not_found"),
        GithubError::Http(_) | GithubError::Malformed => {
            ApiError::new(StatusCode::BAD_GATEWAY, "github_unavailable")
        }
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, GithubError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal"))?
        .map_err(map_error)
}

async fn owns_project(
    state: &AppState,
    user_id: &str,
    project_id: &str,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM projects WHERE user_id = $1 AND project_id = $2)",
    )
    .bind(user_id)
    .bind(project_id)
    .fetch_one(&state.pool)
    .await
}

/// Stores `commits` (idempotent) and stamps the fetch time. Returns the
/// number of commits written.
async fn store_commits(
    state: &AppState,
    user_id: &str,
    project_id: &str,
    github_login: &str,
    commits: &[CommitRecord],
) -> Result<usize, sqlx::Error> {
    let mut tx = state.pool.begin().await?;
    for c in commits {
        let matches = c
            .author_login
            .as_deref()
            .is_some_and(|login| login.eq_ignore_ascii_case(github_login));
        sqlx::query(
            "INSERT INTO project_commits
                 (user_id, project_id, oid, authored_at_ms, committed_at_ms, author_match)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (user_id, project_id, oid) DO UPDATE SET
                 authored_at_ms = EXCLUDED.authored_at_ms,
                 committed_at_ms = EXCLUDED.committed_at_ms,
                 author_match = EXCLUDED.author_match",
        )
        .bind(user_id)
        .bind(project_id)
        .bind(&c.oid)
        .bind(c.authored_at_ms)
        .bind(c.committed_at_ms)
        .bind(matches)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "UPDATE linked_repos SET last_fetch_ms = $3 WHERE user_id = $1 AND project_id = $2",
    )
    .bind(user_id)
    .bind(project_id)
    .bind(i64::try_from(state.now_ms()).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(commits.len())
}

/// Fetches and stores the commits of a linked Project.
async fn fetch_linked(
    state: &AppState,
    client: Arc<GithubClient>,
    user_id: &str,
    project_id: &str,
) -> Result<usize, ApiError> {
    let row: Option<(String, i64, Option<i64>)> = sqlx::query_as(
        "SELECT full_name, installation_id, last_fetch_ms FROM linked_repos
         WHERE user_id = $1 AND project_id = $2",
    )
    .bind(user_id)
    .bind(project_id)
    .fetch_optional(&state.pool)
    .await?;
    let (full_name, installation, last_fetch) =
        row.ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "not_linked"))?;
    let login: Option<String> =
        sqlx::query_scalar("SELECT login FROM github_accounts WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(&state.pool)
            .await?;
    let login = login.unwrap_or_default();
    let since = last_fetch.map(|t| t - REFETCH_OVERLAP_MS);
    let now_sec = state.now_ms() / 1000;
    let repo = full_name.clone();
    let commits = blocking(move || {
        let token = client.installation_token(installation, now_sec)?;
        client.fetch_commits(&token, &repo, since, now_sec)
    })
    .await?;
    Ok(store_commits(state, user_id, project_id, &login, &commits).await?)
}

/// `POST /v1/github/link`.
pub async fn link(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<LinkRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let caller = authenticate(&state, &headers).await?;
    if !valid_repo_name(&request.repo) {
        return Err(ApiError::bad_request("bad_repo"));
    }
    if !owns_project(&state, &caller.user_id, &request.project_id).await? {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found"));
    }
    let client = client(&state)?;
    let now_sec = state.now_ms() / 1000;

    // 1. The user-to-server token must show push access on the repo.
    let (user, access) = {
        let (client, token, repo) = (
            Arc::clone(&client),
            request.user_token,
            request.repo.clone(),
        );
        blocking(move || {
            let user = client.user(&token, now_sec)?;
            let access = client.repo_access(&token, &repo, now_sec)?;
            Ok((user, access))
        })
        .await?
    };
    if !access.push {
        return Err(ApiError::new(StatusCode::FORBIDDEN, "no_push_access"));
    }

    // 2. The App must be installed on the repo (the user picked it there).
    let installation = {
        let (client, repo) = (Arc::clone(&client), access.full_name.clone());
        blocking(move || client.installation_for_repo(&repo, now_sec))
            .await
            .map_err(|e| {
                if e.status == StatusCode::NOT_FOUND {
                    ApiError::new(StatusCode::CONFLICT, "app_not_installed")
                } else {
                    e
                }
            })?
    };

    let now_ms = i64::try_from(state.now_ms()).unwrap_or(i64::MAX);
    let mut tx = state.pool.begin().await?;
    sqlx::query(
        "INSERT INTO github_accounts (user_id, github_id, login, linked_at_ms)
         VALUES ($1, $2, $3, $4)
         ON CONFLICT (user_id) DO UPDATE SET github_id = EXCLUDED.github_id,
                                              login = EXCLUDED.login",
    )
    .bind(&caller.user_id)
    .bind(user.id)
    .bind(&user.login)
    .bind(now_ms)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO linked_repos (user_id, project_id, github_repo_id, full_name,
                                   installation_id, linked_at_ms)
         VALUES ($1, $2, $3, $4, $5, $6)
         ON CONFLICT (user_id, project_id) DO UPDATE SET
             github_repo_id = EXCLUDED.github_repo_id, full_name = EXCLUDED.full_name,
             installation_id = EXCLUDED.installation_id, last_fetch_ms = NULL",
    )
    .bind(&caller.user_id)
    .bind(&request.project_id)
    .bind(access.id)
    .bind(&access.full_name)
    .bind(installation)
    .bind(now_ms)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE projects SET linked_repo = $3 WHERE user_id = $1 AND project_id = $2")
        .bind(&caller.user_id)
        .bind(&request.project_id)
        .bind(&access.full_name)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    // 3. A first fetch. A rate limit leaves the link in place; the commits
    //    arrive on the next sync.
    let body = match fetch_linked(&state, client, &caller.user_id, &request.project_id).await {
        Ok(stored) => json!({ "linked": true, "repo": access.full_name, "commits_stored": stored }),
        Err(e)
            if e.status == StatusCode::SERVICE_UNAVAILABLE && e.code == "github_rate_limited" =>
        {
            json!({ "linked": true, "repo": access.full_name, "commits_pending": true,
                    "retry_after_sec": e.retry_after_sec })
        }
        Err(e) => return Err(e),
    };
    Ok((StatusCode::CREATED, Json(body)))
}

/// `POST /v1/github/projects/{project_id}/sync`.
pub async fn sync(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(project_id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let caller = authenticate(&state, &headers).await?;
    let client = client(&state)?;
    let stored = fetch_linked(&state, client, &caller.user_id, &project_id).await?;
    Ok(Json(json!({ "commits_stored": stored })))
}

/// `DELETE /v1/github/projects/{project_id}`.
pub async fn unlink(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(project_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let caller = authenticate(&state, &headers).await?;
    let mut tx = state.pool.begin().await?;
    let removed = sqlx::query("DELETE FROM linked_repos WHERE user_id = $1 AND project_id = $2")
        .bind(&caller.user_id)
        .bind(&project_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if removed == 0 {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_linked"));
    }
    sqlx::query("UPDATE projects SET linked_repo = NULL WHERE user_id = $1 AND project_id = $2")
        .bind(&caller.user_id)
        .bind(&project_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("DELETE FROM project_commits WHERE user_id = $1 AND project_id = $2")
        .bind(&caller.user_id)
        .bind(&project_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
