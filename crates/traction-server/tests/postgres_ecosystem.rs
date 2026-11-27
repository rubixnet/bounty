//! Seam: S3b ecosystem features against a real server and Postgres 17 (Linux
//! CI; set `TRACTION_TEST_DATABASE_URL` to run locally): soft build
//! provenance, the Project merge API and GitHub linking.

mod common;

use common::*;
use serde_json::{Value, json};
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

fn parse(reply: &Reply) -> Value {
    serde_json::from_str(&reply.body).unwrap_or_else(|_| panic!("not JSON: {}", reply.body))
}

async fn get(server: &TestServer, path: &str, token: &str) -> Reply {
    http(
        "GET",
        format!("{}{path}", server.base),
        Some(token.to_owned()),
        vec![],
        None,
    )
    .await
}

const PROJECT_B: &str = "dir:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A heartbeat whose buckets (and metadata) belong to `project`.
fn heartbeat_in(
    key: &traction_core::DeviceSigningKey,
    seq: u64,
    hash: char,
    window: u64,
    now: u64,
    project: &str,
) -> traction_core::sync::HeartbeatRequest {
    let mut request = heartbeat(key, seq, hash, window, now);
    for b in &mut request.work_buckets {
        b.project_id = project.into();
    }
    request.projects[0].project_id = project.into();
    request.projects[0].project_name = format!("repo-{}", &project[4..8]);
    request
}

async fn merge_call(server: &TestServer, token: &str, from: &str, to: &str) -> Reply {
    post_json(
        format!("{}/v1/projects/merge", server.base),
        Some(token.to_owned()),
        json!({ "from_id": from, "to_id": to }).to_string(),
    )
    .await
}

