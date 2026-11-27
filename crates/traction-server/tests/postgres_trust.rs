//! Seam: Step 4 slice S2 over a real Postgres 17 (Linux CI): anchor timing
//! flags, the read-time multi-device overlap merge, Project visibility and
//! roles, `DELETE /v1/me`, unsigned Timeline Marks and the timeline API.

mod common;

use common::*;
use serde_json::Value;
use traction_core::HarnessId;
use traction_core::sync::{HeartbeatResponse, SyncMode, TimelineMark};
use traction_server::config::Config;
use traction_server::token::hash_secret;

/// 2026-11-26T12:10:00Z: a multiple of the 120 s window.
const NOW: u64 = 1_795_694_400_000 + 600_000;
/// The window of "just now".
const WINDOW: u64 = NOW - 240_000;

fn clock() -> traction_server::app::Clock {
    fixed_clock(NOW)
}

async fn get(server: &TestServer, path: &str, token: Option<&str>) -> Reply {
    http(
        "GET",
        format!("{}{path}", server.base),
        token.map(str::to_owned),
        vec![],
        None,
    )
    .await
}

fn json(reply: &Reply) -> Value {
    serde_json::from_str(&reply.body).unwrap_or_else(|_| panic!("not JSON: {}", reply.body))
}

async fn user_id_of(server: &TestServer, token: &str) -> String {
    let me = get(server, "/v1/me", Some(token)).await;
    assert_eq!(me.status, 200, "{}", me.body);
    json(&me)["user_id"].as_str().unwrap().to_owned()
}

/// Another account with one device, bypassing Pairing (the dev provider
/// logs everybody in as the same `sub`).
async fn add_user(server: &TestServer, sub: &str, token: &str) -> String {
    let user = format!("user-{sub}");
    sqlx::query("INSERT INTO users (id, sub, display_name) VALUES ($1, $2, $2)")
        .bind(&user)
        .bind(sub)
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO devices (id, user_id, label, token_hash) VALUES ($1, $2, 'x', $3)")
        .bind(format!("dev-{sub}"))
        .bind(&user)
        .bind(hash_secret(token).to_vec())
        .execute(&server.pool)
        .await
        .unwrap();
    user
}

fn timeline_path(user: &str, from: u64, to: u64) -> String {
    format!("/v1/users/{user}/projects/{PROJECT}/timeline?from_ms={from}&to_ms={to}")
}

fn mark(minute: u64) -> TimelineMark {
    TimelineMark {
        project_id: PROJECT.into(),
        minute_start_ms: minute,
        mutations_manual: 2,
        mutations_harness: 1,
        turns_started: 1,
        tool_failures: 0,
        harnesses: vec![HarnessId::ClaudeCode],
    }
}

async fn count(server: &TestServer, sql: &str, bind: &str) -> i64 {
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql.to_owned()))
        .bind(bind)
        .fetch_one(&server.pool)
        .await
        .unwrap()
}

