//! Seam: a lost `signing.key` starts a visible, linked successor chain, and
//! the verifier never reports the two chains as one.

mod common;

use std::process::{Command, Stdio};

use common::{TestDaemon, open_db_read_only};
use rusqlite::Connection;
use traction_daemon::{SIGNING_KEY_FILE, SIGNING_PUB_FILE, finalize_settled, verify_database};

const MIN: u64 = 60_000;
const T0: u64 = 1_800_000_000_000 - 1_800_000_000_000 % (2 * MIN);

const CHILD_DIR: &str = "TRACTION_SUCCESSOR_CHILD_DIR";
const CHILD_NOW: &str = "TRACTION_SUCCESSOR_CHILD_NOW";

/// Runs only as the child process: one catch-up finalize pass, so the test
/// can read what the daemon logged to stderr.
#[test]
fn child_finalize() {
    let (Ok(dir), Ok(now)) = (std::env::var(CHILD_DIR), std::env::var(CHILD_NOW)) else {
        return;
    };
    finalize_settled(std::path::Path::new(&dir), now.parse().unwrap()).unwrap();
}

/// [`finalize_settled`] in a child process; returns its stderr.
fn finalize_in_child(dir: &std::path::Path, now: u64) -> String {
    let output = Command::new(std::env::current_exe().expect("test binary"))
        .args([
            "child_finalize",
            "--exact",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_DIR, dir)
        .env(CHILD_NOW, now.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .expect("run the child finalizer");
    assert!(output.status.success(), "the child finalizer failed");
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn seed(dir: &std::path::Path, from: u64, minutes: u64) {
    let db = Connection::open(dir.join(traction_daemon::DB_FILE)).unwrap();
    for minute in 0..minutes {
        db.execute(
            "INSERT INTO presence_minute (minute_start_ms, active_subslots, idle_sec, telemetry_tier)
             VALUES (?1, 0, 60, 'reduced')",
            [i64::try_from(from + minute * MIN).unwrap()],
        )
        .unwrap();
    }
}

fn links(dir: &std::path::Path) -> Vec<(String, i64, i64, String)> {
    let db = open_db_read_only(dir);
    let mut stmt = db
        .prepare("SELECT key_id, seq, window_start_ms, entry_hash FROM chain_link ORDER BY window_start_ms")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn a_deleted_signing_key_starts_a_linked_successor_chain_the_verifier_reports() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let dir = daemon.data_dir().to_path_buf();
    seed(&dir, T0, 8);
    finalize_settled(&dir, T0 + 60 * MIN).unwrap();
    let old = links(&dir);
    assert!(!old.is_empty());
    let one_chain = verify_database(&dir).expect("one chain verifies");
    assert_eq!(one_chain.chains.len(), 1);
    assert!(one_chain.key_discontinuities.is_empty());

    std::fs::remove_file(dir.join(SIGNING_KEY_FILE)).unwrap();
    std::fs::remove_file(dir.join(SIGNING_PUB_FILE)).unwrap();
    seed(&dir, T0 + 120 * MIN, 8);
    let log = finalize_in_child(&dir, T0 + 180 * MIN);
    assert_eq!(
        log.matches("signing.key missing; starting a new chain")
            .count(),
        1,
        "the start warning, once: {log}"
    );
    assert_eq!(
        log.matches("sealing the first link of a new chain").count(),
        1,
        "the first-seal warning, once: {log}"
    );
    // A second pass with the same key is a plain continuation: no warning.
    seed(&dir, T0 + 240 * MIN, 8);
    let quiet = finalize_in_child(&dir, T0 + 300 * MIN);
    assert!(!quiet.contains("new chain"), "no repeat warning: {quiet}");

    let all = links(&dir);
    assert_eq!(&all[..old.len()], old.as_slice(), "old links are untouched");
    let new = &all[old.len()..];
    assert!(!new.is_empty());
    assert_ne!(new[0].0, old[0].0, "a new key id");
    assert_eq!(new[0].1, 0, "a new genesis");
    let report = verify_database(&dir).expect("both chains verify");
    assert_eq!(report.chains.len(), 2);
    assert_eq!(report.key_discontinuities.len(), 1);
    assert!(
        report.key_discontinuities[0].linked,
        "supersedes the old tip"
    );
}

/// Losing the cursor makes the finalizer re-offer already sealed windows.
fn lose_the_cursor(dir: &std::path::Path) {
    Connection::open(dir.join(traction_daemon::DB_FILE))
        .unwrap()
        .execute("DELETE FROM rollup_cursor", [])
        .unwrap();
}

fn cursor(dir: &std::path::Path) -> Option<i64> {
    open_db_read_only(dir)
        .query_row(
            "SELECT max(finalized_through_ms) FROM rollup_cursor",
            [],
            |r| r.get(0),
        )
        .unwrap()
}

fn rollup_windows(dir: &std::path::Path) -> Vec<i64> {
    let db = open_db_read_only(dir);
    let mut stmt = db
        .prepare("SELECT window_start_ms FROM activity_rollup ORDER BY window_start_ms")
        .unwrap();
    stmt.query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn windows(from: u64, count: u64) -> Vec<i64> {
    (0..count)
        .map(|i| i64::try_from(from + i * 2 * MIN).unwrap())
        .collect()
}

#[test]
fn a_batch_straddling_the_tip_drops_only_the_old_windows_and_seals_the_newer_ones() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let dir = daemon.data_dir().to_path_buf();
    seed(&dir, T0, 8);
    seed(&dir, T0 + 20 * MIN, 8);
    finalize_settled(&dir, T0 + 60 * MIN).unwrap();
    let old = links(&dir);
    let old_rows = rollup_windows(&dir);

    lose_the_cursor(&dir);
    // Late data in a gap before the tip, and new data after it, in the same
    // one-day batch as the sealed windows.
    seed(&dir, T0 + 10 * MIN, 4);
    seed(&dir, T0 + 90 * MIN, 8);
    finalize_settled(&dir, T0 + 150 * MIN).unwrap();

    let all = links(&dir);
    assert_eq!(&all[..old.len()], old.as_slice(), "old links are untouched");
    let new_windows: Vec<i64> = all[old.len()..].iter().map(|l| l.2).collect();
    assert_eq!(new_windows, windows(T0 + 90 * MIN, 4));
    assert_eq!(
        rollup_windows(&dir),
        [old_rows, windows(T0 + 90 * MIN, 4)].concat(),
        "no row for the gap window before the tip"
    );
    let report = verify_database(&dir).expect("the chain verifies");
    assert_eq!(report.chains.len(), 1);
}

#[test]
fn a_batch_wholly_behind_the_tip_still_advances_the_cursor() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let dir = daemon.data_dir().to_path_buf();
    seed(&dir, T0, 8);
    finalize_settled(&dir, T0 + 60 * MIN).unwrap();
    let old = links(&dir);
    let old_rows = rollup_windows(&dir);
    let old_cursor = cursor(&dir);
    assert!(old_cursor.is_some());

    lose_the_cursor(&dir);
    finalize_settled(&dir, T0 + 60 * MIN).unwrap();

    assert_eq!(links(&dir), old);
    assert_eq!(rollup_windows(&dir), old_rows);
    assert_eq!(cursor(&dir), old_cursor, "the cursor moved past the batch");
    assert_eq!(
        finalize_settled(&dir, T0 + 60 * MIN).unwrap(),
        0,
        "a restart has nothing to redo"
    );
    verify_database(&dir).expect("the chain still verifies");
}

#[test]
fn a_new_key_with_a_lost_cursor_never_reseals_the_old_chains_windows() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let dir = daemon.data_dir().to_path_buf();
    seed(&dir, T0, 8);
    finalize_settled(&dir, T0 + 60 * MIN).unwrap();
    let old = links(&dir);

    std::fs::remove_file(dir.join(SIGNING_KEY_FILE)).unwrap();
    std::fs::remove_file(dir.join(SIGNING_PUB_FILE)).unwrap();
    lose_the_cursor(&dir);
    seed(&dir, T0 + 90 * MIN, 8);
    finalize_settled(&dir, T0 + 150 * MIN).unwrap();

    let all = links(&dir);
    assert_eq!(&all[..old.len()], old.as_slice());
    let new_windows: Vec<i64> = all[old.len()..].iter().map(|l| l.2).collect();
    assert_eq!(new_windows, windows(T0 + 90 * MIN, 4));
    let report = verify_database(&dir).expect("both chains verify");
    assert_eq!(report.chains.len(), 2);
}

#[test]
fn windows_after_an_all_old_batch_are_sealed() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let dir = daemon.data_dir().to_path_buf();
    seed(&dir, T0, 8);
    finalize_settled(&dir, T0 + 60 * MIN).unwrap();
    let sealed = links(&dir).len();
    assert!(sealed > 0);

    lose_the_cursor(&dir);
    // A day and a half on, past the one-day batch cap, so the first batch
    // holds only already sealed windows.
    seed(&dir, T0 + 36 * 60 * MIN, 8);
    finalize_settled(&dir, T0 + 40 * 60 * MIN).unwrap();

    assert!(
        links(&dir).len() > sealed,
        "windows after the old batch were sealed"
    );
    verify_database(&dir).expect("the chain still verifies");
}
