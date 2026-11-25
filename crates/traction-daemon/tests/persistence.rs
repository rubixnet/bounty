//! Seam 2 persistence tests: duplicate handling, the writer's commit triggers,
//! backpressure, flush on shutdown, and the no-raw-text sentinel scan.
//!
//! Waiting is always "poll until true, with a generous deadline", never a
//! fixed sleep that must be long enough, so slow CI machines only slow the
//! tests down. The one place a test holds something for a fixed time (the
//! retry test) asserts on the outcome after the hold, not on timing.

mod common;

use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use common::{TestDaemon, event_id, stored_event_count, valid_event, valid_event_value};
use rusqlite::{Connection, OpenFlags};
use traction_daemon::DB_FILE;

/// Far longer than any commit should take, even on a loaded CI runner.
const DEADLINE: Duration = Duration::from_secs(20);

/// A string that must never reach disk. Valid in every bounded field's
/// character set, so a rejection is due to the unknown key, not the value.
const SENTINEL: &str = "TRACTION-SENTINEL-c0ffee-raw-prompt-text";

fn db_path(daemon: &TestDaemon) -> std::path::PathBuf {
    daemon.data_dir().join(DB_FILE)
}

/// A read-only connection to a running daemon's database (WAL readers do not
/// block the writer).
fn open_db_while_running(daemon: &TestDaemon) -> Connection {
    Connection::open_with_flags(
        db_path(daemon),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .expect("read-only database")
}

/// Polls the running daemon's database until it holds `expected` events.
fn wait_for_stored_events(daemon: &TestDaemon, expected: i64) {
    let db = open_db_while_running(daemon);
    let started = Instant::now();
    loop {
        let count = stored_event_count(&db);
        if count == expected {
            return;
        }
        assert!(
            started.elapsed() < DEADLINE,
            "expected {expected} committed events, found {count}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

/// Another process taking SQLite's write lock (a backup tool, a stalled disk),
/// so the daemon's writer cannot commit until it is dropped.
fn hold_write_lock(daemon: &TestDaemon) -> Connection {
    let conn = Connection::open(db_path(daemon)).expect("second connection");
    conn.execute_batch("BEGIN IMMEDIATE")
        .expect("take the write lock");
    conn
}

#[test]
fn the_same_event_sent_twice_is_accepted_twice_and_stored_once() {
    let mut daemon = TestDaemon::start();
    let caller = daemon.caller();

    for _ in 0..2 {
        let response = caller.post_event(&valid_event(1));
        assert_eq!(response.status, 202, "{}", response.body);
    }

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 1);
}

#[test]
fn a_retry_after_the_first_copy_was_committed_is_accepted_and_ignored() {
    // `max_batch = 1` commits every event on its own, so the second copy
    // meets an already stored row rather than a duplicate in the same batch.
    let mut daemon = TestDaemon::start_with(|config| config.writer.max_batch = 1);
    let caller = daemon.caller();

    assert_eq!(caller.post_event(&valid_event(1)).status, 202);
    wait_for_stored_events(&daemon, 1);
    let retry = caller.post_event(&valid_event(1));
    assert_eq!(retry.status, 202, "{}", retry.body);
    assert_eq!(caller.post_event(&valid_event(2)).status, 202);

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 2);
}

#[test]
fn a_duplicate_does_not_overwrite_the_stored_event() {
    let mut daemon = TestDaemon::start();
    let caller = daemon.caller();

    let first = valid_event_value(1);
    let mut retry = first.clone();
    retry["timestamp_ms"] = serde_json::json!(1);
    assert_eq!(caller.post_event(&first.to_string()).status, 202);
    assert_eq!(caller.post_event(&retry.to_string()).status, 202);

    let db = daemon.shutdown_and_open_db();
    let timestamp: i64 = db
        .query_row(
            "SELECT timestamp_ms FROM traction_event WHERE event_id = ?1",
            [event_id(1)],
            |row| row.get(0),
        )
        .expect("stored event");
    assert_eq!(timestamp, 1_795_432_800_001);
}

#[test]
fn a_full_batch_is_committed_without_waiting_for_the_timer() {
    // The test config's timer is 60 s, so a commit inside the deadline can
    // only come from the size threshold.
    let daemon = TestDaemon::start_with(|config| config.writer.max_batch = 3);
    let caller = daemon.caller();

    for n in 0..3 {
        assert_eq!(caller.post_event(&valid_event(n)).status, 202);
    }

    wait_for_stored_events(&daemon, 3);
}

#[test]
fn a_partial_batch_is_committed_by_the_timer() {
    let daemon = TestDaemon::start_with(|config| {
        config.writer.commit_interval = Duration::from_millis(100);
        config.writer.max_batch = 1000;
    });
    let caller = daemon.caller();

    assert_eq!(caller.post_event(&valid_event(1)).status, 202);

    wait_for_stored_events(&daemon, 1);
}

#[test]
fn a_full_writer_queue_answers_503_at_once_and_loses_nothing_accepted() {
    let mut daemon = TestDaemon::start_with(|config| {
        config.writer.max_batch = 1;
        config.writer.queue_capacity = 1;
    });
    let caller = daemon.caller();
    let lock = hold_write_lock(&daemon);

    // While the writer is stuck, it holds at most one event and the queue
    // one more, so a 503 comes within three sends.
    let mut accepted = 0;
    let mut busy = None;
    for n in 0..3 {
        let sent = Instant::now();
        let response = caller.post_event(&valid_event(n));
        match response.status {
            202 => accepted += 1,
            503 => {
                busy = Some((response.body, sent.elapsed()));
                break;
            }
            other => panic!("unexpected status {other}: {}", response.body),
        }
    }
    let (body, waited) = busy.expect("a 503 while the writer is stuck");
    assert_eq!(body, r#"{"error":"busy"}"#);
    // A blocking bus would wait at least SQLite's 5 s busy timeout.
    assert!(
        waited < Duration::from_secs(4),
        "503 must not wait for the writer (took {waited:?})"
    );

    drop(lock);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), accepted);
}

#[test]
fn a_batch_whose_commit_fails_is_retried_not_dropped() {
    let mut daemon = TestDaemon::start_with(|config| {
        config.writer.commit_interval = Duration::from_millis(100);
        config.writer.max_batch = 1;
    });
    let caller = daemon.caller();
    let lock = hold_write_lock(&daemon);

    assert_eq!(caller.post_event(&valid_event(1)).status, 202);
    // Longer than SQLite's 5 s busy timeout, so the first commit fails.
    thread::sleep(Duration::from_secs(7));
    drop(lock);

    wait_for_stored_events(&daemon, 1);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 1);
}

