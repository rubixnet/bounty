//! Seam: the whole anchor tracer. A real daemon (real SQLite writer,
//! finalizer sealing and Ed25519 chain) pairs with a real `traction-server`
//! over a real Postgres 17, syncs its sealed windows, and the SSE total goes
//! up. Linux CI; set `TRACTION_TEST_DATABASE_URL` to run locally.

mod common;

use std::path::Path;
use std::time::Duration;

use common::*;
use rusqlite::Connection;
use traction_core::{CategorySec, TRACTION_RULES_1};
use traction_daemon::{DaemonConfig, LoginOptions, SERVER_TOKEN_FILE, finalize_settled};
use traction_server::config::Config;

const WINDOW: u64 = 120_000;
const MIN: u64 = 60_000;

fn dir_project(letter: char) -> String {
    format!("dir:{}", letter.to_string().repeat(64))
}

/// Seeds `windows` sealed-able windows of Presence Minutes and manual file
/// mutations for one Project (the daemon's own pipeline turns them into
/// Work Buckets), and registers the Project's metadata.
fn seed(dir: &Path, t0: u64, windows: u64, project: &str) {
    let db = Connection::open(dir.join(traction_daemon::DB_FILE)).unwrap();
    db.execute(
        "INSERT INTO project (project_id, kind, root_path, home_relative_path, name, first_seen_ms)
         VALUES (?1, 'dir', '/home/u/demo', '~/demo', 'demo', 1)",
        [project],
    )
    .unwrap();
    let category = serde_json::to_string(&CategorySec {
        ide: 30,
        terminal: 30,
        ..CategorySec::default()
    })
    .unwrap();
    for window in 0..windows {
        let start = t0 + window * WINDOW;
        for minute in 0..2 {
            db.execute(
                "INSERT INTO presence_minute
                     (minute_start_ms, key_events, pointer_events, injected_events,
                      active_subslots, cadence_entropy_milli, category_sec,
                      category_switches, idle_sec, telemetry_tier)
                 VALUES (?1, 30, 30, 0, 4, 2500, ?2, 2, 0, 'full')",
                rusqlite::params![(start + minute * MIN) as i64, category],
            )
            .unwrap();
        }
        db.execute(
            "INSERT INTO traction_event
                 (event_id, schema_version, timestamp_ms, harness_id, session_id,
                  parent_orchestrator_id, event_type, end_reason, cwd, project_id, model_id,
                  network_class, attribution_source, prompt_char_len, repo_symbol_hits,
                  has_stacktrace, tool_name, exit_code, diff_lines_delta, mutation_kind)
             VALUES (?1, 1, ?2, NULL, NULL, NULL, 'FILE_MUTATION', NULL, '/home/u/demo', ?3, NULL,
                     'manual', 'file_watcher', NULL, NULL, NULL, NULL, NULL, 2, 'modified')",
            rusqlite::params![
                format!("019a7c2e-8b4d-7000-8000-{window:012x}"),
                (start + 10_000) as i64,
                project
            ],
        )
        .unwrap();
    }
}

