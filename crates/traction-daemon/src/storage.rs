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
    MIGRATION_3,
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
}

/// Loads the [`StoredRegistry`].
pub fn load_registry(conn: &Connection) -> rusqlite::Result<StoredRegistry> {
    Ok(StoredRegistry {
        projects: load_projects(conn)?,
        aliases: load_aliases(conn)?,
        sessions: load_sessions(conn)?,
    })
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
        let Some(harness_id) = HarnessId::ALL.into_iter().find(|h| h.as_str() == harness) else {
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

/// Applies a batch of writes in order, in one transaction.
pub fn apply_batch(conn: &mut Connection, ops: &[WriteOp]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for op in ops {
        match op {
            WriteOp::Event(row) => insert_event(&tx, row)?,
            WriteOp::Project(project) => insert_project(&tx, project)?,
            WriteOp::Session(session) => upsert_session(&tx, session)?,
            WriteOp::Migration(migration) => migrate_project(&tx, migration)?,
        }
    }
    tx.commit()
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
