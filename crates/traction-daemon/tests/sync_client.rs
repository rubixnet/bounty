//! Seam: the daemon's server sync client against a local test HTTP server
//! built from `traction_core::sync` (all 3 OSes, no database). A real sealed
//! chain (through the real writer and finalizer path) is synced to a server
//! that answers as the test scripts it.

mod common;

use std::time::{Duration, Instant};

use common::chain::{Device, T0, WINDOW};
use common::http_server::{HttpServer, Request, Response};
use common::poll_until;
use rusqlite::Connection;
use traction_core::sync::{
    AnchorStatus, ErrorBody, HeartbeatRequest, HeartbeatResponse, ItemKind, ItemRef, RejectedItem,
    SyncMode,
};
use traction_daemon::DaemonConfig;

const TOKEN: &str = "t0k3n";

fn accepted_items(request: &HeartbeatRequest) -> Vec<ItemRef> {
    let mut items: Vec<ItemRef> = request
        .work_buckets
        .iter()
        .map(|b| ItemRef {
            kind: ItemKind::WorkBucket,
            start_ms: b.minute_start_ms,
            project_id: Some(b.project_id.clone()),
        })
        .collect();
    items.extend(request.activity_rollups.iter().map(|r| ItemRef {
        kind: ItemKind::ActivityRollup,
        start_ms: r.window_start_ms,
        project_id: None,
    }));
    items
}

fn ok(request: &HeartbeatRequest) -> Response {
    Response::json(
        200,
        &HeartbeatResponse {
            anchor: AnchorStatus::Stored,
            server_received_at_ms: 1,
            accepted: accepted_items(request),
            rejected: vec![],
        },
    )
}

fn reject_all(request: &HeartbeatRequest, code: &str) -> Response {
    Response::json(
        200,
        &HeartbeatResponse {
            anchor: AnchorStatus::Stored,
            server_received_at_ms: 1,
            accepted: vec![],
            rejected: accepted_items(request)
                .into_iter()
                .map(|item| RejectedItem {
                    item,
                    code: code.into(),
                })
                .collect(),
        },
    )
}

/// Config that runs only the sync client against `server`.
fn syncing(server: &HttpServer, token_dir: &std::path::Path) -> impl FnOnce(&mut DaemonConfig) {
    let url = server.url();
    let token_path = token_dir.join("test.token");
    std::fs::write(&token_path, TOKEN).unwrap();
    move |config: &mut DaemonConfig| {
        config.finalizer.enabled = false;
        config.presence.enabled = false;
        config.commits.enabled = false;
        config.writer.commit_interval = Duration::from_millis(50);
        config.sync.server_url = Some(url);
        config.sync.token_path = Some(token_path);
        config.sync.interval = Duration::from_millis(100);
        config.sync.backoff_base = Duration::from_millis(50);
        config.sync.backoff_max = Duration::from_millis(400);
    }
}

fn heartbeats(server: &HttpServer) -> Vec<HeartbeatRequest> {
    server
        .requests()
        .iter()
        .filter(|r| r.path == "/v1/anchors")
        .map(Request::json::<HeartbeatRequest>)
        .collect()
}

fn cursor(dir: &std::path::Path) -> Option<(i64, i64)> {
    let db = Connection::open(dir.join(traction_daemon::DB_FILE)).unwrap();
    db.query_row(
        "SELECT window_start_ms, terminal_rejections FROM sync_cursor WHERE id = 1",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .ok()
}

fn wait(what: &str, mut ready: impl FnMut() -> bool) {
    poll_until(Duration::from_secs(30), what, || ready().then_some(()));
}

fn count(dir: &std::path::Path, table: &str) -> usize {
    let db = Connection::open(dir.join(traction_daemon::DB_FILE)).unwrap();
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| {
        r.get::<_, i64>(0)
    })
    .unwrap() as usize
}

