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
use traction_core::sync::timeline_marks;
use traction_core::{
    CommitObservation, MINUTE_MS, PresenceMinute, RollupInput, Ruleset, TRACTION_RULES_1,
    TractionEvent, fold_baseline, rollup,
};

use crate::clock::now_ms;
use crate::log;
use crate::signing_key;
use crate::storage::{self, DB_FILE, RollupBatch, Sealer, WriteOp};
use crate::writer::WriterSender;

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

/// Finalizes everything settled at `now_ms` in the database under
/// `data_dir`, batch by batch, returning how many batches were written.
/// For a stopped daemon only (it opens its own writing connection): the
/// catch-up pass the integration tests use to inspect derived tables.
#[doc(hidden)]
pub fn finalize_settled(data_dir: &Path, now_ms: u64) -> rusqlite::Result<usize> {
    let rules = &TRACTION_RULES_1;
    let mut conn = storage::open(data_dir)?;
    let loaded = signing_key::load_or_create(data_dir).map_err(storage::user_error)?;
    let mut sealer = Sealer::open(&conn, loaded)?;
    let mut cursor = read_cursor(&conn, rules)?;
    let mut batches = 0;
    while let Some(batch) = finalize_range(&conn, rules, cursor, now_ms, rules.bucket_settle_ms)? {
        cursor = Some(batch.through_ms);
        storage::apply_batch_with(&mut conn, &[WriteOp::Rollup(Box::new(batch))], &mut sealer)?;
        batches += 1;
    }
    Ok(batches)
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
/// A minute is final once `settle_ms` has passed over the end of everything
/// its Work Bucket can depend on (`Ruleset::context_fwd_ms`: Anchors,
/// steering neighbours, a friction loop that closes later), so the horizon
/// is `now_ms - settle_ms - context_fwd_ms`. Ranges are aligned to the
/// Activity Rollup window, so a rollup never straddles two batches.
///
/// The pure engine runs over the batch plus `context_back_ms` before it and
/// `context_fwd_ms` after it, with the inputs of that whole span loaded, and
/// only the minutes inside the batch are kept: the result equals one engine
/// call over the whole range (given a baseline of fewer than
/// `baseline_min_days`, which the baseline fold otherwise changes between
/// batches by design). Pure with respect to the clock: `now_ms` is a
/// parameter.
pub fn finalize_range(
    conn: &Connection,
    rules: &Ruleset,
    cursor: Option<u64>,
    now_ms: u64,
    settle_ms: u64,
) -> rusqlite::Result<Option<RollupBatch>> {
    let width = rules.activity_rollup_ms;
    let forward = rules.context_fwd_ms();
    let horizon = now_ms.saturating_sub(settle_ms + forward) / width * width;
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
    let (ctx_from, ctx_to) = (
        from.saturating_sub(rules.context_back_ms()) / width * width,
        to + forward,
    );
    let events = load_events(conn, rules, ctx_from, ctx_to)?;
    let presence = load_presence(conn, ctx_from, ctx_to)?;
    let commits = load_commits(conn, ctx_from, ctx_to)?;
    let mut baseline = storage::load_baseline(conn)?;
    let out = rollup(
        rules,
        &RollupInput {
            events: &events,
            presence: &presence,
            commits: &commits,
            baseline: &baseline,
            from_ms: ctx_from,
            to_ms: ctx_to,
        },
    );
    let inside = |ms: u64| ms >= from && ms < to;
    let buckets: Vec<_> = out
        .work_buckets
        .into_iter()
        .filter(|b| inside(b.minute_start_ms))
        .collect();
    let rollups = out
        .activity_rollups
        .into_iter()
        .filter(|r| inside(r.window_start_ms))
        .collect();
    // Unsigned Timeline Marks come from the same stored events: counts and
    // harness IDs only, outside the chain (`docs/adr/0005`).
    let marks = timeline_marks(&events, from, to);
    // The batch's present minutes join the baseline after it was scored
    // against the history that came before.
    fold_baseline(rules, &mut baseline, &buckets);
    Ok(Some(RollupBatch {
        ruleset_id: rules.ruleset_id.to_owned(),
        from_ms: from,
        sealed_at_ms: now_ms,
        through_ms: to,
        buckets,
        rollups,
        marks,
        baseline,
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
                event_type, end_reason, project_id, network_class, attribution_source,
                prompt_char_len, repo_symbol_hits, has_stacktrace, tool_name,
                diff_lines_delta, mutation_kind
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
                "metrics_only": {
                    "prompt_char_len": row.get::<_, Option<i64>>(10)?,
                    "repo_symbol_hits": row.get::<_, Option<i64>>(11)?,
                    "has_stacktrace": row.get::<_, Option<bool>>(12)?,
                    "tool_name": row.get::<_, Option<String>>(13)?,
                    "exit_code": null,
                    "diff_lines_delta": row.get::<_, Option<i64>>(14)?,
                    "mutation_kind": row.get::<_, Option<String>>(15)?,
                },
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
    /// Tests think in "minutes older than the settle delay"; the finalizer
    /// also waits out the forward context, which `finalize` adds to `now`.
    const LAG: u64 = TRACTION_RULES_1.context_fwd_ms();
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
        let batch = finalize_range(conn, &TRACTION_RULES_1, cursor, now + LAG, 120_000).unwrap()?;
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

    /// An event with its own harness (`None`: a manual file mutation) and
    /// canary values in every field that must never reach a mark.
    fn marked_event(n: u32, ts: u64, kind: &str, harness: Option<&str>, project: &str) -> WriteOp {
        let source = if harness.is_some() {
            "harness_hook"
        } else {
            "file_watcher"
        };
        let value = serde_json::json!({
            "schema_version": 1,
            "event_id": format!("019a7c2e-8b4d-7000-8000-{n:012x}"),
            "timestamp_ms": ts,
            "harness_id": harness,
            "session_id": harness.map(|_| "CANARY-SESSION-7f3a"),
            "parent_orchestrator_id": null,
            "event_type": kind,
            "end_reason": null,
            "cwd": "/home/CANARY-CWD-91cd/project",
            "project_id": project,
            "model_id": "CANARY-MODEL-55e1",
            "network_class": "cloud_api",
            "attribution_source": source,
            "metrics_only": {"tool_name": "CANARY-TOOL-02b8"}
        });
        let event: TractionEvent = serde_json::from_value(value).expect("event");
        WriteOp::Event(Box::new(EventRow::try_from(event).expect("row")))
    }

    fn marks_in(conn: &Connection) -> Vec<traction_core::sync::TimelineMark> {
        storage::load_marks(conn, 0, u64::MAX / 2).unwrap()
    }

    #[test]
    fn the_finalizer_writes_timeline_marks_from_stored_events_beside_the_buckets() {
        let (_dir, mut conn) = open();
        let project = "p";
        let mut ops = vec![
            marked_event(1, T0 + 1_000, "PROMPT_SUBMIT", Some("claude-code"), project),
            marked_event(2, T0 + 2_000, "FILE_MUTATION", Some("claude-code"), project),
            marked_event(3, T0 + 3_000, "FILE_MUTATION", None, project),
            marked_event(4, T0 + 4_000, "FILE_MUTATION", None, project),
            marked_event(5, T0 + 5_000, "TOOL_FAIL", Some("codex-cli"), project),
            marked_event(
                6,
                T0 + MIN + 1_000,
                "PROMPT_SUBMIT",
                Some("opencode"),
                project,
            ),
            marked_event(7, T0 + 2 * MIN, "TURN_END", Some("opencode"), project),
        ];
        ops.extend((0..10).map(|i| idle_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();

        let through = finalize(&mut conn, None, T0 + 10 * MIN).unwrap();
        let marks = marks_in(&conn);
        assert_eq!(marks.len(), 3, "{marks:?}");
        assert_eq!(
            (
                marks[0].minute_start_ms,
                marks[0].mutations_manual,
                marks[0].mutations_harness,
                marks[0].turns_started,
                marks[0].tool_failures
            ),
            (T0, 2, 1, 1, 1)
        );
        assert_eq!(
            marks[0].harnesses,
            [
                traction_core::HarnessId::ClaudeCode,
                traction_core::HarnessId::CodexCli
            ]
        );
        assert_eq!(marks[1].turns_started, 1);
        assert_eq!(marks[1].harnesses, [traction_core::HarnessId::OpenCode]);

        // A minute is marked once: a late event never rewrites it.
        apply_batch(
            &mut conn,
            &[marked_event(8, T0 + 6_000, "TOOL_FAIL", None, project)],
        )
        .unwrap();
        assert_eq!(finalize(&mut conn, Some(through), T0 + 10 * MIN), None);
        assert_eq!(marks_in(&conn), marks);
    }

    #[test]
    fn no_raw_text_cwd_model_or_session_reaches_the_marks_table_or_the_heartbeat() {
        use crate::sync::{Cursor, SyncSettings, build_chunk};
        let (_dir, mut conn) = open();
        let mut ops = vec![
            marked_event(1, T0 + 1_000, "PROMPT_SUBMIT", Some("claude-code"), "p"),
            marked_event(2, T0 + 2_000, "TOOL_FAIL", Some("claude-code"), "p"),
            marked_event(3, T0 + 3_000, "FILE_MUTATION", None, "p"),
        ];
        ops.extend((0..10).map(|i| idle_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();
        finalize(&mut conn, None, T0 + 10 * MIN).unwrap();
        assert!(!marks_in(&conn).is_empty());

        let table_text: String = conn
            .prepare(
                "SELECT project_id || '|' || minute_start_ms || '|' || mutations_manual || '|' ||
                        mutations_harness || '|' || turns_started || '|' || tool_failures || '|' ||
                        harnesses FROM timeline_mark",
            )
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect::<Vec<_>>()
            .join("\n");
        let chunk = build_chunk(
            &conn,
            Cursor::default(),
            T0 + 3_600_000,
            &SyncSettings::default(),
            "tester",
        )
        .unwrap();
        // The chunk is only built once the chain has links; the finalizer
        // sealed them, so the heartbeat carries the marks.
        let chunk = chunk.expect("a heartbeat");
        assert!(!chunk.request.timeline_marks.is_empty());
        let heartbeat_marks = serde_json::to_string(&chunk.request.timeline_marks).unwrap();
        for canary in [
            "CANARY-SESSION",
            "CANARY-CWD",
            "CANARY-MODEL",
            "CANARY-TOOL",
            "/home/",
        ] {
            assert!(!table_text.contains(canary), "{canary} in the marks table");
            assert!(!heartbeat_marks.contains(canary), "{canary} in the marks");
        }
        assert!(table_text.contains("claude-code"));
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
        let batch = finalize_range(&conn, &TRACTION_RULES_1, None, T0 + 10 * MIN + LAG, 120_000)
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

    fn event_with(
        n: u32,
        ts: u64,
        kind: &str,
        session: Option<&str>,
        metrics: serde_json::Value,
    ) -> WriteOp {
        let manual = session.is_none();
        let value = serde_json::json!({
            "schema_version": 1,
            "event_id": format!("019a7c2e-8b4d-7000-8000-{n:012x}"),
            "timestamp_ms": ts,
            "harness_id": if manual { None } else { Some("claude-code") },
            "session_id": session,
            "parent_orchestrator_id": null,
            "event_type": kind,
            "end_reason": null,
            "cwd": "/code/p",
            "project_id": "p",
            "model_id": null,
            "network_class": if manual { "manual" } else { "cloud_api" },
            "attribution_source": if manual { "file_watcher" } else { "harness_hook" },
            "metrics_only": metrics,
        });
        let event: TractionEvent = serde_json::from_value(value).expect("event");
        WriteOp::Event(Box::new(EventRow::try_from(event).expect("row")))
    }

    fn working_minute(ts: u64) -> WriteOp {
        WriteOp::Presence(PresenceMinute {
            minute_start_ms: ts,
            key_events: Some(30),
            pointer_events: Some(30),
            injected_events: Some(0),
            active_subslots: Some(4),
            cadence_entropy_milli: Some(2500),
            category_sec: Some(traction_core::CategorySec {
                ide: 30,
                terminal: 30,
                ..traction_core::CategorySec::default()
            }),
            category_switches: Some(2),
            idle_sec: Some(0),
            telemetry_tier: traction_core::TelemetryTier::Full,
        })
    }

    #[test]
    fn struggle_scores_are_finalized_with_the_bucket_and_the_baseline_is_a_compact_aggregate() {
        let (_dir, mut conn) = open();
        let tool = |name: &str| serde_json::json!({ "tool_name": name });
        let mut ops = vec![
            event_with(
                1,
                T0 + 5_000,
                "PROMPT_SUBMIT",
                Some("s"),
                serde_json::json!({"prompt_char_len": 220, "repo_symbol_hits": 3, "has_stacktrace": true}),
            ),
            event_with(2, T0 + 20_000, "TOOL_FAIL", Some("s"), tool("Bash")),
            event_with(
                3,
                T0 + 40_000,
                "FILE_MUTATION",
                Some("s"),
                serde_json::json!({"mutation_kind": "modified", "diff_lines_delta": 3}),
            ),
            event_with(4, T0 + 90_000, "TOOL_FAIL", Some("s"), tool("Bash")),
            event_with(
                5,
                T0 + 110_000,
                "FILE_MUTATION",
                Some("s"),
                serde_json::json!({"mutation_kind": "modified", "diff_lines_delta": 3}),
            ),
            event_with(6, T0 + 150_000, "TOOL_POST", Some("s"), tool("Bash")),
            event_with(
                7,
                T0 + 170_000,
                "TURN_END",
                Some("s"),
                serde_json::json!({}),
            ),
        ];
        ops.extend((0..4).map(|i| working_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();
        finalize(&mut conn, None, T0 + 10 * MIN).unwrap();

        let rows: Vec<(i64, i64, i64, i64)> = conn
            .prepare(
                "SELECT human_active_sec, struggle_credit_sec, verified_struggle, axis_friction
                 FROM work_bucket ORDER BY minute_start_ms",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert!(rows.iter().all(|r| r.1 <= r.0), "{rows:?}");
        assert!(
            rows.iter().any(|r| r.1 > 0 && r.2 >= 600 && r.3 == 1000),
            "{rows:?}"
        );

        // The baseline holds only counts for the finalized day.
        let (days, friction): (i64, String) = conn
            .query_row(
                "SELECT count(*), group_concat(friction_hist) FROM struggle_baseline",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(days, 1);
        assert!(friction.starts_with('['), "{friction}");
        // It is the one table exempt from append-only.
        conn.execute(
            "UPDATE struggle_baseline SET friction_hist = friction_hist",
            [],
        )
        .unwrap();
        // The loaded baseline matches what was stored.
        let loaded = storage::load_baseline(&conn).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded[0].friction_hist.iter().sum::<u32>(),
            rows.iter().filter(|r| r.0 > 0).count() as u32
        );
    }

    /// The "no raw text" gate for everything Step 2 derives: after a real
    /// finalization, every column of every derived table is a number, a
    /// closed enum, an identifier or a JSON object of numbers. A new free-text
    /// column fails here until it is reviewed and listed.
    #[test]
    fn derived_tables_hold_only_numbers_enums_and_identifiers() {
        let (_dir, mut conn) = open();
        let mut ops = vec![
            event_with(
                1,
                T0 + 5_000,
                "PROMPT_SUBMIT",
                Some("s"),
                serde_json::json!({"prompt_char_len": 220, "repo_symbol_hits": 3, "has_stacktrace": true}),
            ),
            event_with(2, T0 + 40_000, "TURN_END", Some("s"), serde_json::json!({})),
            WriteOp::Commit(CommitObservation {
                project_id: "p".to_owned(),
                commit_oid: "a".repeat(40),
                observed_ms: T0 + 1000,
            }),
        ];
        ops.extend((0..4).map(|i| working_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();
        finalize(&mut conn, None, T0 + 10 * MIN).unwrap();

        fn numbers_only(v: &serde_json::Value) -> bool {
            match v {
                serde_json::Value::Number(_) | serde_json::Value::Null => true,
                serde_json::Value::Array(a) => a.iter().all(numbers_only),
                serde_json::Value::Object(o) => o.values().all(numbers_only),
                _ => false,
            }
        }
        for table in [
            "presence_minute",
            "commit_observation",
            "work_bucket",
            "activity_rollup",
            "rollup_cursor",
            "struggle_baseline",
        ] {
            let rows: i64 = conn
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert!(rows > 0, "{table} was not populated");
            let columns: Vec<(String, String)> = conn
                .prepare(&format!(
                    "SELECT name, type FROM pragma_table_info('{table}')"
                ))
                .unwrap()
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            for (name, kind) in columns {
                if kind == "INTEGER" {
                    continue;
                }
                assert_eq!(kind, "TEXT", "{table}.{name}");
                let values: Vec<String> = conn
                    .prepare(&format!(
                        "SELECT \"{name}\" FROM {table} WHERE \"{name}\" IS NOT NULL"
                    ))
                    .unwrap()
                    .query_map([], |r| r.get(0))
                    .unwrap()
                    .map(Result::unwrap)
                    .collect();
                for v in values {
                    let ok = match name.as_str() {
                        "telemetry_tier" => {
                            ["full", "reduced", "idle_only", "none"].contains(&v.as_str())
                        }
                        "ruleset_id" => v.starts_with("traction-rules/"),
                        "project_id" => v.len() <= 80 && v.chars().all(|c| c.is_ascii_graphic()),
                        "commit_oid" => v.len() == 40 && v.chars().all(|c| c.is_ascii_hexdigit()),
                        "category_sec"
                        | "category_share_milli"
                        | "friction_hist"
                        | "specificity_hist" => {
                            serde_json::from_str(&v).is_ok_and(|j| numbers_only(&j))
                        }
                        other => panic!("unreviewed text column {table}.{other}"),
                    };
                    assert!(ok, "{table}.{name} holds a non-conforming value: {v}");
                }
            }
        }
    }

    fn raw_event(
        n: u32,
        ts: u64,
        kind: &str,
        session: Option<&str>,
        metrics: serde_json::Value,
    ) -> TractionEvent {
        let manual = session.is_none();
        serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "event_id": format!("019a7c2e-8b4d-7000-8000-{n:012x}"),
            "timestamp_ms": ts,
            "harness_id": if manual { None } else { Some("claude-code") },
            "session_id": session,
            "parent_orchestrator_id": null,
            "event_type": kind,
            "end_reason": null,
            "cwd": "/code/p",
            "project_id": "p",
            "model_id": null,
            "network_class": if manual { "manual" } else { "cloud_api" },
            "attribution_source": if manual { "file_watcher" } else { "harness_hook" },
            "metrics_only": metrics,
        }))
        .expect("event")
    }

    fn working(ts: u64) -> PresenceMinute {
        match working_minute(ts) {
            WriteOp::Presence(m) => m,
            _ => unreachable!(),
        }
    }

    fn browsing(ts: u64) -> PresenceMinute {
        PresenceMinute {
            category_sec: Some(traction_core::CategorySec {
                browser: 60,
                ..traction_core::CategorySec::default()
            }),
            category_switches: Some(0),
            ..working(ts)
        }
    }

    /// Finalizing a range in many small batches must give exactly what one
    /// call of the pure engine over the whole range gives: a minute's score
    /// reaches across batch edges (a verified minute's 90 s grace, Anchors
    /// 5 to 10 minutes away, a friction loop that closes later, steering's
    /// neighbours), so a batch loads and settles that context.
    #[test]
    fn finalizing_in_many_small_batches_equals_one_whole_range_rollup() {
        let (_dir, mut conn) = open();
        let tool = |name: &str| serde_json::json!({ "tool_name": name });
        let edit = serde_json::json!({"mutation_kind": "modified", "diff_lines_delta": 3});
        let specific = serde_json::json!({"prompt_char_len": 200, "repo_symbol_hits": 3});
        let terse = serde_json::json!({"prompt_char_len": 8, "repo_symbol_hits": 0});
        let none = serde_json::json!({});
        let m = |n: u64, sec: u64| T0 + n * MIN + sec * 1000;
        let events = vec![
            raw_event(1, m(2, 5), "PROMPT_SUBMIT", Some("s"), specific),
            raw_event(2, m(2, 30), "FILE_MUTATION", Some("s"), edit.clone()),
            raw_event(3, m(4, 0), "TURN_END", Some("s"), none.clone()),
            raw_event(4, m(8, 0), "PROMPT_SUBMIT", Some("s"), terse.clone()),
            raw_event(5, m(9, 20), "PERMISSION_REQUEST", Some("s"), none.clone()),
            raw_event(6, m(9, 40), "TOOL_POST", Some("s"), tool("Bash")),
            raw_event(7, m(22, 0), "TURN_END", Some("s"), none.clone()),
            raw_event(8, m(24, 10), "FILE_MUTATION", None, edit.clone()),
            raw_event(9, m(33, 0), "TOOL_FAIL", Some("t"), tool("Bash")),
            raw_event(10, m(36, 30), "FILE_MUTATION", None, edit.clone()),
            raw_event(11, m(41, 0), "TOOL_POST", Some("t"), tool("Bash")),
            raw_event(12, m(45, 0), "PROMPT_SUBMIT", Some("t"), terse),
            raw_event(13, m(46, 0), "FILE_MUTATION", Some("t"), edit),
            raw_event(14, m(47, 0), "TURN_END", Some("t"), none),
        ];
        let presence: Vec<PresenceMinute> = (0..52)
            .filter(|i| ![7, 30].contains(i))
            .map(|i| {
                if (25..32).contains(&i) {
                    browsing(T0 + i * MIN)
                } else {
                    working(T0 + i * MIN)
                }
            })
            .collect();
        let commits = vec![CommitObservation {
            project_id: "p".to_owned(),
            commit_oid: "b".repeat(40),
            observed_ms: m(29, 0),
        }];

        let mut ops: Vec<WriteOp> = events
            .iter()
            .map(|e| WriteOp::Event(Box::new(EventRow::try_from(e.clone()).expect("row"))))
            .collect();
        ops.extend(presence.iter().cloned().map(WriteOp::Presence));
        ops.extend(commits.iter().cloned().map(WriteOp::Commit));
        apply_batch(&mut conn, &ops).unwrap();

        // Many small batches: the clock moves 4 minutes at a time.
        let rules = &TRACTION_RULES_1;
        let (mut cursor, mut batches) = (None, 0);
        let (mut buckets, mut rollups) = (Vec::new(), Vec::new());
        let mut now = T0;
        while now < T0 + 200 * MIN {
            now += 4 * MIN;
            if let Some(batch) = finalize_range(&conn, rules, cursor, now, 120_000).unwrap() {
                cursor = Some(batch.through_ms);
                batches += 1;
                buckets.extend(batch.buckets);
                rollups.extend(batch.rollups);
            }
        }
        assert!(batches >= 8, "{batches} batches");
        let through = cursor.unwrap();
        assert!(through >= T0 + 52 * MIN);

        let whole = rollup(
            rules,
            &RollupInput {
                events: &events,
                presence: &presence,
                commits: &commits,
                baseline: &[],
                from_ms: T0,
                to_ms: through,
            },
        );
        assert!(
            whole.work_buckets.iter().any(|b| b.struggle_credit_sec > 0),
            "the scenario must score something"
        );
        assert_eq!(buckets, whole.work_buckets);
        assert_eq!(rollups, whole.activity_rollups);
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

    // ---- Step 3a: sealing in the rollup transaction ----

    use traction_core::{
        ChainError, ChainReport, DeviceSigningKey, PayloadPart, VerifyPolicy, canonical_json,
        verify_chains,
    };

    const FAR: u64 = u64::MAX / 4;

    trait Canon {
        fn canon(&self) -> Vec<u8>;
    }
    impl<T: serde::Serialize> Canon for T {
        fn canon(&self) -> Vec<u8> {
            canonical_json(&serde_json::to_value(self).unwrap()).unwrap()
        }
    }

    fn rich_minute(ts: u64) -> WriteOp {
        WriteOp::Presence(PresenceMinute {
            minute_start_ms: ts,
            key_events: Some(40),
            pointer_events: Some(10),
            injected_events: Some(2),
            active_subslots: Some(4),
            cadence_entropy_milli: Some(2500),
            category_sec: Some(traction_core::CategorySec {
                ide: 30,
                terminal: 20,
                browser: 5,
                ai_desktop: 0,
                other: 5,
            }),
            category_switches: Some(3),
            idle_sec: Some(0),
            telemetry_tier: traction_core::TelemetryTier::Full,
        })
    }

    fn verify(conn: &Connection) -> Result<ChainReport, ChainError> {
        let stored = storage::load_chains(conn).unwrap();
        let rows = storage::load_sealed_rows(conn, 0, FAR).unwrap();
        verify_chains(&stored.chains, &rows, &VerifyPolicy::default())
    }

    fn key(seed: u8) -> DeviceSigningKey {
        DeviceSigningKey::from_seed(zeroize::Zeroizing::new([seed; 32])).unwrap()
    }

    fn links(conn: &Connection) -> Vec<(i64, i64)> {
        let mut stmt = conn
            .prepare("SELECT seq, window_start_ms FROM chain_link ORDER BY window_start_ms")
            .unwrap();
        stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    /// Every distinct window holding a row, computed straight from SQL.
    fn windows_with_rows(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn
            .prepare(&format!(
                "SELECT w FROM (
                     SELECT window_start_ms AS w FROM activity_rollup
                     UNION SELECT minute_start_ms / {w} * {w} FROM work_bucket
                     UNION SELECT observed_ms / {w} * {w} FROM commit_observation
                 ) ORDER BY w",
                w = TRACTION_RULES_1.activity_rollup_ms
            ))
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn cursor(conn: &Connection) -> Option<u64> {
        read_cursor(conn, &TRACTION_RULES_1).unwrap()
    }

    #[test]
    fn every_non_empty_window_is_sealed_once_and_the_chain_verifies() {
        let (_dir, mut conn) = open();
        let mut ops = vec![
            event(1, T0, "PROMPT_SUBMIT", Some("p")),
            event(2, T0 + 6 * MIN, "TURN_END", Some("p")),
            WriteOp::Commit(CommitObservation {
                project_id: "p".to_owned(),
                commit_oid: "a".repeat(40),
                observed_ms: T0 + 3 * MIN,
            }),
        ];
        // A quiet gap: nothing at all between T0+8 min and T0+30 min.
        ops.extend((0..8).map(|i| rich_minute(T0 + i * MIN)));
        ops.extend((30..34).map(|i| rich_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();

        finalize(&mut conn, None, T0 + 40 * MIN).unwrap();

        let sealed: Vec<i64> = links(&conn).into_iter().map(|(_, w)| w).collect();
        assert_eq!(sealed, windows_with_rows(&conn));
        assert!(sealed.len() >= 6, "{sealed:?}");
        let seqs: Vec<i64> = links(&conn).into_iter().map(|(s, _)| s).collect();
        assert_eq!(
            seqs,
            (0..i64::try_from(seqs.len()).unwrap()).collect::<Vec<_>>()
        );
        let report = verify(&conn).unwrap();
        assert_eq!(report.chains.len(), 1);
        assert!(report.key_discontinuities.is_empty());
        assert_eq!(report.pending_rows_after_tip, 0);
    }

    #[test]
    fn stored_rows_map_back_to_the_engines_exact_canonical_json() {
        let (_dir, mut conn) = open();
        let mut ops = vec![
            event(1, T0, "PROMPT_SUBMIT", Some("p")),
            event(2, T0 + 6 * MIN, "TURN_END", Some("p")),
        ];
        ops.extend((0..8).map(|i| rich_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();
        let batch = finalize_range(&conn, &TRACTION_RULES_1, None, T0 + 40 * MIN + LAG, 120_000)
            .unwrap()
            .unwrap();
        apply_batch(&mut conn, &[WriteOp::Rollup(Box::new(batch.clone()))]).unwrap();

        let rows = storage::load_sealed_rows(&conn, 0, FAR).unwrap();

        let canon = |v: &dyn Canon| v.canon();
        assert!(!batch.buckets.is_empty() && !batch.rollups.is_empty());
        assert!(
            batch
                .rollups
                .iter()
                .any(|r| r.category_share_milli.ide > 0 && r.mean_cadence_entropy_milli.is_some())
        );
        assert_eq!(canon(&rows.work_buckets), canon(&batch.buckets));
        assert_eq!(canon(&rows.rollups), canon(&batch.rollups));
        assert_eq!(rows.work_buckets, batch.buckets);
        assert_eq!(rows.rollups, batch.rollups);
    }

    #[test]
    fn a_failed_seal_rolls_back_rows_links_and_cursor_together_and_the_retry_seals() {
        let (_dir, mut conn) = open();
        let mut ops = vec![
            event(1, T0, "PROMPT_SUBMIT", Some("p")),
            event(2, T0 + 6 * MIN, "TURN_END", Some("p")),
        ];
        ops.extend((0..8).map(|i| rich_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();
        let batch = finalize_range(&conn, &TRACTION_RULES_1, None, T0 + 40 * MIN + LAG, 120_000)
            .unwrap()
            .unwrap();
        let op = [WriteOp::Rollup(Box::new(batch))];
        let mut sealer = storage::Sealer::open(&conn, key(3)).unwrap();
        sealer.fail_after(Some(2));

        let failed = storage::apply_batch_with(&mut conn, &op, &mut sealer);

        assert!(failed.is_err());
        assert_eq!(totals(&conn).0, 0, "no work bucket survives");
        assert!(links(&conn).is_empty());
        assert_eq!(cursor(&conn), None);
        let rollups: i64 = conn
            .query_row("SELECT count(*) FROM activity_rollup", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rollups, 0);

        sealer.fail_after(None);
        storage::apply_batch_with(&mut conn, &op, &mut sealer).unwrap();

        assert_eq!(links(&conn).len(), windows_with_rows(&conn).len());
        assert!(cursor(&conn).is_some());
        verify(&conn).unwrap();
    }

    #[test]
    fn a_clock_that_goes_back_never_refinalizes_and_marks_the_next_link() {
        let (_dir, mut conn) = open();
        // 26 hours of presence: the first batch is capped at one day.
        let ops: Vec<WriteOp> = (0..26 * 60).map(|i| idle_minute(T0 + i * MIN)).collect();
        apply_batch(&mut conn, &ops).unwrap();
        let rules = &TRACTION_RULES_1;
        let now = T0 + 27 * 60 * MIN + LAG;

        let first = finalize_range(&conn, rules, None, now, 120_000)
            .unwrap()
            .unwrap();
        let first_through = first.through_ms;
        assert_eq!(first_through, T0 + 24 * 60 * MIN);
        apply_batch(&mut conn, &[WriteOp::Rollup(Box::new(first))]).unwrap();
        let sealed_before = links(&conn).len();
        assert!(links_regressed(&conn).is_empty());

        // Ten minutes back: the rest of the data is still settled, nothing
        // already finalized is touched, and the next link records the dip.
        let second = finalize_range(&conn, rules, Some(first_through), now - 10 * MIN, 120_000)
            .unwrap()
            .unwrap();
        assert_eq!(second.sealed_at_ms, now - 10 * MIN);
        apply_batch(&mut conn, &[WriteOp::Rollup(Box::new(second))]).unwrap();
        let after_second = cursor(&conn).unwrap();
        assert!(after_second > first_through);

        // A clock far back settles nothing and moves nothing.
        assert!(
            finalize_range(&conn, rules, Some(after_second), T0 + LAG, 120_000)
                .unwrap()
                .is_none()
        );
        // Forward again: the quiet minutes settle, the cursor only moves
        // forward, and no data means no new link.
        let linked = links(&conn).len();
        let third = finalize_range(&conn, rules, Some(after_second), now + 10 * MIN, 120_000)
            .unwrap()
            .unwrap();
        assert!(third.buckets.is_empty() && third.rollups.is_empty());
        apply_batch(&mut conn, &[WriteOp::Rollup(Box::new(third))]).unwrap();
        assert!(cursor(&conn).unwrap() > after_second);
        assert_eq!(links(&conn).len(), linked);

        assert!(links(&conn).len() > sealed_before);
        assert_eq!(
            links_regressed(&conn).len(),
            1,
            "only the first link after the dip"
        );
        let (sealed_at, tip_before): (i64, i64) = conn
            .query_row(
                "SELECT sealed_at_ms, (SELECT sealed_at_ms FROM chain_link p
                                       WHERE p.seq = c.seq - 1)
                 FROM chain_link c WHERE clock_regressed = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            sealed_at,
            i64::try_from(now - 10 * MIN).unwrap(),
            "stored unclamped"
        );
        assert!(sealed_at < tip_before);
        let report = verify(&conn).unwrap();
        assert_eq!(report.clock_regressions, 1);
        assert_eq!(report.chains.len(), 1);
    }

    fn links_regressed(conn: &Connection) -> Vec<i64> {
        let mut stmt = conn
            .prepare("SELECT seq FROM chain_link WHERE clock_regressed = 1")
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    #[test]
    fn a_conflicting_existing_row_is_detected_and_never_silently_resealed() {
        let (_dir, mut conn) = open();
        let mut ops = vec![
            event(1, T0, "PROMPT_SUBMIT", Some("p")),
            event(2, T0 + 6 * MIN, "TURN_END", Some("p")),
        ];
        ops.extend((0..8).map(|i| rich_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();
        // A different Work Bucket for a minute about to be finalized.
        conn.execute(
            "INSERT INTO work_bucket
                 (project_id, minute_start_ms, ruleset_id, agent_compute_sec,
                  idle_autonomous_sec, human_active_sec, permission_wait_ms, commits_human,
                  commits_agent, injected_hid_detected, auto_approver_suspected,
                  telemetry_tier, tier_limited)
             VALUES ('p', ?1, 'traction-rules/1', 7, 0, 0, 0, 0, 0, 0, 0, 'full', 0)",
            [i64::try_from(T0 + 2 * MIN).unwrap()],
        )
        .unwrap();

        finalize(&mut conn, None, T0 + 40 * MIN).unwrap();

        match verify(&conn) {
            Err(ChainError::PayloadHashMismatch {
                part: PayloadPart::Project(project),
                ..
            }) => assert_eq!(project, "p"),
            other => panic!("expected a Project mismatch, got {other:?}"),
        }
    }

    #[test]
    fn a_window_that_does_not_follow_the_tip_is_refused_not_forced() {
        let (_dir, mut conn) = open();
        let ops: Vec<WriteOp> = (0..8).map(|i| idle_minute(T0 + i * MIN)).collect();
        apply_batch(&mut conn, &ops).unwrap();
        finalize(&mut conn, None, T0 + 40 * MIN).unwrap();
        let before = links(&conn);
        let cursor_before = cursor(&conn);
        let late = ActivityRollupFixture::at(T0 - 10 * MIN);

        let stale = RollupBatch {
            ruleset_id: "traction-rules/1".to_owned(),
            from_ms: T0 - 10 * MIN,
            sealed_at_ms: T0,
            through_ms: T0 - 8 * MIN,
            buckets: Vec::new(),
            rollups: vec![late],
            marks: Vec::new(),
            baseline: Vec::new(),
        };
        apply_batch(&mut conn, &[WriteOp::Rollup(Box::new(stale))]).unwrap();

        assert_eq!(links(&conn), before);
        assert_eq!(cursor(&conn), cursor_before);
        let rows: i64 = conn
            .query_row(
                "SELECT count(*) FROM activity_rollup WHERE window_start_ms = ?1",
                [i64::try_from(T0 - 10 * MIN).unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0, "the refused window's rows are dropped too");
    }

    #[test]
    fn a_lost_cursor_never_counts_sealed_minutes_into_the_baseline_twice() {
        let (_dir, mut conn) = open();
        let mut ops = vec![
            event(1, T0, "PROMPT_SUBMIT", Some("p")),
            event(2, T0 + 6 * MIN, "TURN_END", Some("p")),
        ];
        ops.extend((0..8).map(|i| rich_minute(T0 + i * MIN)));
        apply_batch(&mut conn, &ops).unwrap();
        finalize(&mut conn, None, T0 + 40 * MIN).unwrap();
        let once = storage::load_baseline(&conn).unwrap();
        assert!(!once.is_empty(), "the sealed minutes are in the baseline");
        conn.execute("DELETE FROM rollup_cursor", []).unwrap();
        // Newer data with no present minute, so it adds nothing itself.
        let newer: Vec<WriteOp> = (90..98).map(|i| idle_minute(T0 + i * MIN)).collect();
        apply_batch(&mut conn, &newer).unwrap();
        let sealed = links(&conn).len();

        finalize(&mut conn, None, T0 + 150 * MIN).unwrap();

        assert!(links(&conn).len() > sealed, "a mixed batch");
        assert_eq!(storage::load_baseline(&conn).unwrap(), once);
    }

    struct ActivityRollupFixture;
    impl ActivityRollupFixture {
        fn at(window: u64) -> traction_core::ActivityRollup {
            traction_core::ActivityRollup {
                window_start_ms: window,
                ruleset_id: "traction-rules/1".to_owned(),
                minutes_observed: 2,
                category_share_milli: traction_core::CategoryShareMilli::default(),
                switch_rate_milli: 0,
                mean_cadence_entropy_milli: None,
                injected_ratio_milli: None,
                telemetry_tier: traction_core::TelemetryTier::Reduced,
            }
        }
    }

    #[test]
    fn a_second_key_starts_a_linked_successor_chain_and_the_old_chain_is_untouched() {
        let (_dir, mut conn) = open();
        let ops: Vec<WriteOp> = (0..4).map(|i| idle_minute(T0 + i * MIN)).collect();
        apply_batch(&mut conn, &ops).unwrap();
        let cursor = finalize(&mut conn, None, T0 + 20 * MIN).unwrap();
        let old = links(&conn);

        // The old key file is gone: a new key opens on the same database.
        let mut sealer = storage::Sealer::open(&conn, key(9)).unwrap();
        let more: Vec<WriteOp> = (30..34).map(|i| idle_minute(T0 + i * MIN)).collect();
        apply_batch(&mut conn, &more).unwrap();
        let batch = finalize_range(
            &conn,
            &TRACTION_RULES_1,
            Some(cursor),
            T0 + 60 * MIN + LAG,
            120_000,
        )
        .unwrap()
        .unwrap();
        storage::apply_batch_with(&mut conn, &[WriteOp::Rollup(Box::new(batch))], &mut sealer)
            .unwrap();

        assert_eq!(&links(&conn)[..old.len()], old.as_slice());
        let report = verify(&conn).unwrap();
        assert_eq!(report.chains.len(), 2);
        assert_eq!(report.key_discontinuities.len(), 1);
        assert!(report.key_discontinuities[0].linked);
        let genesis: (i64,) = conn
            .query_row(
                "SELECT min(seq) FROM chain_link WHERE key_id = ?1",
                [key(9).key_id().to_string()],
                |r| Ok((r.get(0)?,)),
            )
            .unwrap();
        assert_eq!(genesis.0, 0);
    }
}
