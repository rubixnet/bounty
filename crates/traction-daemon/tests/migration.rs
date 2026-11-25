//! Seam 2: alias-only `dir:` -> `git:` migration and `git:` ID pinning.
//!
//! A `dir:` Project whose root becomes a Git repo with a commit is aliased
//! to the new `git:` Project. Stored events are never rewritten, new (and
//! late, old-ID) events land under the `git:` ID, and the `project_event`
//! view returns the whole history under the `git:` ID.

mod common;

use std::path::Path;
use std::time::Duration;

use common::fixtures::{self, git_id};
use common::{
    TestDaemon, canonical_string, event_id, open_db_read_only, poll_until, routed_event,
    stored_project_id,
};
use rusqlite::Connection;
use traction_daemon::PROJECT_EVENT_VIEW;

const WAIT: Duration = Duration::from_secs(10);

// The helpers below leave out `FILE_MUTATION` rows: the file watcher
// records the fixture's own `git init` and commit as hand edits.

/// Every column of every stored event, as text, ordered by `event_id`.
fn full_rows(db: &Connection) -> Vec<Vec<String>> {
    let mut stmt = db
        .prepare(
            "SELECT * FROM traction_event WHERE event_type <> 'FILE_MUTATION' ORDER BY event_id",
        )
        .expect("prepare");
    let columns = stmt.column_count();
    stmt.query_map([], |row| {
        (0..columns)
            .map(|i| {
                let value: rusqlite::types::Value = row.get(i)?;
                Ok(format!("{value:?}"))
            })
            .collect()
    })
    .expect("query")
    .map(|row| row.expect("row"))
    .collect()
}

/// Event IDs the alias-aware view returns for `project_id`.
fn project_events(db: &Connection, project_id: &str) -> Vec<String> {
    let mut stmt = db
        .prepare(&format!(
            "SELECT event_id FROM {PROJECT_EVENT_VIEW} WHERE project_id = ?1 AND event_type <> 'FILE_MUTATION' ORDER BY event_id"
        ))
        .expect("prepare");
    stmt.query_map([project_id], |row| row.get(0))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect()
}

fn aliases(db: &Connection) -> Vec<(String, String)> {
    let mut stmt = db
        .prepare("SELECT alias_id, project_id FROM project_alias ORDER BY alias_id")
        .expect("prepare");
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect()
}

fn merged_into(db: &Connection, project_id: &str) -> Option<String> {
    db.query_row(
        "SELECT merged_into FROM project WHERE project_id = ?1",
        [project_id],
        |row| row.get(0),
    )
    .expect("project row")
}

fn stored_count(data_dir: &Path) -> i64 {
    open_db_read_only(data_dir)
        .query_row(
            "SELECT COUNT(*) FROM traction_event WHERE event_type <> 'FILE_MUTATION'",
            [],
            |row| row.get(0),
        )
        .unwrap_or(0)
}

/// An event carrying an (Adapter-supplied) `project_id`.
fn event_with_project_id(n: u32, session: &str, cwd: &Path, project_id: &str) -> String {
    let mut event: serde_json::Value =
        serde_json::from_str(&routed_event(n, "TOOL_POST", session, Some(cwd))).unwrap();
    event["project_id"] = serde_json::json!(project_id);
    event.to_string()
}

