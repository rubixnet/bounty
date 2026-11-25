//! Seam 2: the SQLite schema after migration 2 (step 1b): nullable Harness,
//! session, `cwd` and `project_id`, persisted `attribution_source`, the
//! Project registry tables, a clean upgrade of a 1a-era database, and
//! append-only Traction Events enforced by the database itself.

mod common;

use common::{TestDaemon, event_id, open_db_read_only, stored_event_count, valid_event_value};
use rusqlite::Connection;
use traction_daemon::DB_FILE;

/// Migration 1 exactly as Step 1a shipped it.
const MIGRATION_1: &str = "CREATE TABLE traction_event (
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
    ) STRICT, WITHOUT ROWID;";

const PROJECT_ID: &str = "git:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn user_version(db: &Connection) -> i64 {
    db.query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version")
}

fn columns(db: &Connection, table: &str) -> Vec<(String, String, bool)> {
    db.prepare("SELECT name, type, \"notnull\" FROM pragma_table_info(?1) ORDER BY cid")
        .and_then(|mut stmt| {
            stmt.query_map([table], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect()
        })
        .expect("table info")
}

fn column(db: &Connection, table: &str, name: &str) -> (String, bool) {
    columns(db, table)
        .into_iter()
        .find(|(column, _, _)| column == name)
        .map(|(_, kind, not_null)| (kind, not_null))
        .unwrap_or_else(|| panic!("{table}.{name} exists"))
}

#[test]
fn a_new_database_is_at_migration_2() {
    let mut daemon = TestDaemon::start();

    let db = daemon.shutdown_and_open_db();

    assert_eq!(user_version(&db), 2);
}

#[test]
fn harness_session_cwd_and_project_id_are_nullable_and_attribution_source_is_required() {
    let mut daemon = TestDaemon::start();
    let db = daemon.shutdown_and_open_db();

    for name in ["harness_id", "session_id", "cwd", "project_id"] {
        assert_eq!(
            column(&db, "traction_event", name),
            ("TEXT".to_owned(), false),
            "{name} is nullable TEXT"
        );
    }
    assert_eq!(
        column(&db, "traction_event", "attribution_source"),
        ("TEXT".to_owned(), true)
    );
    assert_eq!(
        column(&db, "traction_event", "mutation_kind"),
        ("TEXT".to_owned(), false)
    );
}

#[test]
fn an_event_without_cwd_or_project_id_is_stored_with_its_attribution_source() {
    let mut daemon = TestDaemon::start();
    let mut event = valid_event_value(1);
    event["cwd"] = serde_json::Value::Null;
    event["project_id"] = serde_json::Value::Null;
    event["attribution_source"] = serde_json::json!("harness_log");

    let response = daemon.caller().post_event(&event.to_string());

    assert_eq!(response.status, 202, "{}", response.body);
    let db = daemon.shutdown_and_open_db();
    let row: (Option<String>, Option<String>, String, Option<String>) = db
        .query_row(
            "SELECT cwd, project_id, attribution_source, mutation_kind
             FROM traction_event WHERE event_id = ?1",
            [event_id(1)],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("stored event");
    assert_eq!(row, (None, None, "harness_log".to_owned(), None));
}

#[test]
fn the_project_registry_tables_exist() {
    let mut daemon = TestDaemon::start();
    let db = daemon.shutdown_and_open_db();

    let names = |table| -> Vec<String> {
        columns(&db, table)
            .into_iter()
            .map(|(name, _, _)| name)
            .collect()
    };
    let project = names("project");
    for name in [
        "project_id",
        "kind",
        "root_path",
        "home_relative_path",
        "name",
        "first_seen_ms",
    ] {
        assert!(project.iter().any(|c| c == name), "project.{name}");
    }
    let alias = names("project_alias");
    for name in ["alias_id", "project_id", "migrated_at_ms"] {
        assert!(alias.iter().any(|c| c == name), "project_alias.{name}");
    }
    let session = names("session");
    for name in ["session_id", "harness_id", "start_folder", "last_cwd"] {
        assert!(session.iter().any(|c| c == name), "session.{name}");
    }
}

#[test]
fn project_id_and_alias_id_are_indexed() {
    let mut daemon = TestDaemon::start();
    let db = daemon.shutdown_and_open_db();

    let uses_index = |sql: &str| -> bool {
        let plan: Vec<String> = db
            .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
            .and_then(|mut stmt| stmt.query_map([], |row| row.get(3))?.collect())
            .expect("query plan");
        plan.iter()
            .any(|step| step.contains("INDEX") || step.contains("PRIMARY KEY"))
    };
    assert!(uses_index(
        "SELECT event_id FROM traction_event WHERE project_id = 'x'"
    ));
    assert!(uses_index(
        "SELECT project_id FROM project_alias WHERE alias_id = 'x'"
    ));
}

/// Builds a database exactly as a Step 1a daemon left it, with one event.
fn seed_1a_database(data_dir: &std::path::Path) {
    let db = Connection::open(data_dir.join(DB_FILE)).expect("create 1a database");
    db.pragma_update(None, "journal_mode", "WAL")
        .expect("wal mode");
    db.execute_batch(MIGRATION_1).expect("migration 1");
    db.pragma_update(None, "user_version", 1)
        .expect("user_version 1");
    db.execute(
        "INSERT INTO traction_event VALUES (?1, 1, 1795432800001, 'claude-code', 'session-1',
            NULL, 'TURN_END', 'completed', '/abs/path/to/working/dir', ?2,
            'optional-model-string', 'cloud_api', 142, 3, 1, 'Bash', 1, 24)",
        rusqlite::params![event_id(1), PROJECT_ID],
    )
    .expect("1a event");
}

#[test]
fn a_1a_era_database_upgrades_and_keeps_its_events() {
    let data_dir = tempfile::tempdir().expect("temp data folder");
    seed_1a_database(data_dir.path());

    let handle = common::try_start_on(data_dir.path(), |_| {}).expect("daemon starts on 1a db");
    let caller = common::BusCaller::new(
        handle.local_addr(),
        std::fs::read_to_string(data_dir.path().join(traction_daemon::TOKEN_FILE)).expect("token"),
    );
    let mut event = valid_event_value(2);
    event["project_id"] = serde_json::Value::Null;
    assert_eq!(caller.post_event(&event.to_string()).status, 202);
    handle.shutdown().expect("graceful shutdown");

    let db = open_db_read_only(data_dir.path());
    assert_eq!(user_version(&db), 2);
    assert_eq!(stored_event_count(&db), 2);
    let old: (
        String,
        String,
        String,
        String,
        Option<String>,
        i64,
        Option<bool>,
    ) = db
        .query_row(
            "SELECT harness_id, session_id, project_id, attribution_source, mutation_kind,
                    prompt_char_len, has_stacktrace
             FROM traction_event WHERE event_id = ?1",
            [event_id(1)],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )
        .expect("1a event survives");
    assert_eq!(
        old,
        (
            "claude-code".to_owned(),
            "session-1".to_owned(),
            PROJECT_ID.to_owned(),
            "harness_hook".to_owned(),
            None,
            142,
            Some(true),
        )
    );
}

#[test]
fn stored_events_cannot_be_updated_or_deleted() {
    let mut daemon = TestDaemon::start();
    assert_eq!(
        daemon
            .caller()
            .post_event(&valid_event_value(1).to_string())
            .status,
        202
    );
    daemon.shutdown();

    // A separate read-write connection, as any other code path would open.
    let db = Connection::open(daemon.data_dir().join(DB_FILE)).expect("read-write connection");
    let update = db.execute(
        "UPDATE traction_event SET model_id = NULL WHERE event_id = ?1",
        [event_id(1)],
    );
    let delete = db.execute("DELETE FROM traction_event", []);

    for (what, result) in [("UPDATE", update), ("DELETE", delete)] {
        let error = result.expect_err(&format!("{what} must be aborted"));
        assert!(
            error.to_string().contains("append-only"),
            "{what} aborted by the trigger: {error}"
        );
    }
    let model_id: String = db
        .query_row(
            "SELECT model_id FROM traction_event WHERE event_id = ?1",
            [event_id(1)],
            |row| row.get(0),
        )
        .expect("event still stored");
    assert_eq!(model_id, "optional-model-string");
    assert_eq!(stored_event_count(&db), 1);
}
