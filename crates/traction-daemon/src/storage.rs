//! SQLite persistence: bundled SQLite in WAL mode with `synchronous=NORMAL`,
//! a versioned (`user_version`) migration list, the append-only events
//! table, the Project registry tables and the Session Pause spans.
//!
//! Only validated, typed Traction Event fields are written. No column ever
//! holds a request body. The single writer thread (`writer.rs`) owns the only
//! read-write connection.

use crate::clock::now_ms_i64 as now_ms;
use std::path::Path;

mod chain_store;
// `StoredChains`, `load_chains` and `load_sealed_rows` are the seam the tests
// (and Slice 3's export) read the chain through.
#[allow(unused_imports)]
pub use chain_store::{
    Sealer, StoredChains, VerifyDatabaseError, load_chains, load_sealed_rows, open_read_only,
    user_error, verify_database, verify_database_with,
};

use rusqlite::{Connection, params};
use traction_core::sync::TimelineMark;
use traction_core::{
    ActivityRollup, BaselineDay, CommitObservation, EndReason, HarnessId, MutationKind,
    PresenceMinute, TractionEvent, WorkBucket,
};

pub const DB_FILE: &str = "traction.db";

/// Schema migrations, applied in order. Migration `i` brings the database to
/// `user_version = i + 1`. Later sub-steps append; never edit a shipped entry.
const MIGRATIONS: &[&str] = &[
    "CREATE TABLE traction_event (
        event_id               TEXT PRIMARY KEY NOT NULL,
        schema_version         INTEGER NOT NULL,
        timestamp_ms           INTEGER NOT NULL,
        harness_id             TEXT NOT NULL,
        session_id             TEXT NOT NULL,
        parent_orchestrator_id TEXT,
        event_type             TEXT NOT NULL,
        end_reason             TEXT,
        cwd                    TEXT NOT NULL,
        project_id             TEXT NOT NULL,
        model_id               TEXT,
        network_class          TEXT NOT NULL,
        prompt_char_len        INTEGER,
        repo_symbol_hits       INTEGER,
        has_stacktrace         INTEGER,
        tool_name              TEXT,
        exit_code              INTEGER,
        diff_lines_delta       INTEGER
    ) STRICT, WITHOUT ROWID;",
    MIGRATION_2,
    MIGRATION_3,
    MIGRATION_4,
    MIGRATION_5,
    MIGRATION_6,
    MIGRATION_7,
    MIGRATION_8,
    MIGRATION_9,
    MIGRATION_10,
    MIGRATION_11,
    MIGRATION_12,
    MIGRATION_13,
];

