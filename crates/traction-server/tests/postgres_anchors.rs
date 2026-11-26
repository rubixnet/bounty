//! Seam: the real `traction-server` over a real Postgres 17 (Linux CI; set
//! `TRACTION_TEST_DATABASE_URL` to run locally). Pairing, the heartbeat,
//! idempotency and `409`, `future_window` / `window_too_old`, partitions,
//! rate limiting, revocation, re-pairing and the SSE live totals.

mod common;

use common::*;
use traction_core::sync::{AnchorStatus, ErrorBody, HeartbeatResponse};
use traction_server::config::Config;
use traction_server::fanout::{LiveEvent, Totals};
use traction_server::token::{hash_secret, hex_encode};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn recent_window() -> u64 {
    now_ms() / 120_000 * 120_000 - 4 * 120_000
}

fn error_code(reply: &Reply) -> String {
    serde_json::from_str::<ErrorBody>(&reply.body)
        .unwrap()
        .error
}

#[tokio::test]
async fn pairing_needs_an_explicit_csrf_checked_approval_and_delivers_the_token_once() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let key = device_key(1);
    let start_reply = post_json(
        format!("{}/v1/pairings", server.base),
        None,
        serde_json::json!({"public_key": key.verifying_key().to_hex(), "previous_key_id": null, "device_label": "lap"}).to_string(),
    )
    .await;
    let started: traction_core::sync::PairingStartResponse =
        serde_json::from_str(&start_reply.body).unwrap();
    let poll_body =
        serde_json::json!({"pair_code": started.pair_code, "poll_secret": started.poll_secret})
            .to_string();

    // A GET shows the page and approves nothing.
    let page = http("GET", started.login_url.clone(), None, vec![], None).await;
    assert!(page.body.contains("Approve device") && page.body.contains(&started.pair_code));
    let polled = post_json(
        format!("{}/v1/pairings/poll", server.base),
        None,
        poll_body.clone(),
    )
    .await;
    assert!(polled.body.contains("pending"), "{}", polled.body);

    // A POST without the cookie/field pair, or from a foreign origin, is refused.
    let form = |csrf: &str| {
        Some((
            format!("pair={}&csrf={csrf}", started.pair_code),
            "application/x-www-form-urlencoded",
        ))
    };
    let no_cookie = http(
        "POST",
        format!("{}/auth/approve", server.base),
        None,
        vec![],
        form(&"a".repeat(64)),
    )
    .await;
    assert_eq!(no_cookie.status, 403);
    let csrf = "c".repeat(64);
    let mismatch = http(
        "POST",
        format!("{}/auth/approve", server.base),
        None,
        vec![("cookie", format!("traction_csrf={}", "d".repeat(64)))],
        form(&csrf),
    )
    .await;
    assert_eq!(mismatch.status, 403);
    let foreign = http(
        "POST",
        format!("{}/auth/approve", server.base),
        None,
        vec![
            ("cookie", format!("traction_csrf={csrf}")),
            ("origin", "http://evil.example".into()),
        ],
        form(&csrf),
    )
    .await;
    assert_eq!(foreign.status, 403);
    let still = post_json(
        format!("{}/v1/pairings/poll", server.base),
        None,
        poll_body.clone(),
    )
    .await;
    assert!(still.body.contains("pending"));

    // The real approval.
    let ok = http(
        "POST",
        format!("{}/auth/approve", server.base),
        None,
        vec![("cookie", format!("traction_csrf={csrf}"))],
        form(&csrf),
    )
    .await;
    assert_eq!(ok.status, 200, "{}", ok.body);

    // The wrong poll secret never gets the token.
    let wrong = post_json(
        format!("{}/v1/pairings/poll", server.base),
        None,
        serde_json::json!({"pair_code": started.pair_code, "poll_secret": "0".repeat(64)})
            .to_string(),
    )
    .await;
    assert_eq!(wrong.status, 401);

    let first: traction_core::sync::PairingPollResponse = serde_json::from_str(
        &post_json(
            format!("{}/v1/pairings/poll", server.base),
            None,
            poll_body.clone(),
        )
        .await
        .body,
    )
    .unwrap();
    let token = first
        .device_token
        .expect("the token arrives on the first poll");
    let second: traction_core::sync::PairingPollResponse = serde_json::from_str(
        &post_json(format!("{}/v1/pairings/poll", server.base), None, poll_body)
            .await
            .body,
    )
    .unwrap();
    assert!(second.device_token.is_none(), "the token is delivered once");

    // Only the SHA-256 of the token is stored, and the key is pinned.
    let device_id = first.device_id.unwrap();
    let stored: Vec<u8> = sqlx::query_scalar("SELECT token_hash FROM devices WHERE id = $1")
        .bind(&device_id)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(stored, hash_secret(&token).to_vec());
    assert_ne!(hex_encode(&stored), token);
    let pinned: String = sqlx::query_scalar("SELECT key_id FROM device_keys WHERE device_id = $1")
        .bind(&device_id)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(pinned, key.key_id().as_str());
}