#[tokio::test]
async fn merging_two_devices_projects_is_alias_only_and_every_read_resolves_it() {
    let Some(url) = database_url() else { return };
    let now = T0 + 600_000;
    let server = start(&url, config_trusting_ci(), Some(fixed_clock(now))).await;
    let (key_a, key_b) = (device_key(1), device_key(2));
    let (token_a, _) = pair(&server, &key_a, None).await;
    let (token_b, _) = pair(&server, &key_b, None).await;
    // The same repo seen as two Projects from two machines, in different minutes.
    let hb_a = heartbeat_in(&key_a, 0, 'a', T0, now, PROJECT);
    let hb_b = heartbeat_in(&key_b, 0, 'b', T0 + 240_000, now, PROJECT_B);
    assert_eq!(send_heartbeat(&server, &token_a, &hb_a).await.status, 200);
    assert_eq!(send_heartbeat(&server, &token_b, &hb_b).await.status, 200);

    let user = parse(&get(&server, "/v1/me", &token_a).await)["user_id"]
        .as_str()
        .unwrap()
        .to_owned();
    let list = |token: String| {
        let (server, path) = (&server, format!("/v1/users/{user}/projects"));
        async move { parse(&get(server, &path, &token).await) }
    };
    let before = list(token_a.clone()).await;
    assert_eq!(before["projects"].as_array().unwrap().len(), 2);
    let buckets_before: i64 = sqlx::query_scalar("SELECT count(*) FROM work_buckets")
        .fetch_one(&server.pool)
        .await
        .unwrap();

    // Merge B's Project into A's.
    let merged = merge_call(&server, &token_b, PROJECT_B, PROJECT).await;
    assert_eq!(merged.status, 201, "{}", merged.body);

    let after = list(token_a.clone()).await;
    let projects = after["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0]["project_id"], PROJECT);
    assert_eq!(projects[0]["merged_project_ids"], json!([PROJECT_B]));
    // 40 + 30 s per window, two windows.
    assert_eq!(projects[0]["human_active_sec"], 140);
    let public = parse(
        &http(
            "GET",
            format!("{}/v1/users/{user}/projects", server.base),
            None,
            vec![],
            None,
        )
        .await,
    );
    assert!(public["projects"][0].get("merged_project_ids").is_none());

    // The timeline of either id is the merged Project's, with both devices.
    let path = format!(
        "/v1/users/{user}/projects/{PROJECT_B}/timeline?from_ms={T0}&to_ms={}",
        T0 + 360_000
    );
    let timeline = parse(&get(&server, &path, &token_a).await);
    assert_eq!(timeline["project_id"], PROJECT);
    let devices: std::collections::BTreeSet<_> = timeline["minutes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["work_buckets"].as_array().unwrap())
        .map(|b| b["device_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(devices.len(), 2);

    // Buckets were never rewritten: B's rows still carry B's Project ID.
    let in_b: i64 = sqlx::query_scalar("SELECT count(*) FROM work_buckets WHERE project_id = $1")
        .bind(PROJECT_B)
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(in_b, 2);
    let buckets_after: i64 = sqlx::query_scalar("SELECT count(*) FROM work_buckets")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(buckets_before, buckets_after);

    // Cycles and repeats are refused.
    assert_eq!(
        merge_call(&server, &token_a, PROJECT, PROJECT_B)
            .await
            .status,
        409
    );
    assert_eq!(
        merge_call(&server, &token_a, PROJECT, PROJECT).await.status,
        409
    );
    assert_eq!(
        merge_call(&server, &token_a, PROJECT_B, PROJECT)
            .await
            .status,
        409
    );

    // Unmerging deletes the alias and restores the two Projects.
    let unmerge = http(
        "DELETE",
        format!("{}/v1/projects/merge/{PROJECT_B}", server.base),
        Some(token_a.clone()),
        vec![],
        None,
    )
    .await;
    assert_eq!(unmerge.status, 204);
    assert_eq!(list(token_a).await["projects"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn only_the_users_own_projects_can_be_merged() {
    let Some(url) = database_url() else { return };
    let now = T0 + 600_000;
    let server = start(&url, config_trusting_ci(), Some(fixed_clock(now))).await;
    let key = device_key(1);
    let (token, _) = pair(&server, &key, None).await;
    assert_eq!(
        send_heartbeat(
            &server,
            &token,
            &heartbeat_in(&key, 0, 'a', T0, now, PROJECT)
        )
        .await
        .status,
        200
    );
    // Another account's Project.
    sqlx::query("INSERT INTO users (id, sub, display_name) VALUES ('u2', 'other', 'o')")
        .execute(&server.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO projects (user_id, project_id, project_name, root_path, os_username, updated_at_ms)
         VALUES ('u2', $1, 'x', '/x', 'o', 0)",
    )
    .bind(PROJECT_B)
    .execute(&server.pool)
    .await
    .unwrap();
    assert_eq!(
        merge_call(&server, &token, PROJECT, PROJECT_B).await.status,
        404
    );
    assert_eq!(
        merge_call(&server, &token, PROJECT_B, PROJECT).await.status,
        404
    );
    // A Project the account has never synced reads as not found too.
    let unknown = format!("dir:{}", "c".repeat(64));
    assert_eq!(
        merge_call(&server, &token, &unknown, PROJECT).await.status,
        404
    );
    let aliases: i64 = sqlx::query_scalar("SELECT count(*) FROM project_aliases")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(aliases, 0);
}

// ---- GitHub linking -------------------------------------------------------

use common::github::{self as gh, FakeGithub};

struct GhRig {
    server: TestServer,
    fake: FakeGithub,
    token: String,
    _key: tempfile::TempDir,
}

async fn gh_rig(url: &str) -> GhRig {
    let fake = gh::start().await;
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("app.pem");
    std::fs::write(&key_path, gh::APP_KEY_PEM).unwrap();
    let mut config = config_trusting_ci();
    config.github.api_base_url = fake.base.clone();
    config.github.app_id = gh::APP_ID.into();
    config.github.private_key_file = Some(key_path.to_string_lossy().into_owned());
    config.github.per_page = 2;
    let now = T0 + 400_000;
    let server = start(url, config, Some(fixed_clock(now))).await;
    let key = device_key(1);
    let (token, _) = pair(&server, &key, None).await;
    let request = heartbeat_in(&key, 0, 'a', T0, now, PROJECT);
    assert_eq!(send_heartbeat(&server, &token, &request).await.status, 200);
    GhRig {
        server,
        fake,
        token,
        _key: dir,
    }
}

async fn link(rig: &GhRig, user_token: &str, repo: &str) -> Reply {
    post_json(
        format!("{}/v1/github/link", rig.server.base),
        Some(rig.token.clone()),
        json!({ "project_id": PROJECT, "repo": repo, "user_token": user_token }).to_string(),
    )
    .await
}

#[tokio::test]
async fn linking_needs_push_access_then_stores_only_oids_and_timestamps() {
    let Some(url) = database_url() else { return };
    let rig = gh_rig(&url).await;

    // A read-only user cannot link; nothing is stored.
    let refused = link(&rig, gh::READONLY_USER_TOKEN, gh::REPO).await;
    assert_eq!(refused.status, 403, "{}", refused.body);
    assert_eq!(parse(&refused)["error"], "no_push_access");
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM linked_repos")
        .fetch_one(&rig.server.pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    // A bad token and a malformed repo name are refused too.
    assert_eq!(link(&rig, "ghu_wrong", gh::REPO).await.status, 401);
    assert_eq!(link(&rig, gh::USER_TOKEN, "../etc").await.status, 400);

    // With push access: linked, commits fetched across both pages.
    let linked = link(&rig, gh::USER_TOKEN, gh::REPO).await;
    assert_eq!(linked.status, 201, "{}", linked.body);
    assert_eq!(parse(&linked)["commits_stored"], 3);
    let stored: Vec<(String, i64, bool)> = sqlx::query_as(
        "SELECT oid, committed_at_ms, author_match FROM project_commits ORDER BY committed_at_ms DESC",
    )
    .fetch_all(&rig.server.pool)
    .await
    .unwrap();
    assert_eq!(
        stored,
        vec![
            (
                "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d".into(),
                1_331_075_210_000,
                true
            ),
            (
                "762941318ee16e59dabbacb1b4049eec22f0d303".into(),
                1_315_975_361_000,
                false
            ),
            (
                "553c2077f0edc3d5dc5d17262f6aa498e69d6f8e".into(),
                1_296_068_768_000,
                false
            ),
        ]
    );
    let linked_repo: Option<String> =
        sqlx::query_scalar("SELECT linked_repo FROM projects WHERE project_id = $1")
            .bind(PROJECT)
            .fetch_one(&rig.server.pool)
            .await
            .unwrap();
    assert_eq!(linked_repo.as_deref(), Some(gh::REPO));

    // No commit message, e-mail, name, diff or token reached any table.
    let mut dump = String::new();
    for table in [
        "project_commits",
        "linked_repos",
        "github_accounts",
        "projects",
    ] {
        let rows: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT t::text FROM {table} t"
        )))
        .fetch_all(&rig.server.pool)
        .await
        .unwrap();
        dump.push_str(&rows.join("\n"));
    }
    for forbidden in [
        "Merge pull request",
        "New line at end of file",
        "first commit",
        "octocat@nowhere.com",
        "ghu_user_with_push",
        "ghs_installation_token",
    ] {
        assert!(!dump.contains(forbidden), "{forbidden} was stored");
    }
    // The user token proved access; the installation token fetched commits.
    let tokens = rig.fake.state.recorded.lock().unwrap().tokens.clone();
    assert!(tokens.iter().any(|t| t == gh::USER_TOKEN));
    assert!(tokens.iter().any(|t| t == gh::INSTALLATION_TOKEN));

    // Linking is removable: unlink forgets the commits.
    let unlink = http(
        "DELETE",
        format!("{}/v1/github/projects/{PROJECT}", rig.server.base),
        Some(rig.token.clone()),
        vec![],
        None,
    )
    .await;
    assert_eq!(unlink.status, 204);
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM project_commits")
        .fetch_one(&rig.server.pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn a_missing_app_install_and_an_unowned_project_are_refused() {
    let Some(url) = database_url() else { return };
    let rig = gh_rig(&url).await;
    *rig.fake.state.installed.lock().unwrap() = false;
    let reply = link(&rig, gh::USER_TOKEN, gh::REPO).await;
    assert_eq!(reply.status, 409, "{}", reply.body);
    assert_eq!(parse(&reply)["error"], "app_not_installed");

    let reply = post_json(
        format!("{}/v1/github/link", rig.server.base),
        Some(rig.token.clone()),
        json!({ "project_id": PROJECT_B, "repo": gh::REPO, "user_token": gh::USER_TOKEN })
            .to_string(),
    )
    .await;
    assert_eq!(reply.status, 404);
}

#[tokio::test]
async fn a_rate_limited_first_fetch_keeps_the_link_and_a_later_sync_fills_it() {
    let Some(url) = database_url() else { return };
    let rig = gh_rig(&url).await;
    *rig.fake.state.rate_limit_commits.lock().unwrap() = 1;
    *rig.fake.state.reset_at.lock().unwrap() = (T0 + 400_000) / 1000 + 90;
    let linked = link(&rig, gh::USER_TOKEN, gh::REPO).await;
    assert_eq!(linked.status, 201, "{}", linked.body);
    let body = parse(&linked);
    assert_eq!(body["commits_pending"], true);
    assert_eq!(body["retry_after_sec"], 90);

    let synced = post_json(
        format!("{}/v1/github/projects/{PROJECT}/sync", rig.server.base),
        Some(rig.token.clone()),
        "{}".into(),
    )
    .await;
    assert_eq!(synced.status, 200, "{}", synced.body);
    assert_eq!(parse(&synced)["commits_stored"], 3);
}