#[test]
fn events_accepted_just_before_shutdown_are_stored() {
    let mut daemon = TestDaemon::start();
    let caller = daemon.caller();

    for n in 0..100 {
        assert_eq!(caller.post_event(&valid_event(n)).status, 202);
    }

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 100);
}

#[test]
fn raw_text_sent_through_unknown_fields_never_reaches_disk() {
    // One full batch of valid events, so they are committed to the WAL while
    // the daemon is still running and the WAL file can be scanned too.
    let mut daemon = TestDaemon::start_with(|config| config.writer.max_batch = 20);
    let caller = daemon.caller();

    let mut smuggled = Vec::new();
    let mut top_level = valid_event_value(100);
    top_level["prompt"] = serde_json::json!(SENTINEL);
    smuggled.push(top_level);
    let mut in_metrics = valid_event_value(101);
    in_metrics["metrics_only"]["stdout"] = serde_json::json!(SENTINEL);
    smuggled.push(in_metrics);
    let mut as_key = valid_event_value(102);
    as_key[SENTINEL] = serde_json::json!(1);
    smuggled.push(as_key);
    let mut as_metrics_key = valid_event_value(103);
    as_metrics_key["metrics_only"][SENTINEL] = serde_json::json!(true);
    smuggled.push(as_metrics_key);
    let mut nested = valid_event_value(104);
    nested["metrics_only"]["tool_input"] = serde_json::json!({ "command": SENTINEL });
    smuggled.push(nested);

    for event in &smuggled {
        let response = caller.post_event(&event.to_string());
        assert_eq!(response.status, 400, "{event}: {}", response.body);
        assert!(!response.body.contains(SENTINEL), "rejection echoes input");
    }
    for n in 0..20 {
        assert_eq!(caller.post_event(&valid_event(n)).status, 202);
    }
    wait_for_stored_events(&daemon, 20);
    let scanned_live = assert_no_file_contains_sentinel(daemon.data_dir(), SkipShm::Yes);
    assert!(scanned_live.iter().any(|name| name == "traction.db-wal"));

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 20);
    assert_eq!(sentinel_hits_in_tables(&db), 0);
    drop(db);
    let scanned = assert_no_file_contains_sentinel(daemon.data_dir(), SkipShm::No);
    assert!(scanned.iter().any(|name| name == DB_FILE));
}

#[derive(PartialEq)]
enum SkipShm {
    /// While the daemon runs: Windows may refuse reads of the `-shm` file's
    /// locked byte ranges. It holds only the WAL index, never row data.
    Yes,
    No,
}

/// Counts sentinel copies in every column of every table, read as text.
fn sentinel_hits_in_tables(db: &Connection) -> usize {
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
                .query_row(&sql, [SENTINEL], |row| row.get(0))
                .expect("scan column");
            hits += usize::try_from(count).expect("non-negative");
        }
    }
    hits
}

/// Scans the raw bytes of every file in the data folder: the database, any
/// `-wal`/`-shm` file and the token. Returns the names scanned.
fn assert_no_file_contains_sentinel(data_dir: &Path, skip_shm: SkipShm) -> Vec<String> {
    let mut scanned = Vec::new();
    for entry in std::fs::read_dir(data_dir).expect("data folder") {
        let path = entry.expect("entry").path();
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        if !path.is_file() || (skip_shm == SkipShm::Yes && name.ends_with("-shm")) {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read file");
        assert!(
            !bytes
                .windows(SENTINEL.len())
                .any(|window| window == SENTINEL.as_bytes()),
            "sentinel found in {}",
            path.display()
        );
        scanned.push(name);
    }
    scanned
}
