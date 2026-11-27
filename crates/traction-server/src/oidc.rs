//! Hack Club OIDC `AuthProvider` (spec S2, Q5): authorization code flow with
//! PKCE (`S256`) and a nonce, wired into the Pairing page.
//!
//! The flow, for one Pairing:
//! 1. `begin(pair, None)` records a pending login (`state`, PKCE verifier,
//!    `nonce`) and answers a redirect to the identity provider.
//! 2. The provider redirects back to `<public_url>/auth/callback?code&state`;
//!    `callback` consumes the `state` (once), exchanges the code at the token
//!    endpoint with the client secret and the PKCE verifier, **verifies the
//!    id token** (RS256 signature against the provider's JWKS, `iss`, `aud`,
//!    `exp`, `nbf`, `nonce`) and opens a short in-memory session.
//! 3. `begin(pair, Some(session))` then identifies the browser user, and the
//!    existing CSRF-protected Approve page takes over.
//!
//! Sessions and pending logins live in memory (single node, spec Q10): a
//! restart only asks people to sign in again. Nothing from the provider is
//! logged or echoed; every failure is one value-free [`LoginError`].
//!
//! The client secret comes from the environment or a file, never the config.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::sha2::{Digest, Sha256};
use rsa::{BoxedUint, Pkcs1v15Sign, PublicKey};
use serde::Deserialize;

use crate::app::Clock;
use crate::auth::{AuthProvider, AuthStep, BoxFuture, Identity, LoginDone, LoginError};
use crate::config::AuthConfig;
use crate::token::{hash_secret, hex_encode, random_hex};

/// How long a login may take between the redirect and the callback.
pub const PENDING_TTL_MS: u64 = 10 * 60 * 1000;
/// How long a signed-in browser session lasts.
pub const SESSION_TTL_MS: u64 = 30 * 60 * 1000;
/// Clock leeway for `exp` and `nbf`.
pub const LEEWAY_MS: u64 = 60 * 1000;
/// Bound on in-memory logins and sessions.
const MAX_ENTRIES: usize = 10_000;
/// Longest accepted id token and `sub`.
const MAX_TOKEN_LEN: usize = 16 * 1024;
const MAX_SUB_LEN: usize = 255;
const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

/// Resolved OIDC settings.
#[derive(Debug, Clone)]
pub struct OidcSettings {
    pub issuer: String,
    pub client_id: String,
    pub client_secret: String,
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    pub jwks_uri: String,
    pub redirect_url: String,
}

/// Endpoints must be `https`; plain `http` is allowed only on loopback (the
/// test identity provider).
fn endpoint_ok(url: &str) -> bool {
    traction_core::secure_http_url(url)
}

impl OidcSettings {
    /// Builds the settings from `[auth]`, the secret and the server's public
    /// URL. Refuses a missing client ID or a non-HTTPS endpoint.
    pub fn from_config(
        auth: &AuthConfig,
        client_secret: String,
        public_url: &str,
    ) -> Result<Self, String> {
        if auth.client_id.is_empty() {
            return Err("[auth] client_id is required for the hackclub provider".into());
        }
        let issuer = auth.issuer.trim_end_matches('/').to_owned();
        let endpoint = |explicit: &Option<String>, path: &str| {
            explicit
                .clone()
                .unwrap_or_else(|| format!("{issuer}/{path}"))
        };
        let settings = Self {
            authorization_endpoint: endpoint(&auth.authorization_endpoint, "oauth/authorize"),
            token_endpoint: endpoint(&auth.token_endpoint, "oauth/token"),
            jwks_uri: endpoint(&auth.jwks_uri, "oauth/discovery/keys"),
            redirect_url: format!("{}/auth/callback", public_url.trim_end_matches('/')),
            issuer,
            client_id: auth.client_id.clone(),
            client_secret,
        };
        for url in [
            &settings.issuer,
            &settings.authorization_endpoint,
            &settings.token_endpoint,
            &settings.jwks_uri,
        ] {
            if !endpoint_ok(url) {
                return Err("OIDC endpoints must use https".into());
            }
        }
        Ok(settings)
    }
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
pub fn url_encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(byte));
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The PKCE `S256` challenge of a verifier (RFC 7636 section 4.2).
pub fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn new_verifier() -> String {
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).expect("the OS random source is available");
    URL_SAFE_NO_PAD.encode(raw)
}

