//! The server sync client (Step 4, S1): one thread that sends sealed Chain
//! Links and their rows to `POST /v1/anchors` (`docs/protocol.md` section 3).
//!
//! - It reads through a `mode=ro` connection with `PRAGMA query_only = ON`;
//!   the only thing it writes is the `sync_cursor` row, and only through the
//!   single SQLite writer (`WriteOp::SyncCursor`).
//! - The backlog is cut into ordered requests of at most
//!   [`MAX_LINKS_PER_REQUEST`] Chain Links, never mixing two keys (a request
//!   carries one Chain Tip).
//! - A request is `offline_buffered` once its oldest window ended more than
//!   [`OFFLINE_BUFFERED_AFTER`] ago (the server's own `MAX_ANCHOR_LAG`, so a
//!   `realtime` request is never one the server would call late).
//! - Failures retry with jittered exponential backoff and honour
//!   `Retry-After`. Only a terminal rejection (`window_too_old`) moves the
//!   cursor past rows the server did not take; it is counted in
//!   `sync_cursor.terminal_rejections`. Everything else is retried.
//!
//! Nothing here logs a token, a body or a row value.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::Connection;
use traction_core::TRACTION_RULES_1;
use traction_core::sync::{
    ChainTip, ErrorBody, HeartbeatRequest, HeartbeatResponse, ItemKind, ProjectMeta, SyncMode,
    codes,
};

use crate::log;
use crate::storage::{self, SyncCursorRow, WriteOp};
use crate::writer::WriterSender;

/// The token file in the data folder (`0600` / owner-only DACL).
pub const SERVER_TOKEN_FILE: &str = "server.token";
/// The server's base URL, written by `login`.
pub const SERVER_URL_FILE: &str = "server.url";
/// The `key_id` the server last pinned for this device (written by `login`).
pub const SERVER_KEY_ID_FILE: &str = "server.key_id";

/// How often a caught-up client looks for new Chain Links.
pub const SYNC_INTERVAL: Duration = Duration::from_secs(120);
/// Most Chain Links (120 s windows) in one request: one hour of history.
pub const MAX_LINKS_PER_REQUEST: usize = 30;
/// A request whose oldest window ended longer ago than this is
/// `offline_buffered`.
pub const OFFLINE_BUFFERED_AFTER: Duration = Duration::from_secs(20 * 60);
/// First retry delay; doubles per consecutive failure.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
/// Longest backoff.
pub const BACKOFF_MAX: Duration = Duration::from_secs(5 * 60);
/// A `Retry-After` above this is capped to it.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(60 * 60);
/// Per-request network timeout.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// Sync client tuning. Fields are public so tests can inject a local server
/// and a short schedule.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SyncSettings {
    /// `false` turns the client off.
    pub enabled: bool,
    /// The server's base URL. `None`: read `<data dir>/server.url` each pass.
    pub server_url: Option<String>,
    /// The token file. `None`: `<data dir>/server.token`.
    pub token_path: Option<PathBuf>,
    pub interval: Duration,
    pub max_links_per_request: usize,
    pub offline_after: Duration,
    pub backoff_base: Duration,
    pub backoff_max: Duration,
    pub request_timeout: Duration,
    /// The CI-signed build statement sent on the first heartbeat after the
    /// daemon starts. Defaults to the one compiled into this binary.
    pub build: Option<traction_core::sync::SignedBuildStatement>,
}

impl Default for SyncSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            server_url: None,
            token_path: None,
            interval: SYNC_INTERVAL,
            max_links_per_request: MAX_LINKS_PER_REQUEST,
            offline_after: OFFLINE_BUFFERED_AFTER,
            backoff_base: BACKOFF_BASE,
            backoff_max: BACKOFF_MAX,
            request_timeout: REQUEST_TIMEOUT,
            build: crate::provenance::compiled(),
        }
    }
}