/// Step 1b: schema room on `schema_version: 1`, the Project registry and
/// append-only events.
///
/// The `traction_event` rebuild copies stored events for the last time: it is
/// a schema change, not a history change, and it runs before the triggers
/// that make the table append-only exist. Existing rows (development data
/// only) are backfilled as `harness_hook`, the only 1a source.
///
/// - `project`: one row per registered Project. `git_common_dir` pins a
///   `git:` ID to the repo's canonical common dir (ID pinning);
///   `merged_into` marks a `dir:` Project migrated into a `git:` one.
/// - `project_alias`: old ID (`alias_id`) -> current ID. Stored events keep
///   the ID they were written with; queries resolve through this table.
/// - `session`: each Harness session's start folder and last known `cwd`,
///   the Project Router's fallback when an event has no `cwd`.
const MIGRATION_2: &str = "
    CREATE TABLE traction_event_v2 (
        event_id               TEXT PRIMARY KEY NOT NULL,
        schema_version         INTEGER NOT NULL,
        timestamp_ms           INTEGER NOT NULL,
        harness_id             TEXT,
        session_id             TEXT,
        parent_orchestrator_id TEXT,
        event_type             TEXT NOT NULL,
        end_reason             TEXT,
        cwd                    TEXT,
        project_id             TEXT,
        model_id               TEXT,
        network_class          TEXT NOT NULL,
        attribution_source     TEXT NOT NULL,
        prompt_char_len        INTEGER,
        repo_symbol_hits       INTEGER,
        has_stacktrace         INTEGER,
        tool_name              TEXT,
        exit_code              INTEGER,
        diff_lines_delta       INTEGER,
        mutation_kind          TEXT
    ) STRICT, WITHOUT ROWID;

    INSERT INTO traction_event_v2 (
        event_id, schema_version, timestamp_ms, harness_id, session_id,
        parent_orchestrator_id, event_type, end_reason, cwd, project_id,
        model_id, network_class, attribution_source, prompt_char_len,
        repo_symbol_hits, has_stacktrace, tool_name, exit_code,
        diff_lines_delta, mutation_kind
    )
    SELECT
        event_id, schema_version, timestamp_ms, harness_id, session_id,
        parent_orchestrator_id, event_type, end_reason, cwd, project_id,
        model_id, network_class, 'harness_hook', prompt_char_len,
        repo_symbol_hits, has_stacktrace, tool_name, exit_code,
        diff_lines_delta, NULL
    FROM traction_event;

    DROP TABLE traction_event;
    ALTER TABLE traction_event_v2 RENAME TO traction_event;

    CREATE INDEX traction_event_project_id ON traction_event (project_id);

    CREATE TABLE project (
        project_id         TEXT PRIMARY KEY NOT NULL,
        kind               TEXT NOT NULL CHECK (kind IN ('git', 'dir')),
        root_path          TEXT NOT NULL,
        home_relative_path TEXT,
        name               TEXT NOT NULL,
        first_seen_ms      INTEGER NOT NULL,
        git_common_dir     TEXT UNIQUE,
        merged_into        TEXT
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX project_root_path ON project (root_path);

    CREATE TABLE project_alias (
        alias_id       TEXT PRIMARY KEY NOT NULL,
        project_id     TEXT NOT NULL,
        migrated_at_ms INTEGER NOT NULL,
        CHECK (alias_id <> project_id)
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX project_alias_project_id ON project_alias (project_id);

    CREATE TABLE session (
        harness_id   TEXT NOT NULL,
        session_id   TEXT NOT NULL,
        start_folder TEXT,
        last_cwd     TEXT,
        PRIMARY KEY (harness_id, session_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TRIGGER traction_event_no_update BEFORE UPDATE ON traction_event
    BEGIN
        SELECT RAISE(ABORT, 'traction_event is append-only');
    END;
    CREATE TRIGGER traction_event_no_delete BEFORE DELETE ON traction_event
    BEGIN
        SELECT RAISE(ABORT, 'traction_event is append-only');
    END;
";

/// Step 1b: the alias-aware events view, the only way to read "a Project's
/// events".
///
/// `project_event` has every `traction_event` column, but its `project_id`
/// is the stored ID resolved through `project_alias` to the final ID (alias
/// chains followed up to [`MAX_ALIAS_DEPTH`] links). `stored_project_id`
/// keeps the ID the event was written with (the hash-chain input). Read a
/// Project's events as `SELECT ... FROM project_event WHERE project_id = ?`;
/// never filter `traction_event.project_id` directly.
const MIGRATION_3: &str = "
    CREATE VIEW project_event AS
    WITH RECURSIVE chain (alias_id, project_id, depth) AS (
        SELECT alias_id, project_id, 1 FROM project_alias
        UNION ALL
        SELECT chain.alias_id, next.project_id, chain.depth + 1
        FROM chain JOIN project_alias AS next ON next.alias_id = chain.project_id
        WHERE chain.depth < 32
    ),
    resolved (alias_id, project_id) AS (
        SELECT alias_id, project_id FROM chain
        WHERE NOT EXISTS (
            SELECT 1 FROM project_alias AS a WHERE a.alias_id = chain.project_id
        )
    )
    SELECT
        e.event_id, e.schema_version, e.timestamp_ms, e.harness_id, e.session_id,
        e.parent_orchestrator_id, e.event_type, e.end_reason, e.cwd,
        COALESCE(resolved.project_id, e.project_id) AS project_id,
        e.model_id, e.network_class, e.attribution_source, e.prompt_char_len,
        e.repo_symbol_hits, e.has_stacktrace, e.tool_name, e.exit_code,
        e.diff_lines_delta, e.mutation_kind,
        e.project_id AS stored_project_id
    FROM traction_event AS e
    LEFT JOIN resolved ON resolved.alias_id = e.project_id;
";

/// Step 1c: Session Pause spans (`/stop-track` .. `/resume-track`).
///
/// One row per span; `to_ms` is `NULL` while the span is open, and a
/// session has at most one open span. Only the daemon's single writer
/// inserts a span and closes it; the Router loads them at startup.
const MIGRATION_4: &str = "
    CREATE TABLE session_pause (
        harness_id TEXT NOT NULL,
        session_id TEXT NOT NULL,
        from_ms    INTEGER NOT NULL,
        to_ms      INTEGER,
        PRIMARY KEY (harness_id, session_id, from_ms),
        CHECK (to_ms IS NULL OR to_ms >= from_ms)
    ) STRICT, WITHOUT ROWID;
    CREATE UNIQUE INDEX session_pause_open
        ON session_pause (harness_id, session_id) WHERE to_ms IS NULL;
";

/// Step 1c: the Claude transcript tailer's resume state.
///
/// - `transcript_offset`: per transcript file, its identity (inode + device
///   on Unix, volume serial + file index on Windows) and the byte offset
///   just past the last line whose events are stored. Written in the same
///   transaction as those events.
/// - `transcript_tailer_start`: one row, the tailer's first-ever start. A
///   file with no offset row was created after it, so it is read from 0.
/// - A partial index finding a session's stored `SESSION_START` (the Router
///   loads it at startup, so a tailed session is announced only once).
const MIGRATION_5: &str = "
    CREATE TABLE transcript_offset (
        path          TEXT PRIMARY KEY NOT NULL,
        file_identity TEXT NOT NULL,
        \"offset\"      INTEGER NOT NULL CHECK (\"offset\" >= 0),
        updated_ms    INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE transcript_tailer_start (
        id         INTEGER PRIMARY KEY NOT NULL CHECK (id = 1),
        started_ms INTEGER NOT NULL
    ) STRICT;

    CREATE INDEX traction_event_session_start
        ON traction_event (harness_id, session_id) WHERE event_type = 'SESSION_START';
";

/// Step 1c: hook coverage (`docs/protocol.md`, "Hook coverage").
///
/// - `daemon_run`: one row per daemon start. `last_alive_ms` is bumped by
///   the writer in every commit, so a run's span ends when its last write
///   did.
/// - `session_hook_run`: the timestamp of a session's first accepted hook
///   event in a run.
const MIGRATION_6: &str = "
    CREATE TABLE daemon_run (
        run_id        INTEGER PRIMARY KEY NOT NULL,
        started_ms    INTEGER NOT NULL,
        last_alive_ms INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE session_hook_run (
        harness_id    TEXT NOT NULL,
        session_id    TEXT NOT NULL,
        run_id        INTEGER NOT NULL,
        first_hook_ms INTEGER NOT NULL,
        PRIMARY KEY (harness_id, session_id, run_id)
    ) STRICT, WITHOUT ROWID;
";

/// Step 2a: device-wide Presence Minutes (counts, ratios and closed enums
/// only), append-only like `traction_event`. One row per wall-clock minute;
/// a `NULL` is a field the minute's telemetry tier could not fill.
const MIGRATION_7: &str = "
    CREATE TABLE presence_minute (
        minute_start_ms       INTEGER PRIMARY KEY NOT NULL,
        key_events            INTEGER,
        pointer_events        INTEGER,
        injected_events       INTEGER,
        active_subslots       INTEGER CHECK (active_subslots BETWEEN 0 AND 6),
        cadence_entropy_milli INTEGER,
        category_sec          TEXT,
        category_switches     INTEGER,
        idle_sec              INTEGER,
        telemetry_tier        TEXT NOT NULL
            CHECK (telemetry_tier IN ('full', 'reduced', 'idle_only', 'none'))
    ) STRICT;

    CREATE TRIGGER presence_minute_no_update BEFORE UPDATE ON presence_minute
    BEGIN
        SELECT RAISE(ABORT, 'presence_minute is append-only');
    END;
    CREATE TRIGGER presence_minute_no_delete BEFORE DELETE ON presence_minute
    BEGIN
        SELECT RAISE(ABORT, 'presence_minute is append-only');
    END;
";

/// Step 2b: Commit Observations and the finalized Three-Clock output.
///
/// `commit_observation`, `work_bucket` and `activity_rollup` are append-only
/// like `traction_event`; a new Ruleset writes new rows under its own ID.
/// `rollup_cursor` is the finalizer's bookmark (one row per Ruleset, written
/// in the same transaction as the rows it covers), so a restart resumes
/// without recomputing quiet minutes and without duplicates. Nothing here
/// has a slot for a commit message, author, diff or any raw text.
const MIGRATION_8: &str = "
    CREATE TABLE commit_observation (
        project_id  TEXT NOT NULL,
        commit_oid  TEXT NOT NULL,
        observed_ms INTEGER NOT NULL,
        PRIMARY KEY (project_id, commit_oid, observed_ms)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE work_bucket (
        project_id              TEXT NOT NULL,
        minute_start_ms         INTEGER NOT NULL,
        ruleset_id              TEXT NOT NULL,
        agent_compute_sec       INTEGER NOT NULL CHECK (agent_compute_sec BETWEEN 0 AND 60),
        idle_autonomous_sec     INTEGER NOT NULL CHECK (idle_autonomous_sec BETWEEN 0 AND 60),
        human_active_sec        INTEGER NOT NULL CHECK (human_active_sec BETWEEN 0 AND 60),
        permission_wait_ms      INTEGER NOT NULL,
        commits_human           INTEGER NOT NULL,
        commits_agent           INTEGER NOT NULL,
        injected_hid_detected   INTEGER NOT NULL,
        auto_approver_suspected INTEGER NOT NULL,
        telemetry_tier          TEXT NOT NULL
            CHECK (telemetry_tier IN ('full', 'reduced', 'idle_only', 'none')),
        tier_limited            INTEGER NOT NULL,
        PRIMARY KEY (project_id, minute_start_ms, ruleset_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE activity_rollup (
        window_start_ms            INTEGER NOT NULL,
        ruleset_id                 TEXT NOT NULL,
        minutes_observed           INTEGER NOT NULL,
        category_share_milli       TEXT NOT NULL,
        switch_rate_milli          INTEGER NOT NULL,
        mean_cadence_entropy_milli INTEGER,
        injected_ratio_milli       INTEGER,
        telemetry_tier             TEXT NOT NULL
            CHECK (telemetry_tier IN ('full', 'reduced', 'idle_only', 'none')),
        PRIMARY KEY (window_start_ms, ruleset_id)
    ) STRICT, WITHOUT ROWID;

    CREATE TABLE rollup_cursor (
        ruleset_id          TEXT PRIMARY KEY NOT NULL,
        finalized_through_ms INTEGER NOT NULL
    ) STRICT, WITHOUT ROWID;

    CREATE TRIGGER commit_observation_no_update BEFORE UPDATE ON commit_observation
    BEGIN SELECT RAISE(ABORT, 'commit_observation is append-only'); END;
    CREATE TRIGGER commit_observation_no_delete BEFORE DELETE ON commit_observation
    BEGIN SELECT RAISE(ABORT, 'commit_observation is append-only'); END;
    CREATE TRIGGER work_bucket_no_update BEFORE UPDATE ON work_bucket
    BEGIN SELECT RAISE(ABORT, 'work_bucket is append-only'); END;
    CREATE TRIGGER work_bucket_no_delete BEFORE DELETE ON work_bucket
    BEGIN SELECT RAISE(ABORT, 'work_bucket is append-only'); END;
    CREATE TRIGGER activity_rollup_no_update BEFORE UPDATE ON activity_rollup
    BEGIN SELECT RAISE(ABORT, 'activity_rollup is append-only'); END;
    CREATE TRIGGER activity_rollup_no_delete BEFORE DELETE ON activity_rollup
    BEGIN SELECT RAISE(ABORT, 'activity_rollup is append-only'); END;
";

/// Step 2d: struggle signals on every Work Bucket and the local comfort
/// baseline.
///
/// `work_bucket` keeps its append-only triggers; the new columns are written
/// with the rest of the row. `struggle_baseline` is the one table exempt from
/// append-only (`docs/protocol.md` section 2c): one row per UTC day of
/// histogram counts for the last `baseline_window_days`, upserted as days are
/// finalized and pruned past the window. It holds counts, never events or
/// timestamps.
const MIGRATION_9: &str = "
    ALTER TABLE work_bucket ADD COLUMN axis_steering INTEGER NOT NULL DEFAULT 0
        CHECK (axis_steering BETWEEN 0 AND 1000);
    ALTER TABLE work_bucket ADD COLUMN axis_specificity INTEGER NOT NULL DEFAULT 0
        CHECK (axis_specificity BETWEEN 0 AND 1000);
    ALTER TABLE work_bucket ADD COLUMN axis_friction INTEGER NOT NULL DEFAULT 0
        CHECK (axis_friction BETWEEN 0 AND 1000);
    ALTER TABLE work_bucket ADD COLUMN axis_comfort_delta INTEGER NOT NULL DEFAULT 0
        CHECK (axis_comfort_delta BETWEEN 0 AND 1000);
    ALTER TABLE work_bucket ADD COLUMN verified_struggle INTEGER NOT NULL DEFAULT 0
        CHECK (verified_struggle BETWEEN 0 AND 1000);
    ALTER TABLE work_bucket ADD COLUMN struggle_credit_sec INTEGER NOT NULL DEFAULT 0
        CHECK (struggle_credit_sec BETWEEN 0 AND 60);

    CREATE TABLE struggle_baseline (
        day              INTEGER PRIMARY KEY NOT NULL,
        friction_hist    TEXT NOT NULL,
        specificity_hist TEXT NOT NULL
    ) STRICT;
";

/// Step 3a: the tamper-evident chain (`docs/protocol.md`).
///
/// - `chain_key`: every Device Signing Key's public key the daemon ever held,
///   so a chain stays verifiable after its `signing.key` is gone.
/// - `chain_link`: one signed Chain Link per sealed 120 s window, written in
///   the same transaction as the window's rows and the `rollup_cursor`.
///   Append-only like the tables it covers. `UNIQUE (window_start_ms,
///   ruleset_id)` means a window is sealed at most once.
const MIGRATION_10: &str = "
    CREATE TABLE chain_key (
        key_id        TEXT PRIMARY KEY NOT NULL,
        public_key    TEXT NOT NULL UNIQUE,
        created_at_ms INTEGER NOT NULL
    ) STRICT;

    CREATE TABLE chain_link (
        key_id          TEXT NOT NULL REFERENCES chain_key (key_id),
        seq             INTEGER NOT NULL CHECK (seq >= 0),
        window_start_ms INTEGER NOT NULL,
        ruleset_id      TEXT NOT NULL,
        prev_hash       TEXT NOT NULL,
        payload_digest  TEXT NOT NULL,
        entry_hash      TEXT NOT NULL UNIQUE,
        sealed_at_ms    INTEGER NOT NULL,
        clock_regressed INTEGER NOT NULL CHECK (clock_regressed IN (0, 1)),
        payload_json    TEXT NOT NULL,
        signature       TEXT NOT NULL,
        PRIMARY KEY (key_id, seq),
        UNIQUE (window_start_ms, ruleset_id)
    ) STRICT;

    CREATE TRIGGER chain_key_no_update BEFORE UPDATE ON chain_key
    BEGIN SELECT RAISE(ABORT, 'chain_key is append-only'); END;
    CREATE TRIGGER chain_key_no_delete BEFORE DELETE ON chain_key
    BEGIN SELECT RAISE(ABORT, 'chain_key is append-only'); END;
    CREATE TRIGGER chain_link_no_update BEFORE UPDATE ON chain_link
    BEGIN SELECT RAISE(ABORT, 'chain_link is append-only'); END;
    CREATE TRIGGER chain_link_no_delete BEFORE DELETE ON chain_link
    BEGIN SELECT RAISE(ABORT, 'chain_link is append-only'); END;
";

/// Step 4 (S1): the server sync cursor. One row (`id = 1`): the newest
/// Chain Link window the server acknowledged (or that a terminal rejection
/// moved the cursor past), and a running count of terminal rejections. The
/// only mutable sync state; written through the single writer.
const MIGRATION_11: &str = "
    CREATE TABLE sync_cursor (
        id                  INTEGER PRIMARY KEY CHECK (id = 1),
        window_start_ms     INTEGER NOT NULL,
        key_id              TEXT NOT NULL,
        seq                 INTEGER NOT NULL,
        terminal_rejections INTEGER NOT NULL,
        updated_at_ms       INTEGER NOT NULL
    ) STRICT;
";

/// Step 4 (S2): Timeline Marks (`docs/adr/0005`). Per Project and minute,
/// counts and the `harness_id` set only (a JSON array of wire names), computed
/// by the finalizer from stored events. It sits beside `work_bucket` but
/// outside the hash chain: unsigned context, never proof. A minute is written
/// once (`INSERT OR IGNORE`), through the single writer.
const MIGRATION_12: &str = "
    CREATE TABLE timeline_mark (
        project_id        TEXT NOT NULL,
        minute_start_ms   INTEGER NOT NULL,
        mutations_manual  INTEGER NOT NULL,
        mutations_harness INTEGER NOT NULL,
        turns_started     INTEGER NOT NULL,
        tool_failures     INTEGER NOT NULL,
        harnesses         TEXT NOT NULL,
        PRIMARY KEY (project_id, minute_start_ms)
    ) STRICT, WITHOUT ROWID;
";

/// Step 3b (S1): Frame Records (spec Q8). One row per Frame the
/// `capture-frame` child wrote to the Vault, inserted by the single writer.
/// The signed part (`FrameRecord`: id, hash, capture time, trigger, schedule
/// slot, nonce id) is sealed into its window's `frame_digest`; `width`,
/// `height` and `window_start_ms` are local context. Append-only except
/// `pixels_state` (`present` → `redacted`/`expired`), which Redaction and
/// retention change after sealing and which no digest covers. No text: ids,
/// hashes, numbers and closed enums only.
const MIGRATION_13: &str = "
    CREATE TABLE frame (
        frame_id        TEXT PRIMARY KEY NOT NULL,
        window_start_ms INTEGER NOT NULL,
        captured_at_ms  INTEGER NOT NULL,
        frame_hash      TEXT NOT NULL,
        trigger         TEXT NOT NULL
            CHECK (trigger IN ('scheduled', 'turn_start', 'tool_fail', 'focus_shift')),
        scheduled_at_ms INTEGER,
        nonce_id        TEXT,
        width           INTEGER NOT NULL,
        height          INTEGER NOT NULL,
        pixels_state    TEXT NOT NULL CHECK (pixels_state IN ('present', 'redacted', 'expired'))
    ) STRICT, WITHOUT ROWID;
    CREATE INDEX frame_by_capture ON frame (captured_at_ms);

    CREATE TRIGGER frame_no_delete BEFORE DELETE ON frame
    BEGIN SELECT RAISE(ABORT, 'frame is append-only'); END;
    CREATE TRIGGER frame_pixels_state_only BEFORE UPDATE ON frame
    WHEN NEW.frame_id IS NOT OLD.frame_id OR NEW.window_start_ms IS NOT OLD.window_start_ms
      OR NEW.captured_at_ms IS NOT OLD.captured_at_ms OR NEW.frame_hash IS NOT OLD.frame_hash
      OR NEW.trigger IS NOT OLD.trigger OR NEW.scheduled_at_ms IS NOT OLD.scheduled_at_ms
      OR NEW.nonce_id IS NOT OLD.nonce_id OR NEW.width IS NOT OLD.width
      OR NEW.height IS NOT OLD.height OR OLD.pixels_state != 'present'
    BEGIN SELECT RAISE(ABORT, 'frame records are append-only'); END;
";

/// The alias-aware events view (see [`MIGRATION_3`]).
pub const PROJECT_EVENT_VIEW: &str = "project_event";

/// Longest alias chain followed (the view's recursion limit is the same).
pub const MAX_ALIAS_DEPTH: usize = 32;

/// Opens (creating if needed) the database read-write and migrates it.
pub fn open(data_dir: &Path) -> rusqlite::Result<Connection> {
    let mut conn = Connection::open(data_dir.join(DB_FILE))?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "synchronous", "NORMAL")?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    let current: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    for (version, sql) in (1i64..).zip(MIGRATIONS) {
        if version <= current {
            continue;
        }
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", version)?;
        tx.commit()?;
    }
    Ok(())
}

/// A validated Traction Event converted to the events table's column types.
/// Built on the bus side so a value SQLite cannot hold is rejected there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRow {
    event_id: String,
    schema_version: i64,
    timestamp_ms: i64,
    harness_id: Option<&'static str>,
    session_id: Option<String>,
    parent_orchestrator_id: Option<String>,
    event_type: &'static str,
    end_reason: Option<&'static str>,
    cwd: Option<String>,
    project_id: Option<String>,
    model_id: Option<String>,
    network_class: &'static str,
    attribution_source: &'static str,
    prompt_char_len: Option<i64>,
    repo_symbol_hits: Option<i64>,
    has_stacktrace: Option<bool>,
    tool_name: Option<String>,
    exit_code: Option<i64>,
    diff_lines_delta: Option<i64>,
    mutation_kind: Option<&'static str>,
}

/// Why a validated Traction Event cannot be stored as a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowError {
    /// An unsigned field exceeds SQLite's signed 64-bit integer range.
    IntegerOutOfRange,
}

impl RowError {
    /// A short, stable, machine-readable reason code.
    pub const fn code(self) -> &'static str {
        match self {
            RowError::IntegerOutOfRange => "integer_out_of_range",
        }
    }
}

impl EventRow {
    /// Whether `event` fits the table, without converting it. The bus runs
    /// this before answering, so the Router's later conversion cannot fail.
    pub fn check(event: &TractionEvent) -> Result<(), RowError> {
        let fits = |value: Option<u64>| value.is_none_or(|v| i64::try_from(v).is_ok());
        let metrics = &event.metrics_only;
        if fits(Some(event.timestamp_ms))
            && fits(metrics.prompt_char_len)
            && fits(metrics.repo_symbol_hits)
        {
            Ok(())
        } else {
            Err(RowError::IntegerOutOfRange)
        }
    }
}

impl TryFrom<TractionEvent> for EventRow {
    type Error = RowError;

    fn try_from(event: TractionEvent) -> Result<Self, Self::Error> {
        let int = |value: u64| i64::try_from(value).map_err(|_| RowError::IntegerOutOfRange);
        let metrics = event.metrics_only;
        Ok(Self {
            schema_version: i64::from(event.schema_version),
            timestamp_ms: int(event.timestamp_ms)?,
            harness_id: event.harness_id.map(HarnessId::as_str),
            event_type: event.event_type.as_str(),
            end_reason: event.end_reason.map(EndReason::as_str),
            network_class: event.network_class.as_str(),
            attribution_source: event.attribution_source.as_str(),
            event_id: event.event_id,
            session_id: event.session_id,
            parent_orchestrator_id: event.parent_orchestrator_id,
            cwd: event.cwd,
            project_id: event.project_id,
            model_id: event.model_id,
            prompt_char_len: metrics.prompt_char_len.map(int).transpose()?,
            repo_symbol_hits: metrics.repo_symbol_hits.map(int).transpose()?,
            has_stacktrace: metrics.has_stacktrace,
            tool_name: metrics.tool_name,
            exit_code: metrics.exit_code,
            diff_lines_delta: metrics.diff_lines_delta,
            mutation_kind: metrics.mutation_kind.map(MutationKind::as_str),
        })
    }
}

/// A Project registry row (`project` table), written once on first
/// registration. `git_common_dir` is set for `git:` Projects (ID pinning).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectRow {
    pub project_id: String,
    pub kind: ProjectKind,
    pub root_path: String,
    pub home_relative_path: Option<String>,
    pub name: String,
    pub first_seen_ms: i64,
    pub git_common_dir: Option<String>,
}

/// A Project's ID form.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Git,
    Dir,
}

impl ProjectKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Git => "git",
            Self::Dir => "dir",
        }
    }
}