#[tokio::test]
async fn a_heartbeat_stores_the_anchor_and_typed_rows_and_is_idempotent() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let key = device_key(2);
    let (token, device_id) = pair(&server, &key, None).await;
    let window = recent_window();
    let request = heartbeat(&key, 0, 'a', window, now_ms());

    let first = send_heartbeat(&server, &token, &request).await;
    assert_eq!(first.status, 200, "{}", first.body);
    let response: HeartbeatResponse = serde_json::from_str(&first.body).unwrap();
    assert_eq!(response.anchor, AnchorStatus::Stored);
    assert_eq!(response.accepted.len(), 3);
    assert!(response.rejected.is_empty());

    let mut conn = server.pool.acquire().await.unwrap();
    let buckets = traction_server::store::fetch_buckets(&mut conn, &device_id)
        .await
        .unwrap();
    assert_eq!(buckets, request.work_buckets);
    let rollups = traction_server::store::fetch_rollups(&mut conn, &device_id)
        .await
        .unwrap();
    assert_eq!(rollups, request.activity_rollups);
    let project: String = sqlx::query_scalar("SELECT project_name FROM projects")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(project, "demo");

    // The same tip again: idempotent, no new rows, the Anchor unchanged.
    let again = send_heartbeat(&server, &token, &request).await;
    assert_eq!(again.status, 200);
    let again: HeartbeatResponse = serde_json::from_str(&again.body).unwrap();
    assert_eq!(again.anchor, AnchorStatus::Duplicate);
    assert_eq!(again.server_received_at_ms, response.server_received_at_ms);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM work_buckets")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(count, 2);

    // A different entry_hash for the same (key_id, seq) is 409 and changes nothing.
    let mut forged = request.clone();
    forged.tip.entry_hash = "b".repeat(64);
    let conflict = send_heartbeat(&server, &token, &forged).await;
    assert_eq!(conflict.status, 409);
    assert_eq!(error_code(&conflict), "anchor_conflict");
    let stored: String = sqlx::query_scalar("SELECT entry_hash FROM anchors")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(stored, "a".repeat(64));
}

#[tokio::test]
async fn server_received_at_is_monotonic_per_device() {
    let Some(url) = database_url() else { return };
    // A frozen clock: every heartbeat arrives "at the same instant".
    let now = now_ms();
    let server = start(&url, Config::default(), Some(fixed_clock(now))).await;
    let key = device_key(3);
    let (token, _) = pair(&server, &key, None).await;
    let window = now / 120_000 * 120_000 - 120_000;
    let mut times = Vec::new();
    for seq in 0..3u64 {
        let r = send_heartbeat(
            &server,
            &token,
            &heartbeat(&key, seq, 'a', window + seq * 120_000 - 240_000, now),
        )
        .await;
        assert_eq!(r.status, 200, "{}", r.body);
        times.push(
            serde_json::from_str::<HeartbeatResponse>(&r.body)
                .unwrap()
                .server_received_at_ms,
        );
    }
    let micros: Vec<i64> =
        sqlx::query_scalar("SELECT server_received_us FROM anchors ORDER BY seq")
            .fetch_all(&server.pool)
            .await
            .unwrap();
    assert!(micros.windows(2).all(|w| w[0] < w[1]), "{micros:?}");
}