/// The delay before retry number `attempt` (0 for the first): exponential
/// from `base`, capped at `max`, then scaled into 50% to 100% by `jitter`
/// (0..=1000 per mille) so a fleet of daemons does not retry in step.
pub fn backoff_delay(
    attempt: u32,
    base: Duration,
    max: Duration,
    jitter_permille: u64,
) -> Duration {
    let exponential = base
        .saturating_mul(1u32.checked_shl(attempt.min(30)).unwrap_or(u32::MAX))
        .min(max);
    let factor = 500 + jitter_permille.min(1000) / 2;
    exponential.mul_f64(factor as f64 / 1000.0)
}

/// How long to wait: the backoff, or the server's `Retry-After` if longer.
pub fn wait_after_failure(backoff: Duration, retry_after: Option<Duration>) -> Duration {
    backoff.max(retry_after.unwrap_or_default().min(MAX_RETRY_AFTER))
}

/// `offline_buffered` once the oldest window ended more than `after` ago.
pub fn sync_mode_for(oldest_window_start_ms: u64, now_ms: u64, after: Duration) -> SyncMode {
    let ended = oldest_window_start_ms + TRACTION_RULES_1.activity_rollup_ms;
    let age = now_ms.saturating_sub(ended);
    if u128::from(age) > after.as_millis() {
        SyncMode::OfflineBuffered
    } else {
        SyncMode::Realtime
    }
}

fn jitter_permille() -> u64 {
    let mut raw = [0u8; 2];
    let _ = getrandom::fill(&mut raw);
    u64::from(u16::from_le_bytes(raw)) % 1001
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The sync cursor as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    /// Newest Chain Link window the server acknowledged, or that a terminal
    /// rejection moved past. `None` before the first.
    pub window_start_ms: Option<u64>,
    pub terminal_rejections: u64,
}

pub fn read_cursor(conn: &Connection) -> rusqlite::Result<Cursor> {
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT window_start_ms, terminal_rejections FROM sync_cursor WHERE id = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(match row {
        Some((window, rejected)) => Cursor {
            window_start_ms: u64::try_from(window).ok(),
            terminal_rejections: u64::try_from(rejected).unwrap_or(0),
        },
        None => Cursor::default(),
    })
}

/// The next request to send, and where it leaves the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub request: HeartbeatRequest,
    pub last_key_id: String,
    pub last_seq: u64,
    pub last_window_ms: u64,
    pub links: usize,
    /// Work Buckets plus Activity Rollups carried.
    pub items: u64,
}

