//! Shared harness for the Postgres-backed tests.
//!
//! These tests need a real Postgres 17 and run only when
//! `TRACTION_TEST_DATABASE_URL` is set (CI: the Linux job's service
//! container; docs/adr/0006). Each test gets its own freshly created database
//! and a real server on an ephemeral port. `TRACTION_REQUIRE_DB=1` turns a
//! missing URL into a failure so CI can never skip silently.
#![allow(dead_code)]

pub mod idp;

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::Duration;

use sqlx::PgPool;
use sqlx::postgres::PgPoolOptions;
use tokio::task::JoinHandle;
use traction_core::sync::{HeartbeatRequest, PairingStartRequest, PairingStartResponse};
use traction_core::{ActivityRollup, DeviceSigningKey, StruggleAxes, TelemetryTier, WorkBucket};
use traction_server::app::{AppState, Clock, router};
use traction_server::config::Config;
use zeroize::Zeroizing;

pub fn database_url() -> Option<String> {
    let url = std::env::var("TRACTION_TEST_DATABASE_URL").ok();
    if url.is_none() && std::env::var("TRACTION_REQUIRE_DB").is_ok_and(|v| v == "1") {
        panic!("TRACTION_REQUIRE_DB=1 but TRACTION_TEST_DATABASE_URL is not set");
    }
    if url.is_none() {
        eprintln!("SKIPPED (no TRACTION_TEST_DATABASE_URL): Postgres-backed test");
    }
    url
}

pub struct TestServer {
    pub addr: SocketAddr,
    pub base: String,
    pub pool: PgPool,
    pub state: Arc<AppState>,
    task: JoinHandle<()>,
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub fn fixed_clock(ms: u64) -> Clock {
    Arc::new(move || ms * 1000)
}

/// A fresh database, migrated, with the standard partitions, and a server.
pub async fn start(admin_url: &str, config: Config, clock: Option<Clock>) -> TestServer {
    start_with_engine(admin_url, config, clock, None).await
}

/// [`start`] with a chosen `DecisionEngine` instead of `HeuristicEngine`.
pub async fn start_with_engine(
    admin_url: &str,
    mut config: Config,
    clock: Option<Clock>,
    engine: Option<Arc<dyn traction_server::decision::DecisionEngine>>,
) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    config.server.public_url = format!("http://{addr}");
    let name = format!("traction_t_{}", traction_server::token::random_hex(6));
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(admin_url)
        .await
        .unwrap();
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .execute(&admin)
        .await
        .unwrap();
    let (prefix, _) = admin_url.rsplit_once('/').unwrap();
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&format!("{prefix}/{name}"))
        .await
        .unwrap();
    traction_server::migrate(&pool).await.unwrap();
    let state = Arc::new(match engine {
        Some(engine) => {
            traction_server::build_state_with_engine(pool.clone(), config, clock, engine).unwrap()
        }
        None => traction_server::build_state(pool.clone(), config, clock).unwrap(),
    });
    let months = traction_server::ensure_standard_partitions(&pool, state.now_ms())
        .await
        .unwrap();
    state.known_months.lock().unwrap().extend(months);
    let app = router(Arc::clone(&state));
    let task = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    TestServer {
        addr,
        base: format!("http://{addr}"),
        pool,
        state,
        task,
    }
}

pub struct Reply {
    pub status: u16,
    pub retry_after: Option<String>,
    pub set_cookie: Option<String>,
    pub location: Option<String>,
    pub body: String,
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into()
}