/// A Harness session's routing fallback (`session` table). Upserted:
/// `start_folder` is kept once set, `last_cwd` is replaced when given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    pub harness_id: HarnessId,
    pub session_id: String,
    pub start_folder: Option<String>,
    pub last_cwd: Option<String>,
}

/// A Session Pause span (`session_pause` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseSpanRow {
    pub harness_id: HarnessId,
    pub session_id: String,
    pub from_ms: i64,
    /// `None` while the session is still paused.
    pub to_ms: Option<i64>,
}

/// One write the single writer applies, in channel order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteOp {
    /// Appends a Traction Event. A repeated `event_id` is ignored.
    Event(Box<EventRow>),
    /// Registers a Project. A repeated `project_id` (or pinned
    /// `git_common_dir`) is ignored, so a retry or replay is a no-op.
    Project(ProjectRow),
    /// Upserts a session's start folder and last `cwd`.
    Session(SessionRow),
    /// Alias-only `dir:` -> `git:` migration, in the batch's transaction.
    Migration(MigrationRow),
    /// Inserts a pause span: open (`to_ms` unset), or already closed when
    /// backfilled before a later span. Ignored when the session
    /// already has an open span.
    PauseStart(PauseSpanRow),
    /// Closes the session's open span starting at `from_ms` at `to_ms`.
    PauseEnd(PauseSpanRow),
    /// Events read from one transcript and the offset just past them,
    /// stored together in one transaction.
    Tailed(TailedBatch),
    /// The tailer's first-ever start and the offsets (each file's end) of
    /// the transcripts that existed before it. A replay keeps the first.
    TailerStart(TailerStartRow),
    /// Starts a daemon run (`last_alive_ms` starts at `started_ms`).
    RunStart(RunRow),
    /// A session's first accepted hook event in a run. A repeat is ignored.
    HookRun(HookRunRow),
    /// Appends one Presence Minute. A repeated `minute_start_ms` (a restart
    /// inside the same minute) is ignored.
    Presence(PresenceMinute),
    /// Appends one observed commit (`commit_observation`).
    Commit(CommitObservation),
    /// Appends one finalized range of Work Buckets and Activity Rollups and
    /// advances the finalizer's cursor, in one transaction.
    Rollup(Box<RollupBatch>),
    /// Moves the server sync cursor (`sync_cursor`).
    SyncCursor(SyncCursorRow),
    /// Records one Frame the `capture-frame` child wrote. A Frame whose
    /// window was already sealed is discarded: its Vault file is deleted and
    /// no record is written (it is never re-sealed).
    Frame(Box<FrameRecordWrite>),
}

