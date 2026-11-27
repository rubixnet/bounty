//! Seam: S3b ecosystem features against a real server and Postgres 17 (Linux
//! CI; set `TRACTION_TEST_DATABASE_URL` to run locally): soft build
//! provenance, the Project merge API and GitHub linking.

mod common;

use common::*;
use traction_server::config::Config;

const T0: u64 = 1_800_000_000_000;

async fn tag_of(server: &TestServer, device_id: &str) -> (String, Option<String>) {
    sqlx::query_as("SELECT build_tag, build_version FROM devices WHERE id = $1")
        .bind(device_id)
        .fetch_one(&server.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn pairing_tags_the_device_by_the_build_statement_it_presented() {
    let Some(url) = database_url() else { return };
    let server = start(&url, config_trusting_ci(), Some(fixed_clock(T0))).await;

    // No statement: a developer build.
    let (_, plain) = pair(&server, &device_key(1), None).await;
    assert_eq!(tag_of(&server, &plain).await, ("self_built".into(), None));

    // Signed by the configured CI key.
    let statement = build_statement(&ci_key());
    let (_, signed) = pair_with_build(&server, &device_key(2), None, Some(statement.clone())).await;
    assert_eq!(
        tag_of(&server, &signed).await,
        ("ci_signed".into(), Some("0.1.0".into()))
    );

    // Signed by some other key (a fork's own key): self_built, and the
    // unverified claim is not recorded.
    let forged = build_statement(&device_key(99));
    let (_, fork) = pair_with_build(&server, &device_key(3), None, Some(forged)).await;
    assert_eq!(tag_of(&server, &fork).await, ("self_built".into(), None));

    // A statement with a tampered field fails the signature.
    let mut tampered = statement;
    tampered.version = "9.9.9".into();
    let (_, bad) = pair_with_build(&server, &device_key(4), None, Some(tampered)).await;
    assert_eq!(tag_of(&server, &bad).await, ("self_built".into(), None));
}

#[tokio::test]
async fn a_heartbeat_with_a_statement_retags_the_device_when_the_version_changes() {
    let Some(url) = database_url() else { return };
    let server = start(&url, config_trusting_ci(), Some(fixed_clock(T0 + 400_000))).await;
    let key = device_key(1);
    let (token, device) = pair(&server, &key, None).await;
    assert_eq!(tag_of(&server, &device).await.0, "self_built");

    // The daemon was upgraded to a CI build: the first heartbeat carries it.
    let mut request = heartbeat(&key, 0, 'a', T0, T0 + 400_000);
    request.build = Some(build_statement(&ci_key()));
    assert_eq!(send_heartbeat(&server, &token, &request).await.status, 200);
    assert_eq!(
        tag_of(&server, &device).await,
        ("ci_signed".into(), Some("0.1.0".into()))
    );

    // A later heartbeat with a copied-from-elsewhere statement downgrades it.
    let mut request = heartbeat(&key, 1, 'b', T0 + 120_000, T0 + 400_000);
    request.build = Some(build_statement(&device_key(99)));
    assert_eq!(send_heartbeat(&server, &token, &request).await.status, 200);
    assert_eq!(tag_of(&server, &device).await.0, "self_built");

    // A heartbeat that carries no statement leaves the tag alone.
    let mut request = heartbeat(&key, 2, 'c', T0 + 240_000, T0 + 400_000);
    request.build = Some(build_statement(&ci_key()));
    send_heartbeat(&server, &token, &request).await;
    let request = heartbeat(&key, 3, 'd', T0 + 360_000, T0 + 400_000);
    send_heartbeat(&server, &token, &request).await;
    assert_eq!(tag_of(&server, &device).await.0, "ci_signed");
}

#[tokio::test]
async fn a_config_with_a_malformed_ci_key_is_reported_not_trusted() {
    let mut config = Config::default();
    config.provenance.ci_public_keys = vec!["not-a-key".into()];
    assert!(config.provenance.keys().is_err());
}