#[tokio::test]
async fn two_devices_with_offset_clocks_are_flagged_and_the_minute_is_merged_at_read_time() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), Some(clock())).await;
    let (key_a, key_b) = (device_key(21), device_key(22));
    let (token_a, _) = pair(&server, &key_a, None).await;
    let (token_b, device_b) = pair(&server, &key_b, None).await;

    // Device A's clock is right; device B's is 5 minutes ahead.
    let a = heartbeat(&key_a, 0, 'a', WINDOW, NOW);
    let b = heartbeat(&key_b, 0, 'b', WINDOW, NOW + 300_000);
    assert_eq!(send_heartbeat(&server, &token_a, &a).await.status, 200);
    assert_eq!(send_heartbeat(&server, &token_b, &b).await.status, 200);

    let me = json(&get(&server, "/v1/me", Some(&token_a)).await);
    let devices = me["devices"].as_array().unwrap();
    assert_eq!(devices.len(), 2);
    for device in devices {
        let is_b = device["device_id"] == device_b.as_str();
        assert_eq!(device["clock_skew"], is_b, "{device}");
    }
    let skew_b = devices
        .iter()
        .find(|d| d["device_id"] == device_b.as_str())
        .unwrap()["skew_ms"]
        .as_i64()
        .unwrap();
    assert_eq!(skew_b, -300_000);
    let stored_skew: i64 = sqlx::query_scalar("SELECT skew_ms FROM anchors WHERE device_id = $1")
        .bind(&device_b)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(stored_skew, -300_000, "the skew is stored on the Anchor");

    // Minute WINDOW: 40 + 40 s is capped to 60; the next minute: 30 + 30 = 60.
    let user = user_id_of(&server, &token_a).await;
    let timeline = json(
        &get(
            &server,
            &timeline_path(&user, WINDOW, WINDOW + 120_000),
            Some(&token_a),
        )
        .await,
    );
    let minutes = timeline["minutes"].as_array().unwrap();
    assert_eq!(minutes.len(), 2);
    let first = &minutes[0];
    assert_eq!(first["flags"], serde_json::json!(["multi_device_overlap"]));
    let merged: i64 = first["work_buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["human_active_sec"].as_i64().unwrap())
        .sum();
    assert_eq!(merged, 60);
    assert!(
        first["work_buckets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["human_active_sec_stored"] == 40)
    );
    assert_eq!(minutes[1]["flags"], serde_json::json!([]));

    // The Project list and the live SSE total use the merged figures:
    // 60 + 60 instead of the 140 s that were stored.
    let list = json(
        &get(
            &server,
            &format!("/v1/users/{user}/projects"),
            Some(&token_a),
        )
        .await,
    );
    assert_eq!(list["projects"][0]["human_active_sec"], 120);
    let addr = server.addr;
    let first_event = tokio::task::spawn_blocking(move || Sse::connect(addr, &token_a).next())
        .await
        .unwrap();
    assert_eq!(first_event.0, "totals");
    assert!(
        first_event.1.contains(r#""human_active_sec":120"#),
        "{}",
        first_event.1
    );
    // Stored buckets are never changed by the merge.
    let stored: i64 = sqlx::query_scalar("SELECT SUM(human_active_sec)::bigint FROM work_buckets")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(stored, 140);
}

#[tokio::test]
async fn late_and_offline_buffered_windows_are_tagged_and_still_count_as_logged_hours() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), Some(clock())).await;
    let key = device_key(23);
    let (token, _) = pair(&server, &key, None).await;
    let user = user_id_of(&server, &token).await;

    // On time: the window ended 2 minutes ago.
    let on_time = heartbeat(&key, 0, 'a', WINDOW, NOW);
    // A `realtime` request for a window that ended 30 minutes ago: late.
    let late_window = NOW - 32 * 60_000 - 120_000 * 2;
    let late_window = late_window / 120_000 * 120_000;
    let late = heartbeat(&key, 1, 'b', late_window, NOW);
    // The same lateness sent as `offline_buffered`: tagged, never late.
    let offline_window = late_window - 40 * 60_000 / 120_000 * 120_000;
    let mut offline = heartbeat(&key, 2, 'c', offline_window, NOW);
    offline.sync_mode = SyncMode::OfflineBuffered;
    for request in [&on_time, &late, &offline] {
        let reply = send_heartbeat(&server, &token, request).await;
        assert_eq!(reply.status, 200, "{}", reply.body);
    }

    let from = offline_window;
    let timeline = json(&get(&server, &timeline_path(&user, from, NOW), Some(&token)).await);
    let flags_at = |window: u64| -> Vec<String> {
        timeline["minutes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|m| m["minute_start_ms"] == window)
            .unwrap()["flags"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f.as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(flags_at(WINDOW), Vec::<String>::new());
    assert_eq!(flags_at(late_window), ["late_anchor"]);
    assert_eq!(flags_at(offline_window), ["offline_buffered"]);
    // The Anchor of the late heartbeat is tagged too.
    let late_anchors: i64 =
        sqlx::query_scalar("SELECT count(*) FROM anchors WHERE late_anchor AND seq = 1")
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert_eq!(late_anchors, 1);
    // All three count as Logged Hours.
    let list = json(&get(&server, &format!("/v1/users/{user}/projects"), Some(&token)).await);
    assert_eq!(list["projects"][0]["human_active_sec"], 210);
}

#[tokio::test]
async fn the_public_json_has_no_paths_or_username_and_private_views_follow_roles() {
    let Some(url) = database_url() else { return };
    let mut config = Config::default();
    config.roles.reviewers = vec!["rev".into()];
    let server = start(&url, config, Some(clock())).await;
    let key = device_key(24);
    let (owner_token, _) = pair(&server, &key, None).await;
    let owner = user_id_of(&server, &owner_token).await;
    assert_eq!(
        send_heartbeat(&server, &owner_token, &heartbeat(&key, 0, 'a', WINDOW, NOW))
            .await
            .status,
        200
    );
    add_user(&server, "rev", "reviewer-token").await;
    add_user(&server, "nobody", "stranger-token").await;
    let list_path = format!("/v1/users/{owner}/projects");

    for token in [None, Some("stranger-token")] {
        let reply = get(&server, &list_path, token).await;
        assert_eq!(reply.status, 200);
        let public = json(&reply);
        assert_eq!(public["visibility"], "public");
        assert_eq!(public["projects"][0]["project_name"], "demo");
        assert_eq!(public["projects"][0]["human_active_sec"], 70);
        for secret in [
            "project_root_path",
            "project_home_relative_path",
            "os_username",
            "linked_repo",
            "project_id",
            "/home/u/demo",
            "~/demo",
        ] {
            assert!(!reply.body.contains(secret), "{secret} in {}", reply.body);
        }
    }
    for token in [owner_token.as_str(), "reviewer-token"] {
        let private = json(&get(&server, &list_path, Some(token)).await);
        assert_eq!(private["visibility"], "private");
        let project = &private["projects"][0];
        assert_eq!(project["project_root_path"], "/home/u/demo");
        assert_eq!(project["project_home_relative_path"], "~/demo");
        assert_eq!(project["os_username"], "u");
    }
    // The timeline is private: owner and reviewer only.
    let path = timeline_path(&owner, WINDOW, WINDOW + 120_000);
    assert_eq!(get(&server, &path, None).await.status, 401);
    assert_eq!(
        get(&server, &path, Some("stranger-token")).await.status,
        403
    );
    assert_eq!(
        get(&server, &path, Some("reviewer-token")).await.status,
        200
    );
    assert_eq!(get(&server, &path, Some(&owner_token)).await.status, 200);
    let me = json(&get(&server, "/v1/me", Some("reviewer-token")).await);
    assert_eq!(me["roles"], serde_json::json!(["reviewer"]));
}

#[tokio::test]
async fn delete_me_removes_every_row_the_user_owns_and_nothing_else() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), Some(clock())).await;
    let key = device_key(25);
    let (token, device) = pair(&server, &key, None).await;
    let user = user_id_of(&server, &token).await;
    let mut request = heartbeat(&key, 0, 'a', WINDOW, NOW);
    request.timeline_marks = vec![mark(WINDOW)];
    assert_eq!(send_heartbeat(&server, &token, &request).await.status, 200);
    sqlx::query("INSERT INTO project_commits VALUES ($1, $2, 'abc', $3, $3, TRUE)")
        .bind(&user)
        .bind(PROJECT)
        .bind(WINDOW as i64)
        .execute(&server.pool)
        .await
        .unwrap();
    add_user(&server, "bystander", "bystander-token").await;

    for (table, filter) in [
        ("users", "id = $1"),
        ("devices", "user_id = $1"),
        ("work_buckets", "user_id = $1"),
        ("activity_rollups", "user_id = $1"),
        ("timeline_marks", "user_id = $1"),
        ("projects", "user_id = $1"),
        ("project_commits", "user_id = $1"),
    ] {
        let rows = count(
            &server,
            &format!("SELECT count(*) FROM {table} WHERE {filter}"),
            &user,
        )
        .await;
        assert!(rows > 0, "{table} had no rows to delete");
    }

    let deleted = http(
        "DELETE",
        format!("{}/v1/me", server.base),
        Some(token.clone()),
        vec![],
        None,
    )
    .await;
    assert_eq!(deleted.status, 204);

    for (table, filter, key) in [
        ("users", "id = $1", &user),
        ("devices", "user_id = $1", &user),
        ("device_keys", "device_id = $1", &device),
        ("anchors", "device_id = $1", &device),
        ("work_buckets", "user_id = $1", &user),
        ("activity_rollups", "user_id = $1", &user),
        ("timeline_marks", "user_id = $1", &user),
        ("projects", "user_id = $1", &user),
        ("project_commits", "user_id = $1", &user),
        ("pairings", "approved_user_id = $1", &user),
    ] {
        let rows = count(
            &server,
            &format!("SELECT count(*) FROM {table} WHERE {filter}"),
            key,
        )
        .await;
        assert_eq!(rows, 0, "{table} still has rows");
    }
    assert_eq!(get(&server, "/v1/me", Some(&token)).await.status, 401);
    // The other account is untouched.
    assert_eq!(
        get(&server, "/v1/me", Some("bystander-token")).await.status,
        200
    );
}