/// One Frame Record plus its local context (see [`MIGRATION_13`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameRecordWrite {
    pub record: traction_core::FrameRecord,
    pub width: u32,
    pub height: u32,
    /// The Frame's Vault file, deleted when the Frame is discarded.
    pub vault_file: std::path::PathBuf,
}

/// The `sync_cursor` row. `terminal_rejections` is the running total.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCursorRow {
    pub window_start_ms: u64,
    pub key_id: String,
    pub seq: u64,
    pub terminal_rejections: u64,
    pub updated_at_ms: u64,
}

/// See [`WriteOp::Rollup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollupBatch {
    pub ruleset_id: String,
    /// The first finalized millisecond of the batch (a window boundary).
    pub from_ms: u64,
    /// The wall clock the finalizer read when it built the batch: stored,
    /// unclamped, as each Chain Link's `sealed_at_ms`.
    pub sealed_at_ms: u64,
    /// Every minute before this is finalized once the batch commits.
    pub through_ms: u64,
    pub buckets: Vec<WorkBucket>,
    pub rollups: Vec<ActivityRollup>,
    /// Unsigned Timeline Marks of the batch's minutes (outside the chain).
    pub marks: Vec<TimelineMark>,
    /// The comfort baseline after this batch: stored as is (days not listed
    /// here are pruned).
    pub baseline: Vec<BaselineDay>,
}

impl WriteOp {
    /// How many Traction Events this write stores (the writer's batch size
    /// counts these).
    pub fn event_count(&self) -> usize {
        match self {
            Self::Event(_) => 1,
            Self::Tailed(batch) => batch.events.len(),
            Self::Project(_)
            | Self::Session(_)
            | Self::Migration(_)
            | Self::PauseStart(_)
            | Self::PauseEnd(_)
            | Self::TailerStart(_)
            | Self::RunStart(_)
            | Self::HookRun(_)
            | Self::Presence(_)
            | Self::Commit(_)
            | Self::Rollup(_)
            | Self::SyncCursor(_)
            | Self::Frame(_) => 0,
        }
    }
}

/// One `daemon_run` row, as started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunRow {
    pub run_id: i64,
    pub started_ms: i64,
}

/// A stored run's end: when its last write committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoredRun {
    pub run_id: i64,
    pub last_alive_ms: i64,
}

