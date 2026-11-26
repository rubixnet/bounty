//! Pairing: `traction login` creates a pairing, the user approves it in the
//! browser on a CSRF-protected page, and the device polls for its token.

use std::sync::Arc;

use axum::Form;
use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::header::{COOKIE, HOST, LOCATION, ORIGIN, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;
use traction_core::VerifyingKeyHex;
use traction_core::sync::{
    PairingPollRequest, PairingPollResponse, PairingStartRequest, PairingStartResponse,
    PairingStatus,
};

use crate::app::{AppState, authenticate};
use crate::auth::{
    AuthStep, CSRF_COOKIE, approval_page, cookie_value, csrf_ok, message_page, origin_ok,
};
use crate::error::ApiError;
use crate::token::{hash_secret, new_device_token, random_hex, secure_eq};

/// How long a pairing can be approved.
pub const PAIRING_TTL_MS: u64 = 10 * 60 * 1000;

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";

fn new_pair_code() -> String {
    let mut raw = [0u8; 8];
    getrandom::fill(&mut raw).expect("the OS random source is available");
    let letters: Vec<char> = raw
        .iter()
        .map(|b| char::from(CODE_ALPHABET[usize::from(*b) % CODE_ALPHABET.len()]))
        .collect();
    format!(
        "{}-{}",
        letters[..4].iter().collect::<String>(),
        letters[4..].iter().collect::<String>()
    )
}

pub async fn start(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PairingStartRequest>,
) -> Result<Json<PairingStartResponse>, ApiError> {
    VerifyingKeyHex::parse(&request.public_key)
        .map_err(|_| ApiError::bad_request("bad_public_key"))?;
    if request.device_label.len() > 200 {
        return Err(ApiError::bad_request("label_too_long"));
    }
    let now = state.now_ms();
    let poll_secret = random_hex(32);
    let pair_code = new_pair_code();
    sqlx::query(
        "INSERT INTO pairings (pair_code, poll_secret_hash, public_key, previous_key_id,
                               device_label, created_at_ms, expires_at_ms)
         VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(&pair_code)
    .bind(hash_secret(&poll_secret).to_vec())
    .bind(&request.public_key)
    .bind(&request.previous_key_id)
    .bind(&request.device_label)
    .bind(i64::try_from(now).unwrap_or(i64::MAX))
    .bind(i64::try_from(now + PAIRING_TTL_MS).unwrap_or(i64::MAX))
    .execute(&state.pool)
    .await?;
    let base = state.config.server.public_url.trim_end_matches('/');
    Ok(Json(PairingStartResponse {
        login_url: format!("{base}/auth/login?pair={pair_code}"),
        pair_code,
        poll_secret,
        expires_in_sec: u32::try_from(PAIRING_TTL_MS / 1000).unwrap_or(600),
    }))
}

#[derive(sqlx::FromRow)]
struct PairingRow {
    poll_secret_hash: Vec<u8>,
    expires_at_ms: i64,
    approved_device_id: Option<String>,
    token_delivered: bool,
}

pub async fn poll(
    State(state): State<Arc<AppState>>,
    Json(request): Json<PairingPollRequest>,
) -> Result<Json<PairingPollResponse>, ApiError> {
    let row: Option<PairingRow> = sqlx::query_as(
        "SELECT poll_secret_hash, expires_at_ms, approved_device_id, token_delivered
         FROM pairings WHERE pair_code = $1",
    )
    .bind(&request.pair_code)
    .fetch_optional(&state.pool)
    .await?;
    let row = row.ok_or_else(ApiError::unauthorized)?;
    if !secure_eq(&row.poll_secret_hash, &hash_secret(&request.poll_secret)) {
        return Err(ApiError::unauthorized());
    }
    let respond = |status, token, device| {
        Json(PairingPollResponse {
            status,
            device_token: token,
            device_id: device,
        })
    };
    let Some(device_id) = row.approved_device_id else {
        let expired = i64::try_from(state.now_ms()).unwrap_or(i64::MAX) > row.expires_at_ms;
        let status = if expired {
            PairingStatus::Expired
        } else {
            PairingStatus::Pending
        };
        return Ok(respond(status, None, None));
    };
    if row.token_delivered {
        return Ok(respond(PairingStatus::Approved, None, Some(device_id)));
    }
    // Deliver the token exactly once: the flag flips atomically.
    let mut tx = state.pool.begin().await?;
    let claimed = sqlx::query(
        "UPDATE pairings SET token_delivered = TRUE
         WHERE pair_code = $1 AND token_delivered = FALSE",
    )
    .bind(&request.pair_code)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if claimed == 0 {
        return Ok(respond(PairingStatus::Approved, None, Some(device_id)));
    }
    let token = new_device_token();
    sqlx::query("UPDATE devices SET token_hash = $1, revoked_at = NULL WHERE id = $2")
        .bind(hash_secret(&token).to_vec())
        .bind(&device_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(respond(
        PairingStatus::Approved,
        Some(token),
        Some(device_id),
    ))
}

#[derive(Deserialize)]
pub struct LoginQuery {
    pair: String,
}

#[derive(sqlx::FromRow)]
struct OpenPairing {
    device_label: String,
    expires_at_ms: i64,
    approved_device_id: Option<String>,
}

async fn open_pairing(state: &AppState, code: &str) -> Result<Option<OpenPairing>, ApiError> {
    let row: Option<OpenPairing> = sqlx::query_as(
        "SELECT device_label, expires_at_ms, approved_device_id FROM pairings WHERE pair_code = $1",
    )
    .bind(code)
    .fetch_optional(&state.pool)
    .await?;
    let now = i64::try_from(state.now_ms()).unwrap_or(i64::MAX);
    Ok(row.filter(|p| p.approved_device_id.is_none() && p.expires_at_ms >= now))
}

fn html(status: StatusCode, body: String) -> Response {
    (status, Html(body)).into_response()
}

/// `GET /auth/login?pair=<code>`: shows the approval page. Never approves.
pub async fn login_page(
    State(state): State<Arc<AppState>>,
    Query(query): Query<LoginQuery>,
) -> Result<Response, ApiError> {
    let Some(pairing) = open_pairing(&state, &query.pair).await? else {
        return Ok(html(
            StatusCode::NOT_FOUND,
            message_page("This pairing code is unknown, used or expired. Run the login again."),
        ));
    };
    let identity = match state.auth.begin(&query.pair) {
        AuthStep::Redirect(url) => {
            let mut response = StatusCode::FOUND.into_response();
            if let Ok(value) = HeaderValue::from_str(&url) {
                response.headers_mut().insert(LOCATION, value);
            }
            return Ok(response);
        }
        AuthStep::Identified(identity) => identity,
    };
    let csrf = random_hex(32);
    let mut response = html(
        StatusCode::OK,
        approval_page(
            &query.pair,
            &pairing.device_label,
            &identity.display_name,
            &csrf,
        ),
    );
    let cookie =
        format!("{CSRF_COOKIE}={csrf}; Path=/auth; HttpOnly; SameSite=Strict; Max-Age=600");
    if let Ok(value) = HeaderValue::from_str(&cookie) {
        response.headers_mut().insert(SET_COOKIE, value);
    }
    Ok(response)
}

#[derive(Deserialize)]
pub struct ApproveForm {
    pair: String,
    csrf: Option<String>,
}

fn header<'a>(headers: &'a HeaderMap, name: axum::http::HeaderName) -> Option<&'a str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

/// `POST /auth/approve`: the only thing that approves a device.
pub async fn approve(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Form(form): Form<ApproveForm>,
) -> Result<Response, ApiError> {
    let forbidden = |text: &str| Ok(html(StatusCode::FORBIDDEN, message_page(text)));
    if !origin_ok(header(&headers, ORIGIN), header(&headers, HOST)) {
        return forbidden("Request refused.");
    }
    let cookie = cookie_value(header(&headers, COOKIE), CSRF_COOKIE);
    if !csrf_ok(cookie, form.csrf.as_deref()) {
        return forbidden("Request refused.");
    }
    let identity = match state.auth.begin(&form.pair) {
        AuthStep::Identified(identity) => identity,
        AuthStep::Redirect(_) => return forbidden("Sign in first."),
    };
    let mut tx = state.pool.begin().await?;
    let pairing: Option<(String, Option<String>, i64, Option<String>, String)> = sqlx::query_as(
        "SELECT public_key, previous_key_id, expires_at_ms, approved_device_id, device_label
         FROM pairings WHERE pair_code = $1 FOR UPDATE",
    )
    .bind(&form.pair)
    .fetch_optional(&mut *tx)
    .await?;
    let now = i64::try_from(state.now_ms()).unwrap_or(i64::MAX);
    let Some((public_key, previous, expires, approved, label)) = pairing else {
        return Ok(html(
            StatusCode::NOT_FOUND,
            message_page("Unknown pairing code."),
        ));
    };
    if approved.is_some() || expires < now {
        return Ok(html(
            StatusCode::GONE,
            message_page("This pairing is used or expired."),
        ));
    }
    let Ok(key) = VerifyingKeyHex::parse(&public_key) else {
        return Ok(html(StatusCode::BAD_REQUEST, message_page("Bad key.")));
    };
    let key_id = key.key_id().as_str().to_owned();

    let user_id: String = sqlx::query_scalar(
        "INSERT INTO users (id, sub, display_name) VALUES ($1, $2, $3)
         ON CONFLICT (sub) DO UPDATE SET display_name = EXCLUDED.display_name
         RETURNING id",
    )
    .bind(random_hex(16))
    .bind(&identity.sub)
    .bind(&identity.display_name)
    .fetch_one(&mut *tx)
    .await?;

    // Which device does this key belong to? The same key re-pairs its device;
    // a successor key joins the device that owned `previous_key_id`; anything
    // else is a new device. A key pinned for another account is refused.
    let existing: Option<(String, String)> = sqlx::query_as(
        "SELECT k.device_id, d.user_id FROM device_keys k JOIN devices d ON d.id = k.device_id
         WHERE k.public_key = $1",
    )
    .bind(&public_key)
    .fetch_optional(&mut *tx)
    .await?;
    let device_id = match existing {
        Some((_, owner)) if owner != user_id => {
            return Ok(html(
                StatusCode::CONFLICT,
                message_page("This key is paired to another account."),
            ));
        }
        Some((device, _)) => device,
        None => {
            let predecessor: Option<String> =
                match &previous {
                    Some(previous_key) => sqlx::query_scalar(
                        "SELECT k.device_id FROM device_keys k JOIN devices d ON d.id = k.device_id
                     WHERE k.key_id = $1 AND d.user_id = $2",
                    )
                    .bind(previous_key)
                    .bind(&user_id)
                    .fetch_optional(&mut *tx)
                    .await?,
                    None => None,
                };
            match predecessor {
                Some(device) => device,
                None => {
                    let device = random_hex(16);
                    // The real token is minted when the device polls; until
                    // then the stored hash is of a value nobody holds.
                    sqlx::query(
                        "INSERT INTO devices (id, user_id, label, token_hash) VALUES ($1,$2,$3,$4)",
                    )
                    .bind(&device)
                    .bind(&user_id)
                    .bind(&label)
                    .bind(hash_secret(&random_hex(32)).to_vec())
                    .execute(&mut *tx)
                    .await?;
                    device
                }
            }
        }
    };
    sqlx::query(
        "INSERT INTO device_keys (device_id, key_id, public_key) VALUES ($1,$2,$3)
         ON CONFLICT (device_id, key_id) DO NOTHING",
    )
    .bind(&device_id)
    .bind(&key_id)
    .bind(&public_key)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE pairings SET approved_user_id = $1, approved_device_id = $2 WHERE pair_code = $3",
    )
    .bind(&user_id)
    .bind(&device_id)
    .bind(&form.pair)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(html(
        StatusCode::OK,
        message_page("Device approved. You can close this tab and return to the terminal."),
    ))
}

/// `DELETE /v1/devices/{id}`: revokes a device of the caller's account. Its
/// keys stay trusted for history; it can anchor nothing new.
pub async fn revoke_device(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<StatusCode, ApiError> {
    let caller = authenticate(&state, &headers).await?;
    let revoked = sqlx::query(
        "UPDATE devices SET revoked_at = now() WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL",
    )
    .bind(&id)
    .bind(&caller.user_id)
    .execute(&state.pool)
    .await?
    .rows_affected();
    if revoked == 0 {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not_found"));
    }
    Ok(StatusCode::NO_CONTENT)
}