#[test]
fn a_backlog_is_sent_in_ordered_chunks_and_the_cursor_survives_a_restart() {
    let server = HttpServer::start(|req, _| ok(&req.json()));
    let mut device = Device::sealed(7);
    let scratch = tempfile::tempdir().unwrap();
    let configure = syncing(&server, scratch.path());
    device.start_daemon(move |c| {
        configure(c);
        c.sync.max_links_per_request = 3;
    });

    wait("three chunks", || heartbeats(&server).len() >= 3);
    device.stop_daemon();
    let sent = heartbeats(&server);
    let seqs: Vec<u64> = sent.iter().map(|h| h.tip.seq).collect();
    assert_eq!(seqs, [2, 5, 6], "ordered chunks of at most 3 links");
    for request in server.requests() {
        assert_eq!(
            request.header("authorization"),
            Some(&*format!("Bearer {TOKEN}"))
        );
    }
    for h in &sent {
        assert!(
            h.work_buckets
                .iter()
                .all(|b| b.minute_start_ms / WINDOW * WINDOW <= h.tip.window_start_ms)
        );
        assert_eq!(h.tip.key_id, sent[0].tip.key_id);
    }
    // Every sealed row went out exactly once, across the chunks.
    let buckets: usize = sent.iter().map(|h| h.work_buckets.len()).sum();
    let rollups: usize = sent.iter().map(|h| h.activity_rollups.len()).sum();
    assert_eq!(buckets, count(device.dir(), "work_bucket"));
    assert_eq!(rollups, count(device.dir(), "activity_rollup"));
    assert_eq!(
        cursor(device.dir()),
        Some((Device::window(6) as i64, 0)),
        "the cursor is stored through the writer"
    );

    // A restart resumes: nothing is re-sent.
    device.start_daemon(syncing(&server, scratch.path()));
    std::thread::sleep(Duration::from_millis(800));
    device.stop_daemon();
    assert_eq!(heartbeats(&server).len(), 3);

    // New windows after the restart go out as one new request.
    device.seal_windows(2);
    device.start_daemon(syncing(&server, scratch.path()));
    wait("the new chunk", || heartbeats(&server).len() >= 4);
    device.stop_daemon();
    let all = heartbeats(&server);
    assert_eq!(all.len(), 4);
    assert_eq!(all[3].tip.seq, 8);
    assert!(
        all[3]
            .work_buckets
            .iter()
            .all(|b| b.minute_start_ms >= Device::window(7))
    );
}

#[test]
fn a_429_with_retry_after_and_server_errors_are_retried_with_backoff() {
    let server = HttpServer::start(|req, n| match n {
        0 => Response::json(
            429,
            &ErrorBody {
                error: "rate_limited".into(),
            },
        )
        .with_header("retry-after", "1"),
        1 | 2 => Response::json(
            500,
            &ErrorBody {
                error: "internal".into(),
            },
        ),
        _ => ok(&req.json()),
    });
    let mut device = Device::sealed(2);
    let scratch = tempfile::tempdir().unwrap();
    device.start_daemon(syncing(&server, scratch.path()));

    wait("the retried request to succeed", || {
        cursor(device.dir()).is_some()
    });
    device.stop_daemon();
    let times: Vec<Instant> = server.requests().iter().map(|r| r.at).collect();
    assert!(times.len() >= 4);
    assert!(
        times[1] - times[0] >= Duration::from_millis(950),
        "Retry-After: 1 was honoured"
    );
    // The same tip was retried until it was accepted.
    let tips: Vec<u64> = heartbeats(&server).iter().map(|h| h.tip.seq).collect();
    assert!(tips.iter().all(|seq| *seq == 1), "{tips:?}");
}

#[test]
fn an_unreachable_server_is_retried_until_it_comes_up() {
    let mut device = Device::sealed(1);
    let scratch = tempfile::tempdir().unwrap();
    // Reserve a port, close it, and point the client at it.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let token_path = scratch.path().join("t");
    std::fs::write(&token_path, TOKEN).unwrap();
    device.start_daemon(|c| {
        c.finalizer.enabled = false;
        c.presence.enabled = false;
        c.commits.enabled = false;
        c.writer.commit_interval = Duration::from_millis(50);
        c.sync.server_url = Some(format!("http://127.0.0.1:{port}"));
        c.sync.token_path = Some(token_path);
        c.sync.interval = Duration::from_millis(100);
        c.sync.backoff_base = Duration::from_millis(50);
        c.sync.backoff_max = Duration::from_millis(200);
    });
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(
        cursor(device.dir()),
        None,
        "nothing is acknowledged while down"
    );
    device.stop_daemon();
}