/// One `session_hook_run` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunRow {
    pub harness_id: HarnessId,
    pub session_id: String,
    pub run_id: i64,
    pub first_hook_ms: i64,
}

/// One `transcript_offset` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptOffset {
    pub path: String,
    pub file_identity: String,
    pub offset: i64,
    pub updated_ms: i64,
}

/// See [`WriteOp::Tailed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailedBatch {
    pub events: Vec<EventRow>,
    pub offset: TranscriptOffset,
}

/// See [`WriteOp::TailerStart`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TailerStartRow {
    pub started_ms: i64,
    pub offsets: Vec<TranscriptOffset>,
}

/// The tailer's stored resume state.
#[derive(Debug, Default)]
pub struct TailerState {
    /// `None` until the tailer's first-ever start is committed.
    pub started_ms: Option<i64>,
    pub offsets: Vec<TranscriptOffset>,
}

/// Loads the [`TailerState`].
pub fn load_tailer_state(conn: &Connection) -> rusqlite::Result<TailerState> {
    let started_ms = match conn.query_row(
        "SELECT started_ms FROM transcript_tailer_start WHERE id = 1",
        [],
        |row| row.get(0),
    ) {
        Ok(ms) => Some(ms),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => return Err(error),
    };
    let mut stmt =
        conn.prepare("SELECT path, file_identity, \"offset\", updated_ms FROM transcript_offset")?;
    let offsets = stmt
        .query_map([], |row| {
            Ok(TranscriptOffset {
                path: row.get(0)?,
                file_identity: row.get(1)?,
                offset: row.get(2)?,
                updated_ms: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<_>>()?;
    Ok(TailerState {
        started_ms,
        offsets,
    })
}

/// Migrates the `dir:` Project `alias_id` into `into`: registers `into`
/// (if new), records `project_alias` (`alias_id` -> `into`'s final ID) and
/// marks the `dir:` row merged. Stored events are never touched. Replaying
/// it is a no-op; one that would close an alias cycle is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationRow {
    pub alias_id: String,
    pub into: ProjectRow,
    pub migrated_at_ms: i64,
}

/// Follows `alias_of` from `id` to its final ID. `None` when the chain
/// loops or is longer than [`MAX_ALIAS_DEPTH`].
pub fn final_alias(mut alias_of: impl FnMut(&str) -> Option<String>, id: &str) -> Option<String> {
    let mut current = id.to_owned();
    for _ in 0..=MAX_ALIAS_DEPTH {
        match alias_of(&current) {
            Some(next) if next == id => return None,
            Some(next) => current = next,
            None => return Some(current),
        }
    }
    None
}

/// Everything the Router seeds its in-memory registry from at startup.
#[derive(Debug, Default)]
pub struct StoredRegistry {
    /// Registered, non-merged Projects.
    pub projects: Vec<ProjectRow>,
    /// `project_alias` rows: (`alias_id`, `project_id`).
    pub aliases: Vec<(String, String)>,
    pub sessions: Vec<SessionRow>,
    /// Every Session Pause span, open and closed.
    pub pauses: Vec<PauseSpanRow>,
    /// Sessions with a stored `SESSION_START`.
    pub started_sessions: Vec<(HarnessId, String)>,
    /// Every earlier daemon run.
    pub runs: Vec<StoredRun>,
    /// Every session's first hook in each earlier run.
    pub hook_runs: Vec<HookRunRow>,
}

/// Loads the [`StoredRegistry`].
pub fn load_registry(conn: &Connection) -> rusqlite::Result<StoredRegistry> {
    Ok(StoredRegistry {
        projects: load_projects(conn)?,
        aliases: load_aliases(conn)?,
        sessions: load_sessions(conn)?,
        pauses: load_pauses(conn)?,
        started_sessions: load_started_sessions(conn)?,
        runs: load_runs(conn)?,
        hook_runs: load_hook_runs(conn)?,
    })
}

/// Every stored daemon run, oldest first.
pub fn load_runs(conn: &Connection) -> rusqlite::Result<Vec<StoredRun>> {
    let mut stmt = conn.prepare("SELECT run_id, last_alive_ms FROM daemon_run ORDER BY run_id")?;
    let rows = stmt.query_map([], |row| {
        Ok(StoredRun {
            run_id: row.get(0)?,
            last_alive_ms: row.get(1)?,
        })
    })?;
    rows.collect()
}

/// Every stored `session_hook_run` row.
pub fn load_hook_runs(conn: &Connection) -> rusqlite::Result<Vec<HookRunRow>> {
    let mut stmt =
        conn.prepare("SELECT harness_id, session_id, run_id, first_hook_ms FROM session_hook_run")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get(2)?,
            row.get(3)?,
        ))
    })?;
    let mut runs = Vec::new();
    for row in rows {
        let (harness, session_id, run_id, first_hook_ms) = row?;
        let Some(harness_id) = harness_by_name(&harness) else {
            continue;
        };
        runs.push(HookRunRow {
            harness_id,
            session_id,
            run_id,
            first_hook_ms,
        });
    }
    Ok(runs)
}

/// Every `(harness_id, session_id)` with a stored `SESSION_START` (through
/// the partial index, so the events table is not scanned).
pub fn load_started_sessions(conn: &Connection) -> rusqlite::Result<Vec<(HarnessId, String)>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT harness_id, session_id FROM traction_event
         WHERE event_type = 'SESSION_START' AND harness_id IS NOT NULL
           AND session_id IS NOT NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut started = Vec::new();
    for row in rows {
        let (harness, session_id) = row?;
        if let Some(harness_id) = harness_by_name(&harness) {
            started.push((harness_id, session_id));
        }
    }
    Ok(started)
}

/// Every stored alias (`alias_id`, `project_id`), for the Router.
pub fn load_aliases(conn: &Connection) -> rusqlite::Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare("SELECT alias_id, project_id FROM project_alias")?;
    let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect()
}

/// Registered, non-merged Projects, for the Router's in-memory registry.
pub fn load_projects(conn: &Connection) -> rusqlite::Result<Vec<ProjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT project_id, kind, root_path, home_relative_path, name, first_seen_ms,
                git_common_dir
         FROM project WHERE merged_into IS NULL",
    )?;
    let rows = stmt.query_map([], |row| {
        let kind: String = row.get(1)?;
        Ok(ProjectRow {
            project_id: row.get(0)?,
            kind: if kind == "git" {
                ProjectKind::Git
            } else {
                ProjectKind::Dir
            },
            root_path: row.get(2)?,
            home_relative_path: row.get(3)?,
            name: row.get(4)?,
            first_seen_ms: row.get(5)?,
            git_common_dir: row.get(6)?,
        })
    })?;
    rows.collect()
}

/// Every stored session, for the Router's `cwd` fallback after a restart.
pub fn load_sessions(conn: &Connection) -> rusqlite::Result<Vec<SessionRow>> {
    let mut stmt =
        conn.prepare("SELECT harness_id, session_id, start_folder, last_cwd FROM session")?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get(2)?,
            row.get(3)?,
        ))
    })?;
    let mut sessions = Vec::new();
    for row in rows {
        let (harness, session_id, start_folder, last_cwd) = row?;
        // Only daemon-written spellings are stored; skip anything else.
        let Some(harness_id) = harness_by_name(&harness) else {
            continue;
        };
        sessions.push(SessionRow {
            harness_id,
            session_id,
            start_folder,
            last_cwd,
        });
    }
    Ok(sessions)
}

/// Every stored Session Pause span, oldest first.
pub fn load_pauses(conn: &Connection) -> rusqlite::Result<Vec<PauseSpanRow>> {
    let mut stmt = conn.prepare(
        "SELECT harness_id, session_id, from_ms, to_ms FROM session_pause ORDER BY from_ms",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get(2)?,
            row.get(3)?,
        ))
    })?;
    let mut pauses = Vec::new();
    for row in rows {
        let (harness, session_id, from_ms, to_ms) = row?;
        let Some(harness_id) = harness_by_name(&harness) else {
            continue;
        };
        pauses.push(PauseSpanRow {
            harness_id,
            session_id,
            from_ms,
            to_ms,
        });
    }
    Ok(pauses)
}

/// The Harness stored as `name` (only daemon-written spellings exist).
fn harness_by_name(name: &str) -> Option<HarnessId> {
    HarnessId::ALL.into_iter().find(|h| h.as_str() == name)
}