/// Builds the next ordered request after `cursor`, or `None` when caught up.
pub fn build_chunk(
    conn: &Connection,
    cursor: Cursor,
    now_ms: u64,
    settings: &SyncSettings,
    os_username: &str,
) -> rusqlite::Result<Option<Chunk>> {
    let after = cursor
        .window_start_ms
        .map_or(-1, |w| i64::try_from(w).unwrap_or(i64::MAX));
    let mut stmt = conn.prepare(
        "SELECT key_id, seq, window_start_ms, entry_hash FROM chain_link
         WHERE window_start_ms > ?1 ORDER BY window_start_ms LIMIT ?2",
    )?;
    let limit = i64::try_from(settings.max_links_per_request.max(1)).unwrap_or(i64::MAX);
    let all: Vec<(String, i64, i64, String)> = stmt
        .query_map(rusqlite::params![after, limit], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<Result<_, _>>()?;
    let Some(first) = all.first() else {
        return Ok(None);
    };
    // One request, one key: stop at the first link of another chain.
    let links: Vec<&(String, i64, i64, String)> =
        all.iter().take_while(|link| link.0 == first.0).collect();
    let (Some(last), Some(first)) = (links.last(), links.first()) else {
        return Ok(None);
    };
    let (first_window, last_window) = (
        u64::try_from(first.2).unwrap_or(0),
        u64::try_from(last.2).unwrap_or(0),
    );
    let rows = storage::load_sealed_rows(
        conn,
        first_window,
        last_window + TRACTION_RULES_1.activity_rollup_ms,
    )?;
    let timeline_marks = storage::load_marks(
        conn,
        first_window,
        last_window + TRACTION_RULES_1.activity_rollup_ms,
    )?;
    let mut project_ids: Vec<&str> = rows
        .work_buckets
        .iter()
        .map(|b| b.project_id.as_str())
        .chain(timeline_marks.iter().map(|m| m.project_id.as_str()))
        .collect();
    project_ids.sort_unstable();
    project_ids.dedup();
    let mut projects = Vec::new();
    for id in project_ids {
        let meta = conn.query_row(
            "SELECT name, root_path, home_relative_path FROM project WHERE project_id = ?1",
            [id],
            |row| {
                Ok(ProjectMeta {
                    project_id: id.to_owned(),
                    project_name: row.get(0)?,
                    project_root_path: row.get(1)?,
                    project_home_relative_path: row.get(2)?,
                    os_username: os_username.to_owned(),
                })
            },
        );
        match meta {
            Ok(meta) => projects.push(meta),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(error) => return Err(error),
        }
    }
    let items = (rows.work_buckets.len() + rows.rollups.len()) as u64;
    let tip = ChainTip {
        key_id: key_id_from(&last.0),
        seq: u64::try_from(last.1).unwrap_or(0),
        entry_hash: last.3.clone(),
        window_start_ms: last_window,
    };
    Ok(Some(Chunk {
        request: HeartbeatRequest {
            tip,
            sync_mode: sync_mode_for(first_window, now_ms, settings.offline_after),
            sent_at_ms: now_ms,
            work_buckets: rows.work_buckets,
            activity_rollups: rows.rollups,
            projects,
            timeline_marks,
            build: None,
        },
        last_key_id: last.0.clone(),
        last_seq: u64::try_from(last.1).unwrap_or(0),
        last_window_ms: last_window,
        links: links.len(),
        items,
    }))
}

fn key_id_from(text: &str) -> traction_core::KeyId {
    // `KeyId` is a transparent string on the wire; build it the same way the
    // chain does, through serde.
    serde_json::from_value(serde_json::Value::String(text.to_owned())).expect("a KeyId is a string")
}

/// One HTTP reply.
#[derive(Debug)]
pub(crate) struct Reply {
    pub status: u16,
    pub retry_after: Option<Duration>,
    pub body: Vec<u8>,
}

/// `POST <url>` with a JSON body. Redirects are never followed (a token must
/// not travel to another host).
pub(crate) fn post_json(
    url: &str,
    token: Option<&str>,
    body: &[u8],
    timeout: Duration,
) -> Result<Reply, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(timeout))
        .build()
        .into();
    let mut request = agent.post(url).header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let mut response = request.send(body).map_err(|e| e.to_string())?;
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let status = response.status().as_u16();
    let body = response
        .body_mut()
        .with_config()
        .limit(1024 * 1024)
        .read_to_vec()
        .map_err(|e| e.to_string())?;
    Ok(Reply {
        status,
        retry_after,
        body,
    })
}

/// Why a pass did not finish its work.
#[derive(Debug)]
enum Retry {
    After(Option<Duration>),
}

/// What a pass achieved.
#[derive(Debug, PartialEq, Eq)]
enum Pass {
    /// Not configured, or caught up.
    Idle,
    /// A request was acknowledged; more may remain.
    Progressed,
}

struct Client {
    data_dir: PathBuf,
    settings: SyncSettings,
    writer: WriterSender,
    cursor: Option<Cursor>,
    os_username: String,
    rejected_logs: u64,
    /// Whether the server acknowledged the build statement this run.
    build_sent: bool,
}

impl Client {
    fn server_url(&self) -> Option<String> {
        let text = match &self.settings.server_url {
            Some(url) => url.clone(),
            None => std::fs::read_to_string(self.data_dir.join(SERVER_URL_FILE)).ok()?,
        };
        let text = text.trim().trim_end_matches('/');
        (!text.is_empty()).then(|| text.to_owned())
    }