#[test]
fn only_a_terminal_rejection_moves_the_cursor_and_it_is_counted() {
    // Every row is "too old": terminal. The cursor moves; the count is kept.
    let server = HttpServer::start(|req, _| reject_all(&req.json(), "window_too_old"));
    let mut device = Device::sealed(3);
    let scratch = tempfile::tempdir().unwrap();
    device.start_daemon(syncing(&server, scratch.path()));
    wait("the cursor to move", || cursor(device.dir()).is_some());
    device.stop_daemon();
    let (window, rejected) = cursor(device.dir()).unwrap();
    assert_eq!(window, Device::window(2) as i64);
    let sent = heartbeats(&server);
    let items: usize = sent
        .iter()
        .map(|h| h.work_buckets.len() + h.activity_rollups.len())
        .sum();
    assert_eq!(rejected as usize, items);
    assert_eq!(
        sent.len(),
        1,
        "nothing is re-sent after a terminal rejection"
    );
}

#[test]
fn a_422_window_too_old_for_the_whole_request_is_terminal_too() {
    let server = HttpServer::start(|_, _| {
        Response::json(
            422,
            &ErrorBody {
                error: "window_too_old".into(),
            },
        )
    });
    let mut device = Device::sealed(2);
    let scratch = tempfile::tempdir().unwrap();
    device.start_daemon(syncing(&server, scratch.path()));
    wait("the cursor to move", || cursor(device.dir()).is_some());
    device.stop_daemon();
    let (window, rejected) = cursor(device.dir()).unwrap();
    assert_eq!(window, Device::window(1) as i64);
    assert!(rejected > 0);
}

#[test]
fn any_other_rejection_is_retried_and_never_skipped() {
    let server = HttpServer::start(|req, _| reject_all(&req.json(), "human_active_over_minute"));
    let mut device = Device::sealed(2);
    let scratch = tempfile::tempdir().unwrap();
    device.start_daemon(syncing(&server, scratch.path()));
    wait("several retries", || heartbeats(&server).len() >= 3);
    device.stop_daemon();
    assert_eq!(cursor(device.dir()), None, "the cursor never moved");
}

#[test]
fn requests_for_an_old_backlog_are_offline_buffered_and_fresh_ones_are_realtime() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let server = HttpServer::start(|req, _| ok(&req.json()));
    let scratch = tempfile::tempdir().unwrap();

    // Windows from three hours ago: a backlog.
    let old = (now - 3 * 60 * 60 * 1000) / WINDOW * WINDOW;
    let mut backlog = Device::sealed_from(old, 2);
    backlog.start_daemon(syncing(&server, scratch.path()));
    wait("the backlog request", || !heartbeats(&server).is_empty());
    backlog.stop_daemon();
    assert_eq!(heartbeats(&server)[0].sync_mode, SyncMode::OfflineBuffered);

    // A window that ended a few minutes ago: realtime.
    let fresh = Device::sealed_from((now - 5 * 60 * 1000) / WINDOW * WINDOW, 1);
    let server = HttpServer::start(|req, _| ok(&req.json()));
    let mut fresh = fresh;
    fresh.start_daemon(syncing(&server, scratch.path()));
    wait("the fresh request", || !heartbeats(&server).is_empty());
    fresh.stop_daemon();
    assert_eq!(heartbeats(&server)[0].sync_mode, SyncMode::Realtime);
    let _ = T0;
}

#[test]
fn without_a_token_or_server_nothing_is_sent() {
    let server = HttpServer::start(|req, _| ok(&req.json()));
    let mut device = Device::sealed(2);
    // The server is injected but the token file does not exist.
    let url = server.url();
    device.start_daemon(move |c| {
        c.finalizer.enabled = false;
        c.presence.enabled = false;
        c.commits.enabled = false;
        c.sync.server_url = Some(url);
        c.sync.interval = Duration::from_millis(50);
    });
    std::thread::sleep(Duration::from_millis(500));
    device.stop_daemon();
    assert_eq!(server.count(), 0);
}
