//! A local HTTP server that replays GitHub's REST API for the GitHub client
//! tests.
//!
//! - The commit pages (`tests/data/github/commits_page{1,2}.json`) and the
//!   repo document (`repo_public.json`) are **real responses** recorded from
//!   `octocat/Hello-World` on 2026-11-27 (`per_page=2`; the Link header named
//!   `rel="next"`, which this server rewrites to its own address).
//! - The push-access answer is the recorded repo document with the
//!   `permissions` object GitHub adds for an authenticated user (shape from
//!   the REST docs); the rate-limit reply copies GitHub's documented `403`
//!   headers and body. Those two were not recorded: GitHub only returns them
//!   to an authenticated, over-limit caller.
//! - The App JWT is **really verified** (RS256 against the App's public key),
//!   so a client that signs wrongly is refused like on GitHub.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::{Digest, Sha256};
use rsa::{Pkcs1v15Sign, PrivateKey};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

pub const APP_ID: &str = "123456";
pub const USER_TOKEN: &str = "ghu_user_with_push";
pub const READONLY_USER_TOKEN: &str = "ghu_user_read_only";
pub const INSTALLATION_ID: i64 = 4242;
pub const INSTALLATION_TOKEN: &str = "ghs_installation_token";
pub const REPO: &str = "octocat/Hello-World";
pub const APP_KEY_PEM: &str = include_str!("../data/idp_test_rsa.pem");
const PAGE1: &str = include_str!("../data/github/commits_page1.json");
const PAGE2: &str = include_str!("../data/github/commits_page2.json");
const REPO_DOC: &str = include_str!("../data/github/repo_public.json");

#[derive(Default)]
pub struct Recorded {
    /// `path?query` of every request, in order.
    pub requests: Vec<String>,
    /// Every `Authorization` header value seen.
    pub tokens: Vec<String>,
}

pub struct GithubState {
    base: String,
    app_key: PrivateKey,
    pub recorded: Mutex<Recorded>,
    /// Reply `403` rate-limit to this many upcoming commit requests (after
    /// `pass_first` requests have passed).
    pub rate_limit_commits: Mutex<u32>,
    pub pass_first: Mutex<u32>,
    /// Whether the App is installed on the repo.
    pub installed: Mutex<bool>,
    /// The `x-ratelimit-reset` value the rate-limit reply carries.
    pub reset_at: Mutex<u64>,
}

pub struct FakeGithub {
    pub base: String,
    pub state: Arc<GithubState>,
    task: JoinHandle<()>,
}

impl Drop for FakeGithub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned()
}

fn record(state: &GithubState, path: &str, query: &str, headers: &HeaderMap) {
    let mut recorded = state.recorded.lock().unwrap();
    recorded.requests.push(if query.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{query}")
    });
    recorded.tokens.push(bearer(headers));
}

fn query_string(q: &HashMap<String, String>) -> String {
    let mut pairs: Vec<_> = q.iter().map(|(k, v)| format!("{k}={v}")).collect();
    pairs.sort();
    pairs.join("&")
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({"message": "Bad credentials"})),
    )
        .into_response()
}

/// Verifies the App JWT like GitHub: RS256 over `header.payload`, `iss` is
/// the App ID, and `exp` is within 10 minutes of `iat`.
fn app_jwt_ok(state: &GithubState, token: &str) -> bool {
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature] = parts.as_slice() else {
        return false;
    };
    let decode = |s: &str| URL_SAFE_NO_PAD.decode(s).ok();
    let (Some(h), Some(p), Some(sig)) = (decode(header), decode(payload), decode(signature)) else {
        return false;
    };
    let (Ok(h), Ok(p)): (Result<Value, _>, Result<Value, _>) =
        (serde_json::from_slice(&h), serde_json::from_slice(&p))
    else {
        return false;
    };
    let digest = Sha256::digest(format!("{header}.{payload}").as_bytes());
    let signed = state
        .app_key
        .to_public_key()
        .verify(Pkcs1v15Sign::new::<Sha256>(), &digest, &sig)
        .is_ok();
    let (iat, exp) = (
        p["iat"].as_u64().unwrap_or(0),
        p["exp"].as_u64().unwrap_or(0),
    );
    signed && h["alg"] == "RS256" && p["iss"] == APP_ID && exp > iat && exp - iat <= 600
}

async fn user(State(state): State<Arc<GithubState>>, headers: HeaderMap) -> Response {
    record(&state, "/user", "", &headers);
    match bearer(&headers).as_str() {
        USER_TOKEN | READONLY_USER_TOKEN => {
            Json(json!({"login": "octocat", "id": 583_231, "type": "User"})).into_response()
        }
        _ => unauthorized(),
    }
}