/// What an id token must satisfy.
#[derive(Debug, Clone, Copy)]
pub struct Expect<'a> {
    pub issuer: &'a str,
    pub client_id: &'a str,
    pub nonce: &'a str,
    pub now_ms: u64,
}

#[derive(Deserialize)]
struct JwtHeader {
    alg: String,
    kid: Option<String>,
}

#[derive(Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    aud: Audience,
    exp: u64,
    nbf: Option<u64>,
    azp: Option<String>,
    nonce: Option<String>,
    name: Option<String>,
    nickname: Option<String>,
    preferred_username: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Audience {
    One(String),
    Many(Vec<String>),
}

/// The provider's signing keys (`jwks_uri`).
#[derive(Debug, Deserialize)]
pub struct Jwks {
    keys: Vec<Jwk>,
}

#[derive(Debug, Deserialize)]
struct Jwk {
    kty: String,
    kid: Option<String>,
    n: Option<String>,
    e: Option<String>,
    #[serde(rename = "use")]
    usage: Option<String>,
    alg: Option<String>,
}

impl Jwk {
    fn usable_for_rs256(&self) -> bool {
        self.kty == "RSA"
            && self.usage.as_deref().is_none_or(|u| u == "sig")
            && self.alg.as_deref().is_none_or(|a| a == "RS256")
    }

    fn public_key(&self) -> Option<PublicKey> {
        let n = URL_SAFE_NO_PAD.decode(self.n.as_deref()?).ok()?;
        let e = URL_SAFE_NO_PAD.decode(self.e.as_deref()?).ok()?;
        let bits = |bytes: &[u8]| u32::try_from(bytes.len() * 8).ok();
        let n = BoxedUint::from_be_slice(&n, bits(&n)?).ok()?;
        let e = BoxedUint::from_be_slice(&e, bits(&e)?).ok()?;
        PublicKey::new(n, e).ok()
    }
}

/// Verifies an id token and returns who it identifies. Only `RS256` is
/// accepted (`none` and HMAC algorithms are refused outright); the signing
/// key is chosen by `kid` among the provider's keys.
pub fn verify_id_token(
    token: &str,
    jwks: &Jwks,
    expect: Expect<'_>,
) -> Result<Identity, LoginError> {
    if token.len() > MAX_TOKEN_LEN {
        return Err(LoginError);
    }
    let mut parts = token.split('.');
    let (Some(header_b64), Some(payload_b64), Some(signature_b64), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(LoginError);
    };
    let decode = |part: &str| URL_SAFE_NO_PAD.decode(part).map_err(|_| LoginError);
    let header: JwtHeader = serde_json::from_slice(&decode(header_b64)?).map_err(|_| LoginError)?;
    if header.alg != "RS256" {
        return Err(LoginError);
    }
    let candidates: Vec<&Jwk> = jwks
        .keys
        .iter()
        .filter(|k| k.usable_for_rs256())
        .filter(|k| header.kid.is_none() || k.kid == header.kid)
        .collect();
    // Without a `kid` the key must be unambiguous.
    let [key] = candidates.as_slice() else {
        return Err(LoginError);
    };
    let public_key = key.public_key().ok_or(LoginError)?;
    let signing_input = format!("{header_b64}.{payload_b64}");
    let digest = Sha256::digest(signing_input.as_bytes());
    public_key
        .verify(
            Pkcs1v15Sign::new::<Sha256>(),
            &digest,
            &decode(signature_b64)?,
        )
        .map_err(|_| LoginError)?;

    let claims: Claims = serde_json::from_slice(&decode(payload_b64)?).map_err(|_| LoginError)?;
    if claims.iss != expect.issuer {
        return Err(LoginError);
    }
    let audience_ok = match &claims.aud {
        Audience::One(aud) => aud == expect.client_id,
        Audience::Many(auds) => {
            auds.iter().any(|a| a == expect.client_id)
                && (auds.len() == 1 || claims.azp.as_deref() == Some(expect.client_id))
        }
    };
    if !audience_ok {
        return Err(LoginError);
    }
    let now_s = expect.now_ms / 1000;
    let leeway_s = LEEWAY_MS / 1000;
    if claims.exp.saturating_add(leeway_s) <= now_s {
        return Err(LoginError);
    }
    if claims
        .nbf
        .is_some_and(|nbf| nbf > now_s.saturating_add(leeway_s))
    {
        return Err(LoginError);
    }
    // Compare the nonce in constant time like the other secrets.
    let nonce_ok = claims
        .nonce
        .as_deref()
        .is_some_and(|n| crate::token::secure_eq(n.as_bytes(), expect.nonce.as_bytes()));
    if !nonce_ok || claims.sub.is_empty() || claims.sub.len() > MAX_SUB_LEN {
        return Err(LoginError);
    }
    let display_name = [&claims.name, &claims.nickname, &claims.preferred_username]
        .into_iter()
        .flatten()
        .map(|n| n.trim())
        .find(|n| !n.is_empty())
        .map_or_else(|| claims.sub.clone(), |n| n.chars().take(100).collect());
    Ok(Identity {
        sub: claims.sub,
        display_name,
    })
}