fn local_count(dir: &Path, table: &str) -> i64 {
    let db = Connection::open(dir.join(traction_daemon::DB_FILE)).unwrap();
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn only_sync(config: &mut DaemonConfig) {
    config.finalizer.enabled = false;
    config.presence.enabled = false;
    config.commits.enabled = false;
    config.claude_config_dir = None;
    config.http_port = 0;
    config.writer.commit_interval = Duration::from_millis(50);
    config.sync.interval = Duration::from_millis(100);
    config.sync.max_links_per_request = 2;
}

#[tokio::test]
async fn a_real_daemon_pairs_syncs_and_the_live_total_goes_up() {
    let Some(url) = database_url() else { return };
    let server = start(&url, Config::default(), None).await;
    let data = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let t0 = (now - 3 * 60 * 60 * 1000) / WINDOW * WINDOW;
    let project = dir_project('c');

    // A real daemon creates the database, then is stopped while its sealed
    // chain is prepared through the real finalizer.
    let mut config = DaemonConfig::new(data.path().to_path_buf());
    config.home_dir = Some(home.path().to_path_buf());
    only_sync(&mut config);
    #[cfg(unix)]
    {
        config.socket_path = data.path().join("daemon.sock");
    }
    config.sync.enabled = false;
    traction_daemon::start(config.clone())
        .unwrap()
        .shutdown()
        .unwrap();
    seed(data.path(), t0, 4, &project);
    let sealed = finalize_settled(
        data.path(),
        t0 + 4 * WINDOW + TRACTION_RULES_1.bucket_settle_ms + TRACTION_RULES_1.context_fwd_ms(),
    )
    .unwrap();
    assert!(sealed > 0);
    let links = local_count(data.path(), "chain_link");
    assert_eq!(links, 4);

    // `traction login`: the CLI pairs; the "user" approves on the web page.
    let (tx, rx) = std::sync::mpsc::channel();
    let options = LoginOptions {
        data_dir: data.path().to_path_buf(),
        server_url: server.base.clone(),
        open_browser: true,
        poll_interval: Duration::from_millis(50),
        timeout: Duration::from_secs(30),
    };
    let login = tokio::task::spawn_blocking(move || {
        traction_daemon::login(&options, &mut Vec::new(), &move |url| {
            tx.send(url.to_owned()).unwrap();
        })
    });
    let login_url = tokio::task::spawn_blocking(move || rx.recv().unwrap())
        .await
        .unwrap();
    approve(&server, &login_url).await;
    login.await.unwrap().expect("login completes");
    let token = std::fs::read_to_string(data.path().join(SERVER_TOKEN_FILE)).unwrap();

    // The live stream starts at zero.
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
    assert!(first.1.contains(r#""human_active_sec":0"#), "{}", first.1);

    // The daemon starts; its sync client (server.url + server.token from
    // `login`) sends the sealed chain in ordered chunks.
    config.sync.enabled = true;
    let daemon = traction_daemon::start(config).unwrap();

    let local_buckets = local_count(data.path(), "work_bucket");
    let local_human: i64 = {
        let db = Connection::open(data.path().join(traction_daemon::DB_FILE)).unwrap();
        db.query_row("SELECT sum(human_active_sec) FROM work_bucket", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert!(local_human > 0, "the fixture produced Human Active Time");

    // The SSE total reaches the stored total.
    let total = tokio::task::spawn_blocking(move || {
        let mut sse = sse;
        loop {
            let (event, data) = sse.next();
            assert_eq!(event, "totals");
            let value: serde_json::Value = serde_json::from_str(&data).unwrap();
            if value["human_active_sec"].as_i64() == Some(local_human) {
                return value;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(total["human_active_sec"].as_i64(), Some(local_human));

    // The rows, the Project metadata and one Anchor per chunk are stored.
    let stored: i64 = sqlx::query_scalar("SELECT count(*) FROM work_buckets")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(stored, local_buckets);
    let rollups: i64 = sqlx::query_scalar("SELECT count(*) FROM activity_rollups")
        .fetch_one(&server.pool)
        .await
        .unwrap();
    assert_eq!(rollups, local_count(data.path(), "activity_rollup"));
    let anchors: Vec<i64> = sqlx::query_scalar("SELECT seq FROM anchors ORDER BY seq")
        .fetch_all(&server.pool)
        .await
        .unwrap();
    assert_eq!(anchors, [1, 3], "chunks of 2 links anchor their last tip");
    let (name, path): (String, String) =
        sqlx::query_as("SELECT project_name, root_path FROM projects")
            .fetch_one(&server.pool)
            .await
            .unwrap();
    assert_eq!((name.as_str(), path.as_str()), ("demo", "/home/u/demo"));
    // 3 hours old windows: sent as an offline backlog.
    let modes: Vec<String> = sqlx::query_scalar("SELECT DISTINCT sync_mode FROM anchors")
        .fetch_all(&server.pool)
        .await
        .unwrap();
    assert_eq!(modes, ["offline_buffered"]);
    daemon.shutdown().unwrap();
}
