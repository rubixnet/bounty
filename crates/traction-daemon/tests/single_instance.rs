//! Seam 2: one daemon per data folder (single-instance lock) and a clear
//! error when the configured port is taken.

mod common;

use std::net::{Ipv4Addr, TcpListener};

use common::{TestDaemon, try_start_on, valid_event};
use traction_daemon::StartError;

#[test]
fn second_start_on_the_same_data_folder_is_already_running() {
    let first = TestDaemon::start();

    let second = try_start_on(first.data_dir(), |_| {});

    match second {
        Err(error @ StartError::AlreadyRunning) => {
            let message = error.to_string();
            assert!(message.contains("already running"), "{message}");
            assert!(!message.contains('\n'), "one line: {message}");
        }
        other => panic!(
            "expected AlreadyRunning, got {:?}",
            other.map(|h| h.local_addr())
        ),
    }
}

#[test]
fn a_rejected_second_start_leaves_the_first_daemon_serving() {
    let first = TestDaemon::start();
    let token_before = first.token();

    assert!(try_start_on(first.data_dir(), |_| {}).is_err());

    assert_eq!(first.token(), token_before, "token untouched");
    assert_eq!(first.caller().post_event(&valid_event(1)).status, 202);
}

#[test]
fn a_new_start_succeeds_after_the_first_daemon_shuts_down() {
    let mut first = TestDaemon::start();
    assert_eq!(first.caller().post_event(&valid_event(1)).status, 202);
    first.shutdown();

    let second = try_start_on(first.data_dir(), |_| {}).expect("lock released on shutdown");
    second.shutdown().expect("graceful shutdown");

    let restarted = first.restart();
    assert_eq!(restarted.caller().post_event(&valid_event(2)).status, 202);
}

#[test]
fn dropping_the_handle_releases_the_lock() {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    drop(try_start_on(data_dir.path(), |_| {}).expect("first start"));

    let daemon = try_start_on(data_dir.path(), |_| {}).expect("lock released on drop");
    daemon.shutdown().expect("graceful shutdown");
}

#[test]
fn different_data_folders_run_side_by_side() {
    let first = TestDaemon::start();
    let second = TestDaemon::start();

    assert_eq!(first.caller().post_event(&valid_event(1)).status, 202);
    assert_eq!(second.caller().post_event(&valid_event(1)).status, 202);
}

#[test]
fn a_port_in_use_fails_startup_without_falling_back() {
    // The test itself occupies the port.
    let occupant = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("occupy a port");
    let port = occupant.local_addr().expect("occupied addr").port();
    let data_dir = tempfile::tempdir().expect("temp data folder");

    let result = try_start_on(data_dir.path(), |config| config.http_port = port);

    match result {
        Err(error @ StartError::PortInUse { port: reported }) => {
            assert_eq!(reported, port);
            let message = error.to_string();
            assert!(message.contains(&port.to_string()), "{message}");
            assert!(!message.contains('\n'), "one line: {message}");
        }
        other => panic!(
            "expected PortInUse, got {:?}",
            other.map(|h| h.local_addr())
        ),
    }
}

#[test]
fn a_failed_port_bind_releases_the_lock() {
    let occupant = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("occupy a port");
    let port = occupant.local_addr().expect("occupied addr").port();
    let data_dir = tempfile::tempdir().expect("temp data folder");
    assert!(try_start_on(data_dir.path(), |config| config.http_port = port).is_err());

    let daemon = try_start_on(data_dir.path(), |_| {}).expect("lock was released");
    daemon.shutdown().expect("graceful shutdown");
}