/// [`apply_batch_with`] under a fixed throwaway key (unit tests that do not
/// care about the chain; every call continues the same chain).
#[cfg(test)]
pub fn apply_batch(conn: &mut Connection, ops: &[WriteOp]) -> rusqlite::Result<()> {
    let key = traction_core::DeviceSigningKey::from_seed(zeroize::Zeroizing::new([3u8; 32]))
        .expect("test key");
    let mut sealer = Sealer::open(conn, key)?;
    apply_batch_with(conn, ops, &mut sealer)
}

/// Applies a batch of writes in order, in one transaction. A finalized
/// range is sealed in that same transaction (see [`Sealer`]).
pub fn apply_batch_with(
    conn: &mut Connection,
    ops: &[WriteOp],
    sealer: &mut Sealer,
) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for op in ops {
        match op {
            WriteOp::Event(row) => insert_event(&tx, row)?,
            WriteOp::Project(project) => insert_project(&tx, project)?,
            WriteOp::Session(session) => upsert_session(&tx, session)?,
            WriteOp::Migration(migration) => migrate_project(&tx, migration)?,
            WriteOp::PauseStart(span) => start_pause(&tx, span)?,
            WriteOp::PauseEnd(span) => end_pause(&tx, span)?,
            WriteOp::Tailed(batch) => {
                for row in &batch.events {
                    insert_event(&tx, row)?;
                }
                upsert_offset(&tx, &batch.offset)?;
            }
            WriteOp::TailerStart(start) => {
                let inserted = tx
                    .prepare_cached(
                        "INSERT OR IGNORE INTO transcript_tailer_start (id, started_ms)
                         VALUES (1, ?1)",
                    )?
                    .execute([start.started_ms])?;
                // A replay (the first start already stored) keeps the
                // offsets stored since.
                if inserted == 1 {
                    for offset in &start.offsets {
                        upsert_offset(&tx, offset)?;
                    }
                }
            }
            WriteOp::RunStart(run) => {
                tx.prepare_cached(
                    "INSERT OR IGNORE INTO daemon_run (run_id, started_ms, last_alive_ms)
                     VALUES (?1, ?2, ?2)",
                )?
                .execute(params![run.run_id, run.started_ms])?;
            }
            WriteOp::Presence(minute) => insert_presence(&tx, minute)?,
            WriteOp::Commit(commit) => insert_commit(&tx, commit)?,
            WriteOp::Rollup(batch) => insert_rollup(&tx, batch, sealer)?,
            WriteOp::SyncCursor(cursor) => {
                tx.prepare_cached(
                    "INSERT INTO sync_cursor
                         (id, window_start_ms, key_id, seq, terminal_rejections, updated_at_ms)
                     VALUES (1, ?1, ?2, ?3, ?4, ?5)
                     ON CONFLICT (id) DO UPDATE SET
                         window_start_ms = excluded.window_start_ms, key_id = excluded.key_id,
                         seq = excluded.seq, terminal_rejections = excluded.terminal_rejections,
                         updated_at_ms = excluded.updated_at_ms",
                )?
                .execute(params![
                    to_i64(cursor.window_start_ms),
                    cursor.key_id,
                    to_i64(cursor.seq),
                    to_i64(cursor.terminal_rejections),
                    to_i64(cursor.updated_at_ms),
                ])?;
            }
            WriteOp::Frame(frame) => {
                insert_frame(&tx, frame)?;
            }
            WriteOp::HookRun(hook) => {
                tx.prepare_cached(
                    "INSERT OR IGNORE INTO session_hook_run
                         (harness_id, session_id, run_id, first_hook_ms)
                     VALUES (?1, ?2, ?3, ?4)",
                )?
                .execute(params![
                    hook.harness_id.as_str(),
                    hook.session_id,
                    hook.run_id,
                    hook.first_hook_ms
                ])?;
            }
        }
    }
    // The newest run is the live one: every commit proves it was alive.
    tx.prepare_cached(
        "UPDATE daemon_run SET last_alive_ms = max(last_alive_ms, ?1)
         WHERE run_id = (SELECT max(run_id) FROM daemon_run)",
    )?
    .execute([now_ms()])?;
    tx.commit()
}

/// Records one `written` capture report for a stopped daemon, through the
/// writer's own insert (tests drive the post-grab path with it).
#[doc(hidden)]
pub fn record_frame(data_dir: &Path, frame: FrameRecordWrite) -> rusqlite::Result<bool> {
    let mut conn = open(data_dir)?;
    let tx = conn.transaction()?;
    let inserted = insert_frame(&tx, &frame)?;
    tx.commit()?;
    Ok(inserted)
}

/// Removes Vault files that no Frame Record names (see
/// [`crate::capture::vault::remove_orphans`]).
pub fn remove_vault_orphans(conn: &Connection, data_dir: &Path) -> std::io::Result<usize> {
    let mut stmt = conn
        .prepare("SELECT 1 FROM frame WHERE frame_id = ?1")
        .map_err(std::io::Error::other)?;
    crate::capture::vault::remove_orphans(data_dir, |id| stmt.exists([id]).unwrap_or(true))
}

/// Inserts a Frame Record unless its window is already finalized (then the
/// Frame is discarded and its Vault file deleted). Returns whether it was
/// recorded.
fn insert_frame(conn: &Connection, frame: &FrameRecordWrite) -> rusqlite::Result<bool> {
    let record = &frame.record;
    let width = traction_core::TRACTION_RULES_1.activity_rollup_ms;
    let window = record.captured_at_ms / width * width;
    let finalized: Option<i64> = conn.query_row(
        "SELECT max(finalized_through_ms) FROM rollup_cursor",
        [],
        |row| row.get(0),
    )?;
    let sealed = finalized.is_some_and(|through| to_i64(window) < through);
    if sealed || record.validate().is_err() {
        let _ = std::fs::remove_file(&frame.vault_file);
        crate::log::warn("discarded a Frame that reported after its window was sealed");
        return Ok(false);
    }
    let inserted =
        conn.prepare_cached(
            "INSERT OR IGNORE INTO frame
                     (frame_id, window_start_ms, captured_at_ms, frame_hash, trigger,
                      scheduled_at_ms, nonce_id, width, height, pixels_state)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'present')",
        )?
        .execute(params![
            record.frame_id,
            to_i64(window),
            to_i64(record.captured_at_ms),
            record.frame_hash,
            record.trigger.as_str(),
            record.scheduled_at_ms.map(to_i64),
            record.nonce_id,
            frame.width,
            frame.height,
        ])? == 1;
    if !inserted {
        // An id that is already recorded: its file belongs to that record,
        // so it is kept.
        crate::log::warn("a Frame reused a recorded frame_id; not recorded");
    }
    Ok(inserted)
}

fn insert_presence(conn: &Connection, minute: &PresenceMinute) -> rusqlite::Result<()> {
    let category_sec = minute
        .category_sec
        .as_ref()
        .and_then(|sec| serde_json::to_string(sec).ok());
    let tier = serde_json::to_value(minute.telemetry_tier)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default();
    conn.prepare_cached(
        "INSERT OR IGNORE INTO presence_minute
             (minute_start_ms, key_events, pointer_events, injected_events,
              active_subslots, cadence_entropy_milli, category_sec,
              category_switches, idle_sec, telemetry_tier)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?
    .execute(params![
        i64::try_from(minute.minute_start_ms).unwrap_or(i64::MAX),
        minute.key_events,
        minute.pointer_events,
        minute.injected_events,
        minute.active_subslots,
        minute.cadence_entropy_milli,
        category_sec,
        minute.category_switches,
        minute.idle_sec,
        tier,
    ])?;
    Ok(())
}

fn insert_commit(conn: &Connection, commit: &CommitObservation) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO commit_observation (project_id, commit_oid, observed_ms)
         VALUES (?1, ?2, ?3)",
    )?
    .execute(params![
        commit.project_id,
        commit.commit_oid,
        i64::try_from(commit.observed_ms).unwrap_or(i64::MAX),
    ])?;
    Ok(())
}

