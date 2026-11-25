//! The `traction-daemon` binary: a thin wrapper that wires OS signals to the
//! graceful shutdown and turns start errors into one line and a non-zero exit.
//!
//! Each test runs the real binary on its own temp data folder and an
//! ephemeral port (`--data-dir`, `--port 0`), never the user's `~/.traction`.
//! On Unix the socket goes in that folder too (`--socket-path`), or in a temp
//! `XDG_RUNTIME_DIR` set on the child only, never the real runtime folder.

mod common;

use std::io::{BufRead, BufReader};
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
use std::process::{Child, ChildStderr, Command, ExitStatus, Output, Stdio};
use std::time::{Duration, Instant};

use common::BusCaller;
#[cfg(unix)]
use common::{open_db_read_only, stored_event_count, valid_event};
use traction_daemon::TOKEN_FILE;
#[cfg(unix)]
use traction_daemon::{DATA_DIR_SOCKET_FILE, RUNTIME_SOCKET_FILE};

const BIN: &str = env!("CARGO_BIN_EXE_traction-daemon");

/// The binary on `data_dir` and `port`, with its socket inside `data_dir`.
fn command(data_dir: &Path, port: u16) -> Command {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut command = command_without_socket_path(data_dir, port);
    #[cfg(unix)]
    command
        .arg("--socket-path")
        .arg(data_dir.join(DATA_DIR_SOCKET_FILE));
    command
}

