//! Seam: the `export-bundle` and `verify` commands of the real binary, and
//! the read-only export next to a daemon that is sealing.
//!
//! Output lines and exit codes are pinned here; the tamper table lives in
//! `tamper_red_team.rs`.

mod common;

use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::chain::{ALPHA, Device};
use common::{TestDaemon, fixtures, open_db_read_only};
use rusqlite::Connection;
use traction_core::{BundlePolicy, verify_bundle};
use traction_daemon::SIGNING_PUB_FILE;

const BIN: &str = env!("CARGO_BIN_EXE_traction-daemon");
const MIN: u64 = 60_000;

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .output()
        .expect("run traction-daemon")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn path(path: &Path) -> &str {
    path.to_str().unwrap()
}

fn export(device: &Device, project: &str, out: &Path) -> Output {
    run(&[
        "export-bundle",
        "--project",
        project,
        "--out",
        path(out),
        "--data-dir",
        path(device.dir()),
    ])
}

#[test]
fn export_then_verify_round_trips_through_the_real_binary() {
    let device = Device::sealed(8);
    let out = device.dir().join("alpha.traction-bundle.json");
    let exported = export(&device, ALPHA, &out);
    assert_eq!(exported.status.code(), Some(0), "{}", stderr(&exported));

    let verified = run(&["verify", path(&out)]);
    assert_eq!(verified.status.code(), Some(0), "{}", stdout(&verified));
    let text = stdout(&verified);
    let ok = text.lines().next().unwrap();
    assert!(
        ok.starts_with("OK chains=1 links=8 tip=ed25519:") && ok.contains(":7:"),
        "{ok}"
    );
    assert!(
        ok.ends_with("pinned=false clock_regressions=0 pending=0 legacy=0"),
        "{ok}"
    );
    assert!(
        text.contains("NOTE keys are self-asserted"),
        "an unpinned verify says so: {text}"
    );

    // Pinned to the public key the user copied from `signing.pub`.
    let public = std::fs::read_to_string(device.dir().join(SIGNING_PUB_FILE)).unwrap();
    let pinned = run(&["verify", path(&out), "--trusted-key", public.trim()]);
    assert_eq!(pinned.status.code(), Some(0), "{}", stdout(&pinned));
    let text = stdout(&pinned);
    assert!(text.lines().next().unwrap().contains("pinned=true"));
    assert!(!text.contains("self-asserted"), "{text}");

    // A different key pinned: the same bundle is rejected.
    let wrong = format!("{:0>64}", "9");
    let rejected = run(&["verify", path(&out), "--trusted-key", &wrong]);
    assert_eq!(rejected.status.code(), Some(1));

    // The tip learned out of band.
    let tip = ok
        .split_whitespace()
        .nth(3)
        .unwrap()
        .strip_prefix("tip=")
        .unwrap();
    let pinned_tip = run(&["verify", path(&out), "--expect-tip", tip]);
    assert_eq!(pinned_tip.status.code(), Some(0), "{}", stdout(&pinned_tip));
    let stale = tip.replace(":7:", ":6:");
    let stale = run(&["verify", path(&out), "--expect-tip", &stale]);
    assert_eq!(stale.status.code(), Some(1));
    assert!(
        stdout(&stale).starts_with("FAIL tip_mismatch "),
        "{}",
        stdout(&stale)
    );
}

#[test]
fn verify_data_dir_reports_the_local_database_without_a_daemon() {
    let device = Device::sealed(6);
    let verified = run(&["verify", "--data-dir", path(device.dir())]);
    assert_eq!(verified.status.code(), Some(0), "{}", stdout(&verified));
    assert!(
        stdout(&verified).starts_with("OK chains=1 links=6 tip=ed25519:"),
        "{}",
        stdout(&verified)
    );
}

#[test]
fn a_key_discontinuity_is_exit_code_two_in_the_real_binary() {
    let mut device = Device::sealed(4);
    device.lose_the_signing_key();
    device.seal_windows(3);
    let verified = run(&["verify", "--data-dir", path(device.dir())]);
    assert_eq!(verified.status.code(), Some(2), "{}", stdout(&verified));
    let text = stdout(&verified);
    assert!(text.starts_with("OK chains=2 links=7 "), "{text}");
    let warn = text
        .lines()
        .find(|l| l.starts_with("WARN key_discontinuity from=ed25519:"))
        .expect("a discontinuity line");
    assert!(warn.contains(" to=ed25519:") && warn.contains(" at_window="));
    assert!(warn.ends_with(" linked=true"), "{warn}");
}