fn tier_str(tier: traction_core::TelemetryTier) -> String {
    serde_json::to_value(tier)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Rows are `INSERT OR IGNORE`: a finalized minute is never rewritten, so a
/// replay after a crash between the write and the cursor is a no-op. A range
/// that reaches back to the chain tip (a lost cursor) is trimmed to what
/// comes after it, and the cursor still moves past the whole range.
fn insert_rollup(
    conn: &Connection,
    batch: &RollupBatch,
    sealer: &mut Sealer,
) -> rusqlite::Result<()> {
    let trimmed = sealer.after_tip(conn, batch)?;
    let unsealed = trimmed.as_ref().unwrap_or(batch);
    insert_rollup_rows(conn, unsealed)?;
    sealer.seal_batch(conn, unsealed)?;
    advance_cursor(conn, batch)
}

fn advance_cursor(conn: &Connection, batch: &RollupBatch) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO rollup_cursor (ruleset_id, finalized_through_ms) VALUES (?1, ?2)
         ON CONFLICT (ruleset_id) DO UPDATE SET
            finalized_through_ms = max(finalized_through_ms, excluded.finalized_through_ms)",
    )?
    .execute(params![batch.ruleset_id, to_i64(batch.through_ms)])?;
    Ok(())
}

fn insert_rollup_rows(conn: &Connection, batch: &RollupBatch) -> rusqlite::Result<()> {
    for b in &batch.buckets {
        conn.prepare_cached(
            "INSERT OR IGNORE INTO work_bucket
                 (project_id, minute_start_ms, ruleset_id, agent_compute_sec,
                  idle_autonomous_sec, human_active_sec, permission_wait_ms,
                  commits_human, commits_agent, injected_hid_detected,
                  auto_approver_suspected, telemetry_tier, tier_limited,
                  axis_steering, axis_specificity, axis_friction, axis_comfort_delta,
                  verified_struggle, struggle_credit_sec)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                     ?14, ?15, ?16, ?17, ?18, ?19)",
        )?
        .execute(params![
            b.project_id,
            to_i64(b.minute_start_ms),
            b.ruleset_id,
            b.agent_compute_sec,
            b.idle_autonomous_sec,
            b.human_active_sec,
            to_i64(b.permission_wait_ms),
            b.commits_human,
            b.commits_agent,
            b.injected_hid_detected,
            b.auto_approver_suspected,
            tier_str(b.telemetry_tier),
            b.tier_limited,
            b.struggle_axes.steering,
            b.struggle_axes.specificity,
            b.struggle_axes.friction,
            b.struggle_axes.comfort_delta,
            b.verified_struggle,
            b.struggle_credit_sec,
        ])?;
    }
    replace_baseline(conn, &batch.baseline)?;
    for m in &batch.marks {
        conn.prepare_cached(
            "INSERT OR IGNORE INTO timeline_mark
                 (project_id, minute_start_ms, mutations_manual, mutations_harness,
                  turns_started, tool_failures, harnesses)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?
        .execute(params![
            m.project_id,
            to_i64(m.minute_start_ms),
            m.mutations_manual,
            m.mutations_harness,
            m.turns_started,
            m.tool_failures,
            serde_json::to_string(&m.harnesses).unwrap_or_else(|_| "[]".to_owned()),
        ])?;
    }
    for r in &batch.rollups {
        let share = serde_json::to_string(&r.category_share_milli).unwrap_or_default();
        conn.prepare_cached(
            "INSERT OR IGNORE INTO activity_rollup
                 (window_start_ms, ruleset_id, minutes_observed, category_share_milli,
                  switch_rate_milli, mean_cadence_entropy_milli, injected_ratio_milli,
                  telemetry_tier)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        )?
        .execute(params![
            to_i64(r.window_start_ms),
            r.ruleset_id,
            r.minutes_observed,
            share,
            r.switch_rate_milli,
            r.mean_cadence_entropy_milli,
            r.injected_ratio_milli,
            tier_str(r.telemetry_tier),
        ])?;
    }
    Ok(())
}

/// The stored Timeline Marks of `[from, to)`, oldest first.
pub fn load_marks(conn: &Connection, from: u64, to: u64) -> rusqlite::Result<Vec<TimelineMark>> {
    let mut stmt = conn.prepare(
        "SELECT project_id, minute_start_ms, mutations_manual, mutations_harness,
                turns_started, tool_failures, harnesses
         FROM timeline_mark WHERE minute_start_ms >= ?1 AND minute_start_ms < ?2
         ORDER BY minute_start_ms, project_id",
    )?;
    let rows = stmt.query_map(params![to_i64(from), to_i64(to)], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, u32>(2)?,
            row.get::<_, u32>(3)?,
            row.get::<_, u32>(4)?,
            row.get::<_, u32>(5)?,
            row.get::<_, String>(6)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (project_id, minute, manual, harness, turns, failures, harnesses) = row?;
        let (Ok(minute_start_ms), Ok(harnesses)) =
            (u64::try_from(minute), serde_json::from_str(&harnesses))
        else {
            continue;
        };
        out.push(TimelineMark {
            project_id,
            minute_start_ms,
            mutations_manual: manual,
            mutations_harness: harness,
            turns_started: turns,
            tool_failures: failures,
            harnesses,
        });
    }
    Ok(out)
}

/// Stores `days` as the whole baseline (upserting each, pruning the rest).
fn replace_baseline(conn: &Connection, days: &[BaselineDay]) -> rusqlite::Result<()> {
    let oldest = days.iter().map(|d| d.day).min();
    for d in days {
        conn.prepare_cached(
            "INSERT OR REPLACE INTO struggle_baseline (day, friction_hist, specificity_hist)
             VALUES (?1, ?2, ?3)",
        )?
        .execute(params![
            to_i64(d.day),
            serde_json::to_string(&d.friction_hist).unwrap_or_default(),
            serde_json::to_string(&d.specificity_hist).unwrap_or_default(),
        ])?;
    }
    if let Some(oldest) = oldest {
        conn.prepare_cached("DELETE FROM struggle_baseline WHERE day < ?1")?
            .execute([to_i64(oldest)])?;
    }
    Ok(())
}

/// The stored comfort baseline, oldest day first.
pub fn load_baseline(conn: &Connection) -> rusqlite::Result<Vec<BaselineDay>> {
    let mut stmt = conn.prepare(
        "SELECT day, friction_hist, specificity_hist FROM struggle_baseline ORDER BY day",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (day, friction, specificity) = row?;
        let (Ok(day), Ok(friction_hist), Ok(specificity_hist)) = (
            u64::try_from(day),
            serde_json::from_str(&friction),
            serde_json::from_str(&specificity),
        ) else {
            continue;
        };
        out.push(BaselineDay {
            day,
            friction_hist,
            specificity_hist,
        });
    }
    Ok(out)
}

fn upsert_offset(conn: &Connection, row: &TranscriptOffset) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO transcript_offset (path, file_identity, \"offset\", updated_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (path) DO UPDATE SET
            file_identity = excluded.file_identity,
            \"offset\" = excluded.\"offset\",
            updated_ms = excluded.updated_ms",
    )?
    .execute(params![
        row.path,
        row.file_identity,
        row.offset,
        row.updated_ms
    ])?;
    Ok(())
}

fn insert_project(conn: &Connection, row: &ProjectRow) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO project (
            project_id, kind, root_path, home_relative_path, name, first_seen_ms,
            git_common_dir, merged_into
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
    )?
    .execute(params![
        row.project_id,
        row.kind.as_str(),
        row.root_path,
        row.home_relative_path,
        row.name,
        row.first_seen_ms,
        row.git_common_dir,
    ])?;
    Ok(())
}

fn migrate_project(conn: &Connection, row: &MigrationRow) -> rusqlite::Result<()> {
    let mut lookup =
        conn.prepare_cached("SELECT project_id FROM project_alias WHERE alias_id = ?1")?;
    let mut lookup_error = None;
    let mut alias_of = |id: &str| match lookup.query_row([id], |r| r.get(0)) {
        Ok(next) => Some(next),
        Err(rusqlite::Error::QueryReturnedNoRows) => None,
        Err(error) => {
            lookup_error = Some(error);
            None
        }
    };
    let already = alias_of(&row.alias_id).is_some();
    let target = final_alias(&mut alias_of, &row.into.project_id);
    drop(lookup);
    if let Some(error) = lookup_error {
        return Err(error);
    }
    if already {
        // Already migrated: a replay is a no-op.
        return Ok(());
    }
    let Some(target) = target.filter(|target| *target != row.alias_id) else {
        crate::log::warn("refused a Project migration that would close an alias cycle");
        return Ok(());
    };
    insert_project(conn, &row.into)?;
    conn.prepare_cached(
        "INSERT OR IGNORE INTO project_alias (alias_id, project_id, migrated_at_ms)
         VALUES (?1, ?2, ?3)",
    )?
    .execute(params![row.alias_id, target, row.migrated_at_ms])?;
    conn.prepare_cached(
        "UPDATE project SET merged_into = ?2 WHERE project_id = ?1 AND merged_into IS NULL",
    )?
    .execute(params![row.alias_id, target])?;
    Ok(())
}

