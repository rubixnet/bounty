//! `traction-server`: the Step 4 backend. Axum + runtime-checked SQLx, one
//! `traction-server.toml`, embedded migrations and native monthly partitions.
//!
//! Module map: `config` (TOML, secrets by reference), `app` (state, router,
//! bearer auth), `pairing` and `auth` (login, CSRF, `AuthProvider`),
//! `anchors` (`POST /v1/anchors`), `store` (typed-column rows), `merge` (read-time
//! overlap merge), `views` (read APIs, `DELETE /v1/me`), `oidc` (Hack Club
//! login), `partition`
//! (monthly partitions), `fanout` + `live` (SSE totals), `rate_limit`,
//! `token`, `error`, `blob` (`BlobStore`: R2 SigV4 + local HMAC), `decision`
//! (`DecisionEngine` seam), `heuristic` and `submission` (bundle upload,
//! verification and triage).

pub mod anchors;
pub mod app;
pub mod auth;
pub mod blob;
pub mod config;
pub mod decision;
pub mod error;
pub mod fanout;
pub mod heuristic;
pub mod live;
pub mod merge;
pub mod oidc;
pub mod pairing;
pub mod partition;
pub mod provenance;
pub mod rate_limit;
pub mod store;
pub mod submission;
pub mod token;
pub mod views;

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;

use crate::app::{AppState, Clock, system_clock};
use crate::auth::{AuthProvider, DevAuth};
use crate::config::Config;
use crate::decision::DecisionEngine;
use crate::fanout::BroadcastFanout;
use crate::heuristic::HeuristicEngine;
use crate::oidc::{OidcProvider, OidcSettings};
use crate::rate_limit::RateLimiter;

/// Applies the embedded migrations.
pub async fn migrate(pool: &PgPool) -> Result<(), sqlx::migrate::MigrateError> {
    sqlx::migrate!("./migrations").run(pool).await
}

/// Creates the previous, current and next two months of partitions.
pub async fn ensure_standard_partitions(
    pool: &PgPool,
    now_ms: u64,
) -> Result<Vec<partition::Month>, sqlx::Error> {
    let months = partition::standard_window(now_ms);
    partition::ensure_months(pool, &months).await?;
    Ok(months.to_vec())
}

/// Builds the shared state. `clock` is microseconds since the Unix epoch.
pub fn build_state(pool: PgPool, config: Config, clock: Option<Clock>) -> Result<AppState, String> {
    let engine: Arc<dyn DecisionEngine> = Arc::new(HeuristicEngine::new(config.heuristic.clone()));
    build_state_with_engine(pool, config, clock, engine)
}

/// [`build_state`] with a chosen `DecisionEngine` (S4 wires `JevEngine`).
pub fn build_state_with_engine(
    pool: PgPool,
    config: Config,
    clock: Option<Clock>,
    engine: Arc<dyn DecisionEngine>,
) -> Result<AppState, String> {
    let clock = clock.unwrap_or_else(system_clock);
    let blob_clock = Arc::clone(&clock);
    let blobs = blob::build(
        &config.blob,
        &config.server.public_url,
        Arc::new(move || blob_clock() / 1000),
        |name| std::env::var(name).ok(),
    )?;
    let auth: Box<dyn AuthProvider> = match config.auth.provider.as_str() {
        "hackclub" => {
            let secret = config
                .auth
                .client_secret(|name| std::env::var(name).ok())
                .map_err(|e| e.to_string())?;
            let settings =
                OidcSettings::from_config(&config.auth, secret, &config.server.public_url)?;
            Box::new(OidcProvider::new(settings, Arc::clone(&clock)))
        }
        "dev" => Box::new(DevAuth {
            sub: config.auth.dev_sub.clone(),
            display_name: config.auth.dev_display_name.clone(),
        }),
        other => return Err(format!("unknown auth provider `{other}`")),
    };
    let limiter = RateLimiter::new(
        config.anchor_policy.rate_limit_burst,
        config.anchor_policy.rate_limit_refill_ms,
    );
    Ok(AppState {
        pool,
        config,
        auth,
        blobs,
        engine,
        fanout: Arc::new(BroadcastFanout::new(64)),
        limiter,
        clock,
        known_months: Mutex::new(HashSet::new()),
    })
}

/// Connects to Postgres, migrates, creates partitions and starts the daily
/// partition task. Returns the state ready for [`app::router`].
pub async fn start(config: Config, database_url: &str) -> Result<Arc<AppState>, String> {
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(database_url)
        .await
        .map_err(|e| format!("cannot connect to the database: {e}"))?;
    migrate(&pool)
        .await
        .map_err(|e| format!("migration failed: {e}"))?;
    let state = Arc::new(build_state(pool, config, None)?);
    let months = ensure_standard_partitions(&state.pool, state.now_ms())
        .await
        .map_err(|e| format!("cannot create partitions: {e}"))?;
    state
        .known_months
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .extend(months);
    let daily = Arc::clone(&state);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        tick.tick().await;
        loop {
            tick.tick().await;
            match ensure_standard_partitions(&daily.pool, daily.now_ms()).await {
                Ok(months) => daily
                    .known_months
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .extend(months),
                Err(error) => eprintln!("traction-server: daily partition run failed: {error}"),
            }
        }
    });
    Ok(state)
}