#[test]
fn git_init_on_a_dir_project_aliases_it_without_rewriting_history() {
    let daemon = TestDaemon::start_with(|config| {
        config.writer.commit_interval = Duration::from_millis(20);
        config.writer.max_batch = 1;
    });
    let notes = fixtures::folder(&daemon.home().join("notes"), &[]);
    let dir_id = daemon.dir_id(&notes);
    let caller = daemon.caller();

    // Pre-migration: the empty folder is a session start folder (`dir:`).
    for (n, kind) in [(1, "SESSION_START"), (2, "PROMPT_SUBMIT")] {
        let response = caller.post_event(&routed_event(n, kind, "s1", Some(&notes)));
        assert_eq!(response.status, 202, "{}", response.body);
    }
    poll_until(WAIT, "pre-migration rows", || {
        (stored_count(daemon.data_dir()) == 2).then_some(())
    });
    let before = full_rows(&open_db_read_only(daemon.data_dir()));
    assert_eq!(before.len(), 2);

    // `git init` + first commit, then events at once (inside the cache
    // TTL): the `.git` appearing at the `dir:` root invalidates the cache.
    fixtures::repo(&notes, None);
    let git = git_id(&notes, None);
    let src = notes.join("src");
    assert_eq!(
        caller
            .post_event(&routed_event(3, "TOOL_PRE", "s1", Some(&src)))
            .status,
        202
    );
    // A late event still carrying the old `dir:` ID, from the old root.
    assert_eq!(
        caller
            .post_event(&event_with_project_id(4, "s1", &notes, &dir_id))
            .status,
        202
    );
    poll_until(WAIT, "post-migration rows", || {
        (stored_count(daemon.data_dir()) == 4).then_some(())
    });

    let check = |db: &Connection| {
        let after = full_rows(db);
        assert_eq!(after[..2], before[..], "pre-migration rows byte-for-byte");
        assert_eq!(stored_project_id(db, 1), Some(dir_id.clone()));
        assert_eq!(stored_project_id(db, 2), Some(dir_id.clone()));
        assert_eq!(stored_project_id(db, 3), Some(git.clone()));
        assert_eq!(
            stored_project_id(db, 4),
            Some(git.clone()),
            "late old-ID event"
        );
        assert_eq!(aliases(db), vec![(dir_id.clone(), git.clone())]);
        assert_eq!(merged_into(db, &dir_id), Some(git.clone()));
        assert_eq!(merged_into(db, &git), None);
        let (kind, root): (String, String) = db
            .query_row(
                "SELECT kind, root_path FROM project WHERE project_id = ?1",
                [&git],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("git project registered");
        assert_eq!((kind.as_str(), root), ("git", canonical_string(&notes)));
    };

    let mut daemon = daemon.restart();
    {
        let db = open_db_read_only(daemon.data_dir());
        check(&db);
        assert_eq!(
            project_events(&db, &git),
            (1..=4).map(event_id).collect::<Vec<_>>(),
            "alias-aware view: pre- and post-migration events together"
        );
        assert!(project_events(&db, &dir_id).is_empty());
    }

    // After a restart: another late old-ID event, and replaying the
    // migration check changes nothing.
    let caller = daemon.caller();
    assert_eq!(
        caller
            .post_event(&event_with_project_id(5, "s1", &notes, &dir_id))
            .status,
        202
    );
    assert_eq!(
        caller
            .post_event(&routed_event(6, "PROMPT_SUBMIT", "s1", None))
            .status,
        202
    );
    let db = daemon.shutdown_and_open_db();
    check(&db);
    assert_eq!(stored_project_id(&db, 5), Some(git.clone()));
    assert_eq!(stored_project_id(&db, 6), Some(git.clone()));
    assert_eq!(
        project_events(&db, &git),
        (1..=6).map(event_id).collect::<Vec<_>>()
    );
    let projects: i64 = db
        .query_row("SELECT COUNT(*) FROM project", [], |row| row.get(0))
        .unwrap();
    assert_eq!(projects, 2, "the dir: row stays, merged; no duplicate");
}

#[test]
fn registering_a_migrated_dir_root_answers_the_git_id() {
    let daemon = TestDaemon::start();
    let app = fixtures::folder(&daemon.home().join("app"), &["package.json"]);
    let dir_id = daemon.dir_id(&app);
    let caller = daemon.caller();
    let first = caller.register_project(&app);
    assert_eq!(first.status, 200, "{}", first.body);
    assert!(first.body.contains(&dir_id), "{}", first.body);

    fixtures::repo(&app, None);
    let git = git_id(&app, None);
    let second = caller.register_project(&app);
    assert_eq!(second.status, 200, "{}", second.body);
    assert!(second.body.contains(&git), "{}", second.body);

    let mut daemon = daemon.restart();
    let again = daemon.caller().register_project(&app);
    assert!(again.body.contains(&git), "{}", again.body);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(aliases(&db), vec![(dir_id.clone(), git.clone())]);
    assert_eq!(merged_into(&db, &dir_id), Some(git));
}

#[test]
fn adding_origin_after_first_routing_keeps_the_pinned_git_id() {
    let daemon = TestDaemon::start_with(|config| {
        config.writer.commit_interval = Duration::from_millis(20);
        config.writer.max_batch = 1;
    });
    let repo = fixtures::repo(&daemon.home().join("code/app"), None);
    let pinned = git_id(&repo, None);
    let caller = daemon.caller();
    assert_eq!(
        caller
            .post_event(&routed_event(1, "SESSION_START", "s", Some(&repo)))
            .status,
        202
    );

    // Routed (and pinned) before `origin` exists.
    poll_until(WAIT, "first event", || {
        (stored_count(daemon.data_dir()) == 1).then_some(())
    });
    fixtures::git(
        &repo,
        &["remote", "add", "origin", "git@github.com:Acme/App.git"],
    );
    assert_ne!(git_id(&repo, Some("git@github.com:Acme/App.git")), pinned);
    // A folder never resolved before (no cached answer) in the same repo.
    std::fs::create_dir_all(repo.join("docs")).unwrap();
    assert_eq!(
        caller
            .post_event(&routed_event(2, "TOOL_PRE", "s", Some(&repo.join("docs"))))
            .status,
        202
    );

    let mut daemon = daemon.restart();
    let after_restart = routed_event(3, "TOOL_POST", "s", Some(&repo.join("src")));
    assert_eq!(daemon.caller().post_event(&after_restart).status, 202);

    let db = daemon.shutdown_and_open_db();
    for n in 1..=3 {
        assert_eq!(stored_project_id(&db, n), Some(pinned.clone()), "event {n}");
    }
    let git_projects: Vec<String> = db
        .prepare("SELECT project_id FROM project WHERE kind = 'git'")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(|r| r.unwrap())
        .collect();
    assert_eq!(git_projects, vec![pinned]);
}