#[tokio::test]
async fn future_and_too_old_windows_are_422_and_the_wrong_key_is_403() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let key = device_key(4);
    let (token, _) = pair(&server, &key, None).await;
    let now = now_ms();

    let future = heartbeat(&key, 0, 'a', now / 120_000 * 120_000 + 3 * 120_000, now);
    let reply = send_heartbeat(&server, &token, &future).await;
    assert_eq!(
        (reply.status, error_code(&reply).as_str()),
        (422, "future_window")
    );

    let age = 46 * 24 * 60 * 60 * 1000;
    let old = heartbeat(&key, 1, 'a', (now - age) / 120_000 * 120_000, now);
    let reply = send_heartbeat(&server, &token, &old).await;
    assert_eq!(
        (reply.status, error_code(&reply).as_str()),
        (422, "window_too_old")
    );

    let stranger = device_key(99);
    let reply = send_heartbeat(
        &server,
        &token,
        &heartbeat(&stranger, 0, 'a', recent_window(), now),
    )
    .await;
    assert_eq!(
        (reply.status, error_code(&reply).as_str()),
        (403, "key_not_pinned")
    );

    let reply = post_json(
        format!("{}/v1/anchors", server.base),
        Some(token.clone()),
        "{\"tip\": 1}".into(),
    )
    .await;
    assert_eq!(reply.status, 400);
    let reply = send_heartbeat(&server, "0".repeat(64).as_str(), &future).await;
    assert_eq!(reply.status, 401);
}

#[tokio::test]
async fn invalid_items_are_rejected_with_codes_and_a_too_old_item_alongside_a_good_tip() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let key = device_key(5);
    let (token, _) = pair(&server, &key, None).await;
    let now = now_ms();
    let window = recent_window();
    let mut request = heartbeat(&key, 0, 'a', window, now);
    request.work_buckets[0].human_active_sec = 99;
    let old_minute = (now - 50 * 24 * 60 * 60 * 1000) / 60_000 * 60_000;
    request.work_buckets.push(bucket(old_minute, 10));
    let reply = send_heartbeat(&server, &token, &request).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let response: HeartbeatResponse = serde_json::from_str(&reply.body).unwrap();
    let codes: Vec<&str> = response.rejected.iter().map(|r| r.code.as_str()).collect();
    assert_eq!(codes, ["human_active_over_minute", "window_too_old"]);
}

#[tokio::test]
async fn partitions_are_created_at_startup_and_on_demand_with_no_default() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let names = |table: &'static str| {
        let pool = server.pool.clone();
        async move {
            sqlx::query_scalar::<_, String>(
                "SELECT c.relname FROM pg_inherits i
                 JOIN pg_class c ON c.oid = i.inhrelid
                 JOIN pg_class p ON p.oid = i.inhparent
                 WHERE p.relname = $1 ORDER BY c.relname",
            )
            .bind(table)
            .fetch_all(&pool)
            .await
            .unwrap()
        }
    };
    let months = traction_server::partition::standard_window(now_ms());
    for table in ["work_buckets", "activity_rollups", "anchors"] {
        let found = names(table).await;
        assert_eq!(found.len(), 4, "{table}: {found:?}");
        for month in months {
            let suffix = format!("{table}_{:04}_{:02}", month.year, month.month);
            assert!(found.contains(&suffix), "{suffix} missing from {found:?}");
        }
    }
    let defaults: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_class WHERE relispartition AND pg_get_expr(relpartbound, oid) = 'DEFAULT'",
    )
    .fetch_one(&server.pool)
    .await
    .unwrap();
    assert_eq!(defaults, 0);

    // A backlog window 35 days back lands in a partition made on demand.
    let key = device_key(6);
    let (token, device_id) = pair(&server, &key, None).await;
    let now = now_ms();
    let window = (now - 35 * 24 * 60 * 60 * 1000) / 120_000 * 120_000;
    let before = names("work_buckets").await.len();
    let reply = send_heartbeat(&server, &token, &heartbeat(&key, 0, 'a', window, now)).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let mut conn = server.pool.acquire().await.unwrap();
    assert_eq!(
        traction_server::store::fetch_buckets(&mut conn, &device_id)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(names("work_buckets").await.len() >= before);
    let month = traction_server::partition::Month::of_ms(window);
    assert!(names("work_buckets").await.contains(&format!(
        "work_buckets_{:04}_{:02}",
        month.year, month.month
    )));
}

