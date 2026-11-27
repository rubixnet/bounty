//! Alias-only Project merge (spec S3b, Q17). `project_aliases(from_id -> to_id)`
//! says "treat `from_id` as `to_id`". Stored buckets are never rewritten and
//! bundle verification keeps the original IDs; every read that reports
//! Project totals, the Project list or a timeline resolves aliases through
//! [`AliasMap`]. Merging works within one user's own Projects only.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::app::{AppState, authenticate};
use crate::error::ApiError;

/// Why a merge was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AliasError {
    /// A Project cannot be merged into itself.
    SameProject,
    /// `from` already points somewhere; unmerge it first.
    AlreadyMerged,
    /// Following the chain from `to` reaches `from`.
    Cycle,
}

impl AliasError {
    pub fn code(self) -> &'static str {
        match self {
            Self::SameProject => "same_project",
            Self::AlreadyMerged => "already_merged",
            Self::Cycle => "alias_cycle",
        }
    }
}

/// One user's aliases, `from -> to`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AliasMap {
    to_of: HashMap<String, String>,
}

impl AliasMap {
    pub fn new(pairs: impl IntoIterator<Item = (String, String)>) -> Self {
        Self {
            to_of: pairs.into_iter().collect(),
        }
    }

    /// The Project `id` finally resolves to, following chains. A corrupt
    /// cycle in stored data stops at the first repeat instead of looping.
    pub fn resolve(&self, id: &str) -> String {
        let mut seen = HashSet::new();
        let mut current = id;
        while let Some(next) = self.to_of.get(current) {
            if !seen.insert(current) {
                break;
            }
            current = next;
        }
        current.to_owned()
    }

    /// Whether `id` is merged into another Project.
    pub fn is_alias(&self, id: &str) -> bool {
        self.to_of.contains_key(id)
    }

    /// `target` and every Project that resolves to it, sorted.
    pub fn members_of(&self, target: &str) -> Vec<String> {
        let mut members: Vec<String> = self
            .to_of
            .keys()
            .filter(|from| self.resolve(from) == target)
            .cloned()
            .collect();
        members.push(target.to_owned());
        members.sort();
        members.dedup();
        members
    }

    /// Checks that adding `from -> to` is allowed.
    pub fn check_merge(&self, from: &str, to: &str) -> Result<(), AliasError> {
        if from == to {
            return Err(AliasError::SameProject);
        }
        if self.is_alias(from) {
            return Err(AliasError::AlreadyMerged);
        }
        if self.resolve(to) == from {
            return Err(AliasError::Cycle);
        }
        Ok(())
    }
}

/// Loads the user's aliases.
pub async fn load(pool: &PgPool, user_id: &str) -> Result<AliasMap, sqlx::Error> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT from_id, to_id FROM project_aliases WHERE user_id = $1")
            .bind(user_id)
            .fetch_all(pool)
            .await?;
    Ok(AliasMap::new(rows))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergeRequest {
    pub from_id: String,
    pub to_id: String,
}

async fn owns(pool: &PgPool, user_id: &str, project_id: &str) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT EXISTS (SELECT 1 FROM projects WHERE user_id = $1 AND project_id = $2)",
    )
    .bind(user_id)
    .bind(project_id)
    .fetch_one(pool)
    .await
}

/// `POST /v1/projects/merge`: merges one of the caller's Projects into
/// another (for example the same repo seen from two devices).
pub async fn merge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(request): Json<MergeRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let caller = authenticate(&state, &headers).await?;
    // A Project that is not the caller's reads as not found: no existence
    // oracle for other users' Project IDs.
    for id in [&request.from_id, &request.to_id] {
        if !owns(&state.pool, &caller.user_id, id).await? {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found"));
        }
    }
    // Serialize merges per user so two concurrent requests cannot together
    // close a cycle.
    let mut tx = state.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended('alias:' || $1, 0))")
        .bind(&caller.user_id)
        .execute(&mut *tx)
        .await?;
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT from_id, to_id FROM project_aliases WHERE user_id = $1")
            .bind(&caller.user_id)
            .fetch_all(&mut *tx)
            .await?;
    let map = AliasMap::new(rows);
    map.check_merge(&request.from_id, &request.to_id)
        .map_err(|e| ApiError::new(StatusCode::CONFLICT, e.code()))?;
    sqlx::query(
        "INSERT INTO project_aliases (user_id, from_id, to_id, created_at_ms)
         VALUES ($1, $2, $3, $4)",
    )
    .bind(&caller.user_id)
    .bind(&request.from_id)
    .bind(&request.to_id)
    .bind(i64::try_from(state.now_ms()).unwrap_or(i64::MAX))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    let resolved = map.resolve(&request.to_id);
    Ok((
        StatusCode::CREATED,
        Json(json!({ "from_id": request.from_id, "resolves_to": resolved })),
    ))
}

/// `DELETE /v1/projects/merge/{from_id}`: unmerges (deletes the alias).
pub async fn unmerge(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(from_id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let caller = authenticate(&state, &headers).await?;
    let removed = sqlx::query("DELETE FROM project_aliases WHERE user_id = $1 AND from_id = $2")
        .bind(&caller.user_id)
        .bind(&from_id)
        .execute(&state.pool)
        .await?
        .rows_affected();
    if removed == 0 {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found"));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> AliasMap {
        AliasMap::new(
            pairs
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned())),
        )
    }

    #[test]
    fn chains_are_followed_to_the_final_project() {
        let m = map(&[("a", "b"), ("b", "c")]);
        assert_eq!(m.resolve("a"), "c");
        assert_eq!(m.resolve("b"), "c");
        assert_eq!(m.resolve("c"), "c");
        assert_eq!(m.resolve("unrelated"), "unrelated");
        assert_eq!(m.members_of("c"), ["a", "b", "c"]);
        // Only the final target collects members.
        assert_eq!(m.members_of("b"), ["b"]);
    }

    #[test]
    fn a_cycle_a_self_merge_and_a_second_alias_are_refused() {
        let m = map(&[("a", "b"), ("b", "c")]);
        assert_eq!(m.check_merge("c", "a"), Err(AliasError::Cycle));
        assert_eq!(m.check_merge("c", "b"), Err(AliasError::Cycle));
        assert_eq!(m.check_merge("a", "a"), Err(AliasError::SameProject));
        assert_eq!(m.check_merge("a", "d"), Err(AliasError::AlreadyMerged));
        assert_eq!(m.check_merge("d", "a"), Ok(()));
        assert_eq!(m.check_merge("c", "d"), Ok(()));
    }

    #[test]
    fn corrupt_stored_cycles_never_loop() {
        let m = map(&[("a", "b"), ("b", "a")]);
        let _ = m.resolve("a");
        let _ = m.members_of("a");
    }
}
