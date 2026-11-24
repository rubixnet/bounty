//! SQLite persistence: bundled SQLite in WAL mode with `synchronous=NORMAL`,
//! a versioned (`user_version`) migration list, and the events table.
//!
//! Only validated, typed Traction Event fields are written. No column ever
//! holds a request body. The single writer thread (`writer.rs`) owns the only
//! read-write connection.

use std::path::Path;

use rusqlite::{Connection, params};
use traction_core::TractionEvent;

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
    harness_id: String,
    session_id: String,
    parent_orchestrator_id: Option<String>,
    event_type: String,
    end_reason: Option<String>,
    cwd: String,
    project_id: String,
    model_id: Option<String>,
    network_class: String,
    prompt_char_len: Option<i64>,
    repo_symbol_hits: Option<i64>,
    has_stacktrace: Option<bool>,
    tool_name: Option<String>,
    exit_code: Option<i64>,
    diff_lines_delta: Option<i64>,
}

/// An unsigned field exceeds SQLite's signed 64-bit integer range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntegerOutOfRange;

impl IntegerOutOfRange {
    pub const CODE: &'static str = "integer_out_of_range";
}

impl TryFrom<TractionEvent> for EventRow {
    type Error = IntegerOutOfRange;

    fn try_from(event: TractionEvent) -> Result<Self, Self::Error> {
        let int = |value: u64| i64::try_from(value).map_err(|_| IntegerOutOfRange);
        let metrics = event.metrics_only;
        Ok(Self {
            schema_version: i64::from(event.schema_version),
            timestamp_ms: int(event.timestamp_ms)?,
            harness_id: wire(serde_json::to_value(event.harness_id)),
            event_type: wire(serde_json::to_value(event.event_type)),
            end_reason: event.end_reason.map(|r| wire(serde_json::to_value(r))),
            network_class: wire(serde_json::to_value(event.network_class)),
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
        })
    }
}

/// The protocol spelling of a `traction-core` enum, via its serde form so the
/// daemon never keeps a second copy of the wire strings.
fn wire(value: serde_json::Result<serde_json::Value>) -> String {
    match value {
        Ok(serde_json::Value::String(spelling)) => spelling,
        _ => unreachable!("traction-core enums serialize as strings"),
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