#[tokio::test]
async fn the_timeline_lines_up_buckets_rollups_commits_and_unsigned_marks() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), Some(clock())).await;
    let key = device_key(26);
    let (token, _) = pair(&server, &key, None).await;
    let user = user_id_of(&server, &token).await;
    let mut request = heartbeat(&key, 0, 'a', WINDOW, NOW);
    request.timeline_marks = vec![mark(WINDOW), mark(WINDOW + 60_000)];
    let reply = send_heartbeat(&server, &token, &request).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let response: HeartbeatResponse = serde_json::from_str(&reply.body).unwrap();
    assert!(response.rejected.is_empty(), "{:?}", response.rejected);
    sqlx::query("INSERT INTO project_commits VALUES ($1, $2, 'deadbeef', $3, $4, TRUE)")
        .bind(&user)
        .bind(PROJECT)
        .bind((WINDOW + 10_000) as i64)
        .bind((WINDOW + 20_000) as i64)
        .execute(&server.pool)
        .await
        .unwrap();

    let body = get(
        &server,
        &timeline_path(&user, WINDOW, WINDOW + 180_000),
        Some(&token),
    )
    .await;
    let timeline = json(&body);
    assert_eq!(timeline["timeline_marks_integrity"], "unsigned");
    let minutes = timeline["minutes"].as_array().unwrap();
    assert_eq!(minutes.len(), 2);
    let first = &minutes[0];
    assert_eq!(first["minute_start_ms"], WINDOW);
    assert_eq!(first["work_buckets"].as_array().unwrap().len(), 1);
    assert_eq!(first["work_buckets"][0]["human_active_sec"], 40);
    assert_eq!(first["activity_rollups"].as_array().unwrap().len(), 1);
    assert_eq!(first["commits"][0]["oid"], "deadbeef");
    assert_eq!(first["timeline_marks"][0]["integrity"], "unsigned");
    assert_eq!(first["timeline_marks"][0]["mutations_manual"], 2);
    assert_eq!(first["timeline_marks"][0]["harnesses"][0], "claude-code");
    // The rollup's window covers both minutes; the commit only the first.
    assert_eq!(minutes[1]["activity_rollups"].as_array().unwrap().len(), 1);
    assert!(minutes[1]["commits"].as_array().unwrap().is_empty());
    // No content-shaped field exists anywhere in the response.
    for forbidden in ["cwd", "model_id", "session_id"] {
        assert!(!body.body.contains(forbidden), "{forbidden}");
    }
    // A range wider than a week, or backwards, is refused.
    let bad = get(
        &server,
        &timeline_path(&user, WINDOW, WINDOW + 8 * 24 * 3_600_000),
        Some(&token),
    )
    .await;
    assert_eq!(bad.status, 422);
}