struct Pending {
    pair: String,
    verifier: String,
    nonce: String,
    expires_ms: u64,
}

struct Session {
    identity: Identity,
    expires_ms: u64,
}

#[derive(Default)]
struct Inner {
    pending: HashMap<String, Pending>,
    /// Keyed by the SHA-256 hex of the session token.
    sessions: HashMap<String, Session>,
}

pub struct OidcProvider {
    settings: OidcSettings,
    clock: Clock,
    inner: Mutex<Inner>,
}

#[derive(Deserialize)]
struct TokenResponse {
    id_token: String,
}

fn http_agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(HTTP_TIMEOUT))
        .build()
        .into()
}

fn read_json<T: serde::de::DeserializeOwned>(
    mut response: ureq::http::Response<ureq::Body>,
) -> Result<T, LoginError> {
    if response.status().as_u16() != 200 {
        return Err(LoginError);
    }
    let bytes = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_vec()
        .map_err(|_| LoginError)?;
    serde_json::from_slice(&bytes).map_err(|_| LoginError)
}

impl OidcProvider {
    pub fn new(settings: OidcSettings, clock: Clock) -> Self {
        Self {
            settings,
            clock,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn now_ms(&self) -> u64 {
        (self.clock)() / 1000
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn session_key(token: &str) -> String {
        hex_encode(&hash_secret(token))
    }

    fn authorize_url(&self, state: &str, nonce: &str, verifier: &str) -> String {
        let s = &self.settings;
        let query = [
            ("response_type", "code"),
            ("client_id", s.client_id.as_str()),
            ("redirect_uri", s.redirect_url.as_str()),
            ("scope", "openid profile"),
            ("state", state),
            ("nonce", nonce),
            ("code_challenge", &pkce_challenge(verifier)),
            ("code_challenge_method", "S256"),
        ]
        .iter()
        .map(|(k, v)| format!("{k}={}", url_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
        let separator = if s.authorization_endpoint.contains('?') {
            '&'
        } else {
            '?'
        };
        format!("{}{separator}{query}", s.authorization_endpoint)
    }
}

impl AuthProvider for OidcProvider {
    fn id(&self) -> &'static str {
        "hackclub"
    }

    fn begin(&self, pair: &str, session: Option<&str>) -> AuthStep {
        let now = self.now_ms();
        let mut inner = self.lock();
        if let Some(token) = session
            && let Some(found) = inner.sessions.get(&Self::session_key(token))
            && found.expires_ms > now
        {
            return AuthStep::Identified(found.identity.clone());
        }
        inner.pending.retain(|_, p| p.expires_ms > now);
        if inner.pending.len() >= MAX_ENTRIES {
            inner.pending.clear();
        }
        let (state, nonce, verifier) = (random_hex(16), random_hex(16), new_verifier());
        let url = self.authorize_url(&state, &nonce, &verifier);
        inner.pending.insert(
            state,
            Pending {
                pair: pair.to_owned(),
                verifier,
                nonce,
                expires_ms: now + PENDING_TTL_MS,
            },
        );
        AuthStep::Redirect(url)
    }

    fn callback<'a>(
        &'a self,
        code: &'a str,
        state: &'a str,
    ) -> BoxFuture<'a, Result<LoginDone, LoginError>> {
        Box::pin(async move {
            // The state is consumed whether or not the rest succeeds: one
            // redirect, one attempt.
            let pending = self.lock().pending.remove(state).ok_or(LoginError)?;
            if pending.expires_ms <= self.now_ms() || code.is_empty() || code.len() > 2048 {
                return Err(LoginError);
            }
            let settings = self.settings.clone();
            let (code, verifier) = (code.to_owned(), pending.verifier.clone());
            // Blocking HTTP on a blocking thread.
            let (token, jwks) = tokio::task::spawn_blocking(move || {
                let agent = http_agent();
                let response = agent
                    .post(&settings.token_endpoint)
                    .send_form([
                        ("grant_type", "authorization_code"),
                        ("code", code.as_str()),
                        ("redirect_uri", settings.redirect_url.as_str()),
                        ("client_id", settings.client_id.as_str()),
                        ("client_secret", settings.client_secret.as_str()),
                        ("code_verifier", verifier.as_str()),
                    ])
                    .map_err(|_| LoginError)?;
                let token: TokenResponse = read_json(response)?;
                let jwks: Jwks = read_json(
                    agent
                        .get(&settings.jwks_uri)
                        .call()
                        .map_err(|_| LoginError)?,
                )?;
                Ok::<_, LoginError>((token.id_token, jwks))
            })
            .await
            .map_err(|_| LoginError)??;
            let now = self.now_ms();
            let identity = verify_id_token(
                &token,
                &jwks,
                Expect {
                    issuer: &self.settings.issuer,
                    client_id: &self.settings.client_id,
                    nonce: &pending.nonce,
                    now_ms: now,
                },
            )?;
            let session = random_hex(32);
            let mut inner = self.lock();
            inner.sessions.retain(|_, s| s.expires_ms > now);
            if inner.sessions.len() >= MAX_ENTRIES {
                inner.sessions.clear();
            }
            inner.sessions.insert(
                Self::session_key(&session),
                Session {
                    identity,
                    expires_ms: now + SESSION_TTL_MS,
                },
            );
            Ok(LoginDone {
                session,
                pair: pending.pair,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pkce_challenge_matches_the_rfc_7636_example() {
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn url_encoding_keeps_only_unreserved_characters() {
        assert_eq!(url_encode("openid profile"), "openid%20profile");
        assert_eq!(
            url_encode("http://a/b?c=d&e"),
            "http%3A%2F%2Fa%2Fb%3Fc%3Dd%26e"
        );
        assert_eq!(url_encode("A-z_0.9~"), "A-z_0.9~");
    }

    #[test]
    fn endpoints_must_be_https_except_on_loopback() {
        assert!(endpoint_ok("https://auth.hackclub.com"));
        assert!(endpoint_ok("http://127.0.0.1:9/x"));
        assert!(endpoint_ok("http://localhost"));
        assert!(!endpoint_ok("http://auth.hackclub.com"));
        assert!(!endpoint_ok("http://127.0.0.1.evil.example/x"));
        assert!(!endpoint_ok("ftp://x"));
    }

    #[test]
    fn settings_default_to_the_hack_club_endpoints_and_need_a_client_id() {
        let mut auth = AuthConfig {
            provider: "hackclub".into(),
            ..AuthConfig::default()
        };
        assert!(OidcSettings::from_config(&auth, "s".into(), "https://t.example").is_err());
        auth.client_id = "cid".into();
        let settings = OidcSettings::from_config(&auth, "s".into(), "https://t.example/").unwrap();
        assert_eq!(
            settings.authorization_endpoint,
            "https://auth.hackclub.com/oauth/authorize"
        );
        assert_eq!(settings.redirect_url, "https://t.example/auth/callback");
        auth.token_endpoint = Some("http://evil.example/token".into());
        assert!(OidcSettings::from_config(&auth, "s".into(), "https://t.example").is_err());
    }
}
