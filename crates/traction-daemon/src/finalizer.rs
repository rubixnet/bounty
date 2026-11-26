//! The finalizer: turns settled minutes into append-only Work Buckets and
//! Activity Rollups.
//!
//! Every [`FinalizerSettings::interval`] it reads the stored events, Presence
//! Minutes and Commit Observations (through its own read-only connection),
//! runs the pure `traction-core` rollup for every minute older than
//! `bucket_settle_ms`, and hands the result to the single SQLite writer as
//! one [`WriteOp::Rollup`]. A minute is computed once: the writer stores the
//! rows and advances the `rollup_cursor` in one transaction, rows are
//! `INSERT OR IGNORE`, and events that arrive later never rewrite a finalized
//! minute (attribution is live-only). Minutes the daemon was down for have no
//! Presence Minute, so the engine writes them `telemetry_tier: none` with no
//! Human Active Time. The first pass waits `start_delay` so a restarting
//! tailer can fill those minutes' events in before they are finalized.

use std::io;
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags, params};
use traction_core::{
    CommitObservation, PresenceMinute, RollupInput, Ruleset, TRACTION_RULES_1, TractionEvent,
    rollup,
};

use crate::log;
use crate::presence::now_ms;
use crate::storage::{DB_FILE, RollupBatch, WriteOp};
use crate::writer::WriterSender;

const MINUTE_MS: u64 = 60_000;
/// The most minutes one batch covers (a day), so catching up after a long
/// pause stays bounded.
const MAX_BATCH_MINUTES: u64 = 24 * 60;

/// Finalizer tuning. Production uses the defaults; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct FinalizerSettings {
    pub enabled: bool,
    /// How often settled minutes are looked for.
    pub interval: Duration,
    /// Wait before the first pass (the tailer's catch-up window).
    pub start_delay: Duration,
    /// Test-only override of `bucket_settle_ms`.
    pub settle: Duration,
}

impl Default for FinalizerSettings {
    fn default() -> Self {
        let settle = Duration::from_millis(TRACTION_RULES_1.bucket_settle_ms);
        Self {
            enabled: true,
            interval: Duration::from_secs(15),
            start_delay: settle,
            settle,
        }
    }
}

pub struct FinalizerThread {
    stop: Sender<()>,
    handle: JoinHandle<()>,
}

impl FinalizerThread {
    /// Stops the thread and waits for it (dropping its writer sender).
    pub fn stop(self) -> thread::Result<()> {
        let _ = self.stop.send(());
        self.handle.join()
    }
}

pub fn spawn(
    data_dir: &Path,
    writer: WriterSender,
    settings: FinalizerSettings,
) -> io::Result<FinalizerThread> {
    let (stop, rx) = mpsc::channel();
    let db_path = data_dir.join(DB_FILE);
    let handle = thread::Builder::new()
        .name("traction-finalizer".to_owned())
        .spawn(move || {
            let mut wait = settings.start_delay;
            let mut cursor: Option<Option<u64>> = None;
            loop {
                match rx.recv_timeout(wait) {
                    Err(RecvTimeoutError::Timeout) => {}
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                }
                wait = settings.interval;
                let conn = match open_read_only(&db_path) {
                    Ok(conn) => conn,
                    Err(error) => {
                        log::error("finalizer cannot open the database", &error);
                        continue;
                    }
                };
                let rules = &TRACTION_RULES_1;
                let settle_ms = u64::try_from(settings.settle.as_millis()).unwrap_or(u64::MAX);
                let current = match cursor {
                    Some(known) => known,
                    None => match read_cursor(&conn, rules) {
                        Ok(stored) => stored,
                        Err(error) => {
                            log::error("finalizer cannot read its cursor", &error);
                            continue;
                        }
                    },
                };
                let mut current = current;
                loop {
                    match finalize_range(&conn, rules, current, now_ms(), settle_ms) {
                        Ok(Some(batch)) => {
                            current = Some(batch.through_ms);
                            cursor = Some(current);
                            if writer.send(WriteOp::Rollup(Box::new(batch))).is_err() {
                                return;
                            }
                        }
                        Ok(None) => {
                            cursor = Some(current);
                            break;
                        }
                        Err(error) => {
                            log::error("finalizer pass failed, will retry", &error);
                            break;
                        }
                    }
                }
            }
        })?;
    Ok(FinalizerThread { stop, handle })
}