/// Leaves the socket path to the binary's default. Callers must control
/// `XDG_RUNTIME_DIR` on the child.
fn command_without_socket_path(data_dir: &Path, port: u16) -> Command {
    let mut command = Command::new(BIN);
    command
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--port")
        .arg(port.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    command
}

/// A running binary. Killed on drop if a test fails midway.
struct RunningDaemon {
    child: Child,
    // The Windows build runs only the start-failure tests.
    #[cfg_attr(not(unix), allow(dead_code))]
    addr: SocketAddr,
    _stderr: BufReader<ChildStderr>,
}

impl RunningDaemon {
    fn spawn(data_dir: &Path) -> Self {
        Self::spawn_command(&mut command(data_dir, 0))
    }

    fn spawn_command(command: &mut Command) -> Self {
        let mut child = command.spawn().expect("spawn daemon binary");
        let mut stderr = BufReader::new(child.stderr.take().expect("stderr"));
        let mut line = String::new();
        stderr.read_line(&mut line).expect("read startup line");
        let addr = line
            .trim()
            .strip_prefix("traction-daemon: listening on ")
            .and_then(|addr| addr.parse().ok())
            .unwrap_or_else(|| panic!("unexpected startup line: {line:?}"));
        Self {
            child,
            addr,
            _stderr: stderr,
        }
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn caller(&self, data_dir: &Path) -> BusCaller {
        let token = std::fs::read_to_string(data_dir.join(TOKEN_FILE)).expect("token file");
        BusCaller::new(self.addr, token)
    }

    #[cfg(unix)]
    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(self.child.id().to_string())
            .status()
            .expect("run kill");
        assert!(status.success(), "kill -{name}");
    }

    #[cfg_attr(not(unix), allow(dead_code))]
    fn wait(&mut self, timeout: Duration) -> ExitStatus {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll child") {
                return status;
            }
            assert!(Instant::now() < deadline, "daemon did not exit in time");
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for RunningDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn run_to_exit(command: &mut Command) -> Output {
    command.output().expect("run daemon binary")
}

fn assert_one_line_failure(output: &Output, needle: &str) {
    assert!(!output.status.success(), "must exit non-zero");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stderr.lines().count(), 1, "one line: {stderr:?}");
    assert!(stderr.contains(needle), "{stderr:?}");
}

#[test]
fn second_binary_on_the_same_data_folder_prints_one_line_and_exits_non_zero() {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    let _first = RunningDaemon::spawn(data_dir.path());

    let output = run_to_exit(&mut command(data_dir.path(), 0));

    assert_one_line_failure(&output, "already running");
}

#[test]
fn binary_on_a_taken_port_prints_one_line_and_exits_non_zero() {
    let occupant = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("occupy a port");
    let port = occupant.local_addr().expect("occupied addr").port();
    let data_dir = tempfile::tempdir().expect("temp data folder");

    let output = run_to_exit(&mut command(data_dir.path(), port));

    assert_one_line_failure(&output, &port.to_string());
}

#[cfg(unix)]
fn assert_signal_flushes_accepted_events(signal: &str) {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    let mut daemon = RunningDaemon::spawn(data_dir.path());
    let caller = daemon.caller(data_dir.path());
    for n in 1..=3 {
        assert_eq!(caller.post_event(&valid_event(n)).status, 202);
    }

    // The default commit interval (5 s) has not elapsed: only the graceful
    // shutdown can have committed these.
    daemon.signal(signal);
    let status = daemon.wait(Duration::from_secs(10));

    assert!(status.success(), "clean exit after {signal}: {status:?}");
    assert_eq!(stored_event_count(&open_db_read_only(data_dir.path())), 3);
}

#[cfg(unix)]
#[test]
fn sigterm_flushes_accepted_events_and_exits_cleanly() {
    assert_signal_flushes_accepted_events("TERM");
}

#[cfg(unix)]
#[test]
fn sigint_flushes_accepted_events_and_exits_cleanly() {
    assert_signal_flushes_accepted_events("INT");
}

#[cfg(unix)]
#[test]
fn the_lock_is_released_when_the_daemon_is_killed() {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    let mut crashed = RunningDaemon::spawn(data_dir.path());
    crashed.signal("KILL");
    crashed.wait(Duration::from_secs(10));

    let mut restarted = RunningDaemon::spawn(data_dir.path());
    restarted.signal("TERM");
    assert!(restarted.wait(Duration::from_secs(10)).success());
}

#[cfg(unix)]
fn assert_socket_accepts_events(daemon: &RunningDaemon, data_dir: &Path, socket: &Path) {
    let caller = daemon.caller(data_dir).via_socket(socket.to_path_buf());
    assert_eq!(caller.post_event(&valid_event(1)).status, 202);
}

#[cfg(unix)]
#[test]
fn the_socket_defaults_to_traction_sock_in_xdg_runtime_dir() {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    let runtime_dir = tempfile::tempdir().expect("temp runtime folder");
    let mut command = command_without_socket_path(data_dir.path(), 0);
    command.env("XDG_RUNTIME_DIR", runtime_dir.path());

    let daemon = RunningDaemon::spawn_command(&mut command);

    assert_socket_accepts_events(
        &daemon,
        data_dir.path(),
        &runtime_dir.path().join(RUNTIME_SOCKET_FILE),
    );
    assert!(!data_dir.path().join(DATA_DIR_SOCKET_FILE).exists());
}

#[cfg(unix)]
#[test]
fn without_xdg_runtime_dir_the_socket_is_daemon_sock_in_the_data_folder() {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    let mut command = command_without_socket_path(data_dir.path(), 0);
    command.env_remove("XDG_RUNTIME_DIR");

    let daemon = RunningDaemon::spawn_command(&mut command);

    assert_socket_accepts_events(
        &daemon,
        data_dir.path(),
        &data_dir.path().join(DATA_DIR_SOCKET_FILE),
    );
}

#[cfg(unix)]
#[test]
fn socket_path_option_overrides_the_default() {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    let socket_dir = tempfile::tempdir().expect("temp socket folder");
    let socket = socket_dir.path().join("custom.sock");
    let mut command = command_without_socket_path(data_dir.path(), 0);
    command
        .arg("--socket-path")
        .arg(&socket)
        .env_remove("XDG_RUNTIME_DIR");

    let daemon = RunningDaemon::spawn_command(&mut command);

    assert_socket_accepts_events(&daemon, data_dir.path(), &socket);
}
