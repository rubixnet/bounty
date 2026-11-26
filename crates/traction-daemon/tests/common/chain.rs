//! A real sealed chain for the audit and tamper tests: a stopped daemon's
//! database seeded with Presence Minutes, manual file mutations and commit
//! observations for two Projects, then finalized and sealed through the real
//! writer path (`finalize_settled`) with an injected clock.
#![allow(dead_code)]

use std::path::{Path, PathBuf};

use rusqlite::Connection;
use traction_core::{CategorySec, TRACTION_RULES_1};
use traction_daemon::{SIGNING_KEY_FILE, SIGNING_PUB_FILE, finalize_settled};

use super::TestDaemon;

pub const MIN: u64 = 60_000;
pub const WINDOW: u64 = 2 * MIN;
pub const T0: u64 = 1_800_000_000_000 - 1_800_000_000_000 % WINDOW;
pub const ALPHA: &str = "dir:alpha";
pub const BETA: &str = "dir:beta";
pub const OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

/// A data folder holding a sealed chain. The daemon is stopped.
pub struct Device {
    daemon: TestDaemon,
    next_window: u64,
    /// The first window's start (`T0` unless built with `sealed_from`).
    t0: u64,
}

impl Device {
    /// `windows` sealed windows: Alpha works in every window, Beta in the odd
    /// ones; Alpha has a commit in window 1, Beta in window 3.
    pub fn sealed(windows: u64) -> Self {
        Self::sealed_from(T0, windows)
    }

    /// [`Self::sealed`] with the first window at `t0` (window aligned), so a
    /// test can place the chain in the recent or the distant past.
    pub fn sealed_from(t0: u64, windows: u64) -> Self {
        assert_eq!(t0 % WINDOW, 0);
        let mut daemon = TestDaemon::start();
        daemon.shutdown();
        let mut device = Self {
            daemon,
            next_window: 0,
            t0,
        };
        device.seal_windows(windows);
        device
    }

    /// Starts a daemon on this data folder (the chain is already sealed).
    pub fn start_daemon(&mut self, adjust: impl FnOnce(&mut traction_daemon::DaemonConfig)) {
        self.daemon.restart_in_place(adjust);
    }

    /// Gracefully stops the daemon started by [`Self::start_daemon`].
    pub fn stop_daemon(&mut self) {
        self.daemon.shutdown();
    }

    pub fn dir(&self) -> &Path {
        self.daemon.data_dir()
    }

    /// Seeds and seals `count` more windows after the ones already there.
    pub fn seal_windows(&mut self, count: u64) {
        let first = self.next_window;
        seed(self.dir(), self.t0, first, count);
        self.next_window += count;
        // Just enough clock for these windows to settle, so a later batch is
        // never behind the finalizer's cursor.
        let now = self.t0
            + self.next_window * WINDOW
            + TRACTION_RULES_1.bucket_settle_ms
            + TRACTION_RULES_1.context_fwd_ms();
        finalize_settled(self.dir(), now).expect("finalize and seal");
    }

    /// Leaves `count` windows empty (no rows, so no Chain Link).
    pub fn skip_windows(&mut self, count: u64) {
        self.next_window += count;
    }

    /// Honest key loss: both key files are deleted; the next seal starts a
    /// successor chain.
    pub fn lose_the_signing_key(&self) {
        std::fs::remove_file(self.dir().join(SIGNING_KEY_FILE)).unwrap();
        std::fs::remove_file(self.dir().join(SIGNING_PUB_FILE)).unwrap();
    }

    /// The window start of window index `n`.
    pub fn window(n: u64) -> u64 {
        T0 + n * WINDOW
    }

    /// A read-write connection with the named triggers dropped (an attacker
    /// with file access; inside the test only).
    pub fn attacker(&self, drop_triggers: &[&str]) -> Connection {
        let db = Connection::open(self.dir().join(traction_daemon::DB_FILE)).unwrap();
        for trigger in drop_triggers {
            db.execute_batch(&format!("DROP TRIGGER {trigger}"))
                .unwrap();
        }
        db
    }

    pub fn export(&self, project: &str, now_ms: u64) -> PathBuf {
        let out = self.dir().join(format!(
            "{}.traction-bundle.json",
            project.replace(':', "_")
        ));
        traction_daemon::export_bundle(self.dir(), project, &out, now_ms).expect("export");
        out
    }
}

fn event(db: &Connection, n: u64, project: &str, timestamp_ms: u64) {
    db.execute(
        "INSERT INTO traction_event
             (event_id, schema_version, timestamp_ms, harness_id, session_id,
              parent_orchestrator_id, event_type, end_reason, cwd, project_id, model_id,
              network_class, attribution_source, prompt_char_len, repo_symbol_hits,
              has_stacktrace, tool_name, exit_code, diff_lines_delta, mutation_kind)
         VALUES (?1, 1, ?2, NULL, NULL, NULL, 'FILE_MUTATION', NULL, ?3, ?4, NULL,
                 'manual', 'file_watcher', NULL, NULL, NULL, NULL, NULL, 2, 'modified')",
        rusqlite::params![
            format!("019a7c2e-8b4d-7000-8000-{n:012x}"),
            i64::try_from(timestamp_ms).unwrap(),
            format!("/code/{}", &project[4..]),
            project,
        ],
    )
    .unwrap();
}

fn seed(dir: &Path, t0: u64, first_window: u64, count: u64) {
    let db = Connection::open(dir.join(traction_daemon::DB_FILE)).unwrap();
    let category = serde_json::to_string(&CategorySec {
        ide: 30,
        terminal: 30,
        ..CategorySec::default()
    })
    .unwrap();
    for window in first_window..first_window + count {
        let start = t0 + window * WINDOW;
        for minute in 0..2 {
            db.execute(
                "INSERT INTO presence_minute
                     (minute_start_ms, key_events, pointer_events, injected_events,
                      active_subslots, cadence_entropy_milli, category_sec,
                      category_switches, idle_sec, telemetry_tier)
                 VALUES (?1, 30, 30, 0, 4, 2500, ?2, 2, 0, 'full')",
                rusqlite::params![i64::try_from(start + minute * MIN).unwrap(), category],
            )
            .unwrap();
        }
        event(&db, window * 10, ALPHA, start + 10_000);
        if window % 2 == 1 {
            event(&db, window * 10 + 1, BETA, start + MIN + 10_000);
        }
        let commit = match window {
            1 => Some((ALPHA, OID_A)),
            3 => Some((BETA, OID_B)),
            _ => None,
        };
        if let Some((project, oid)) = commit {
            db.execute(
                "INSERT INTO commit_observation (project_id, commit_oid, observed_ms)
                 VALUES (?1, ?2, ?3)",
                rusqlite::params![project, oid, i64::try_from(start + 30_000).unwrap()],
            )
            .unwrap();
        }
    }
}