async fn repo(
    State(state): State<Arc<GithubState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    record(&state, &format!("/repos/{owner}/{name}"), "", &headers);
    let push = match bearer(&headers).as_str() {
        USER_TOKEN => true,
        READONLY_USER_TOKEN => false,
        _ => return unauthorized(),
    };
    if format!("{owner}/{name}") != REPO {
        return (StatusCode::NOT_FOUND, Json(json!({"message": "Not Found"}))).into_response();
    }
    let mut doc: Value = serde_json::from_str(REPO_DOC).unwrap();
    doc["permissions"] = json!({
        "admin": false, "maintain": false, "push": push, "triage": push, "pull": true
    });
    Json(doc).into_response()
}

async fn installation(
    State(state): State<Arc<GithubState>>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    record(
        &state,
        &format!("/repos/{owner}/{name}/installation"),
        "",
        &headers,
    );
    if !app_jwt_ok(&state, &bearer(&headers)) {
        return unauthorized();
    }
    if !*state.installed.lock().unwrap() || format!("{owner}/{name}") != REPO {
        return (StatusCode::NOT_FOUND, Json(json!({"message": "Not Found"}))).into_response();
    }
    Json(json!({"id": INSTALLATION_ID, "app_id": 123_456})).into_response()
}

async fn access_token(
    State(state): State<Arc<GithubState>>,
    Path(id): Path<i64>,
    headers: HeaderMap,
) -> Response {
    record(
        &state,
        &format!("/app/installations/{id}/access_tokens"),
        "",
        &headers,
    );
    if !app_jwt_ok(&state, &bearer(&headers)) {
        return unauthorized();
    }
    if id != INSTALLATION_ID {
        return (StatusCode::NOT_FOUND, Json(json!({"message": "Not Found"}))).into_response();
    }
    (
        StatusCode::CREATED,
        Json(json!({
            "token": INSTALLATION_TOKEN,
            "expires_at": "2026-11-27T13:00:00Z",
            "permissions": {"contents": "read", "metadata": "read"},
        })),
    )
        .into_response()
}

async fn commits(
    State(state): State<Arc<GithubState>>,
    path: String,
    q: HashMap<String, String>,
    headers: HeaderMap,
) -> Response {
    record(&state, &path, &query_string(&q), &headers);
    if bearer(&headers) != INSTALLATION_TOKEN {
        return unauthorized();
    }
    let passes = {
        let mut pass = state.pass_first.lock().unwrap();
        let passes = *pass > 0;
        *pass = pass.saturating_sub(1);
        passes
    };
    if !passes {
        let mut left = state.rate_limit_commits.lock().unwrap();
        if *left > 0 {
            *left -= 1;
            let mut response = (
                StatusCode::FORBIDDEN,
                Json(json!({
                    "message": "API rate limit exceeded for installation ID 4242.",
                    "documentation_url": "https://docs.github.com/rest/overview/rate-limits-for-the-rest-api",
                })),
            )
                .into_response();
            let reset = state.reset_at.lock().unwrap().to_string();
            let h = response.headers_mut();
            h.insert("x-ratelimit-limit", HeaderValue::from_static("5000"));
            h.insert("x-ratelimit-remaining", HeaderValue::from_static("0"));
            h.insert("x-ratelimit-resource", HeaderValue::from_static("core"));
            h.insert("x-ratelimit-reset", HeaderValue::from_str(&reset).unwrap());
            return response;
        }
    }
    let page = q.get("page").map_or("1", String::as_str);
    let (body, link) = if page == "1" {
        (
            PAGE1,
            Some(format!(
                "<{0}/repositories/1296269/commits?per_page=2&page=2>; rel=\"next\", \
                 <{0}/repositories/1296269/commits?per_page=2&page=2>; rel=\"last\"",
                state.base
            )),
        )
    } else {
        (PAGE2, None)
    };
    let mut response = (
        StatusCode::OK,
        [("content-type", "application/json; charset=utf-8")],
        body.to_owned(),
    )
        .into_response();
    if let Some(link) = link {
        response
            .headers_mut()
            .insert("link", HeaderValue::from_str(&link).unwrap());
    }
    response
}

async fn commits_by_name(
    state: State<Arc<GithubState>>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    commits(state, format!("/repos/{owner}/{name}/commits"), q, headers).await
}

async fn commits_by_id(
    state: State<Arc<GithubState>>,
    Path(id): Path<i64>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    commits(state, format!("/repositories/{id}/commits"), q, headers).await
}

pub async fn start() -> FakeGithub {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(GithubState {
        base: base.clone(),
        app_key: PrivateKey::from_pkcs8_pem(APP_KEY_PEM).unwrap(),
        recorded: Mutex::new(Recorded::default()),
        rate_limit_commits: Mutex::new(0),
        pass_first: Mutex::new(0),
        installed: Mutex::new(true),
        reset_at: Mutex::new(0),
    });
    let app = axum::Router::new()
        .route("/user", get(user))
        .route("/repos/{owner}/{name}", get(repo))
        .route("/repos/{owner}/{name}/installation", get(installation))
        .route("/repos/{owner}/{name}/commits", get(commits_by_name))
        .route("/repositories/{id}/commits", get(commits_by_id))
        .route("/app/installations/{id}/access_tokens", post(access_token))
        .with_state(Arc::clone(&state));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    FakeGithub { base, state, task }
}
