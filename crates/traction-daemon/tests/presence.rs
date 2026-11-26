//! Slice 1 (Presence Sensor), through the daemon's public seam: a running
//! daemon on the real host OS writes `presence_minute` rows.
//!
//! Sub-slots are shortened (a minute is 6 x `SUBSLOT`) so a test sees
//! several minutes in a second or two.

mod common;

use std::time::Duration;

use common::TestDaemon;
use rusqlite::Connection;
use traction_daemon::DB_FILE;

const SUBSLOT: Duration = Duration::from_millis(60);
const MINUTE_MS: i64 = 360;

#[derive(Debug)]
struct Row {
    minute_start_ms: i64,
    key_events: Option<i64>,
    pointer_events: Option<i64>,
    injected_events: Option<i64>,
    active_subslots: Option<i64>,
    category_sec: Option<String>,
    idle_sec: Option<i64>,
    tier: String,
}

fn fast_presence(config: &mut traction_daemon::DaemonConfig) {
    config.presence.subslot = SUBSLOT;
    config.writer.commit_interval = Duration::from_millis(40);
}

fn rows(db: &Connection) -> Vec<Row> {
    let mut stmt = db
        .prepare(
            "SELECT minute_start_ms, key_events, pointer_events, injected_events,
                    active_subslots, category_sec, idle_sec, telemetry_tier
             FROM presence_minute ORDER BY minute_start_ms",
        )
        .expect("prepare");
    stmt.query_map([], |r| {
        Ok(Row {
            minute_start_ms: r.get(0)?,
            key_events: r.get(1)?,
            pointer_events: r.get(2)?,
            injected_events: r.get(3)?,
            active_subslots: r.get(4)?,
            category_sec: r.get(5)?,
            idle_sec: r.get(6)?,
            tier: r.get(7)?,
        })
    })
    .expect("query")
    .map(|r| r.expect("row"))
    .collect()
}

/// Runs a daemon for `span`, then returns its rows.
fn run_for(span: Duration) -> (TestDaemon, Vec<Row>) {
    let mut daemon = TestDaemon::start_with(fast_presence);
    std::thread::sleep(span);
    let db = daemon.shutdown_and_open_db();
    let rows = rows(&db);
    drop(db);
    (daemon, rows)
}

#[test]
fn a_running_daemon_writes_one_content_free_row_per_wall_clock_minute() {
    let (_daemon, rows) = run_for(Duration::from_millis(1800));
    assert!(rows.len() >= 2, "{rows:?}");
    for pair in rows.windows(2) {
        assert_eq!(
            pair[1].minute_start_ms - pair[0].minute_start_ms,
            MINUTE_MS,
            "one row per minute, none missing: {rows:?}"
        );
    }
    for row in &rows {
        assert_eq!(row.minute_start_ms % MINUTE_MS, 0);
        if let Some(active) = row.active_subslots {
            assert!((0..=6).contains(&active));
        }
    }
}

#[cfg(any(target_os = "macos", windows))]
#[test]
fn macos_and_windows_report_the_reduced_tier_with_idle_and_category() {
    let (_daemon, rows) = run_for(Duration::from_millis(1000));
    assert!(!rows.is_empty());
    for row in &rows {
        assert_eq!(row.tier, "reduced");
        assert!(row.idle_sec.is_some(), "idle is read on every host");
        // A headless runner may have no foreground window at all; when
        // there is one its seconds are counted.
        if let Some(json) = &row.category_sec {
            // (Seconds are 0 here: test sub-slots are shorter than a second.)
            let _: traction_core::CategorySec = serde_json::from_str(json).expect("json");
        }
        assert_eq!(row.key_events, None, "reduced never invents input counts");
        assert_eq!(row.pointer_events, None);
        assert_eq!(row.injected_events, None);
    }
}

/// No prompt-free injection signal exists on macOS (spike outcome), so the
/// injection test cannot run there. The skip is loud on purpose: CI greps
/// the output for `SKIPPED (macOS injection)` and puts it in the job summary.
#[cfg(target_os = "macos")]
#[test]
fn macos_injection_test_is_skipped_loudly() {
    println!(
        "SKIPPED (macOS injection): no prompt-free signal separates synthetic from hardware \
         input (CGEventPost from an untrusted process is dropped by the OS; spike in ticket \
         01), so the backend is `reduced`. Counter-delta logic is unit-tested instead."
    );
    let (_daemon, rows) = run_for(Duration::from_millis(500));
    assert!(rows.iter().all(|row| row.injected_events.is_none()));
}

#[cfg(target_os = "linux")]
fn x11_session() -> bool {
    std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
        && std::env::var_os("WAYLAND_DISPLAY").is_none_or(|v| v.is_empty())
}

#[cfg(target_os = "linux")]
fn xdotool_available() -> bool {
    std::process::Command::new("xdotool")
        .arg("version")
        .output()
        .is_ok_and(|o| o.status.success())
}

#[cfg(target_os = "linux")]
#[test]
fn linux_reports_full_on_x11_and_none_without_a_display() {
    let (_daemon, rows) = run_for(Duration::from_millis(1000));
    assert!(!rows.is_empty());
    let expected = if x11_session() { "full" } else { "none" };
    for row in &rows {
        assert_eq!(row.tier, expected);
        if expected == "none" {
            assert_eq!(row.idle_sec, None);
            assert_eq!(row.category_sec, None);
            assert_eq!(row.active_subslots, None);
        } else {
            assert!(row.idle_sec.is_some());
            assert!(row.category_sec.is_some());
        }
    }
}