fn start_pause(conn: &Connection, row: &PauseSpanRow) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO session_pause (harness_id, session_id, from_ms, to_ms)
         VALUES (?1, ?2, ?3, ?4)",
    )?
    .execute(params![
        row.harness_id.as_str(),
        row.session_id,
        row.from_ms,
        row.to_ms,
    ])?;
    Ok(())
}

fn end_pause(conn: &Connection, row: &PauseSpanRow) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "UPDATE session_pause SET to_ms = ?4
         WHERE harness_id = ?1 AND session_id = ?2 AND from_ms = ?3 AND to_ms IS NULL",
    )?
    .execute(params![
        row.harness_id.as_str(),
        row.session_id,
        row.from_ms,
        row.to_ms,
    ])?;
    Ok(())
}

fn upsert_session(conn: &Connection, row: &SessionRow) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT INTO session (harness_id, session_id, start_folder, last_cwd)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (harness_id, session_id) DO UPDATE SET
            start_folder = COALESCE(session.start_folder, excluded.start_folder),
            last_cwd = COALESCE(excluded.last_cwd, session.last_cwd)",
    )?
    .execute(params![
        row.harness_id.as_str(),
        row.session_id,
        row.start_folder,
        row.last_cwd,
    ])?;
    Ok(())
}

fn insert_event(conn: &Connection, row: &EventRow) -> rusqlite::Result<()> {
    conn.prepare_cached(
        "INSERT OR IGNORE INTO traction_event (
            event_id, schema_version, timestamp_ms, harness_id, session_id,
            parent_orchestrator_id, event_type, end_reason, cwd, project_id,
            model_id, network_class, attribution_source, prompt_char_len,
            repo_symbol_hits, has_stacktrace, tool_name, exit_code,
            diff_lines_delta, mutation_kind
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                  ?16, ?17, ?18, ?19, ?20)",
    )?
    .execute(params![
        row.event_id,
        row.schema_version,
        row.timestamp_ms,
        row.harness_id,
        row.session_id,
        row.parent_orchestrator_id,
        row.event_type,
        row.end_reason,
        row.cwd,
        row.project_id,
        row.model_id,
        row.network_class,
        row.attribution_source,
        row.prompt_char_len,
        row.repo_symbol_hits,
        row.has_stacktrace,
        row.tool_name,
        row.exit_code,
        row.diff_lines_delta,
        row.mutation_kind,
    ])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    //! The writer's migration handler against a real SQLite file: replay,
    //! alias chains and cycles (states the Router never produces in v1, so
    //! Seam 2 cannot reach them).

    use std::collections::HashMap;

    use super::*;

    fn project(id: &str, kind: ProjectKind) -> ProjectRow {
        ProjectRow {
            project_id: id.to_owned(),
            kind,
            root_path: format!("/code/{id}"),
            home_relative_path: None,
            name: id.to_owned(),
            first_seen_ms: 1,
            git_common_dir: None,
        }
    }

    fn migration(alias: &str, into: &str) -> WriteOp {
        WriteOp::Migration(MigrationRow {
            alias_id: alias.to_owned(),
            into: project(into, ProjectKind::Git),
            migrated_at_ms: 7,
        })
    }

    fn event(n: u8, project_id: &str) -> WriteOp {
        let value = serde_json::json!({
            "schema_version": 1,
            "event_id": format!("019a7c2e-8b4d-7000-8000-0000000000{n:02x}"),
            "timestamp_ms": 1u64,
            "harness_id": "claude-code",
            "session_id": "s",
            "parent_orchestrator_id": null,
            "event_type": "PROMPT_SUBMIT",
            "end_reason": null,
            "cwd": null,
            "project_id": project_id,
            "model_id": null,
            "network_class": "cloud_api",
            "attribution_source": "harness_hook",
            "metrics_only": {}
        });
        let event: TractionEvent = serde_json::from_value(value).expect("event");
        WriteOp::Event(Box::new(EventRow::try_from(event).expect("row")))
    }

    fn rows(conn: &Connection, sql: &str) -> Vec<(String, Option<String>)> {
        let mut stmt = conn.prepare(sql).expect("prepare");
        stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("query")
            .map(|row| row.expect("row"))
            .collect()
    }

    fn aliases(conn: &Connection) -> Vec<(String, Option<String>)> {
        rows(
            conn,
            "SELECT alias_id, project_id FROM project_alias ORDER BY alias_id",
        )
    }

    fn view(conn: &Connection) -> Vec<(String, Option<String>)> {
        rows(
            conn,
            "SELECT stored_project_id, project_id FROM project_event ORDER BY event_id",
        )
    }

    fn open_temp() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().expect("temp dir");
        let conn = open(dir.path()).expect("open");
        (dir, conn)
    }

    fn pair(a: &str, b: &str) -> (String, Option<String>) {
        (a.to_owned(), Some(b.to_owned()))
    }

    #[test]
    fn a_replayed_migration_is_a_no_op() {
        let (_dir, mut conn) = open_temp();
        apply_batch(
            &mut conn,
            &[
                WriteOp::Project(project("dir:a", ProjectKind::Dir)),
                event(1, "dir:a"),
                migration("dir:a", "git:x"),
            ],
        )
        .unwrap();
        apply_batch(&mut conn, &[migration("dir:a", "git:x")]).unwrap();
        apply_batch(&mut conn, &[migration("dir:a", "git:other")]).unwrap();

        assert_eq!(aliases(&conn), vec![pair("dir:a", "git:x")]);
        assert_eq!(
            rows(
                &conn,
                "SELECT project_id, merged_into FROM project ORDER BY project_id"
            ),
            vec![pair("dir:a", "git:x"), ("git:x".to_owned(), None)]
        );
        assert_eq!(view(&conn), vec![pair("dir:a", "git:x")]);
    }

    #[test]
    fn alias_chains_resolve_to_the_final_id() {
        let (_dir, mut conn) = open_temp();
        apply_batch(
            &mut conn,
            &[
                WriteOp::Project(project("dir:a", ProjectKind::Dir)),
                event(1, "dir:a"),
                migration("dir:a", "git:b"),
                event(2, "git:b"),
                // `git:b` is later aliased again.
                migration("git:b", "git:c"),
                event(3, "git:c"),
                // A migration into an aliased ID lands on its final ID.
                WriteOp::Project(project("dir:d", ProjectKind::Dir)),
                migration("dir:d", "git:b"),
            ],
        )
        .unwrap();

        assert_eq!(
            view(&conn),
            vec![
                pair("dir:a", "git:c"),
                pair("git:b", "git:c"),
                pair("git:c", "git:c"),
            ]
        );
        assert_eq!(
            aliases(&conn),
            vec![
                pair("dir:a", "git:b"),
                pair("dir:d", "git:c"),
                pair("git:b", "git:c"),
            ]
        );
        let map: HashMap<_, _> = load_aliases(&conn).unwrap().into_iter().collect();
        let resolve = |id: &str| final_alias(|current| map.get(current).cloned(), id);
        assert_eq!(resolve("dir:a").as_deref(), Some("git:c"));
        assert_eq!(resolve("git:c").as_deref(), Some("git:c"));
    }

    #[test]
    fn a_migration_that_would_close_a_cycle_is_refused() {
        let (_dir, mut conn) = open_temp();
        apply_batch(
            &mut conn,
            &[
                migration("dir:a", "git:b"),
                migration("git:b", "git:c"),
                // c -> a would close a -> b -> c -> a.
                migration("git:c", "dir:a"),
                // A self-alias.
                migration("git:z", "git:z"),
                event(1, "dir:a"),
            ],
        )
        .unwrap();

        assert_eq!(
            aliases(&conn),
            vec![pair("dir:a", "git:b"), pair("git:b", "git:c")]
        );
        assert_eq!(view(&conn), vec![pair("dir:a", "git:c")]);
        let looped: HashMap<String, String> = [("a", "b"), ("b", "a")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(final_alias(|id| looped.get(id).cloned(), "a"), None);
    }
}
