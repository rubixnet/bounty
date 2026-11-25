//! Shared Seam 2 helper: a real in-process daemon on a fresh temp data folder
//! and an ephemeral loopback port, plus a raw HTTP/1.1 client.
//!
//! Tests interact only through HTTP, the token file the daemon wrote and a
//! read-only SQLite connection opened after shutdown. The client writes raw
//! request bytes so later tickets can send arbitrary (or missing) `Host`,
//! `Origin` and `Authorization` headers.
//!
//! Each test binary uses a different subset of this module.
#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;
use traction_daemon::{DaemonConfig, DaemonHandle};

/// A daemon running on its own temp data folder. Shut down on drop.
pub struct TestDaemon {
    data_dir: TempDir,
    handle: Option<DaemonHandle>,
    addr: SocketAddr,
}

impl TestDaemon {
    /// Starts a daemon with test defaults: port `0`, temp data folder and a
    /// commit interval long enough that only shutdown (or batch size) flushes.
    pub fn start() -> Self {
        Self::start_with(|_| {})
    }

    /// Starts a daemon after letting the test adjust the config.
    pub fn start_with(adjust: impl FnOnce(&mut DaemonConfig)) -> Self {
        let data_dir = tempfile::tempdir().expect("temp data folder");
        let mut config = test_config(data_dir.path());
        adjust(&mut config);
        let handle = traction_daemon::start(config).expect("daemon starts");
        let addr = handle.local_addr();
        Self {
            data_dir,
            handle: Some(handle),
            addr,
        }
    }

    /// Starts a new daemon on the data folder of a previous, shut-down one.
    pub fn restart(mut self) -> Self {
        self.shutdown();
        let config = test_config(self.data_dir.path());
        let handle = traction_daemon::start(config).expect("daemon restarts");
        self.addr = handle.local_addr();
        self.handle = Some(handle);
        self
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn data_dir(&self) -> &Path {
        self.data_dir.path()
    }

    pub fn token_path(&self) -> PathBuf {
        self.data_dir().join("daemon.token")
    }

    /// The bearer token the daemon wrote to `<data dir>/daemon.token`.
    pub fn token(&self) -> String {
        std::fs::read_to_string(self.token_path()).expect("token file")
    }

    /// A client holding the daemon's real token.
    pub fn client(&self) -> BusClient {
        BusClient {
            addr: self.addr,
            token: self.token(),
        }
    }

    /// Graceful shutdown: flushes pending events, then stops. Idempotent.
    pub fn shutdown(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.shutdown().expect("graceful shutdown");
        }
    }

    /// Shuts the daemon down and opens its database read-only (`mode=ro`).
    pub fn shutdown_and_open_db(&mut self) -> Connection {
        self.shutdown();
        let path = self.data_dir().join("traction.db");
        Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .expect("read-only database")
    }
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.shutdown();
        }
    }
}

/// Starts another daemon on `data_dir` with the default test config and
/// returns the raw result (single-instance and port-conflict tests).
pub fn try_start_on(
    data_dir: &Path,
    adjust: impl FnOnce(&mut DaemonConfig),
) -> Result<DaemonHandle, traction_daemon::StartError> {
    let mut config = test_config(data_dir);
    adjust(&mut config);
    traction_daemon::start(config)
}

pub fn test_config(data_dir: &Path) -> DaemonConfig {
    let mut config = DaemonConfig::new(data_dir.to_path_buf());
    config.http_port = 0;
    config.commit_interval = Duration::from_secs(60);
    config
}

/// Raw HTTP/1.1 client for the Local Ingestion Bus.
#[derive(Clone)]
pub struct BusClient {
    addr: SocketAddr,
    token: String,
}

impl BusClient {
    pub fn token(&self) -> &str {
        &self.token
    }

    /// `POST /v1/event` with the default, valid `Host` and `Authorization`.
    pub fn post_event(&self, body: &str) -> BusResponse {
        self.request("POST", "/v1/event").body(body).send()
    }

    /// A request preloaded with a valid `Host` and bearer token. Headers can
    /// be replaced or removed before sending.
    pub fn request(&self, method: &str, path: &str) -> BusRequest {
        BusRequest {
            addr: self.addr,
            method: method.to_owned(),
            path: path.to_owned(),
            headers: vec![
                ("Host".to_owned(), format!("127.0.0.1:{}", self.addr.port())),
                ("Authorization".to_owned(), format!("Bearer {}", self.token)),
                ("Content-Type".to_owned(), "application/json".to_owned()),
                ("Connection".to_owned(), "close".to_owned()),
            ],
            body: Vec::new(),
        }
    }
}

pub struct BusRequest {
    addr: SocketAddr,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl BusRequest {
    /// Sets a header, replacing any existing one with the same name.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self = self.without_header(name);
        self.headers.push((name.to_owned(), value.to_owned()));
        self
    }

    pub fn without_header(mut self, name: &str) -> Self {
        self.headers.retain(|(n, _)| !n.eq_ignore_ascii_case(name));
        self
    }

    pub fn body(mut self, body: impl AsRef<[u8]>) -> Self {
        self.body = body.as_ref().to_vec();
        self
    }

    pub fn send(self) -> BusResponse {
        let mut raw = format!("{} {} HTTP/1.1\r\n", self.method, self.path).into_bytes();
        for (name, value) in &self.headers {
            raw.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        raw.extend_from_slice(format!("Content-Length: {}\r\n\r\n", self.body.len()).as_bytes());
        raw.extend_from_slice(&self.body);

        let mut stream = TcpStream::connect(self.addr).expect("connect to bus");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("read timeout");
        stream.write_all(&raw).expect("send request");
        let mut response = Vec::new();
        stream.read_to_end(&mut response).expect("read response");
        BusResponse::parse(&response)
    }
}

#[derive(Debug)]
pub struct BusResponse {
    pub status: u16,
    pub body: String,
}

impl BusResponse {
    fn parse(raw: &[u8]) -> Self {
        let text = String::from_utf8_lossy(raw);
        let (head, body) = text.split_once("\r\n\r\n").expect("HTTP response head");
        let status = head
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .expect("HTTP status code");
        Self {
            status,
            body: body.to_owned(),
        }
    }
}

/// A valid Traction Event (the `docs/protocol.md` example) whose `event_id`
/// ends in `n`, so each `n` is a distinct event.
pub fn valid_event(n: u32) -> String {
    valid_event_value(n).to_string()
}

pub fn valid_event_value(n: u32) -> serde_json::Value {
    serde_json::json!({
        "schema_version": 1,
        "event_id": event_id(n),
        "timestamp_ms": 1795432800000u64 + u64::from(n),
        "harness_id": "claude-code",
        "session_id": "session-1",
        "parent_orchestrator_id": null,
        "event_type": "TURN_END",
        "end_reason": "completed",
        "cwd": "/abs/path/to/working/dir",
        "project_id": "git:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        "model_id": "optional-model-string",
        "network_class": "cloud_api",
        "metrics_only": {
            "prompt_char_len": 142,
            "repo_symbol_hits": 3,
            "has_stacktrace": true,
            "tool_name": "Bash",
            "exit_code": 1,
            "diff_lines_delta": 24
        }
    })
}

pub fn event_id(n: u32) -> String {
    format!("019a7c2e-8b4d-7000-8000-{n:012x}")
}

/// Number of stored Traction Events.
pub fn stored_event_count(db: &Connection) -> i64 {
    db.query_row("SELECT COUNT(*) FROM traction_event", [], |row| row.get(0))
        .expect("count events")
}