/// Real XTEST input under Xvfb: counted as injected, never as real input,
/// and the typed canary leaves no trace in the data folder.
#[cfg(target_os = "linux")]
#[test]
fn linux_xdotool_input_is_flagged_injected_and_leaves_no_text() {
    const CANARY: &str = "presencecanary9f3a";
    if !x11_session() || !xdotool_available() {
        println!("SKIPPED (linux injection): needs an X server (DISPLAY) and xdotool");
        return;
    }
    let mut daemon = TestDaemon::start_with(fast_presence);
    std::thread::sleep(Duration::from_millis(150));
    let end = std::time::Instant::now() + Duration::from_millis(1500);
    while std::time::Instant::now() < end {
        let typed = std::process::Command::new("xdotool")
            .args(["type", "--delay", "10", CANARY])
            .status()
            .expect("run xdotool");
        assert!(typed.success());
        let moved = std::process::Command::new("xdotool")
            .args(["mousemove_relative", "--", "3", "3"])
            .status()
            .expect("run xdotool");
        assert!(moved.success());
    }
    let db = daemon.shutdown_and_open_db();
    let rows = rows(&db);
    drop(db);
    let injected: i64 = rows.iter().filter_map(|r| r.injected_events).sum();
    let real_keys: i64 = rows.iter().filter_map(|r| r.key_events).sum();
    let real_pointer: i64 = rows.iter().filter_map(|r| r.pointer_events).sum();
    assert!(injected > 0, "{rows:?}");
    // Key and pointer counts include injected events: here all of them are.
    assert_eq!(
        real_keys + real_pointer,
        injected,
        "XTEST input is never real"
    );
    assert!(
        rows.iter().all(|r| r.active_subslots == Some(0)),
        "{rows:?}"
    );

    let mut bytes = Vec::new();
    for entry in std::fs::read_dir(daemon.data_dir()).expect("data dir") {
        let path = entry.expect("entry").path();
        if path.is_file() {
            bytes.extend(std::fs::read(&path).unwrap_or_default());
        }
    }
    assert!(
        !bytes.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()),
        "typed text reached the data folder"
    );
}

#[test]
fn presence_minutes_are_append_only() {
    let (daemon, rows) = run_for(Duration::from_millis(800));
    assert!(!rows.is_empty());
    let db = Connection::open(daemon.data_dir().join(DB_FILE)).expect("open rw");
    let update = db.execute("UPDATE presence_minute SET idle_sec = 99", []);
    assert!(
        update.unwrap_err().to_string().contains("append-only"),
        "update must be refused"
    );
    let delete = db.execute("DELETE FROM presence_minute", []);
    assert!(
        delete.unwrap_err().to_string().contains("append-only"),
        "delete must be refused"
    );
}

/// Waits (bounded) until `done` holds for the live rows: a positive barrier
/// instead of a guessed sleep.
fn wait_until(daemon: &TestDaemon, done: impl Fn(&[Row]) -> bool) -> Vec<Row> {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let db = daemon.live_db();
        let all = rows(&db);
        if done(&all) {
            return all;
        }
        assert!(std::time::Instant::now() < deadline, "rows never arrived");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn wall_ms() -> i64 {
    let since = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock");
    i64::try_from(since.as_millis()).expect("ms")
}

#[test]
fn a_restart_leaves_no_rows_for_the_minutes_the_daemon_was_down() {
    let first = TestDaemon::start_with(fast_presence);
    wait_until(&first, |r| r.len() >= 2);
    let (down_from, down_to) = (
        std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
        std::sync::Arc::new(std::sync::atomic::AtomicI64::new(0)),
    );
    let (from, to) = (down_from.clone(), down_to.clone());
    let mut second = first.restart_with(move |config| {
        // The first daemon is already stopped; stay down for several minutes.
        from.store(wall_ms(), std::sync::atomic::Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1100));
        to.store(wall_ms(), std::sync::atomic::Ordering::SeqCst);
        fast_presence(config);
    });
    let down_to = down_to.load(std::sync::atomic::Ordering::SeqCst);
    let down_from = down_from.load(std::sync::atomic::Ordering::SeqCst);
    // Positive barrier: rows from the new run exist before we look for holes.
    wait_until(&second, |r| {
        r.iter().filter(|x| x.minute_start_ms >= down_to).count() >= 2
    });
    let db = second.shutdown_and_open_db();
    let rows = rows(&db);
    drop(db);
    assert!(
        rows.iter()
            .any(|r| r.minute_start_ms + MINUTE_MS <= down_from)
    );
    // No row starts while the daemon was down, except a minute it joined
    // within its first sub-slot.
    let slot = i64::try_from(SUBSLOT.as_millis()).unwrap();
    for r in &rows {
        assert!(
            r.minute_start_ms + MINUTE_MS <= down_from || r.minute_start_ms + slot >= down_to,
            "row {} inside the down time {down_from}..{down_to}",
            r.minute_start_ms
        );
    }
}