    fn token(&self) -> Option<String> {
        let path = self
            .settings
            .token_path
            .clone()
            .unwrap_or_else(|| self.data_dir.join(SERVER_TOKEN_FILE));
        let text = std::fs::read_to_string(path).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    fn pass(&mut self) -> Result<Pass, Retry> {
        let (Some(url), Some(token)) = (self.server_url(), self.token()) else {
            return Ok(Pass::Idle);
        };
        let conn = match storage::open_read_only(&self.data_dir) {
            Ok(conn) => conn,
            Err(error) => {
                log::error("sync cannot open the database", &error);
                return Err(Retry::After(None));
            }
        };
        let cursor = match self.cursor {
            Some(cursor) => cursor,
            None => {
                let cursor = read_cursor(&conn).map_err(|error| {
                    log::error("sync cannot read its cursor", &error);
                    Retry::After(None)
                })?;
                self.cursor = Some(cursor);
                cursor
            }
        };
        let now = now_ms();
        let chunk = build_chunk(&conn, cursor, now, &self.settings, &self.os_username).map_err(
            |error| {
                log::error("sync cannot read sealed rows", &error);
                Retry::After(None)
            },
        )?;
        let Some(mut chunk) = chunk else {
            return Ok(Pass::Idle);
        };
        let sending_build = !self.build_sent && self.settings.build.is_some();
        if sending_build {
            chunk.request.build = self.settings.build.clone();
        }
        let body = serde_json::to_vec(&chunk.request).map_err(|_| Retry::After(None))?;
        let reply = post_json(
            &format!("{url}/v1/anchors"),
            Some(&token),
            &body,
            self.settings.request_timeout,
        )
        .map_err(|error| {
            log::error("sync request failed", &error);
            Retry::After(None)
        })?;
        match reply.status {
            200 => {
                self.build_sent |= sending_build;
                let response: HeartbeatResponse =
                    serde_json::from_slice(&reply.body).map_err(|_| {
                        log::warn("the server's heartbeat answer was not understood");
                        Retry::After(None)
                    })?;
                // Timeline Marks are unsigned context (`docs/adr/0005`): a
                // refused mark never holds the cursor back or is retried.
                let rejected: Vec<_> = response
                    .rejected
                    .iter()
                    .filter(|r| r.item.kind != ItemKind::TimelineMark)
                    .collect();
                let terminal = rejected
                    .iter()
                    .filter(|r| r.code == codes::WINDOW_TOO_OLD)
                    .count() as u64;
                if rejected.len() as u64 > terminal {
                    self.rejected_logs += 1;
                    log::warn_counted(
                        "the server rejected synced rows; will retry them",
                        self.rejected_logs,
                    );
                    return Err(Retry::After(None));
                }
                self.advance(&chunk, terminal);
                Ok(Pass::Progressed)
            }
            422 if error_code(&reply.body).as_deref() == Some(codes::WINDOW_TOO_OLD) => {
                // Terminal: the whole request is older than the server keeps.
                self.advance(&chunk, chunk.items);
                Ok(Pass::Progressed)
            }
            429 => Err(Retry::After(reply.retry_after)),
            _ => {
                self.rejected_logs += 1;
                log::warn_counted(
                    "the server did not accept a sync request",
                    self.rejected_logs,
                );
                Err(Retry::After(reply.retry_after))
            }
        }
    }

    fn advance(&mut self, chunk: &Chunk, terminal: u64) {
        let total = self.cursor.map_or(0, |c| c.terminal_rejections) + terminal;
        self.cursor = Some(Cursor {
            window_start_ms: Some(chunk.last_window_ms),
            terminal_rejections: total,
        });
        let _ = self.writer.send(WriteOp::SyncCursor(SyncCursorRow {
            window_start_ms: chunk.last_window_ms,
            key_id: chunk.last_key_id.clone(),
            seq: chunk.last_seq,
            terminal_rejections: total,
            updated_at_ms: now_ms(),
        }));
    }
}

fn error_code(body: &[u8]) -> Option<String> {
    serde_json::from_slice::<ErrorBody>(body)
        .ok()
        .map(|b| b.error)
}

/// The OS account name, for Project metadata.
fn os_username() -> String {
    ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "unknown".to_owned())
}

