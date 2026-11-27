//! Seam: the Hack Club OIDC `AuthProvider` against a test identity provider
//! that runs inside the test and issues real RS256-signed tokens. Needs no
//! database, so it runs on all 3 OSes.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::idp::{self, Behaviour, CLIENT_ID, CLIENT_SECRET, Idp};
use traction_server::app::{Clock, system_clock};
use traction_server::auth::{AuthProvider, AuthStep, Identity, LoginDone, LoginError};
use traction_server::oidc::{OidcProvider, OidcSettings, SESSION_TTL_MS, pkce_challenge};

const REDIRECT: &str = "http://127.0.0.1:9/auth/callback";

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn provider(idp: &Idp, clock: Clock) -> OidcProvider {
    OidcProvider::new(
        OidcSettings {
            issuer: idp.issuer(),
            client_id: CLIENT_ID.into(),
            client_secret: CLIENT_SECRET.into(),
            authorization_endpoint: format!("{}/authorize", idp.base),
            token_endpoint: format!("{}/token", idp.base),
            jwks_uri: format!("{}/keys", idp.base),
            redirect_url: REDIRECT.into(),
        },
        clock,
    )
}

fn query_of(url: &str) -> std::collections::HashMap<String, String> {
    url.split_once('?')
        .map(|(_, q)| q)
        .unwrap_or_default()
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect()
}

/// What the browser does: open the authorization URL at the identity
/// provider and read the redirect back to the callback.
async fn follow(url: String) -> (u16, Option<String>) {
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .into();
        let response = agent.get(&url).call().unwrap();
        let location = response
            .headers()
            .get("location")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        (response.status().as_u16(), location)
    })
    .await
    .unwrap()
}

async fn login(provider: &OidcProvider, pair: &str) -> Result<LoginDone, LoginError> {
    let AuthStep::Redirect(url) = provider.begin(pair, None) else {
        panic!("a browser without a session is sent to the identity provider");
    };
    let (status, location) = follow(url).await;
    assert_eq!(status, 302);
    let query = query_of(&location.unwrap());
    provider.callback(&query["code"], &query["state"]).await
}

#[tokio::test]
async fn a_login_runs_code_plus_pkce_and_ends_in_a_session_for_the_pairing() {
    let idp = idp::start(now_ms()).await;
    let provider = provider(&idp, system_clock());

    let AuthStep::Redirect(url) = provider.begin("PAIR-1", None) else {
        panic!("expected a redirect");
    };
    assert!(url.starts_with(&format!("{}/authorize?", idp.base)));
    let q = query_of(&url);
    assert_eq!(q["response_type"], "code");
    assert_eq!(q["client_id"], CLIENT_ID);
    assert_eq!(q["code_challenge_method"], "S256");
    assert_eq!(q["scope"], "openid%20profile");
    assert_eq!(
        q["redirect_uri"],
        "http%3A%2F%2F127.0.0.1%3A9%2Fauth%2Fcallback"
    );
    assert_eq!(
        q["code_challenge"].len(),
        43,
        "an S256 challenge is 43 characters"
    );
    assert!(q["state"].len() >= 32 && q["nonce"].len() >= 32);
    assert!(
        !url.contains(CLIENT_SECRET),
        "the secret never travels in the browser URL"
    );

    // The identity provider checks the PKCE verifier against that challenge
    // when the code is exchanged, so a finished login proves the whole flow.
    let (_, location) = follow(url).await;
    let callback = query_of(&location.unwrap());
    let done = provider
        .callback(&callback["code"], &callback["state"])
        .await
        .unwrap();
    assert_eq!(done.pair, "PAIR-1");
    assert_eq!(
        provider.begin("PAIR-1", Some(&done.session)),
        AuthStep::Identified(Identity {
            sub: "hc-user-1".into(),
            display_name: "Ada Lovelace".into(),
        })
    );
    // A different or forged session is just another trip to the provider.
    assert!(matches!(
        provider.begin("PAIR-1", Some("forged")),
        AuthStep::Redirect(_)
    ));
    assert!(matches!(
        provider.begin("PAIR-1", None),
        AuthStep::Redirect(_)
    ));
    // The helper agrees with the library's PKCE (RFC 7636 sample).
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[tokio::test]
async fn a_state_works_once_and_an_unknown_state_never() {
    let idp = idp::start(now_ms()).await;
    let provider = provider(&idp, system_clock());
    let AuthStep::Redirect(url) = provider.begin("P", None) else {
        panic!()
    };
    let (_, location) = follow(url).await;
    let q = query_of(&location.unwrap());
    assert!(provider.callback(&q["code"], &q["state"]).await.is_ok());
    // Replaying the same redirect fails, before any request to the provider.
    let requests = *idp.state.token_requests.lock().unwrap();
    assert_eq!(
        provider.callback(&q["code"], &q["state"]).await,
        Err(LoginError)
    );
    assert_eq!(*idp.state.token_requests.lock().unwrap(), requests);
    assert_eq!(
        provider.callback("code-1", "made-up-state").await,
        Err(LoginError)
    );
}

#[tokio::test]
async fn every_way_of_issuing_a_bad_id_token_is_refused() {
    let idp = idp::start(now_ms()).await;
    let provider = provider(&idp, system_clock());
    for behaviour in [
        Behaviour::WrongAudience,
        Behaviour::Expired,
        Behaviour::WrongNonce,
        Behaviour::NoNonce,
        Behaviour::WrongIssuer,
        Behaviour::SignedByAnotherKey,
        Behaviour::AlgNone,
        Behaviour::AlgHs256,
        Behaviour::TamperedSignature,
        Behaviour::TamperedClaims,
    ] {
        idp.set_behaviour(behaviour);
        assert_eq!(
            login(&provider, "P").await,
            Err(LoginError),
            "{behaviour:?}"
        );
    }
    // And the honest provider still works afterwards.
    idp.set_behaviour(Behaviour::Good);
    assert!(login(&provider, "P").await.is_ok());
}

#[tokio::test]
async fn a_session_expires() {
    let idp = idp::start(1_000_000_000_000).await;
    let now = Arc::new(AtomicU64::new(1_000_000_000_000 * 1000));
    let clock: Clock = {
        let now = Arc::clone(&now);
        Arc::new(move || now.load(Ordering::SeqCst))
    };
    let provider = provider(&idp, clock);
    let done = login(&provider, "P").await.unwrap();
    assert!(matches!(
        provider.begin("P", Some(&done.session)),
        AuthStep::Identified(_)
    ));
    now.fetch_add((SESSION_TTL_MS + 1) * 1000, Ordering::SeqCst);
    assert!(matches!(
        provider.begin("P", Some(&done.session)),
        AuthStep::Redirect(_)
    ));
}

#[test]
fn the_hackclub_provider_needs_its_secret_from_the_environment_or_a_file() {
    let config = traction_server::config::Config::parse(
        "[auth]\nprovider = \"hackclub\"\nclient_id = \"x\"\nclient_secret_env = \"TRACTION_TEST_NO_SUCH_VAR\"\n",
    )
    .unwrap();
    assert!(config.auth.client_secret(|_| None).is_err());
    assert_eq!(
        config
            .auth
            .client_secret(|name| (name == "TRACTION_TEST_NO_SUCH_VAR").then(|| "s3".into()))
            .unwrap(),
        "s3"
    );
    // An inline secret is refused at load time.
    assert!(
        traction_server::config::Config::parse("[auth]\nclient_secret = \"hunter2\"\n").is_err()
    );
}
