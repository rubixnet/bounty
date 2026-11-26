//! Seam: the signed chain across a real process kill. A daemon process is
//! killed (`SIGKILL` on Unix, `TerminateProcess` on Windows) after some
//! windows are sealed, then restarted; every finalized window is sealed
//! exactly once, `seq` is contiguous and there is still one chain.
//!
//! The "daemon process" is this very test binary re-run on the hidden
//! `child_daemon` test, so the real library runs with a fast finalizer and no
//! production-only hook.

mod common;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{TestDaemon, open_db_read_only, poll_until, test_config};
use rusqlite::Connection;

const CHILD_DIR: &str = "TRACTION_CHAIN_RESTART_CHILD_DIR";
const MIN: u64 = 60_000;

/// Runs only as the child process: a real daemon with a fast finalizer.
#[test]
fn child_daemon() {
    let Ok(dir) = std::env::var(CHILD_DIR) else {
        return;
    };
    let mut config = test_config(Path::new(&dir));
    config.writer.commit_interval = Duration::from_millis(20);
    config.finalizer.interval = Duration::from_millis(50);
    config.finalizer.start_delay = Duration::ZERO;
    config.finalizer.settle = Duration::ZERO;
    let _daemon = traction_daemon::start(config).expect("child daemon starts");
    loop {
        std::thread::park();
    }
}

fn spawn_child(dir: &Path) -> Child {
    spawn_child_with(dir, None, Stdio::null())
}

/// `fail_seal_after` sets `TRACTION_TEST_FAIL_SEAL_AFTER` for the child, so
/// every sealing transaction with that many windows fails mid-batch.
fn spawn_child_with(dir: &Path, fail_seal_after: Option<&str>, stderr: Stdio) -> Child {
    let mut command = Command::new(std::env::current_exe().expect("test binary"));
    command
        .args(["child_daemon", "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD_DIR, dir)
        .env_remove("TRACTION_TEST_FAIL_SEAL_AFTER")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(stderr);
    if let Some(n) = fail_seal_after {
        command.env("TRACTION_TEST_FAIL_SEAL_AFTER", n);
    }
    command.spawn().expect("spawn child daemon")
}

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// Three days of idle Presence Minutes, ending a day before now.
fn seed_three_days(dir: &Path) -> u64 {
    let start = (now_ms() - 100 * 60 * MIN) / (2 * MIN) * (2 * MIN);
    let mut db = Connection::open(dir.join(traction_daemon::DB_FILE)).unwrap();
    let tx = db.transaction().unwrap();
    {
        let mut insert = tx
            .prepare(
                "INSERT INTO presence_minute
                     (minute_start_ms, active_subslots, idle_sec, telemetry_tier)
                 VALUES (?1, 0, 60, 'reduced')",
            )
            .unwrap();
        for minute in 0..3 * 24 * 60u64 {
            insert
                .execute([i64::try_from(start + minute * MIN).unwrap()])
                .unwrap();
        }
    }
    tx.commit().unwrap();
    start + 3 * 24 * 60 * MIN
}

fn count(db: &Connection, sql: &str) -> i64 {
    db.query_row(sql, [], |row| row.get(0)).unwrap()
}

#[test]
fn a_killed_daemon_resumes_the_same_chain_without_forking_or_double_sealing() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let dir = daemon.data_dir().to_path_buf();
    let seeded_end = seed_three_days(&dir);

    let mut child = spawn_child(&dir);
    poll_until(Duration::from_secs(60), "the first sealed windows", || {
        let db = open_db_read_only(&dir);
        (count(&db, "SELECT count(*) FROM chain_link") > 0).then_some(())
    });
    child.kill().expect("kill the daemon process");
    child.wait().expect("reap the daemon process");
    let sealed_when_killed = count(&open_db_read_only(&dir), "SELECT count(*) FROM chain_link");

    let mut child = spawn_child(&dir);
    poll_until(Duration::from_secs(120), "the restart to finish", || {
        let db = open_db_read_only(&dir);
        let cursor: Option<i64> = db
            .query_row(
                "SELECT finalized_through_ms FROM rollup_cursor",
                [],
                |row| row.get(0),
            )
            .ok();
        cursor
            .is_some_and(|c| u64::try_from(c).unwrap() >= seeded_end)
            .then_some(())
    });
    child.kill().expect("stop the restarted daemon");
    child.wait().expect("reap the restarted daemon");

    let db = open_db_read_only(&dir);
    assert!(count(&db, "SELECT count(*) FROM chain_link") >= sealed_when_killed);
    drop(db);
    assert_one_sealed_chain(&dir);
}

#[test]
fn a_seal_that_fails_mid_batch_leaves_nothing_unsealed_across_a_kill_and_restart() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let dir = daemon.data_dir().to_path_buf();
    let seeded_end = seed_three_days(&dir);

    // Every batch holds many windows, so the second seal always fails.
    let log_path = dir.join("child-a.stderr");
    let log = std::fs::File::create(&log_path).unwrap();
    let mut child = spawn_child_with(&dir, Some("2"), Stdio::from(log));
    poll_until(Duration::from_secs(60), "a seal to fail mid-batch", || {
        std::fs::read_to_string(&log_path)
            .is_ok_and(|text| text.contains("injected seal failure"))
            .then_some(())
    });
    child.kill().expect("kill the daemon process");
    child.wait().expect("reap the daemon process");

    let db = open_db_read_only(&dir);
    assert_eq!(
        count(&db, "SELECT count(*) FROM chain_link"),
        0,
        "the failed transaction left no link"
    );
    assert_eq!(count(&db, "SELECT count(*) FROM activity_rollup"), 0);
    assert_eq!(count(&db, "SELECT count(*) FROM rollup_cursor"), 0);
    drop(db);
    assert_one_sealed_chain(&dir);

    let mut child = spawn_child(&dir);
    poll_until(Duration::from_secs(120), "the restart to finish", || {
        let db = open_db_read_only(&dir);
        let cursor: Option<i64> = db
            .query_row(
                "SELECT finalized_through_ms FROM rollup_cursor",
                [],
                |row| row.get(0),
            )
            .ok();
        cursor
            .is_some_and(|c| u64::try_from(c).unwrap() >= seeded_end)
            .then_some(())
    });
    child.kill().expect("stop the restarted daemon");
    child.wait().expect("reap the restarted daemon");

    assert!(count(&open_db_read_only(&dir), "SELECT count(*) FROM chain_link") > 0);
    assert_one_sealed_chain(&dir);
}