#[tokio::test]
async fn a_bad_timeline_mark_is_rejected_by_code_and_never_blocks_the_heartbeat() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), Some(clock())).await;
    let key = device_key(27);
    let (token, _) = pair(&server, &key, None).await;
    let mut request = heartbeat(&key, 0, 'a', WINDOW, NOW);
    let mut bad_project = mark(WINDOW);
    bad_project.project_id = "nope".into();
    let mut misaligned = mark(WINDOW);
    misaligned.minute_start_ms += 1;
    let after_tip = mark(WINDOW + 240_000);
    request.timeline_marks = vec![bad_project, misaligned, after_tip, mark(WINDOW)];
    let reply = send_heartbeat(&server, &token, &request).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let response: HeartbeatResponse = serde_json::from_str(&reply.body).unwrap();
    let codes: Vec<&str> = response.rejected.iter().map(|r| r.code.as_str()).collect();
    assert_eq!(
        codes,
        [
            "invalid_project_id",
            "misaligned_minute",
            "window_after_tip"
        ]
    );
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM timeline_marks")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(stored, 1);
    // The buckets were stored regardless.
    assert_eq!(response.accepted.len(), 4);
}

#[tokio::test]
async fn pairing_signs_in_through_oidc_before_the_csrf_checked_approval() {
    let Some(url) = database_url() else { return };
    let idp = common::idp::start(NOW).await;
    let secret_dir = tempfile::tempdir().unwrap();
    let secret_file = secret_dir.path().join("oidc.secret");
    std::fs::write(&secret_file, format!("{}\n", common::idp::CLIENT_SECRET)).unwrap();
    let config = Config {
        auth: common::idp::auth_config(&idp, &secret_file),
        ..Config::default()
    };
    let server = start(&url, config, Some(clock())).await;

    let key = device_key(28);
    let started = post_json(
        format!("{}/v1/pairings", server.base),
        None,
        serde_json::json!({
            "public_key": key.verifying_key().to_hex(),
            "previous_key_id": null,
            "device_label": "oidc laptop"
        })
        .to_string(),
    )
    .await;
    let started: traction_core::sync::PairingStartResponse =
        serde_json::from_str(&started.body).unwrap();

    // Opening the login URL without a session sends the browser to the
    // identity provider (302), which sends it back to the callback.
    let login = http("GET", started.login_url.clone(), None, vec![], None).await;
    assert_eq!(login.status, 302);
    let authorize = login.location.unwrap();
    assert!(authorize.starts_with(&idp.base), "{authorize}");
    let back = http("GET", authorize, None, vec![], None).await;
    assert_eq!(back.status, 302);
    let callback = http("GET", back.location.unwrap(), None, vec![], None).await;
    assert_eq!(callback.status, 302, "{}", callback.body);
    let cookie = callback.set_cookie.unwrap();
    assert!(
        cookie.contains("HttpOnly") && cookie.contains("SameSite=Lax"),
        "{cookie}"
    );
    let session = cookie.split(';').next().unwrap().to_owned();
    assert!(session.starts_with("traction_session="));
    assert_eq!(
        callback.location.unwrap(),
        started.login_url.replace(&server.base, "")
    );

    // With the session the approval page appears, naming the signed-in user.
    let page = http(
        "GET",
        started.login_url.clone(),
        None,
        vec![("cookie", session.clone())],
        None,
    )
    .await;
    assert_eq!(page.status, 200);
    assert!(page.body.contains("Ada Lovelace") && page.body.contains("Approve device"));
    let csrf = page
        .set_cookie
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("traction_csrf=")
        .unwrap()
        .to_owned();

    // Approving needs the session as well as the CSRF pair.
    let form = format!("pair={}&csrf={csrf}", started.pair_code);
    let approve = |cookies: String| {
        http(
            "POST",
            format!("{}/auth/approve", server.base),
            None,
            vec![("cookie", cookies)],
            Some((form.clone(), "application/x-www-form-urlencoded")),
        )
    };
    assert_eq!(approve(format!("traction_csrf={csrf}")).await.status, 403);
    assert_eq!(
        approve(format!("traction_csrf={csrf}; {session}"))
            .await
            .status,
        200
    );
    let polled = post_json(
        format!("{}/v1/pairings/poll", server.base),
        None,
        serde_json::json!({"pair_code": started.pair_code, "poll_secret": started.poll_secret})
            .to_string(),
    )
    .await;
    let polled: traction_core::sync::PairingPollResponse =
        serde_json::from_str(&polled.body).unwrap();
    let me = json(&get(&server, "/v1/me", polled.device_token.as_deref()).await);
    assert_eq!(me["display_name"], "Ada Lovelace");
    let sub: String = sqlx::query_scalar("SELECT sub FROM users")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(sub, "hc-user-1");

    // A callback the provider refused is a plain 400 that echoes nothing.
    let bad = http(
        "GET",
        format!("{}/auth/callback?error=access_denied&state=x", server.base),
        None,
        vec![],
        None,
    )
    .await;
    assert_eq!(bad.status, 400);
    assert!(!bad.body.contains("access_denied"));
}