fn reply(mut response: ureq::http::Response<ureq::Body>) -> Reply {
    let header = |name: &str| {
        response
            .headers()
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    Reply {
        status: response.status().as_u16(),
        retry_after: header("retry-after"),
        set_cookie: header("set-cookie"),
        location: header("location"),
        body: response.body_mut().read_to_string().unwrap_or_default(),
    }
}

/// A blocking HTTP call, run on a blocking thread.
pub async fn http(
    method: &'static str,
    url: String,
    token: Option<String>,
    headers: Vec<(&'static str, String)>,
    body: Option<(String, &'static str)>,
) -> Reply {
    tokio::task::spawn_blocking(move || {
        let agent = agent();
        let apply = |mut b: ureq::RequestBuilder<ureq::typestate::WithBody>| {
            if let Some(t) = &token {
                b = b.header("authorization", format!("Bearer {t}"));
            }
            for (k, v) in &headers {
                b = b.header(*k, v);
            }
            b
        };
        let response = match (method, body) {
            ("GET", _) => {
                let mut b = agent.get(&url);
                if let Some(t) = &token {
                    b = b.header("authorization", format!("Bearer {t}"));
                }
                for (k, v) in &headers {
                    b = b.header(*k, v);
                }
                b.call()
            }
            ("DELETE", _) => {
                let mut b = agent.delete(&url);
                if let Some(t) = &token {
                    b = b.header("authorization", format!("Bearer {t}"));
                }
                b.call()
            }
            (_, Some((text, content_type))) => apply(agent.post(&url))
                .header("content-type", content_type)
                .send(text.as_str()),
            _ => apply(agent.post(&url)).send_empty(),
        };
        reply(response.expect("the request completes"))
    })
    .await
    .unwrap()
}

pub async fn post_json(url: String, token: Option<String>, json: String) -> Reply {
    http("POST", url, token, vec![], Some((json, "application/json"))).await
}

pub fn device_key(seed: u8) -> DeviceSigningKey {
    DeviceSigningKey::from_seed(Zeroizing::new([seed; 32])).unwrap()
}

/// Runs the whole Pairing flow for `key` (a fresh device, or a successor key
/// of `previous_key_id`) and returns `(device_token, device_id)`.
pub async fn pair(
    server: &TestServer,
    key: &DeviceSigningKey,
    previous_key_id: Option<String>,
) -> (String, String) {
    let request = PairingStartRequest {
        public_key: key.verifying_key().to_hex(),
        previous_key_id,
        device_label: "test laptop".into(),
    };
    let reply = post_json(
        format!("{}/v1/pairings", server.base),
        None,
        serde_json::to_string(&request).unwrap(),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let started: PairingStartResponse = serde_json::from_str(&reply.body).unwrap();
    approve(server, &started.login_url).await;
    let poll = serde_json::json!({
        "pair_code": started.pair_code,
        "poll_secret": started.poll_secret
    })
    .to_string();
    let polled = post_json(format!("{}/v1/pairings/poll", server.base), None, poll).await;
    let polled: traction_core::sync::PairingPollResponse =
        serde_json::from_str(&polled.body).unwrap();
    (polled.device_token.unwrap(), polled.device_id.unwrap())
}

/// What a user does in the browser: open the login URL, then press
/// **Approve device** (with the CSRF cookie the page set).
pub async fn approve(server: &TestServer, login_url: &str) {
    let pair_code = login_url.rsplit_once("pair=").unwrap().1.to_owned();
    let page = http("GET", login_url.to_owned(), None, vec![], None).await;
    assert_eq!(page.status, 200);
    let cookie = page.set_cookie.unwrap();
    let csrf = cookie
        .split(';')
        .next()
        .unwrap()
        .strip_prefix("traction_csrf=")
        .unwrap()
        .to_owned();
    let approve = http(
        "POST",
        format!("{}/auth/approve", server.base),
        None,
        vec![("cookie", format!("traction_csrf={csrf}"))],
        Some((
            format!("pair={pair_code}&csrf={csrf}"),
            "application/x-www-form-urlencoded",
        )),
    )
    .await;
    assert_eq!(approve.status, 200, "{}", approve.body);
}

pub const PROJECT: &str = "dir:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

pub fn bucket(minute_start_ms: u64, human: u32) -> WorkBucket {
    WorkBucket {
        project_id: PROJECT.into(),
        minute_start_ms,
        ruleset_id: "traction-rules/1".into(),
        agent_compute_sec: 20,
        idle_autonomous_sec: 5,
        human_active_sec: human,
        permission_wait_ms: 100,
        commits_human: 1,
        commits_agent: 0,
        injected_hid_detected: false,
        auto_approver_suspected: false,
        telemetry_tier: TelemetryTier::Full,
        tier_limited: false,
        struggle_axes: StruggleAxes {
            steering: 1,
            specificity: 2,
            friction: 3,
            comfort_delta: 4,
        },
        verified_struggle: 600,
        struggle_credit_sec: human / 2,
    }
}

pub fn rollup(window_start_ms: u64) -> ActivityRollup {
    ActivityRollup {
        window_start_ms,
        ruleset_id: "traction-rules/1".into(),
        minutes_observed: 2,
        category_share_milli: Default::default(),
        switch_rate_milli: 3,
        mean_cadence_entropy_milli: Some(9),
        injected_ratio_milli: None,
        telemetry_tier: TelemetryTier::Full,
    }
}

/// A heartbeat for the window starting at `window` holding one bucket in
/// each of its two minutes.
pub fn heartbeat(
    key: &DeviceSigningKey,
    seq: u64,
    hash_char: char,
    window: u64,
    now: u64,
) -> HeartbeatRequest {
    HeartbeatRequest {
        tip: traction_core::sync::ChainTip {
            key_id: key.key_id(),
            seq,
            entry_hash: hash_char.to_string().repeat(64),
            window_start_ms: window,
        },
        sync_mode: traction_core::sync::SyncMode::Realtime,
        sent_at_ms: now,
        work_buckets: vec![bucket(window, 40), bucket(window + 60_000, 30)],
        activity_rollups: vec![rollup(window)],
        projects: vec![traction_core::sync::ProjectMeta {
            project_id: PROJECT.into(),
            project_name: "demo".into(),
            project_root_path: "/home/u/demo".into(),
            project_home_relative_path: Some("~/demo".into()),
            os_username: "u".into(),
        }],
        timeline_marks: vec![],
    }
}

pub async fn send_heartbeat(server: &TestServer, token: &str, request: &HeartbeatRequest) -> Reply {
    post_json(
        format!("{}/v1/anchors", server.base),
        Some(token.to_owned()),
        serde_json::to_string(request).unwrap(),
    )
    .await
}

/// A minimal blocking SSE client over a raw socket.
pub struct Sse {
    reader: BufReader<TcpStream>,
}

impl Sse {
    pub fn connect(addr: SocketAddr, token: &str) -> Self {
        let mut stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        write!(
            stream,
            "GET /v1/live HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {token}\r\nAccept: text/event-stream\r\n\r\n"
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let mut status = String::new();
        reader.read_line(&mut status).unwrap();
        assert!(status.contains("200"), "SSE status: {status}");
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            if line == "\r\n" {
                break;
            }
        }
        Self { reader }
    }

    /// The next `(event, data)`, skipping keep-alive comments.
    pub fn next(&mut self) -> (String, String) {
        let (mut event, mut data) = (String::new(), String::new());
        loop {
            let mut line = String::new();
            let n = self.reader.read_line(&mut line).expect("an SSE line");
            assert!(n > 0, "the SSE stream closed");
            let line = line.trim_end();
            if let Some(v) = line.strip_prefix("event:") {
                event = v.trim().to_owned();
            } else if let Some(v) = line.strip_prefix("data:") {
                data = v.trim().to_owned();
            } else if line.is_empty() && !event.is_empty() {
                return (event, data);
            }
        }
    }
}

impl Read for Sse {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reader.read(buf)
    }
}
