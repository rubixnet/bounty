//! Seam: Commit Observations through a real daemon watching real Git repos
//! (real `git`, real worktrees). A commit is recorded only for a new local
//! commit; checkout, switch, a fast-forward pull, a reset and an amend are
//! not commits. The commit message is never read.

mod common;

use std::path::Path;
use std::time::Duration;

use common::fixtures::{self, git, git_output};
use common::{TestDaemon, poll_until, routed_event};
use rusqlite::Connection;

const CANARY: &str = "CANARY-commit-message-7f3a91c2";

fn watching_daemon() -> TestDaemon {
    TestDaemon::start_with(|config| {
        config.presence.enabled = false;
        config.commits.poll = Duration::from_millis(25);
        config.writer.commit_interval = Duration::from_millis(25);
    })
}

/// Lets the observer notice what just happened (poll plus Git runs).
fn settle() {
    std::thread::sleep(Duration::from_millis(250));
}

fn observed(db: &Connection) -> Vec<(String, String)> {
    let mut stmt = db
        .prepare("SELECT project_id, commit_oid FROM commit_observation ORDER BY observed_ms")
        .expect("prepare");
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect()
}

fn register(daemon: &TestDaemon, repo: &Path) -> String {
    let response = daemon.caller().register_project(repo);
    assert_eq!(response.status, 200, "{response:?}");
    // The observer takes its baseline when it processes the watch message.
    settle();
    fixtures::git_id(repo, None)
}

fn commit(repo: &Path, message: &str) -> String {
    git(repo, &["commit", "-q", "--allow-empty", "-m", message]);
    git_output(repo, &["rev-parse", "HEAD"])
}

#[test]
fn a_git_commit_is_one_observation_and_the_message_never_reaches_sqlite() {
    let mut daemon = watching_daemon();
    let repo = fixtures::repo(&daemon.home().join("code").join("alpha"), None);
    let project = register(&daemon, &repo);

    let oid = commit(&repo, CANARY);
    let db = daemon.live_db();
    let rows = poll_until(Duration::from_secs(10), "the commit observation", || {
        let rows = observed(&db);
        (!rows.is_empty()).then_some(rows)
    });
    assert_eq!(rows, [(project, oid)]);
    drop(db);

    daemon.shutdown();
    for name in ["traction.db", "traction.db-wal"] {
        let path = daemon.data_dir().join(name);
        if let Ok(bytes) = std::fs::read(&path) {
            assert!(
                !bytes.windows(CANARY.len()).any(|w| w == CANARY.as_bytes()),
                "the commit message reached {name}"
            );
        }
    }
}

#[test]
fn branch_switches_pulls_resets_and_amends_are_not_commits() {
    let mut daemon = watching_daemon();
    let home = daemon.home();
    let code = home.join("code");

    // A remote, the watched clone, and a second clone that will push.
    let remote = code.join("remote.git");
    std::fs::create_dir_all(&remote).unwrap();
    git(&remote, &["init", "-q", "--bare", "-b", "main"]);
    let seed = fixtures::repo(&code.join("seed"), None);
    git(&seed, &["branch", "-M", "main"]);
    git(
        &seed,
        &["remote", "add", "origin", remote.to_str().unwrap()],
    );
    git(&seed, &["push", "-q", "origin", "main"]);
    let watched = code.join("watched");
    git(&code, &["clone", "-q", remote.to_str().unwrap(), "watched"]);
    let other = code.join("other");
    git(&code, &["clone", "-q", remote.to_str().unwrap(), "other"]);

    // A side branch with one extra commit, made before watching starts.
    git(&watched, &["checkout", "-q", "-b", "side"]);
    commit(&watched, "side work");
    git(&watched, &["checkout", "-q", "main"]);
    // The second clone pushes a new commit the watched clone will pull.
    commit(&other, "pushed from the other clone");
    git(&other, &["push", "-q", "origin", "main"]);

    register(&daemon, &watched);

    let step = |args: &[&str]| {
        git(&watched, args);
        settle();
    };
    step(&["checkout", "-q", "side"]);
    step(&["switch", "-q", "main"]);
    step(&["pull", "-q", "--ff-only"]);
    step(&["checkout", "-q", "side"]);
    step(&["reset", "-q", "--hard", "HEAD~1"]);
    step(&["switch", "-q", "main"]);
    step(&["commit", "-q", "--amend", "--allow-empty", "-m", "amended"]);

    // The observer is alive and counting: one real commit is exactly one row.
    let oid = commit(&watched, "a real new commit");
    let db = daemon.live_db();
    let rows = poll_until(Duration::from_secs(10), "the real commit", || {
        let rows = observed(&db);
        (!rows.is_empty()).then_some(rows)
    });
    settle();
    let rows_after = observed(&db);
    assert_eq!(rows_after.len(), 1, "only the real commit counts: {rows:?}");
    assert_eq!(rows_after[0].1, oid);
    drop(db);
    daemon.shutdown();
}

#[test]
fn a_commit_in_a_linked_worktree_is_observed_for_that_worktree_only() {
    let mut daemon = watching_daemon();
    let code = daemon.home().join("code");
    let main = fixtures::repo(&code.join("alpha"), None);
    let linked = fixtures::worktree(&main, &code.join("alpha-feature"), "feature");
    let project = register(&daemon, &main);

    // An event from inside the linked worktree makes the Router resolve it,
    // which is how the observer learns that worktree.
    let response = daemon
        .caller()
        .post_event(&routed_event(1, "TOOL_POST", "s1", Some(&linked)));
    assert_eq!(response.status, 202, "{response:?}");
    settle();

    let linked_oid = commit(&linked, "in the linked worktree");
    let db = daemon.live_db();
    poll_until(Duration::from_secs(10), "the linked commit", || {
        (!observed(&db).is_empty()).then_some(())
    });
    settle();
    assert_eq!(
        observed(&db),
        [(project.clone(), linked_oid)],
        "the main worktree must not double-count it"
    );

    let main_oid = commit(&main, "in the main worktree");
    let rows = poll_until(Duration::from_secs(10), "the main commit", || {
        let rows = observed(&db);
        (rows.len() >= 2).then_some(rows)
    });
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1], (project, main_oid));
    drop(db);
    daemon.shutdown();
}
