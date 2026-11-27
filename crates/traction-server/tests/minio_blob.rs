//! Seam: `R2BlobStore` against a real S3-compatible store (MinIO), no
//! Postgres needed. Linux CI starts MinIO and sets
//! `TRACTION_TEST_MINIO_ENDPOINT`, `_BUCKET` (default `traction-test`),
//! `_ACCESS_KEY` and `_SECRET_KEY`; `TRACTION_REQUIRE_MINIO=1` turns a missing
//! endpoint into a failure so CI never skips silently.

use std::sync::Arc;

use traction_server::blob::{BlobStore, R2BlobStore, sigv4::Credentials};

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn store() -> Option<R2BlobStore> {
    let Some(endpoint) = env("TRACTION_TEST_MINIO_ENDPOINT") else {
        if env("TRACTION_REQUIRE_MINIO").is_some_and(|v| v == "1") {
            panic!("TRACTION_REQUIRE_MINIO=1 but TRACTION_TEST_MINIO_ENDPOINT is not set");
        }
        eprintln!("SKIPPED (no TRACTION_TEST_MINIO_ENDPOINT): MinIO-backed test");
        return None;
    };
    Some(
        R2BlobStore::new(
            Credentials {
                access_key_id: env("TRACTION_TEST_MINIO_ACCESS_KEY").expect("access key"),
                secret_access_key: env("TRACTION_TEST_MINIO_SECRET_KEY").expect("secret key"),
            },
            &endpoint,
            &env("TRACTION_TEST_MINIO_BUCKET").unwrap_or_else(|| "traction-test".into()),
            "us-east-1",
            64 * 1024 * 1024,
            900,
            Arc::new(now_ms),
        )
        .unwrap(),
    )
}

fn put(url: &str, body: &[u8]) -> u16 {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    agent.put(url).send(body).unwrap().status().as_u16()
}

async fn put_async(url: String, body: Vec<u8>) -> u16 {
    tokio::task::spawn_blocking(move || put(&url, &body))
        .await
        .unwrap()
}

#[tokio::test]
async fn presigned_put_get_and_delete_round_trip_with_the_exact_content_length() {
    let Some(store) = store() else { return };
    let key = format!(
        "bundles/test-{}.json",
        traction_server::token::random_hex(6)
    );
    let body = b"{\"hello\":\"minio\"}".to_vec();

    let target = store
        .presign_put(&key, body.len() as u64, now_ms())
        .unwrap();
    // A body of another length does not match the signed Content-Length.
    let wrong = put_async(target.url.clone(), b"{\"hello\":\"minio!!!\"}".to_vec()).await;
    assert_eq!(wrong, 403);
    assert_eq!(store.get(&key).await.unwrap(), None);

    assert_eq!(put_async(target.url.clone(), body.clone()).await, 200);
    assert_eq!(store.get(&key).await.unwrap(), Some(body));
    store.delete(&key).await.unwrap();
    store.delete(&key).await.unwrap();
    assert_eq!(store.get(&key).await.unwrap(), None);
}

#[tokio::test]
async fn an_expired_or_tampered_url_is_refused_by_the_store() {
    let Some(store) = store() else { return };
    let key = format!(
        "bundles/test-{}.json",
        traction_server::token::random_hex(6)
    );
    // Signed 20 minutes ago with a 15-minute expiry.
    let old = store.presign_put(&key, 2, now_ms() - 20 * 60_000).unwrap();
    assert_eq!(put_async(old.url, b"{}".to_vec()).await, 403);
    let fresh = store.presign_put(&key, 2, now_ms()).unwrap();
    let tampered = fresh.url.replace("X-Amz-Expires=900", "X-Amz-Expires=9000");
    assert_eq!(put_async(tampered, b"{}".to_vec()).await, 403);
    assert_eq!(store.get(&key).await.unwrap(), None);
}
