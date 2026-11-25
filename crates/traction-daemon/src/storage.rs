//! SQLite persistence: bundled SQLite in WAL mode with `synchronous=NORMAL`,
//! a versioned (`user_version`) migration list, the append-only events
//! table and the Project registry tables.
//!
//! Only validated, typed Traction Event fields are written. No column ever
//! holds a request body. The single writer thread (`writer.rs`) owns the only
//! read-write connection.

use std::path::Path;

use rusqlite::{Connection, params};
use traction_core::{EndReason, HarnessId, MutationKind, TractionEvent};

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

/// Inserts a batch in one transaction. A repeated `event_id` is ignored.
pub fn insert_batch(conn: &mut Connection, rows: &[EventRow]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut insert = tx.prepare_cached(
            "INSERT OR IGNORE INTO traction_event (
                event_id, schema_version, timestamp_ms, harness_id, session_id,
                parent_orchestrator_id, event_type, end_reason, cwd, project_id,
                model_id, network_class, attribution_source, prompt_char_len,
                repo_symbol_hits, has_stacktrace, tool_name, exit_code,
                diff_lines_delta, mutation_kind
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                      ?16, ?17, ?18, ?19, ?20)",
        )?;
        for row in rows {
            insert.execute(params![
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
        }
    }
    tx.commit()
}