fn open_read_only(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

/// The minute-aligned time every earlier minute is finalized through, as
/// stored for `rules`.
pub fn read_cursor(conn: &Connection, rules: &Ruleset) -> rusqlite::Result<Option<u64>> {
    let value: Option<i64> = conn
        .query_row(
            "SELECT finalized_through_ms FROM rollup_cursor WHERE ruleset_id = ?1",
            [rules.ruleset_id],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })?;
    Ok(value.and_then(|v| u64::try_from(v).ok()))
}

/// The next batch to finalize, or `None` when nothing is settled yet.
///
/// `cursor` is where the previous batch ended (`None` on the very first
/// pass, which starts at the earliest stored event or Presence Minute).
/// Everything older than `settle_ms` before `now_ms` is settled; ranges are
/// aligned to the Activity Rollup window, so a rollup never straddles two
/// batches. Pure with respect to the clock: `now_ms` is a parameter.
pub fn finalize_range(
    conn: &Connection,
    rules: &Ruleset,
    cursor: Option<u64>,
    now_ms: u64,
    settle_ms: u64,
) -> rusqlite::Result<Option<RollupBatch>> {
    let width = rules.activity_rollup_ms;
    let horizon = now_ms.saturating_sub(settle_ms) / width * width;
    let from = match cursor {
        Some(c) => c,
        None => match earliest(conn)? {
            Some(first) => first / width * width,
            None => return Ok(None),
        },
    };
    if from >= horizon {
        return Ok(None);
    }
    let to = horizon.min(from + MAX_BATCH_MINUTES * MINUTE_MS / width * width);
    let events = load_events(conn, rules, from, to)?;
    let presence = load_presence(conn, from, to + MINUTE_MS * 3)?;
    let commits = load_commits(conn, from, to)?;
    let out = rollup(
        rules,
        &RollupInput {
            events: &events,
            presence: &presence,
            commits: &commits,
            from_ms: from,
            to_ms: to,
        },
    );
    Ok(Some(RollupBatch {
        ruleset_id: rules.ruleset_id.to_owned(),
        through_ms: to,
        buckets: out.work_buckets,
        rollups: out.activity_rollups,
    }))
}

