//! Shared Seam 2 helper: a real in-process daemon on a fresh temp data folder
//! and an ephemeral loopback port, plus a raw HTTP/1.1 caller.
//!
//! Tests interact only through the bus (HTTP or the Unix socket), the token
//! file the daemon wrote and a read-only SQLite connection opened after
//! shutdown. The caller writes raw request bytes so tests can send arbitrary
//! (or missing) `Host`, `Origin` and `Authorization` headers.
//!
//! Since Step 1b every daemon also gets a fake `home_dir` (its own temp
//! folder, canonicalized), so routing fixtures live under a fake home and
//! never the developer's real one. [`fixtures`] builds real Git repos and
//! folders; [`poll_until`] waits for asynchronous results.
//!
//! Each test binary uses a different subset of this module.
#![allow(dead_code)]

pub mod fixtures;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::{Connection, OpenFlags};
use tempfile::TempDir;
#[cfg(unix)]
use traction_daemon::DATA_DIR_SOCKET_FILE;
use traction_daemon::{DB_FILE, DaemonConfig, DaemonHandle, TOKEN_FILE};

/// A daemon running on its own temp data folder. Shut down on drop.
pub struct TestDaemon {
    data_dir: TempDir,
    /// The daemon's `home_dir`: a temp folder, canonicalized.
    home: TempDir,
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
        let home = tempfile::tempdir().expect("temp fake home");
        let mut config = test_config(data_dir.path());
        config.home_dir = Some(canonical(home.path()));
        adjust(&mut config);
        let handle = traction_daemon::start(config).expect("daemon starts");
        let addr = handle.local_addr();
        Self {
            data_dir,
            home,
            handle: Some(handle),
            addr,
        }
    }

    /// Starts a new daemon on the data folder of a previous, shut-down one.
    pub fn restart(self) -> Self {
        self.restart_with(|_| {})
    }

    /// [`Self::restart`], letting the test adjust the new daemon's config.
    pub fn restart_with(mut self, adjust: impl FnOnce(&mut DaemonConfig)) -> Self {
        self.shutdown();
        let mut config = test_config(self.data_dir.path());
        config.home_dir = Some(self.home());
        adjust(&mut config);
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

    /// The daemon's fake `$HOME` (canonical). Build fixtures below it.
    pub fn home(&self) -> PathBuf {
        canonical(self.home.path())
    }

    /// The 32-byte device secret the daemon wrote (keys `dir:` IDs).
    pub fn device_secret(&self) -> [u8; 32] {
        std::fs::read(self.data_dir().join(traction_daemon::DEVICE_SECRET_FILE))
            .expect("device secret")
            .try_into()
            .expect("32 bytes")
    }

    /// The `dir:` Project ID this daemon derives for the Project Root `root`.
    pub fn dir_id(&self, root: &Path) -> String {
        traction_core::dir_project_id(&self.device_secret(), &canonical_string(root))
    }

    pub fn token_path(&self) -> PathBuf {
        self.data_dir().join(TOKEN_FILE)
    }

    /// The bearer token the daemon wrote to `<data dir>/daemon.token`.
    pub fn token(&self) -> String {
        std::fs::read_to_string(self.token_path()).expect("token file")
    }

    /// A caller holding the daemon's real token.
    pub fn caller(&self) -> BusCaller {
        BusCaller::new(self.addr, self.token())
    }

    /// The configured Unix socket path (`<temp data dir>/daemon.sock`).
    #[cfg(unix)]
    pub fn socket_path(&self) -> PathBuf {
        self.handle
            .as_ref()
            .expect("daemon is running")
            .socket_path()
            .to_path_buf()
    }

    /// A caller holding the real token that talks over the Unix socket.
    #[cfg(unix)]
    pub fn socket_caller(&self) -> BusCaller {
        self.caller().via_socket(self.socket_path())
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
        open_db_read_only(self.data_dir())
    }

    /// A read-only connection to the running daemon's database (WAL readers
    /// never block the writer). Sees only committed batches.
    pub fn live_db(&self) -> Connection {
        open_db_read_only(self.data_dir())
    }

    /// Polls the running daemon until exactly `count` `FILE_MUTATION` rows
    /// are committed (panics on more, or after [`WATCH_DEADLINE`]).
    pub fn wait_for_mutations(&self, count: usize) -> Vec<MutationRow> {
        let db = self.live_db();
        poll_until(WATCH_DEADLINE, "FILE_MUTATION rows", || {
            let rows = stored_mutations(&db);
            assert!(
                rows.len() <= count,
                "more mutations than expected: {rows:?}"
            );
            (rows.len() == count).then_some(rows)
        })
    }
}