/// One key, every row-holding window sealed exactly once, contiguous `seq`,
/// and the whole chain verifies.
fn assert_one_sealed_chain(dir: &Path) {
    let db = open_db_read_only(dir);
    assert_eq!(
        count(&db, "SELECT count(*) FROM chain_key"),
        1,
        "the key survived the kill"
    );
    // Every window holding a row is sealed exactly once (the table's UNIQUE
    // constraint forbids twice).
    assert_eq!(
        count(
            &db,
            "SELECT count(*) FROM (
                 SELECT window_start_ms AS w FROM activity_rollup
                 UNION SELECT minute_start_ms / 120000 * 120000 FROM work_bucket
             ) AS rows_ LEFT JOIN chain_link ON chain_link.window_start_ms = rows_.w
             WHERE chain_link.seq IS NULL"
        ),
        0
    );
    assert_eq!(
        count(&db, "SELECT count(*) FROM chain_link"),
        count(
            &db,
            "SELECT count(DISTINCT window_start_ms) FROM chain_link"
        )
    );
    assert_eq!(
        count(&db, "SELECT coalesce(max(seq) + 1, 0) FROM chain_link"),
        count(&db, "SELECT count(*) FROM chain_link"),
        "seq is contiguous"
    );
    drop(db);
    let report = traction_daemon::verify_database(dir).expect("the chain verifies");
    assert_eq!(
        report.chains.len(),
        usize::from(count(&open_db_read_only(dir), "SELECT count(*) FROM chain_link") > 0),
        "one chain once anything is sealed"
    );
    assert!(report.key_discontinuities.is_empty());
}