#[tokio::test]
async fn a_row_outside_every_partition_is_a_structured_error_not_a_default() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let err = sqlx::query(
        "INSERT INTO anchors (device_id, key_id, seq, entry_hash, window_start_ms, sync_mode,
            sent_at_ms, server_received_us, server_received_at)
         VALUES ('d','k',0,'h',0,'realtime',0,0,'1990-01-01+00')",
    )
    .execute(&server.pool)
    .await
    .unwrap_err();
    assert!(traction_server::error::ApiError::database(&err).code == "no_partition");
}

#[tokio::test]
async fn the_rate_limit_answers_429_with_retry_after() {
    let Some(url) = database_url() else { return };
    let mut config = Config::default();
    config.anchor_policy.rate_limit_burst = 2;
    config.anchor_policy.rate_limit_refill_ms = 60_000;
    let server = start(&url, config, None).await;
    let key = device_key(7);
    let (token, _) = pair(&server, &key, None).await;
    let request = heartbeat(&key, 0, 'a', recent_window(), now_ms());
    assert_eq!(send_heartbeat(&server, &token, &request).await.status, 200);
    assert_eq!(send_heartbeat(&server, &token, &request).await.status, 200);
    let limited = send_heartbeat(&server, &token, &request).await;
    assert_eq!(limited.status, 429);
    assert!(
        limited
            .retry_after
            .as_deref()
            .and_then(|v| v.parse::<u64>().ok())
            .is_some_and(|s| s >= 1)
    );
}

#[tokio::test]
async fn revoking_a_device_stops_new_anchors_but_keeps_its_keys() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let key = device_key(8);
    let (token, device_id) = pair(&server, &key, None).await;
    let request = heartbeat(&key, 0, 'a', recent_window(), now_ms());
    assert_eq!(send_heartbeat(&server, &token, &request).await.status, 200);

    let revoke = http(
        "DELETE",
        format!("{}/v1/devices/{device_id}", server.base),
        Some(token.clone()),
        vec![],
        None,
    )
    .await;
    assert_eq!(revoke.status, 204);
    let next = heartbeat(&key, 1, 'c', recent_window() + 120_000, now_ms());
    assert_eq!(send_heartbeat(&server, &token, &next).await.status, 401);
    let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM device_keys WHERE device_id = $1")
        .bind(&device_id)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(keys, 1);
    let anchors: i64 = sqlx::query_scalar("SELECT count(*) FROM anchors")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(anchors, 1);
}

#[tokio::test]
async fn re_pairing_a_successor_chain_keeps_every_historical_key_trusted() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let old = device_key(10);
    let (old_token, device_id) = pair(&server, &old, None).await;
    let new = device_key(11);
    let (new_token, same_device) =
        pair(&server, &new, Some(old.key_id().as_str().to_owned())).await;
    assert_eq!(
        same_device, device_id,
        "the successor key joins the same device"
    );
    assert_ne!(old_token, new_token);

    let ids: Vec<String> =
        sqlx::query_scalar("SELECT key_id FROM device_keys WHERE device_id = $1 ORDER BY key_id")
            .bind(&device_id)
            .fetch_all(&server.pool)
            .await
            .unwrap();
    let mut expected = vec![
        old.key_id().as_str().to_owned(),
        new.key_id().as_str().to_owned(),
    ];
    expected.sort();
    assert_eq!(ids, expected);

    // The new token anchors under either key; the old token no longer works.
    let window = recent_window();
    assert_eq!(
        send_heartbeat(
            &server,
            &new_token,
            &heartbeat(&old, 0, 'a', window, now_ms())
        )
        .await
        .status,
        200
    );
    assert_eq!(
        send_heartbeat(
            &server,
            &new_token,
            &heartbeat(&new, 0, 'b', window + 120_000, now_ms())
        )
        .await
        .status,
        200
    );
    assert_eq!(
        send_heartbeat(
            &server,
            &old_token,
            &heartbeat(&old, 1, 'c', window + 240_000, now_ms())
        )
        .await
        .status,
        401
    );
}

