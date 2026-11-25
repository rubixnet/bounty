//! SQLite persistence: bundled SQLite in WAL mode with `synchronous=NORMAL`,
//! a versioned (`user_version`) migration list, and the events table.
//!
//! Only validated, typed Traction Event fields are written. No column ever
//! holds a request body. The single writer thread (`writer.rs`) owns the only
//! read-write connection.

use std::path::Path;

use rusqlite::{Connection, params};
use traction_core::{EndReason, TractionEvent};

pub const DB_FILE: &str = "traction.db";

/// Schema migrations, applied in order. Migration `i` brings the database to
/// `user_version = i + 1`. Later sub-steps append; never edit a shipped entry.
const MIGRATIONS: &[&str] = &["CREATE TABLE traction_event (
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
    ) STRICT, WITHOUT ROWID;"];

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
    harness_id: &'static str,
    session_id: String,
    parent_orchestrator_id: Option<String>,
    event_type: &'static str,
    end_reason: Option<&'static str>,
    cwd: String,
    project_id: String,
    model_id: Option<String>,
    network_class: &'static str,
    prompt_char_len: Option<i64>,
    repo_symbol_hits: Option<i64>,
    has_stacktrace: Option<bool>,
    tool_name: Option<String>,
    exit_code: Option<i64>,
    diff_lines_delta: Option<i64>,
}

/// Why a validated Traction Event cannot be stored as a row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowError {
    /// An unsigned field exceeds SQLite's signed 64-bit integer range.
    IntegerOutOfRange,
    /// The event lacks a Harness, session, `cwd` or `project_id`, which the
    /// migration-1 events table still requires. Core accepts such events
    /// since Step 1b; the table catches up in a later migration.
    NotStorable,
}

impl RowError {
    /// A short, stable, machine-readable reason code.
    pub const fn code(self) -> &'static str {
        match self {
            RowError::IntegerOutOfRange => "integer_out_of_range",
            RowError::NotStorable => "event_not_storable",
        }
    }
}

impl TryFrom<TractionEvent> for EventRow {
    type Error = RowError;

    fn try_from(event: TractionEvent) -> Result<Self, Self::Error> {
        let int = |value: u64| i64::try_from(value).map_err(|_| RowError::IntegerOutOfRange);
        let (Some(harness_id), Some(session_id), Some(cwd), Some(project_id)) = (
            event.harness_id,
            event.session_id,
            event.cwd,
            event.project_id,
        ) else {
            return Err(RowError::NotStorable);
        };
        let metrics = event.metrics_only;
        Ok(Self {
            schema_version: i64::from(event.schema_version),
            timestamp_ms: int(event.timestamp_ms)?,
            harness_id: harness_id.as_str(),
            event_type: event.event_type.as_str(),
            end_reason: event.end_reason.map(EndReason::as_str),
            network_class: event.network_class.as_str(),
            event_id: event.event_id,
            session_id,
            parent_orchestrator_id: event.parent_orchestrator_id,
            cwd,
            project_id,
            model_id: event.model_id,
            prompt_char_len: metrics.prompt_char_len.map(int).transpose()?,
            repo_symbol_hits: metrics.repo_symbol_hits.map(int).transpose()?,
            has_stacktrace: metrics.has_stacktrace,
            tool_name: metrics.tool_name,
            exit_code: metrics.exit_code,
            diff_lines_delta: metrics.diff_lines_delta,
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
                model_id, network_class, prompt_char_len, repo_symbol_hits,
                has_stacktrace, tool_name, exit_code, diff_lines_delta
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
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
                row.prompt_char_len,
                row.repo_symbol_hits,
                row.has_stacktrace,
                row.tool_name,
                row.exit_code,
                row.diff_lines_delta,
            ])?;
        }
    }
    tx.commit()
}