#[test]
fn failures_and_bad_input_exit_one_with_one_line() {
    let device = Device::sealed(4);
    let out = device.dir().join("alpha.traction-bundle.json");
    assert_eq!(export(&device, ALPHA, &out).status.code(), Some(0));

    let unknown = export(&device, "dir:nobody", &device.dir().join("x.json"));
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        stderr(&unknown).contains("unknown project id"),
        "{}",
        stderr(&unknown)
    );
    assert!(!device.dir().join("x.json").exists());

    let junk = device.dir().join("junk.json");
    std::fs::write(&junk, b"{\"not\": \"a bundle\"}").unwrap();
    let malformed = run(&["verify", path(&junk)]);
    assert_eq!(malformed.status.code(), Some(1));
    assert!(
        stdout(&malformed).starts_with("FAIL malformed_bundle "),
        "{}",
        stdout(&malformed)
    );

    let missing = run(&["verify", path(&device.dir().join("absent.json"))]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        stderr(&missing).contains("cannot read"),
        "{}",
        stderr(&missing)
    );

    let usage = run(&["verify"]);
    assert_eq!(usage.status.code(), Some(1));
    assert!(stderr(&usage).contains("usage:"), "{}", stderr(&usage));
    let bad_key = run(&["verify", path(&out), "--trusted-key", "zz"]);
    assert_eq!(bad_key.status.code(), Some(1));
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

fn link_count(dir: &Path) -> i64 {
    open_db_read_only(dir)
        .query_row("SELECT count(*) FROM chain_link", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn export_is_read_only_and_safe_while_the_daemon_is_sealing() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("code").join("alpha"), None);
    assert_eq!(daemon.caller().register_project(&repo).status, 200);
    let project = {
        let db = daemon.shutdown_and_open_db();
        common::registered_projects(&db)[0].project_id.clone()
    };

    // Five days (five finalizer batches) of idle Presence Minutes that ended
    // long before now.
    let start = (now_ms() - 40 * 60 * MIN) / (2 * MIN) * (2 * MIN) - 5 * 24 * 60 * MIN;
    let db = Connection::open(daemon.data_dir().join(traction_daemon::DB_FILE)).unwrap();
    db.execute_batch("BEGIN").unwrap();
    for minute in 0..5 * 24 * 60u64 {
        db.execute(
            "INSERT INTO presence_minute (minute_start_ms, active_subslots, idle_sec, telemetry_tier)
             VALUES (?1, 0, 60, 'reduced')",
            [i64::try_from(start + minute * MIN).unwrap()],
        )
        .unwrap();
    }
    db.execute_batch("COMMIT").unwrap();
    drop(db);
    // A reader's open snapshot, as a slow export holds one: it is taken
    // before the first link exists and must not stop the writer.
    let reader = open_db_read_only(daemon.data_dir());
    reader.execute_batch("BEGIN").unwrap();
    let held: i64 = reader
        .query_row("SELECT count(*) FROM chain_link", [], |r| r.get(0))
        .unwrap();
    assert_eq!(held, 0);
    let daemon = daemon.restart_with(|config| {
        config.writer.commit_interval = Duration::from_millis(20);
        config.finalizer.interval = Duration::from_millis(30);
        config.finalizer.start_delay = Duration::ZERO;
        config.finalizer.settle = Duration::ZERO;
    });
    let dir = daemon.data_dir().to_path_buf();
    let out = dir.join("live.traction-bundle.json");

    let total = 5 * 720;
    let deadline = Instant::now() + Duration::from_secs(180);
    while link_count(&dir) == 0 {
        assert!(
            Instant::now() < deadline,
            "an open reader blocked the writer"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    // Export at least once, then keep exporting until everything is sealed.
    loop {
        assert!(Instant::now() < deadline, "the writer stopped sealing");
        traction_daemon::export_bundle(&dir, &project, &out, now_ms()).expect("a live export");
        let bytes = std::fs::read(&out).unwrap();
        verify_bundle(&bytes, &BundlePolicy::default()).expect("a live bundle verifies");
        if link_count(&dir) >= total {
            break;
        }
    }
    let still: i64 = reader
        .query_row("SELECT count(*) FROM chain_link", [], |r| r.get(0))
        .unwrap();
    assert_eq!(still, held, "the reader's snapshot never moved");
    reader.execute_batch("COMMIT").unwrap();
}
