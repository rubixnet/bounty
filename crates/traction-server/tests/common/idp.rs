//! A test OpenID Connect identity provider that runs inside the test. It is
//! a real HTTP server on loopback that checks the authorization-code + PKCE
//! exchange and issues real RS256-signed id tokens (the committed test key
//! `tests/data/idp_test_rsa.pem`, which protects nothing), so the server's
//! OIDC provider is exercised against genuine signatures, not stubs.
#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Query, State};
use axum::http::{HeaderValue, StatusCode, header::LOCATION};
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::{Digest, Sha256};
use rsa::traits::PublicKeyParts;
use rsa::{Pkcs1v15Sign, PrivateKey};
use serde_json::json;
use tokio::task::JoinHandle;

pub const CLIENT_ID: &str = "traction-test-client";
pub const CLIENT_SECRET: &str = "test-client-secret-value";
pub const KID: &str = "test-key-1";

/// How the next id token is (mis)issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Behaviour {
    Good,
    WrongAudience,
    Expired,
    WrongNonce,
    WrongIssuer,
    SignedByAnotherKey,
    AlgNone,
    AlgHs256,
    TamperedSignature,
    TamperedClaims,
    NoNonce,
}

struct Issued {
    challenge: String,
    nonce: String,
    redirect_uri: String,
}

pub struct IdpState {
    issuer: String,
    key: PrivateKey,
    other: PrivateKey,
    now_ms: u64,
    codes: Mutex<HashMap<String, Issued>>,
    behaviour: Mutex<Behaviour>,
    pub token_requests: Mutex<u32>,
}

pub struct Idp {
    pub base: String,
    pub state: Arc<IdpState>,
    task: JoinHandle<()>,
}

impl Drop for Idp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Idp {
    pub fn issuer(&self) -> String {
        self.base.clone()
    }

    pub fn set_behaviour(&self, behaviour: Behaviour) {
        *self.state.behaviour.lock().unwrap() = behaviour;
    }
}

fn load(pem: &str) -> PrivateKey {
    PrivateKey::from_pkcs8_pem(pem).expect("the committed test key parses")
}