fn earliest(conn: &Connection) -> rusqlite::Result<Option<u64>> {
    let value: Option<i64> = conn.query_row(
        "SELECT min(m) FROM (
             SELECT min(minute_start_ms) AS m FROM presence_minute
             UNION ALL
             SELECT min(timestamp_ms) FROM traction_event WHERE project_id IS NOT NULL
         )",
        [],
        |row| row.get(0),
    )?;
    Ok(value.and_then(|v| u64::try_from(v).ok()))
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Events the engine needs for `[from, to)`: the window plus a look-back for
/// Project assignment and open turns, a look-ahead for permission replies,
/// and the events since each active session's last earlier prompt, so a turn
/// that began before the window keeps its real start.
fn load_events(
    conn: &Connection,
    rules: &Ruleset,
    from: u64,
    to: u64,
) -> rusqlite::Result<Vec<TractionEvent>> {
    let base = from.saturating_sub(
        rules.stale_turn_timeout_ms + rules.presence_project_lookback_ms.max(rules.agent_quiet_ms),
    );
    let ext = to + rules.permission_latency_cap_ms + 1;
    let mut stmt = conn.prepare(
        "SELECT event_id, timestamp_ms, harness_id, session_id, parent_orchestrator_id,
                event_type, end_reason, project_id, network_class, attribution_source
         FROM project_event
         WHERE project_id IS NOT NULL AND (
             (timestamp_ms >= :base AND timestamp_ms < :ext)
             OR (timestamp_ms < :base
                 AND session_id IN (
                     SELECT session_id FROM traction_event
                     WHERE timestamp_ms >= :from AND timestamp_ms < :to
                       AND session_id IS NOT NULL)
                 AND timestamp_ms >= COALESCE(
                     (SELECT max(p.timestamp_ms) FROM traction_event AS p
                      WHERE p.session_id = project_event.session_id
                        AND p.event_type = 'PROMPT_SUBMIT'
                        AND p.timestamp_ms < :base),
                     :base))
         )",
    )?;
    let rows = stmt.query_map(
        rusqlite::named_params! {
            ":base": to_i64(base), ":ext": to_i64(ext),
            ":from": to_i64(from), ":to": to_i64(to),
        },
        |row| {
            Ok(serde_json::json!({
                "schema_version": 1,
                "event_id": row.get::<_, String>(0)?,
                "timestamp_ms": row.get::<_, i64>(1)?,
                "harness_id": row.get::<_, Option<String>>(2)?,
                "session_id": row.get::<_, Option<String>>(3)?,
                "parent_orchestrator_id": row.get::<_, Option<String>>(4)?,
                "event_type": row.get::<_, String>(5)?,
                "end_reason": row.get::<_, Option<String>>(6)?,
                "cwd": null,
                "project_id": row.get::<_, Option<String>>(7)?,
                "model_id": null,
                "network_class": row.get::<_, String>(8)?,
                "attribution_source": row.get::<_, String>(9)?,
                "metrics_only": {},
            }))
        },
    )?;
    let mut events = Vec::new();
    for value in rows {
        // A row this build cannot read (a newer enum value) is skipped, not
        // fatal: it can only make a bucket weaker, never invent time.
        if let Ok(event) = serde_json::from_value::<TractionEvent>(value?) {
            events.push(event);
        }
    }
    Ok(events)
}

