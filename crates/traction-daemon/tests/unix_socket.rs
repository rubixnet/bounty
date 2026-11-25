//! Seam 2, Unix-socket transport (macOS/Linux only): the bus socket applies
//! the same token, body cap, validation and writer path as loopback HTTP and
//! skips only the `Host` check. Every socket lives in a temp data folder.
#![cfg(unix)]

mod common;

use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::UnixListener;
use std::path::Path;

use common::{BusClient, TestDaemon, event_id, stored_event_count, try_start_on, valid_event};
use traction_daemon::{MAX_BODY_BYTES, StartError};

fn read_token(data_dir: &Path) -> String {
    std::fs::read_to_string(data_dir.join("daemon.token")).expect("token file")
}

#[test]
fn a_valid_event_over_the_socket_returns_202_and_is_stored() {
    let mut daemon = TestDaemon::start();

    let response = daemon.socket_client().post_event(&valid_event(1));

    assert_eq!(response.status, 202, "{}", response.body);
    let db = daemon.shutdown_and_open_db();
    let stored: String = db
        .query_row("SELECT event_id FROM traction_event", [], |row| row.get(0))
        .expect("one stored event");
    assert_eq!(stored, event_id(1));
    assert_eq!(stored_event_count(&db), 1);
}

#[test]
fn a_wrong_or_missing_token_over_the_socket_returns_401_and_nothing_is_stored() {
    let mut daemon = TestDaemon::start();
    let client = daemon.socket_client();
    let mut wrong = client.token().to_owned();
    let last = wrong.pop().expect("non-empty token");
    wrong.push(if last == 'a' { 'b' } else { 'a' });

    let wrong_token = client
        .request("POST", "/v1/event")
        .header("Authorization", &format!("Bearer {wrong}"))
        .body(valid_event(1))
        .send();
    let missing_token = client
        .request("POST", "/v1/event")
        .without_header("Authorization")
        .body(valid_event(2))
        .send();

    for response in [wrong_token, missing_token] {
        assert_eq!(response.status, 401);
        assert_eq!(response.body, r#"{"error":"unauthorized"}"#);
    }
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
}

#[test]
fn the_socket_file_is_a_socket_with_mode_0600() {
    let daemon = TestDaemon::start();

    let meta = std::fs::symlink_metadata(daemon.socket_path()).expect("socket exists");

    assert!(meta.file_type().is_socket());
    assert_eq!(meta.permissions().mode() & 0o7777, 0o600);
    // No staging folder is left next to the socket.
    let leftovers: Vec<_> = std::fs::read_dir(daemon.data_dir())
        .expect("data folder")
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".tr"))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn the_socket_skips_only_the_host_check() {
    let mut daemon = TestDaemon::start();
    let client = daemon.socket_client();

    let no_host = client
        .request("POST", "/v1/event")
        .without_header("Host")
        .body(valid_event(1))
        .send();
    let foreign_host = client
        .request("POST", "/v1/event")
        .header("Host", "evil.example:41937")
        .body(valid_event(2))
        .send();
    let foreign_origin = client
        .request("POST", "/v1/event")
        .header("Origin", "http://evil.example")
        .body(valid_event(3))
        .send();
    let too_large = client
        .request("POST", "/v1/event")
        .body(vec![b' '; MAX_BODY_BYTES + 1])
        .send();
    let invalid = client.post_event("{}");

    assert_eq!(no_host.status, 202, "{}", no_host.body);
    assert_eq!(foreign_host.status, 202, "{}", foreign_host.body);
    assert_eq!(foreign_origin.status, 403);
    assert_eq!(foreign_origin.body, r#"{"error":"forbidden_origin"}"#);
    assert_eq!(too_large.status, 413);
    assert_eq!(too_large.body, r#"{"error":"body_too_large"}"#);
    assert_eq!(invalid.status, 400);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 2);
}

#[test]
fn a_stale_socket_left_by_a_dead_daemon_is_replaced() {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    let path = data_dir.path().join("daemon.sock");
    // A bound-then-dropped listener leaves a socket file nobody serves.
    drop(UnixListener::bind(&path).expect("stale socket"));

    let daemon = try_start_on(data_dir.path(), |_| {}).expect("daemon replaces the stale socket");
    let client =
        BusClient::new(daemon.local_addr(), read_token(data_dir.path())).via_socket(path.clone());

    assert_eq!(client.post_event(&valid_event(1)).status, 202);
    let mode = std::fs::symlink_metadata(&path)
        .expect("socket")
        .permissions()
        .mode();
    assert_eq!(mode & 0o7777, 0o600);
}

#[test]
fn a_live_socket_is_never_replaced() {
    let other = TestDaemon::start();
    let data_dir = tempfile::tempdir().expect("temp data folder");

    let result = try_start_on(data_dir.path(), |config| {
        config.socket_path = other.socket_path();
    });

    assert!(
        matches!(result, Err(StartError::SocketInUse { .. })),
        "{:?}",
        result.err()
    );
    assert_eq!(
        other.socket_client().post_event(&valid_event(1)).status,
        202
    );
}

#[test]
fn a_file_that_is_not_a_socket_is_left_untouched() {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    let path = data_dir.path().join("daemon.sock");
    std::fs::write(&path, b"not a socket").expect("regular file");

    let result = try_start_on(data_dir.path(), |_| {});

    assert!(
        matches!(result, Err(StartError::Socket { .. })),
        "{:?}",
        result.err()
    );
    assert_eq!(std::fs::read(&path).expect("file kept"), b"not a socket");
}

#[test]
fn a_second_daemon_without_the_lock_leaves_the_socket_alone() {
    let daemon = TestDaemon::start();
    let inode = std::fs::metadata(daemon.socket_path())
        .expect("socket")
        .ino();

    let second = try_start_on(daemon.data_dir(), |_| {});

    assert!(matches!(second, Err(StartError::AlreadyRunning)));
    let after = std::fs::metadata(daemon.socket_path())
        .expect("socket")
        .ino();
    assert_eq!(after, inode);
    assert_eq!(
        daemon.socket_client().post_event(&valid_event(1)).status,
        202
    );
}

#[test]
fn shutdown_removes_the_socket_file() {
    let mut daemon = TestDaemon::start();
    let path = daemon.socket_path();

    daemon.shutdown();

    assert!(!path.exists());
}
