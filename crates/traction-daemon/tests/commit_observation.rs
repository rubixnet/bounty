//! Seam: Commit Observations through a real daemon watching real Git repos
//! (real `git`, real worktrees). A commit is recorded only for a new local
//! commit; checkout, switch, a fast-forward pull, a reset and an amend are
//! not commits. The commit message is never read.

mod common;

use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::time::Duration;

use common::fixtures::{self, git, git_output};
use common::{TestDaemon, poll_until, routed_event};
use rusqlite::Connection;
use traction_daemon::DaemonConfig;

const CANARY: &str = "CANARY-commit-message-7f3a91c2";

fn watching_daemon() -> TestDaemon {
    watching_daemon_with(|_| {})
}

fn watching_daemon_with(adjust: impl FnOnce(&mut DaemonConfig)) -> TestDaemon {
    TestDaemon::start_with(|config| {
        config.presence.enabled = false;
        config.commits.poll = Duration::from_millis(25);
        config.writer.commit_interval = Duration::from_millis(25);
        adjust(config);
    })
}

/// Spaces two Git operations by more than a poll so the observer sees them
/// as separate transitions. It proves nothing by itself: no assertion may
/// rely on "nothing happened during the wait"; negative claims are checked
/// after a later, known commit has been observed (the observer handles a
/// worktree's transitions in order).
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
    daemon.await_commit_baselines(1);
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
    // The real commit comes after every operation above and the observer
    // handles them in order, so anything wrongly counted is already stored.
    assert_eq!(rows.len(), 1, "only the real commit counts: {rows:?}");
    assert_eq!(rows[0].1, oid);
    drop(db);
    daemon.shutdown();
}

#[test]
fn a_commit_in_a_linked_worktree_is_observed_for_that_worktree_only() {
    linked_worktree_scenario(watching_daemon());
}

/// Guards the Windows failure on every Unix: where starting `git` is slow,
/// discovering a worktree outlasts any fixed sleep.
#[cfg(unix)]
#[test]
fn a_linked_worktree_commit_is_observed_when_git_is_slow() {
    let bin = tempfile::tempdir().expect("temp bin folder");
    let slow_git = slow_git(bin.path());
    linked_worktree_scenario(watching_daemon_with(|config| {
        config.git_program = slow_git;
    }));
}

/// A `git` that waits before running the real one.
#[cfg(unix)]
fn slow_git(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join("git");
    std::fs::write(&path, "#!/bin/sh\nsleep 0.08\nexec git \"$@\"\n").expect("write script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    // Another test thread's fork can briefly hold the script open for
    // writing (ETXTBSY); once it has run, that can no longer happen.
    poll_until(Duration::from_secs(10), "the slow git script", || {
        std::process::Command::new(&path)
            .arg("--version")
            .output()
            .ok()
    });
    path
}

fn linked_worktree_scenario(mut daemon: TestDaemon) {
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
    // `202` only means queued: wait for the second worktree's baseline.
    daemon.await_commit_baselines(2);

    let linked_oid = commit(&linked, "in the linked worktree");
    let db = daemon.live_db();
    poll_until(Duration::from_secs(10), "the linked commit", || {
        (!observed(&db).is_empty()).then_some(())
    });

    // Later known commits in the main worktree: once they are observed, a
    // double count of the linked commit would already be stored too.
    let first_main = commit(&main, "in the main worktree");
    poll_until(Duration::from_secs(10), "the first main commit", || {
        (observed(&db).len() >= 2).then_some(())
    });
    let second_main = commit(&main, "and again");
    let rows = poll_until(Duration::from_secs(10), "the main commits", || {
        let rows = observed(&db);
        (rows.len() >= 3).then_some(rows)
    });
    assert_eq!(
        rows,
        [
            (project.clone(), linked_oid),
            (project.clone(), first_main),
            (project, second_main)
        ],
        "the main worktree must not double-count the linked commit"
    );
    drop(db);
    daemon.shutdown();
}