/// How long a test waits for the watcher: generous, for loaded CI runners.
pub const WATCH_DEADLINE: Duration = Duration::from_secs(30);

/// Watcher test settings: a short hold (the test-only override of
/// `LOG_TAIL_HOLD`) and frequent commits, so rows appear while the daemon
/// runs.
pub fn fast_watcher(config: &mut DaemonConfig) {
    config.writer.commit_interval = Duration::from_millis(50);
    config.watcher.hold = Duration::from_secs(1);
}

/// A stored `FILE_MUTATION`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MutationRow {
    pub project_id: Option<String>,
    pub cwd: Option<String>,
    pub harness_id: Option<String>,
    pub session_id: Option<String>,
    pub network_class: String,
    pub attribution_source: String,
    pub mutation_kind: Option<String>,
}

/// Every stored `FILE_MUTATION`, oldest first.
pub fn stored_mutations(db: &Connection) -> Vec<MutationRow> {
    let mut stmt = db
        .prepare(
            "SELECT project_id, cwd, harness_id, session_id, network_class,
                    attribution_source, mutation_kind
             FROM traction_event WHERE event_type = 'FILE_MUTATION' ORDER BY timestamp_ms, event_id",
        )
        .expect("prepare");
    stmt.query_map([], |row| {
        Ok(MutationRow {
            project_id: row.get(0)?,
            cwd: row.get(1)?,
            harness_id: row.get(2)?,
            session_id: row.get(3)?,
            network_class: row.get(4)?,
            attribution_source: row.get(5)?,
            mutation_kind: row.get(6)?,
        })
    })
    .expect("query")
    .map(|row| row.expect("row"))
    .collect()
}