fn load_presence(conn: &Connection, from: u64, to: u64) -> rusqlite::Result<Vec<PresenceMinute>> {
    let mut stmt = conn.prepare(
        "SELECT minute_start_ms, key_events, pointer_events, injected_events,
                active_subslots, cadence_entropy_milli, category_sec,
                category_switches, idle_sec, telemetry_tier
         FROM presence_minute WHERE minute_start_ms >= ?1 AND minute_start_ms < ?2",
    )?;
    let rows = stmt.query_map(params![to_i64(from), to_i64(to)], |row| {
        let category: Option<String> = row.get(6)?;
        let tier: String = row.get(9)?;
        Ok((
            row.get::<_, i64>(0)?,
            [row.get::<_, Option<u32>>(1)?, row.get(2)?, row.get(3)?],
            row.get::<_, Option<u8>>(4)?,
            row.get::<_, Option<u32>>(5)?,
            category,
            row.get::<_, Option<u32>>(7)?,
            row.get::<_, Option<u32>>(8)?,
            tier,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (minute, [key, pointer, injected], subslots, entropy, category, switches, idle, tier) =
            row?;
        let Ok(telemetry_tier) = serde_json::from_value(serde_json::Value::String(tier)) else {
            continue;
        };
        out.push(PresenceMinute {
            minute_start_ms: u64::try_from(minute).unwrap_or(0),
            key_events: key,
            pointer_events: pointer,
            injected_events: injected,
            active_subslots: subslots,
            cadence_entropy_milli: entropy,
            category_sec: category.and_then(|c| serde_json::from_str(&c).ok()),
            category_switches: switches,
            idle_sec: idle,
            telemetry_tier,
        });
    }
    Ok(out)
}

fn load_commits(conn: &Connection, from: u64, to: u64) -> rusqlite::Result<Vec<CommitObservation>> {
    let mut stmt = conn.prepare(
        "SELECT project_id, commit_oid, observed_ms FROM commit_observation
         WHERE observed_ms >= ?1 AND observed_ms < ?2",
    )?;
    let rows = stmt.query_map(params![to_i64(from), to_i64(to)], |row| {
        Ok(CommitObservation {
            project_id: row.get(0)?,
            commit_oid: row.get(1)?,
            observed_ms: u64::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
        })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{self, EventRow, apply_batch};

    const MIN: u64 = 60_000;
    const T0: u64 = 1_800_000_000_000 - 1_800_000_000_000 % (2 * MIN);

    fn event(n: u32, ts: u64, kind: &str, project: Option<&str>) -> WriteOp {
        let value = serde_json::json!({
            "schema_version": 1,
            "event_id": format!("019a7c2e-8b4d-7000-8000-{n:012x}"),
            "timestamp_ms": ts,
            "harness_id": "claude-code",
            "session_id": "s1",
            "parent_orchestrator_id": null,
            "event_type": kind,
            "end_reason": null,
            "cwd": "/code/p",
            "project_id": project,
            "model_id": null,
            "network_class": "cloud_api",
            "attribution_source": "harness_hook",
            "metrics_only": {}
        });
        let event: TractionEvent = serde_json::from_value(value).expect("event");
        WriteOp::Event(Box::new(EventRow::try_from(event).expect("row")))
    }

    fn idle_minute(ts: u64) -> WriteOp {
        WriteOp::Presence(PresenceMinute {
            minute_start_ms: ts,
            key_events: None,
            pointer_events: None,
            injected_events: None,
            active_subslots: Some(0),
            cadence_entropy_milli: None,
            category_sec: None,
            category_switches: None,
            idle_sec: Some(60),
            telemetry_tier: traction_core::TelemetryTier::Reduced,
        })
    }

    fn open() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = storage::open(dir.path()).unwrap();
        (dir, conn)
    }

    fn totals(conn: &Connection) -> (i64, i64, i64, i64) {
        conn.query_row(
            "SELECT count(*), coalesce(sum(agent_compute_sec), 0),
                    coalesce(sum(human_active_sec), 0), coalesce(sum(idle_autonomous_sec), 0)
             FROM work_bucket",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap()
    }

    fn finalize(conn: &mut Connection, cursor: Option<u64>, now: u64) -> Option<u64> {
        let batch = finalize_range(conn, &TRACTION_RULES_1, cursor, now, 120_000).unwrap()?;
        let through = batch.through_ms;
        apply_batch(conn, &[WriteOp::Rollup(Box::new(batch))]).unwrap();
        Some(through)
    }

    #[test]
    fn the_gate_simulation_is_finalized_once_the_minutes_settle() {
        let (_dir, mut conn) = open();
        let mut ops = vec![
            event(1, T0, "PROMPT_SUBMIT", Some("p")),
            event(2, T0 + 10 * MIN, "TURN_END", Some("p")),
        ];
        ops.extend((0..10).map(|i| idle_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();

        // Only minutes older than BUCKET_SETTLE are finalized (aligned down
        // to a whole Activity Rollup window): T0+8 min at T0+10 min.
        let first = finalize(&mut conn, None, T0 + 10 * MIN).unwrap();
        assert_eq!(first, T0 + 8 * MIN);
        assert_eq!(totals(&conn), (8, 480, 0, 390));
        // Nothing new settles a minute later.
        assert_eq!(finalize(&mut conn, Some(first), T0 + 11 * MIN), None);

        let through = finalize(&mut conn, Some(first), T0 + 12 * MIN).unwrap();
        assert_eq!(through, T0 + 10 * MIN);
        assert_eq!(totals(&conn), (10, 600, 0, 510));
        let rollups: i64 = conn
            .query_row("SELECT count(*) FROM activity_rollup", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rollups, 5);
        let id: String = conn
            .query_row("SELECT DISTINCT ruleset_id FROM work_bucket", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(id, "traction-rules/1");
    }

    #[test]
    fn a_finalized_minute_is_never_rewritten_by_later_events() {
        let (_dir, mut conn) = open();
        apply_batch(
            &mut conn,
            &[
                event(1, T0, "PROMPT_SUBMIT", Some("p")),
                event(2, T0 + 2 * MIN, "TURN_END", Some("p")),
            ],
        )
        .unwrap();
        let now = T0 + 10 * MIN;
        let through = finalize(&mut conn, None, now).unwrap();
        let before = totals(&conn);
        assert_eq!(before.1, 120);

        // A late event inside the finalized range, then another pass.
        apply_batch(
            &mut conn,
            &[
                event(3, T0 + 3 * MIN, "PROMPT_SUBMIT", Some("p")),
                event(4, T0 + 4 * MIN, "TURN_END", Some("p")),
            ],
        )
        .unwrap();
        assert_eq!(finalize(&mut conn, Some(through), now), None);
        assert_eq!(totals(&conn), before);
    }

    #[test]
    fn a_restart_resumes_from_the_stored_cursor_without_duplicates() {
        let (_dir, mut conn) = open();
        apply_batch(
            &mut conn,
            &[
                event(1, T0, "PROMPT_SUBMIT", Some("p")),
                event(2, T0 + 2 * MIN, "TURN_END", Some("p")),
            ],
        )
        .unwrap();
        let first = finalize(&mut conn, None, T0 + 10 * MIN).unwrap();
        let before = totals(&conn);
        // "Restart": the in-memory cursor is gone; read it back from SQLite.
        let stored = read_cursor(&conn, &TRACTION_RULES_1).unwrap();
        assert_eq!(stored, Some(first));
        assert_eq!(finalize(&mut conn, stored, T0 + 10 * MIN), None);
        assert_eq!(totals(&conn), before);
        // Replaying the same range (a crash before the cursor moved) is a
        // no-op for the rows.
        let batch = finalize_range(&conn, &TRACTION_RULES_1, None, T0 + 10 * MIN, 120_000)
            .unwrap()
            .unwrap();
        apply_batch(&mut conn, &[WriteOp::Rollup(Box::new(batch))]).unwrap();
        assert_eq!(totals(&conn), before);
    }

    #[test]
    fn minutes_the_daemon_was_down_for_have_tier_none_and_no_human_time() {
        let (_dir, mut conn) = open();
        // Tailer-filled events, but no Presence Minute at all.
        apply_batch(
            &mut conn,
            &[
                event(1, T0, "PROMPT_SUBMIT", Some("p")),
                event(2, T0 + 2 * MIN, "TURN_END", Some("p")),
            ],
        )
        .unwrap();
        finalize(&mut conn, None, T0 + 10 * MIN).unwrap();
        let tiers: Vec<(String, i64)> = conn
            .prepare("SELECT telemetry_tier, human_active_sec FROM work_bucket")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(tiers.len(), 2);
        assert!(
            tiers.iter().all(|(t, h)| t == "none" && *h == 0),
            "{tiers:?}"
        );
    }

    #[test]
    fn the_new_tables_are_append_only() {
        let (_dir, mut conn) = open();
        apply_batch(
            &mut conn,
            &[
                idle_minute(T0),
                idle_minute(T0 + MIN),
                event(1, T0, "PROMPT_SUBMIT", Some("p")),
                event(2, T0 + 2 * MIN, "TURN_END", Some("p")),
                WriteOp::Commit(CommitObservation {
                    project_id: "p".to_owned(),
                    commit_oid: "a".repeat(40),
                    observed_ms: T0 + 1000,
                }),
            ],
        )
        .unwrap();
        finalize(&mut conn, None, T0 + 10 * MIN).unwrap();
        for table in ["commit_observation", "work_bucket", "activity_rollup"] {
            let update = conn.execute(&format!("UPDATE {table} SET ruleset_id = 'x'"), []);
            let update = if table == "commit_observation" {
                conn.execute("UPDATE commit_observation SET observed_ms = 1", [])
            } else {
                update
            };
            assert!(update.is_err(), "{table} update");
            assert!(
                conn.execute(&format!("DELETE FROM {table}"), []).is_err(),
                "{table} delete"
            );
        }
    }
}
