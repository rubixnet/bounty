//! Seam 2: the file watcher. Hand edits in a known Project become manual
//! `FILE_MUTATION` rows (metadata only), ignored paths never do, bursts
//! coalesce, watchers re-attach from SQLite on startup, nothing that changed
//! while the daemon was down is reported, and shutdown flushes what is held.
//!
//! Every wait polls with a generous deadline. A negative ("no row") claim is
//! always followed by a positive control edit: rows are decided in the order
//! their edits were observed, so once the control's row is committed, any
//! row an earlier ignored edit could have produced would be committed too.

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::{
    MutationRow, TestDaemon, canonical_string, fast_watcher, fixtures, kinds, routed_event,
    stored_mutations,
};

/// Asserts `row` is manual work in the Project `project_id` rooted at `root`.
fn assert_manual(row: &MutationRow, project_id: &str, root: &Path) {
    assert_eq!(row.project_id.as_deref(), Some(project_id), "{row:?}");
    assert_eq!(row.cwd.as_deref(), Some(canonical_string(root).as_str()));
    assert_eq!(row.harness_id, None);
    assert_eq!(row.session_id, None);
    assert_eq!(row.network_class, "manual");
    assert_eq!(row.attribution_source, "file_watcher");
}

fn register(daemon: &TestDaemon, root: &Path) -> String {
    let response = daemon.caller().register_project(root);
    assert_eq!(response.status, 200, "{}", response.body);
    serde_json::from_str::<serde_json::Value>(&response.body).unwrap()["project_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[test]
fn hand_edits_in_a_registered_project_are_manual_mutations() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let root = fixtures::folder(&daemon.home().join("sketches"), &[]);
    fs::write(root.join("notes.txt"), "draft").unwrap();
    fs::write(root.join("old.txt"), "old").unwrap();
    let project_id = register(&daemon, &root);

    fs::write(root.join("new.txt"), "fresh").unwrap();
    let rows = daemon.wait_for_mutations(1);
    assert_eq!(rows[0].mutation_kind.as_deref(), Some("created"));

    fs::write(root.join("notes.txt"), "draft, revised").unwrap();
    let rows = daemon.wait_for_mutations(2);
    assert_eq!(rows[1].mutation_kind.as_deref(), Some("modified"));

    fs::remove_file(root.join("old.txt")).unwrap();
    let rows = daemon.wait_for_mutations(3);
    assert_eq!(rows[2].mutation_kind.as_deref(), Some("removed"));

    for row in &rows {
        assert_manual(row, &project_id, &root);
    }
}

#[test]
fn ignored_dependency_and_git_paths_produce_no_rows() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let repo = fixtures::repo(&daemon.home().join("app"), None);
    fs::write(repo.join(".gitignore"), "build/\n*.log\n").unwrap();
    fs::create_dir_all(repo.join("src/private")).unwrap();
    fs::write(repo.join("src/.tractionignore"), "private/\nsecret.txt\n").unwrap();
    fs::create_dir_all(repo.join("node_modules/left-pad")).unwrap();
    fs::create_dir_all(repo.join("build")).unwrap();
    let project_id = register(&daemon, &repo);

    fs::write(repo.join("build/out.bin"), "artifact").unwrap();
    fs::write(repo.join("debug.log"), "log line").unwrap();
    fs::write(repo.join("src/secret.txt"), "hidden").unwrap();
    fs::write(repo.join("src/private/key.txt"), "hidden").unwrap();
    fs::write(repo.join("node_modules/left-pad/index.js"), "x").unwrap();
    fs::create_dir_all(repo.join("node_modules/new-dep")).unwrap();
    fs::write(repo.join("node_modules/new-dep/index.js"), "x").unwrap();
    fs::write(repo.join(".git/scratch"), "x").unwrap();
    fs::write(repo.join("src/main.txt"), "edited by hand").unwrap();
    // Control: a plain new file, observed after every ignored edit.
    fs::write(repo.join("src/control.txt"), "control").unwrap();

    let rows = daemon.wait_for_mutations(2);
    assert_eq!(kinds(&rows), ["created", "modified"]);
    for row in &rows {
        assert_manual(row, &project_id, &repo);
    }
}