pub struct SyncThread {
    stop: Sender<()>,
    handle: JoinHandle<()>,
}

impl SyncThread {
    /// Stops the thread and waits for it (dropping its writer sender).
    pub fn stop(self) -> thread::Result<()> {
        let _ = self.stop.send(());
        self.handle.join()
    }
}

pub fn spawn(
    data_dir: &Path,
    writer: WriterSender,
    settings: SyncSettings,
) -> io::Result<SyncThread> {
    let (stop, rx) = mpsc::channel();
    let mut client = Client {
        data_dir: data_dir.to_path_buf(),
        settings,
        writer,
        cursor: None,
        os_username: os_username(),
        rejected_logs: 0,
        build_sent: false,
    };
    let handle = thread::Builder::new()
        .name("traction-sync".to_owned())
        .spawn(move || {
            let mut wait = Duration::ZERO;
            let mut failures = 0u32;
            loop {
                match rx.recv_timeout(wait) {
                    Err(RecvTimeoutError::Timeout) => {}
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                }
                match client.pass() {
                    Ok(Pass::Progressed) => {
                        failures = 0;
                        wait = Duration::ZERO;
                    }
                    Ok(Pass::Idle) => {
                        failures = 0;
                        wait = client.settings.interval;
                    }
                    Err(Retry::After(retry_after)) => {
                        let backoff = backoff_delay(
                            failures,
                            client.settings.backoff_base,
                            client.settings.backoff_max,
                            jitter_permille(),
                        );
                        failures = failures.saturating_add(1);
                        wait = wait_after_failure(backoff, retry_after);
                    }
                }
            }
        })?;
    Ok(SyncThread { stop, handle })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap_with_jitter_between_half_and_full() {
        let base = Duration::from_secs(1);
        let max = Duration::from_secs(300);
        assert_eq!(backoff_delay(0, base, max, 1000), Duration::from_secs(1));
        assert_eq!(backoff_delay(0, base, max, 0), Duration::from_millis(500));
        assert_eq!(backoff_delay(3, base, max, 1000), Duration::from_secs(8));
        assert_eq!(backoff_delay(20, base, max, 1000), max);
        assert_eq!(backoff_delay(200, base, max, 0), Duration::from_secs(150));
    }

    #[test]
    fn retry_after_wins_when_longer_and_is_capped() {
        let backoff = Duration::from_secs(2);
        assert_eq!(wait_after_failure(backoff, None), backoff);
        assert_eq!(
            wait_after_failure(backoff, Some(Duration::from_secs(30))),
            Duration::from_secs(30)
        );
        assert_eq!(
            wait_after_failure(backoff, Some(Duration::from_secs(1))),
            backoff
        );
        assert_eq!(
            wait_after_failure(backoff, Some(Duration::from_secs(999_999))),
            MAX_RETRY_AFTER
        );
    }

    #[test]
    fn a_backlog_older_than_the_constant_is_offline_buffered() {
        let after = OFFLINE_BUFFERED_AFTER;
        let window = 1_800_000_000_000 / 120_000 * 120_000;
        let ended = window + 120_000;
        let at = |age_ms: u64| sync_mode_for(window, ended + age_ms, after);
        assert_eq!(at(0), SyncMode::Realtime);
        assert_eq!(at(20 * 60_000), SyncMode::Realtime);
        assert_eq!(at(20 * 60_000 + 1), SyncMode::OfflineBuffered);
    }
}