/// The sorted `mutation_kind`s of `rows`.
pub fn kinds(rows: &[MutationRow]) -> Vec<String> {
    let mut kinds: Vec<String> = rows
        .iter()
        .map(|row| row.mutation_kind.clone().expect("mutation_kind"))
        .collect();
    kinds.sort();
    kinds
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

/// Opens `<data dir>/traction.db` read-only (`mode=ro`).
pub fn open_db_read_only(data_dir: &Path) -> Connection {
    Connection::open_with_flags(
        data_dir.join(DB_FILE),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("read-only database")
}

pub fn test_config(data_dir: &Path) -> DaemonConfig {
    let mut config = DaemonConfig::new(data_dir.to_path_buf());
    // Hermetic: never the developer's real home folder.
    config.home_dir = Some(data_dir.join("fake-home"));
    config.http_port = 0;
    config.writer.commit_interval = Duration::from_secs(60);
    // Hermetic: never the real `$XDG_RUNTIME_DIR`. A temp data folder keeps
    // the path well under the macOS 104-byte `sun_path` limit.
    #[cfg(unix)]
    {
        config.socket_path = data_dir.join(DATA_DIR_SOCKET_FILE);
    }
    config
}

/// Raw HTTP/1.1 caller for the Local Ingestion Bus.
#[derive(Clone)]
pub struct BusCaller {
    addr: SocketAddr,
    token: String,
    endpoint: Endpoint,
}

/// Where a request is sent: loopback TCP or (Unix only) the bus socket.
#[derive(Clone)]
enum Endpoint {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

impl BusCaller {
    /// A caller for a daemon started outside [`TestDaemon`] (e.g. the binary).
    pub fn new(addr: SocketAddr, token: String) -> Self {
        Self {
            addr,
            token,
            endpoint: Endpoint::Tcp(addr),
        }
    }

    /// The same caller, but sending over the Unix socket at `path`. Requests
    /// still carry the HTTP `Host` header unless a test removes it.
    #[cfg(unix)]
    pub fn via_socket(mut self, path: PathBuf) -> Self {
        self.endpoint = Endpoint::Unix(path);
        self
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// The daemon's bound loopback port.
    pub fn port(&self) -> u16 {
        self.addr.port()
    }

    /// `POST /v1/event` with the default, valid `Host` and `Authorization`.
    pub fn post_event(&self, body: &str) -> BusResponse {
        self.request("POST", "/v1/event").body(body).send()
    }

    /// `POST /v1/project` registering `path`.
    pub fn register_project(&self, path: &Path) -> BusResponse {
        let body = serde_json::json!({ "path": path_str(path) }).to_string();
        self.request("POST", "/v1/project").body(body).send()
    }

    /// A request preloaded with a valid `Host` and bearer token. Headers can
    /// be replaced or removed before sending.
    pub fn request(&self, method: &str, path: &str) -> BusRequest {
        BusRequest {
            endpoint: self.endpoint.clone(),
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
    endpoint: Endpoint,
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
        self.send_with(Delivery::Together)
    }

    /// Sends the body [`BODY_PAUSE`] after the head and fails unless every
    /// byte was written and the daemon closed cleanly (no reset). The pause
    /// lets the daemon refuse on headers alone before the body arrives, which
    /// is when closing with the body unread makes the kernel reset the
    /// connection. Callers on Windows lose already-received bytes on a reset,
    /// so real Adapters rely on this.
    pub fn send_expecting_clean_close(self) -> BusResponse {
        self.send_with(Delivery::Paused)
    }

    /// Sends only the head (still declaring the body's `Content-Length`) and
    /// keeps the connection open: a stalled caller.
    pub fn send_withholding_body(self) -> BusResponse {
        self.send_with(Delivery::Withheld)
    }

    fn send_with(self, delivery: Delivery) -> BusResponse {
        let mut raw = format!("{} {} HTTP/1.1\r\n", self.method, self.path).into_bytes();
        for (name, value) in &self.headers {
            raw.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        raw.extend_from_slice(format!("Content-Length: {}\r\n\r\n", self.body.len()).as_bytes());

        let response = match &self.endpoint {
            Endpoint::Tcp(addr) => {
                let stream = TcpStream::connect(addr).expect("connect to bus");
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("read timeout");
                exchange(stream, &raw, &self.body, delivery)
            }
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                let stream =
                    std::os::unix::net::UnixStream::connect(path).expect("connect to bus socket");
                stream
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .expect("read timeout");
                exchange(stream, &raw, &self.body, delivery)
            }
        };
        BusResponse::parse(&response)
    }
}

/// How long [`BusRequest::send_expecting_clean_close`] waits between the
/// head and the body, and again before reading. Well inside the daemon's
/// drain window.
const BODY_PAUSE: Duration = Duration::from_millis(10);

/// How [`exchange`] sends the body, and so how it treats the close.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Delivery {
    /// Head and body back to back. A failed write, or a reset after response
    /// bytes arrived, is tolerated.
    Together,
    /// The body follows the head after [`BODY_PAUSE`]. Every byte must be
    /// written and the read must end in a clean EOF.
    Paused,
    /// The body is never sent. Tolerates disconnects like `Together`.
    Withheld,
}

/// Writes the request and reads until the daemon closes the connection.
///
/// The daemon may answer and close before reading the whole body (an early
/// refusal), so unless [`Delivery::Paused`] demands a clean close, a failed
/// write is not fatal and a reset after response bytes arrived ends the read
/// instead of failing it. A reset before any response byte always fails the
/// test: that is a lost response.
fn exchange(
    mut stream: impl Read + Write,
    head: &[u8],
    body: &[u8],
    delivery: Delivery,
) -> Vec<u8> {
    let may_reset = delivery != Delivery::Paused;
    let written = stream.write_all(head).and_then(|()| match delivery {
        Delivery::Together => stream.write_all(body),
        Delivery::Paused => {
            std::thread::sleep(BODY_PAUSE);
            stream.write_all(body)?;
            // Let a reset, if the daemon sends one, arrive before reading.
            std::thread::sleep(BODY_PAUSE);
            Ok(())
        }
        Delivery::Withheld => Ok(()),
    });
    if let Err(error) = written {
        assert!(may_reset && is_disconnect(&error), "send request: {error}");
    }
    let mut response = Vec::new();
    let mut chunk = [0; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&chunk[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) if may_reset && is_disconnect(&error) && !response.is_empty() => break,
            Err(error) => panic!("read response: {error}"),
        }
    }
    response
}

/// The peer closed or reset the connection.
fn is_disconnect(error: &std::io::Error) -> bool {
    use std::io::ErrorKind::{BrokenPipe, ConnectionAborted, ConnectionReset};
    matches!(
        error.kind(),
        BrokenPipe | ConnectionAborted | ConnectionReset
    )
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
        "attribution_source": "harness_hook",
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

/// Calls `probe` until it returns `Some`, or panics after `timeout`.
pub fn poll_until<T>(timeout: Duration, what: &str, mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `path` as UTF-8 (fixture paths always are).
pub fn path_str(path: &Path) -> &str {
    path.to_str().expect("UTF-8 path")
}

/// The OS canonical form of `path`, as the daemon stores it: symlinks
/// resolved and, on Windows, no `\\?\` prefix and an uppercase drive letter.
pub fn canonical(path: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(path).expect("canonicalize fixture path");
    #[cfg(windows)]
    {
        let text = canon.to_str().expect("UTF-8 path");
        let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else {
            text.strip_prefix(r"\\?\").unwrap_or(text).to_owned()
        };
        let mut text = text.into_bytes();
        if text.get(1) == Some(&b':') {
            text[0] = text[0].to_ascii_uppercase();
        }
        PathBuf::from(String::from_utf8(text).expect("UTF-8 path"))
    }
    #[cfg(not(windows))]
    canon
}

pub fn canonical_string(path: &Path) -> String {
    path_str(&canonical(path)).to_owned()
}

/// A routed Traction Event: no `project_id`, the given type, session and
/// optional `cwd`. `n` makes the `event_id` (and timestamp) distinct.
pub fn routed_event(n: u32, event_type: &str, session: &str, cwd: Option<&Path>) -> String {
    let mut event = valid_event_value(n);
    event["project_id"] = serde_json::Value::Null;
    event["event_type"] = serde_json::json!(event_type);
    event["session_id"] = serde_json::json!(session);
    event["cwd"] = match cwd {
        Some(cwd) => serde_json::json!(path_str(cwd)),
        None => serde_json::Value::Null,
    };
    if event_type != "TURN_END" {
        event["end_reason"] = serde_json::Value::Null;
    }
    event.to_string()
}

/// Every stored `(event_id, project_id)`, ordered by `event_id`.
pub fn stored_project_ids(db: &Connection) -> Vec<(String, Option<String>)> {
    let mut stmt = db
        .prepare("SELECT event_id, project_id FROM traction_event ORDER BY event_id")
        .expect("prepare");
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect()
}

/// The `project_id` stored for event `n`.
pub fn stored_project_id(db: &Connection, n: u32) -> Option<String> {
    db.query_row(
        "SELECT project_id FROM traction_event WHERE event_id = ?1",
        [event_id(n)],
        |row| row.get(0),
    )
    .expect("stored event")
}

/// A registered Project row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub project_id: String,
    pub kind: String,
    pub root_path: String,
    pub home_relative_path: Option<String>,
    pub name: String,
    pub git_common_dir: Option<String>,
}

/// Every registered Project, ordered by `root_path`.
pub fn registered_projects(db: &Connection) -> Vec<ProjectRow> {
    let mut stmt = db
        .prepare(
            "SELECT project_id, kind, root_path, home_relative_path, name, git_common_dir
             FROM project ORDER BY root_path",
        )
        .expect("prepare");
    stmt.query_map([], |row| {
        Ok(ProjectRow {
            project_id: row.get(0)?,
            kind: row.get(1)?,
            root_path: row.get(2)?,
            home_relative_path: row.get(3)?,
            name: row.get(4)?,
            git_common_dir: row.get(5)?,
        })
    })
    .expect("query")
    .map(|row| row.expect("row"))
    .collect()
}

/// Counts copies of `needle` in every column of every table, read as text.
pub fn sentinel_hits_in_tables(db: &Connection, needle: &str) -> usize {
    let tables: Vec<String> = db
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table'")
        .and_then(|mut stmt| stmt.query_map([], |row| row.get(0))?.collect())
        .expect("list tables");
    assert!(tables.iter().any(|t| t == "traction_event"));
    let mut hits = 0;
    for table in tables {
        let columns: Vec<String> = db
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .and_then(|mut stmt| stmt.query_map([], |row| row.get(0))?.collect())
            .expect("list columns");
        for column in columns {
            let sql = format!(
                "SELECT COUNT(*) FROM \"{table}\" WHERE instr(CAST(\"{column}\" AS TEXT), ?1) > 0"
            );
            let count: i64 = db
                .query_row(&sql, [needle], |row| row.get(0))
                .expect("scan column");
            hits += usize::try_from(count).expect("non-negative");
        }
    }
    hits
}

/// Asserts no file in `data_dir` (database, `-wal`/`-shm`, token, secret)
/// holds `needle` as raw bytes. Call after shutdown. Returns the names
/// scanned.
pub fn assert_no_file_contains(data_dir: &Path, needle: &str) -> Vec<String> {
    let mut scanned = Vec::new();
    for entry in std::fs::read_dir(data_dir).expect("data folder") {
        let path = entry.expect("entry").path();
        if !path.is_file() {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read file");
        assert!(
            !bytes
                .windows(needle.len())
                .any(|window| window == needle.as_bytes()),
            "sentinel found in {}",
            path.display()
        );
        scanned.push(
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .into_owned(),
        );
    }
    scanned
}