#[tokio::test]
async fn a_key_paired_to_another_account_is_refused() {
    let Some(url) = database_url() else { return };
    let mut config = Config::default();
    config.auth.dev_sub = "someone-else".into();
    let server = start(&url, config, None).await;
    let key = device_key(12);
    let (_, device_id) = pair(&server, &key, None).await;
    // Move the device to another user, then try to pair the same key as the dev user.
    sqlx::query("INSERT INTO users (id, sub, display_name) VALUES ('other','another','Other')")
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query("UPDATE devices SET user_id = 'other' WHERE id = $1")
        .bind(&device_id)
        .execute(&server.pool)
        .await
        .unwrap();
    let started = post_json(
        format!("{}/v1/pairings", server.base), None,
        serde_json::json!({"public_key": key.verifying_key().to_hex(), "previous_key_id": null, "device_label": "x"}).to_string(),
    ).await;
    let started: traction_core::sync::PairingStartResponse =
        serde_json::from_str(&started.body).unwrap();
    let csrf = "e".repeat(64);
    let approve = http(
        "POST",
        format!("{}/auth/approve", server.base),
        None,
        vec![("cookie", format!("traction_csrf={csrf}"))],
        Some((
            format!("pair={}&csrf={csrf}", started.pair_code),
            "application/x-www-form-urlencoded",
        )),
    )
    .await;
    assert_eq!(approve.status, 409);
}