#[test]
fn a_burst_of_saves_is_one_row_and_created_then_removed_is_none() {
    let daemon = TestDaemon::start_with(|config| {
        fast_watcher(config);
        // Long enough that the whole burst lands inside one hold.
        config.watcher.hold = Duration::from_secs(2);
    });
    let root = fixtures::folder(&daemon.home().join("essay"), &[]);
    fs::write(root.join("draft.md"), "v0").unwrap();
    register(&daemon, &root);

    for n in 1..=20 {
        fs::write(root.join("draft.md"), format!("v{n}")).unwrap();
    }
    fs::write(root.join("scratch.tmp"), "short-lived").unwrap();
    fs::remove_file(root.join("scratch.tmp")).unwrap();
    let rows = daemon.wait_for_mutations(1);
    assert_eq!(kinds(&rows), ["modified"]);

    fs::write(root.join("control.md"), "control").unwrap();
    let rows = daemon.wait_for_mutations(2);
    assert_eq!(kinds(&rows), ["created", "modified"]);
}

#[test]
fn watchers_reattach_on_restart_and_downtime_edits_are_never_reported() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let home = daemon.home();
    // One Project registered by a routed Harness event, one by the endpoint,
    // and one whose root is deleted while the daemon is down.
    let routed = fixtures::folder(&home.join("web"), &["package.json"]);
    let endpoint = fixtures::folder(&home.join("notes"), &[]);
    let doomed = fixtures::folder(&home.join("doomed"), &[]);
    fs::write(routed.join("downtime.txt"), "before").unwrap();
    fs::write(endpoint.join("downtime.txt"), "before").unwrap();
    let event = routed_event(1, "PROMPT_SUBMIT", "s-1", Some(&routed));
    assert_eq!(daemon.caller().post_event(&event).status, 202);
    let endpoint_id = register(&daemon, &endpoint);
    register(&daemon, &doomed);
    let routed_id = daemon.dir_id(&routed);

    let mut stopped = daemon;
    stopped.shutdown();
    fs::write(routed.join("downtime.txt"), "edited while down").unwrap();
    fs::write(endpoint.join("downtime.txt"), "edited while down").unwrap();
    fs::write(endpoint.join("made-while-down.txt"), "x").unwrap();
    fs::remove_dir_all(&doomed).unwrap();
    let daemon = stopped.restart_with(fast_watcher);

    fs::write(routed.join("after.txt"), "after restart").unwrap();
    fs::write(endpoint.join("after.txt"), "after restart").unwrap();
    let rows = daemon.wait_for_mutations(2);
    assert_eq!(kinds(&rows), ["created", "created"]);
    let mut projects: Vec<&str> = rows
        .iter()
        .map(|row| row.project_id.as_deref().unwrap())
        .collect();
    projects.sort_unstable();
    let mut expected = [routed_id.as_str(), endpoint_id.as_str()];
    expected.sort_unstable();
    assert_eq!(projects, expected);
}

#[test]
fn shutdown_flushes_held_mutations_and_the_pending_cap_decides_early() {
    let mut daemon = TestDaemon::start_with(|config| {
        fast_watcher(config);
        // Far longer than the test: only the cap or shutdown can decide.
        config.watcher.hold = Duration::from_secs(600);
        config.watcher.pending_cap = 1;
    });
    // `inner` is a Project nested in `outer`, so every backend reports both
    // through `outer`'s watch, in edit order.
    let outer = fixtures::folder(&daemon.home().join("journal"), &[]);
    let inner = fixtures::folder(&outer.join("drafts"), &["package.json"]);
    let outer_id = register(&daemon, &outer);
    let inner_id = register(&daemon, &inner);

    fs::write(outer.join("first.txt"), "1").unwrap();
    fs::write(inner.join("held.txt"), "held").unwrap();
    fs::write(outer.join("second.txt"), "2").unwrap();
    // The second file's observation pushed the first out of `outer`'s full
    // cap, so `held.txt` (observed in between) is held by now.
    let db = daemon.live_db();
    common::poll_until(common::WATCH_DEADLINE, "an early decision", || {
        let rows = stored_mutations(&db);
        assert!(
            rows.iter()
                .all(|r| r.project_id.as_deref() == Some(&outer_id)),
            "{rows:?}"
        );
        (!rows.is_empty()).then_some(())
    });
    drop(db);

    let db = daemon.shutdown_and_open_db();
    let rows = stored_mutations(&db);
    let flushed: Vec<&MutationRow> = rows
        .iter()
        .filter(|r| r.project_id.as_deref() == Some(&inner_id))
        .collect();
    assert_eq!(flushed.len(), 1, "{rows:?}");
    assert_manual(flushed[0], &inner_id, &inner);
    assert_eq!(flushed[0].mutation_kind.as_deref(), Some("created"));
    for row in rows
        .iter()
        .filter(|r| r.project_id.as_deref() == Some(&outer_id))
    {
        assert_manual(row, &outer_id, &outer);
    }
}

