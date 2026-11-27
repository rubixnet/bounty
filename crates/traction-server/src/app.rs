//! Shared state, the router and bearer authentication.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::DefaultBodyLimit;
use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use axum::routing::{delete, get, post};
use sqlx::PgPool;

use crate::auth::AuthProvider;
use crate::config::Config;
use crate::error::ApiError;
use crate::fanout::LiveFanout;
use crate::partition::Month;
use crate::rate_limit::RateLimiter;
use crate::token::{bearer, hash_secret};
use crate::{anchors, live, pairing, views};

/// Microseconds since the Unix epoch. Injectable so tests control time.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
    })
}

pub struct AppState {
    pub pool: PgPool,
    pub config: Config,
    pub auth: Box<dyn AuthProvider>,
    pub fanout: Arc<dyn LiveFanout>,
    pub limiter: RateLimiter,
    pub clock: Clock,
    /// Months whose partitions this process already created.
    pub known_months: Mutex<HashSet<Month>>,
}

impl AppState {
    pub fn now_ms(&self) -> u64 {
        (self.clock)() / 1000
    }
}

/// An authenticated, non-revoked device.
pub struct Device {
    pub id: String,
    pub user_id: String,
}

pub async fn authenticate(state: &AppState, headers: &HeaderMap) -> Result<Device, ApiError> {
    let token = bearer(headers.get(AUTHORIZATION).and_then(|v| v.to_str().ok()))
        .ok_or_else(ApiError::unauthorized)?;
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT id, user_id FROM devices WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(hash_secret(token).to_vec())
    .fetch_optional(&state.pool)
    .await?;
    row.map(|(id, user_id)| Device { id, user_id })
        .ok_or_else(ApiError::unauthorized)
}

pub fn router(state: Arc<AppState>) -> Router {
    let cap = state.config.anchor_policy.max_body_bytes;
    Router::new()
        .route("/v1/pairings", post(pairing::start))
        .route("/v1/pairings/poll", post(pairing::poll))
        .route("/auth/login", get(pairing::login_page))
        .route("/auth/approve", post(pairing::approve))
        .route("/auth/callback", get(pairing::callback))
        .route("/v1/devices/{id}", delete(pairing::revoke_device))
        .route("/v1/anchors", post(anchors::post_anchors))
        .route("/v1/live", get(live::stream))
        .route("/v1/me", get(views::get_me).delete(views::delete_me))
        .route("/v1/users/{user_id}/projects", get(views::list_projects))
        .route(
            "/v1/users/{user_id}/projects/{project_id}/timeline",
            get(views::project_timeline),
        )
        .layer(DefaultBodyLimit::max(cap))
        .with_state(state)
}