#[tokio::test]
async fn the_sse_total_goes_up_after_a_heartbeat() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let key = device_key(13);
    let (token, _) = pair(&server, &key, None).await;
    let addr = server.addr;
    let sse_token = token.clone();
    let reader = tokio::task::spawn_blocking(move || {
        let mut sse = Sse::connect(addr, &sse_token);
        let first = sse.next();
        (sse, first)
    });
    let (sse, first) = reader.await.unwrap();
    assert_eq!(first.0, "totals");
    assert_eq!(
        first.1,
        r#"{"agent_compute_sec":0,"human_active_sec":0,"struggle_credit_sec":0}"#
    );

    let request = heartbeat(&key, 0, 'a', recent_window(), now_ms());
    assert_eq!(send_heartbeat(&server, &token, &request).await.status, 200);
    let (_, (event, data)) = tokio::task::spawn_blocking(move || {
        let mut sse = sse;
        let next = sse.next();
        (sse, next)
    })
    .await
    .unwrap();
    assert_eq!(event, "totals");
    // 40 + 30 s of Human Active Time in the two buckets.
    assert!(data.contains(r#""human_active_sec":70"#), "{data}");
}

#[tokio::test]
async fn a_lagging_stream_gets_a_resync_event_then_fresh_totals() {
    let Some(url) = database_url() else { return };
    // The server task cannot run while this current-thread test does not
    // await, so a flood of publishes is guaranteed to overflow the channel.
    let server = start(&url, Config::default(), None).await;
    let key = device_key(14);
    let (token, _) = pair(&server, &key, None).await;
    let addr = server.addr;
    let sse_token = token.clone();
    let (sse, first) = tokio::task::spawn_blocking(move || {
        let mut sse = Sse::connect(addr, &sse_token);
        let first = sse.next();
        (sse, first)
    })
    .await
    .unwrap();
    assert_eq!(first.0, "totals");
    let user: String = sqlx::query_scalar("SELECT user_id FROM devices LIMIT 1")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    for sec in 0..500 {
        server.state.fanout.publish(
            &user,
            LiveEvent::Totals(Totals {
                human_active_sec: sec,
                ..Totals::default()
            }),
        );
    }
    let events = tokio::task::spawn_blocking(move || {
        let mut sse = sse;
        (0..2).map(|_| sse.next().0).collect::<Vec<_>>()
    })
    .await
    .unwrap();
    assert_eq!(events, ["resync", "totals"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn buckets_round_trip_exactly_through_the_typed_columns() {
    use proptest::prelude::*;
    use traction_core::{CategoryShareMilli, StruggleAxes, TelemetryTier, WorkBucket};
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    // Partitions for the whole proptest range are created up front.
    traction_server::partition::ensure_months(
        &server.pool,
        &[
            traction_server::partition::Month {
                year: 1970,
                month: 1,
            },
            traction_server::partition::Month {
                year: 1970,
                month: 2,
            },
        ],
    )
    .await
    .unwrap();

    let tier = prop_oneof![
        Just(TelemetryTier::Full),
        Just(TelemetryTier::Reduced),
        Just(TelemetryTier::IdleOnly),
        Just(TelemetryTier::None)
    ];
    let bucket_strategy = (
        (0u64..40_000).prop_map(|m| m * 60_000),
        (
            0u32..=60,
            0u32..=60,
            0u32..=60,
            any::<u64>(),
            any::<u32>(),
            any::<u32>(),
        ),
        (any::<bool>(), any::<bool>(), tier.clone(), any::<bool>()),
        (
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
        ),
        "[0-9a-f]{64}",
    )
        .prop_map(
            |(
                minute,
                (a, i, h, wait, ch, ca),
                (inj, auto, tier, lim),
                (s1, s2, s3, s4, vs, cr),
                hash,
            )| WorkBucket {
                project_id: format!("git:{hash}"),
                minute_start_ms: minute,
                ruleset_id: "traction-rules/1".into(),
                agent_compute_sec: a,
                idle_autonomous_sec: i,
                human_active_sec: h,
                permission_wait_ms: wait % (i64::MAX as u64),
                commits_human: ch,
                commits_agent: ca,
                injected_hid_detected: inj,
                auto_approver_suspected: auto,
                telemetry_tier: tier,
                tier_limited: lim,
                struggle_axes: StruggleAxes {
                    steering: s1,
                    specificity: s2,
                    friction: s3,
                    comfort_delta: s4,
                },
                verified_struggle: vs,
                struggle_credit_sec: cr,
            },
        );
    let rollup_strategy = (
        (0u64..20_000).prop_map(|w| w * 120_000),
        any::<u32>(),
        (
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
        ),
        any::<u32>(),
        proptest::option::of(any::<u32>()),
        proptest::option::of(any::<u32>()),
        tier,
    )
        .prop_map(|(w, obs, (a, b, c, d, e), sw, ent, inj, tier)| {
            traction_core::ActivityRollup {
                window_start_ms: w,
                ruleset_id: "traction-rules/1".into(),
                minutes_observed: obs,
                category_share_milli: CategoryShareMilli {
                    ide: a,
                    terminal: b,
                    browser: c,
                    ai_desktop: d,
                    other: e,
                },
                switch_rate_milli: sw,
                mean_cadence_entropy_milli: ent,
                injected_ratio_milli: inj,
                telemetry_tier: tier,
            }
        });

    let mut runner = proptest::test_runner::TestRunner::new(proptest::test_runner::Config {
        cases: 64,
        ..Default::default()
    });
    let n = std::sync::atomic::AtomicU32::new(0);
    runner
        .run(&(bucket_strategy, rollup_strategy), |(bucket, rollup)| {
            let device = format!(
                "dev{}",
                n.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let (got_b, got_r) = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let mut conn = server.pool.acquire().await.unwrap();
                    traction_server::store::insert_bucket(&mut conn, "u", &device, &bucket)
                        .await
                        .unwrap();
                    traction_server::store::insert_rollup(&mut conn, "u", &device, &rollup)
                        .await
                        .unwrap();
                    (
                        traction_server::store::fetch_buckets(&mut conn, &device)
                            .await
                            .unwrap(),
                        traction_server::store::fetch_rollups(&mut conn, &device)
                            .await
                            .unwrap(),
                    )
                })
            });
            prop_assert_eq!(got_b, vec![bucket]);
            prop_assert_eq!(got_r, vec![rollup]);
            Ok(())
        })
        .unwrap();
}