#[test]
fn after_migration_hand_edits_land_under_the_git_project() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let notes = fixtures::folder(&daemon.home().join("notes"), &[]);
    let dir_id = register(&daemon, &notes);
    fixtures::repo(&notes, None);
    let git_id = fixtures::git_id(&notes, None);
    // An event in the new repo triggers the migration check.
    let event = routed_event(1, "PROMPT_SUBMIT", "s-1", Some(&notes));
    assert_eq!(daemon.caller().post_event(&event).status, 202);
    assert_eq!(register(&daemon, &notes), git_id);
    let db = daemon.live_db();
    // The fixture's own `git init`/commit writes may be decided under either
    // ID, so this test asserts only on a file created after the migration.
    // Let those fixture writes settle (several holds) before the baseline.
    std::thread::sleep(Duration::from_secs(3));
    let created_under_git = |db: &rusqlite::Connection| {
        stored_mutations(db)
            .iter()
            .filter(|row| {
                row.project_id.as_deref() == Some(git_id.as_str())
                    && row.mutation_kind.as_deref() == Some("created")
            })
            .count()
    };
    let baseline = created_under_git(&db);
    fs::write(notes.join("after.txt"), "post-migration").unwrap();
    common::poll_until(common::WATCH_DEADLINE, "the post-migration create", || {
        (created_under_git(&db) > baseline).then_some(())
    });

    // The only removal: decided after the migration.
    fs::remove_file(notes.join("after.txt")).unwrap();
    let removed = common::poll_until(common::WATCH_DEADLINE, "the post-migration edit", || {
        stored_mutations(&db)
            .into_iter()
            .find(|row| row.mutation_kind.as_deref() == Some("removed"))
    });
    assert_manual(&removed, &git_id, &notes);
    assert_ne!(git_id, dir_id);
}

#[test]
fn an_edit_made_right_before_the_migration_is_still_recorded() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let notes = fixtures::folder(&daemon.home().join("handover"), &[]);
    register(&daemon, &notes);
    fixtures::repo(&notes, None);
    let git_id = fixtures::git_id(&notes, None);
    let db = daemon.live_db();
    // Let the fixture's own writes be decided (under the `dir:` ID).
    std::thread::sleep(Duration::from_secs(3));
    let created_under_git = |db: &rusqlite::Connection| {
        stored_mutations(db)
            .iter()
            .filter(|row| {
                row.project_id.as_deref() == Some(git_id.as_str())
                    && row.mutation_kind.as_deref() == Some("created")
            })
            .count()
    };
    let baseline = created_under_git(&db);

    // The edit's notification is still in flight (or held) when the event
    // that migrates the Project arrives: the watch is handed over in place,
    // so the edit is neither lost nor dropped as predating a new watch.
    fs::write(notes.join("in-flight.txt"), "edited at the hand-over").unwrap();
    let event = routed_event(1, "PROMPT_SUBMIT", "s-1", Some(&notes));
    assert_eq!(daemon.caller().post_event(&event).status, 202);

    common::poll_until(common::WATCH_DEADLINE, "the hand-over edit", || {
        (created_under_git(&db) > baseline).then_some(())
    });
}

/// Valid in a file name on every OS and never part of any fixture path.
const SENTINEL: &str = "TRACTION-SENTINEL-c0ffee-watched-file";

#[test]
fn a_sentinel_file_name_and_contents_never_reach_disk() {
    let mut daemon = TestDaemon::start_with(fast_watcher);
    let root = fixtures::folder(&daemon.home().join("vault"), &[]);
    register(&daemon, &root);

    fs::write(
        root.join(format!("{SENTINEL}.env")),
        format!("API_KEY={SENTINEL}\n"),
    )
    .unwrap();
    daemon.wait_for_mutations(1);

    let db = daemon.shutdown_and_open_db();
    assert_eq!(common::sentinel_hits_in_tables(&db, SENTINEL), 0);
    drop(db);
    let scanned = common::assert_no_file_contains(daemon.data_dir(), SENTINEL);
    assert!(scanned.iter().any(|name| name == traction_daemon::DB_FILE));
}

#[test]
fn the_hold_defaults_to_log_tail_hold() {
    assert_eq!(traction_daemon::LOG_TAIL_HOLD, Duration::from_secs(5));
    let config = traction_daemon::DaemonConfig::new("unused".into());
    assert_eq!(config.watcher.hold, traction_daemon::LOG_TAIL_HOLD);
}