fn b64(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

fn sign(key: &PrivateKey, input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    b64(&key.sign(Pkcs1v15Sign::new::<Sha256>(), &digest).unwrap())
}

fn id_token(state: &IdpState, nonce: &str, behaviour: Behaviour) -> String {
    let now_s = state.now_ms / 1000;
    let mut claims = json!({
        "iss": if behaviour == Behaviour::WrongIssuer { "https://evil.example" } else { state.issuer.as_str() },
        "sub": "hc-user-1",
        "aud": if behaviour == Behaviour::WrongAudience { "someone-else" } else { CLIENT_ID },
        "exp": if behaviour == Behaviour::Expired { now_s - 3600 } else { now_s + 300 },
        "iat": now_s,
        "nonce": if behaviour == Behaviour::WrongNonce { "not-the-nonce" } else { nonce },
        "name": "Ada Lovelace",
    });
    if behaviour == Behaviour::NoNonce {
        claims.as_object_mut().unwrap().remove("nonce");
    }
    let alg = match behaviour {
        Behaviour::AlgNone => "none",
        Behaviour::AlgHs256 => "HS256",
        _ => "RS256",
    };
    let header = b64(json!({"alg": alg, "typ": "JWT", "kid": KID})
        .to_string()
        .as_bytes());
    let payload = b64(claims.to_string().as_bytes());
    let input = format!("{header}.{payload}");
    match behaviour {
        Behaviour::AlgNone => format!("{input}."),
        Behaviour::AlgHs256 => format!("{input}.{}", b64(b"not a real mac")),
        Behaviour::SignedByAnotherKey => format!("{input}.{}", sign(&state.other, &input)),
        Behaviour::TamperedSignature => {
            let mut signature = URL_SAFE_NO_PAD.decode(sign(&state.key, &input)).unwrap();
            let last = signature.len() - 1;
            signature[last] ^= 1;
            format!("{input}.{}", b64(&signature))
        }
        Behaviour::TamperedClaims => {
            // A valid signature over the honest claims, but a different payload.
            let signature = sign(&state.key, &input);
            let forged = b64(claims
                .to_string()
                .replace("hc-user-1", "hc-admin")
                .as_bytes());
            format!("{header}.{forged}.{signature}")
        }
        _ => format!("{input}.{}", sign(&state.key, &input)),
    }
}

async fn authorize(
    State(state): State<Arc<IdpState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let get = |k: &str| q.get(k).map(String::as_str).unwrap_or_default();
    if get("response_type") != "code"
        || get("client_id") != CLIENT_ID
        || get("code_challenge_method") != "S256"
        || get("code_challenge").len() < 43
        || get("state").is_empty()
        || get("nonce").is_empty()
        || !get("scope").split(' ').any(|s| s == "openid")
    {
        return (StatusCode::BAD_REQUEST, "bad authorization request").into_response();
    }
    let code = format!("code-{}", state.codes.lock().unwrap().len() + 1);
    state.codes.lock().unwrap().insert(
        code.clone(),
        Issued {
            challenge: get("code_challenge").to_owned(),
            nonce: get("nonce").to_owned(),
            redirect_uri: get("redirect_uri").to_owned(),
        },
    );
    let location = format!("{}?code={code}&state={}", get("redirect_uri"), get("state"));
    let mut response = StatusCode::FOUND.into_response();
    response
        .headers_mut()
        .insert(LOCATION, HeaderValue::from_str(&location).unwrap());
    response
}

async fn token(
    State(state): State<Arc<IdpState>>,
    Form(form): Form<HashMap<String, String>>,
) -> Response {
    *state.token_requests.lock().unwrap() += 1;
    let get = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
    let issued = state.codes.lock().unwrap().remove(get("code"));
    let Some(issued) = issued else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    };
    let pkce_ok = b64(&Sha256::digest(get("code_verifier").as_bytes())) == issued.challenge;
    if get("grant_type") != "authorization_code"
        || get("client_id") != CLIENT_ID
        || get("client_secret") != CLIENT_SECRET
        || get("redirect_uri") != issued.redirect_uri
        || !pkce_ok
    {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid_grant"})),
        )
            .into_response();
    }
    let behaviour = *state.behaviour.lock().unwrap();
    Json(json!({
        "access_token": "unused",
        "token_type": "Bearer",
        "id_token": id_token(&state, &issued.nonce, behaviour),
    }))
    .into_response()
}

async fn keys(State(state): State<Arc<IdpState>>) -> Json<serde_json::Value> {
    Json(json!({"keys": [{
        "kty": "RSA", "use": "sig", "alg": "RS256", "kid": KID,
        "n": b64(&state.key.n().to_be_bytes()),
        "e": b64(&state.key.e().to_be_bytes()),
    }]}))
}

/// Starts the identity provider; its tokens are issued as of `now_ms`.
pub async fn start(now_ms: u64) -> Idp {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let state = Arc::new(IdpState {
        issuer: base.clone(),
        key: load(include_str!("../data/idp_test_rsa.pem")),
        other: load(include_str!("../data/idp_other_rsa.pem")),
        now_ms,
        codes: Mutex::new(HashMap::new()),
        behaviour: Mutex::new(Behaviour::Good),
        token_requests: Mutex::new(0),
    });
    let app = axum::Router::new()
        .route("/authorize", get(authorize))
        .route("/token", post(token))
        .route("/keys", get(keys))
        .with_state(Arc::clone(&state));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Idp { base, state, task }
}

/// The `auth` config section pointing at this provider.
pub fn auth_config(
    idp: &Idp,
    secret_file: &std::path::Path,
) -> traction_server::config::AuthConfig {
    traction_server::config::AuthConfig {
        provider: "hackclub".into(),
        issuer: idp.issuer(),
        client_id: CLIENT_ID.into(),
        client_secret_file: Some(secret_file.to_string_lossy().into_owned()),
        authorization_endpoint: Some(format!("{}/authorize", idp.base)),
        token_endpoint: Some(format!("{}/token", idp.base)),
        jwks_uri: Some(format!("{}/keys", idp.base)),
        ..Default::default()
    }
}
