//! Seam 2: the Project Router. Events sent with only a `cwd` are stored
//! under the right Project ID, worktrees collapse into their repo, the
//! registry holds exactly the Projects seen, and the user's Git is never
//! disturbed (no `.git/index.lock`, `.git/index` untouched).

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime};

use common::fixtures::{self, git_id};
use common::{
    TestDaemon, canonical_string, event_id, registered_projects, routed_event, stored_project_id,
    stored_project_ids, valid_event_value,
};

const ORIGIN: &str = "git@github.com:Acme/Alpha.git";

/// `.git/index` bytes and mtime, to prove routing never rewrote it.
fn index_snapshot(git_dir: &Path) -> (Vec<u8>, SystemTime) {
    let index = git_dir.join("index");
    let bytes = std::fs::read(&index).expect("read .git/index");
    let mtime = std::fs::metadata(&index)
        .and_then(|m| m.modified())
        .expect("index mtime");
    (bytes, mtime)
}

/// Polls every given Git dir for `index.lock` until stopped. Returns a
/// handle whose join yields the lock paths ever seen.
fn watch_for_index_locks(
    git_dirs: Vec<PathBuf>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<Vec<PathBuf>> {
    std::thread::spawn(move || {
        let mut seen = Vec::new();
        while !stop.load(Ordering::Acquire) {
            for dir in &git_dirs {
                let lock = dir.join("index.lock");
                if lock.exists() && !seen.contains(&lock) {
                    seen.push(lock);
                }
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        seen
    })
}

#[test]
fn routing_gate_three_repos_a_worktree_and_two_folders_route_to_five_projects() {
    let mut daemon = TestDaemon::start();
    let home = daemon.home();
    let code = home.join("code");
    let alpha = fixtures::repo(&code.join("alpha"), Some(ORIGIN));
    let beta = fixtures::repo(&code.join("beta"), None);
    let gamma = fixtures::repo(&code.join("gamma"), None);
    let alpha_wt = fixtures::worktree(&alpha, &code.join("alpha-feature"), "feature");
    let web = fixtures::folder(&code.join("web"), &["package.json"]);
    std::fs::create_dir_all(web.join("src/components")).unwrap();
    let notes = fixtures::folder(&home.join("notes"), &[]);
    std::fs::create_dir_all(notes.join("drafts")).unwrap();

    let git_dirs = vec![
        alpha.join(".git"),
        beta.join(".git"),
        gamma.join(".git"),
        alpha.join(".git/worktrees/alpha-feature"),
    ];
    let before: Vec<_> = git_dirs[..3].iter().map(|d| index_snapshot(d)).collect();
    let stop = Arc::new(AtomicBool::new(false));
    let watcher = watch_for_index_locks(git_dirs.clone(), Arc::clone(&stop));

    let alpha_id = git_id(&alpha, Some(ORIGIN));
    let beta_id = git_id(&beta, None);
    let gamma_id = git_id(&gamma, None);
    let web_id = daemon.dir_id(&web);
    let notes_id = daemon.dir_id(&notes);

    // (n, type, session, cwd, expected project_id)
    let notes_drafts = notes.join("drafts");
    let alpha_src = alpha.join("src");
    let alpha_wt_src = alpha_wt.join("src");
    let beta_src = beta.join("src");
    let web_components = web.join("src/components");
    let plan: Vec<(u32, &str, &str, Option<&Path>, &str)> = vec![
        (1, "SESSION_START", "s-alpha", Some(&alpha), &alpha_id),
        (2, "SESSION_START", "s-notes", Some(&notes), &notes_id),
        (3, "PROMPT_SUBMIT", "s-alpha", Some(&alpha_src), &alpha_id),
        (4, "TOOL_PRE", "s-wt", Some(&alpha_wt_src), &alpha_id),
        (
            5,
            "PROMPT_SUBMIT",
            "s-notes",
            Some(&notes_drafts),
            &notes_id,
        ),
        (6, "TOOL_PRE", "s-beta", Some(&beta_src), &beta_id),
        (7, "CWD_CHANGE", "s-alpha", Some(&gamma), &gamma_id),
        // No `cwd`: the session's last `cwd` (gamma, after CWD_CHANGE).
        (8, "TOOL_POST", "s-alpha", None, &gamma_id),
        (9, "TOOL_PRE", "s-web", Some(&web_components), &web_id),
        (10, "TOOL_POST", "s-wt", Some(&alpha_wt), &alpha_id),
        (11, "TURN_END", "s-beta", Some(&beta), &beta_id),
        (12, "TOOL_POST", "s-web", Some(&web), &web_id),
        (13, "TURN_END", "s-notes", None, &notes_id),
    ];
    let caller = daemon.caller();
    for (n, event_type, session, cwd, _) in &plan {
        let response = caller.post_event(&routed_event(*n, event_type, session, *cwd));
        assert_eq!(response.status, 202, "event {n}: {}", response.body);
    }

    let db = daemon.shutdown_and_open_db();
    stop.store(true, Ordering::Release);
    assert_eq!(watcher.join().unwrap(), Vec::<PathBuf>::new());
    for (dir, before) in git_dirs[..3].iter().zip(before) {
        assert_eq!(index_snapshot(dir), before, "{} changed", dir.display());
    }

    let expected: Vec<_> = plan
        .iter()
        .map(|(n, .., id)| (event_id(*n), Some((*id).to_owned())))
        .collect();
    assert_eq!(stored_project_ids(&db), expected);

    let projects = registered_projects(&db);
    let mut roots: Vec<_> = projects
        .iter()
        .map(|p| (p.root_path.clone(), p.kind.clone(), p.project_id.clone()))
        .collect();
    roots.sort();
    let mut want = vec![
        (canonical_string(&alpha), "git".to_owned(), alpha_id.clone()),
        (canonical_string(&beta), "git".to_owned(), beta_id),
        (canonical_string(&gamma), "git".to_owned(), gamma_id),
        (canonical_string(&web), "dir".to_owned(), web_id),
        (canonical_string(&notes), "dir".to_owned(), notes_id),
    ];
    want.sort();
    assert_eq!(roots, want, "exactly 5 Projects");

    let alpha_row = projects.iter().find(|p| p.project_id == alpha_id).unwrap();
    assert_eq!(alpha_row.name, "alpha");
    assert_eq!(
        alpha_row.home_relative_path.as_deref(),
        Some("~/code/alpha")
    );
    assert_eq!(
        alpha_row.git_common_dir.as_deref(),
        Some(canonical_string(&alpha.join(".git")).as_str()),
        "git: ID stored against the canonical common dir"
    );
    let notes_row = projects
        .iter()
        .find(|p| p.kind == "dir" && p.name == "notes");
    assert_eq!(notes_row.unwrap().git_common_dir, None);
}

#[test]
fn an_adapter_supplied_project_id_is_replaced_by_the_router() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("code/app"), None);
    let mut event = valid_event_value(1);
    event["cwd"] = serde_json::json!(common::path_str(&repo.join("src")));
    // A well-formed but wrong ID.
    event["project_id"] = serde_json::json!(format!("dir:{}", "ab".repeat(32)));

    assert_eq!(daemon.caller().post_event(&event.to_string()).status, 202);

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_project_id(&db, 1), Some(git_id(&repo, None)));
}

#[test]
fn an_event_whose_cwd_matches_no_project_is_stored_without_project_id() {
    let mut daemon = TestDaemon::start();
    let missing = daemon.home().join("does-not-exist");

    let caller = daemon.caller();
    let first = routed_event(1, "SESSION_START", "s", Some(&missing));
    assert_eq!(caller.post_event(&first).status, 202);
    let no_cwd = routed_event(2, "PROMPT_SUBMIT", "other-session", None);
    assert_eq!(caller.post_event(&no_cwd).status, 202);

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_project_id(&db, 1), None);
    assert_eq!(stored_project_id(&db, 2), None);
    assert!(registered_projects(&db).is_empty());
}

#[test]
fn a_session_start_folder_survives_a_restart() {
    let daemon = TestDaemon::start();
    let notes = fixtures::folder(&daemon.home().join("notes"), &[]);
    let start = routed_event(1, "SESSION_START", "s-notes", Some(&notes));
    assert_eq!(daemon.caller().post_event(&start).status, 202);

    let mut daemon = daemon.restart();
    let later = routed_event(2, "PROMPT_SUBMIT", "s-notes", None);
    assert_eq!(daemon.caller().post_event(&later).status, 202);

    let notes_id = daemon.dir_id(&notes);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_project_id(&db, 2), Some(notes_id));
    let (start_folder, last_cwd): (Option<String>, Option<String>) = db
        .query_row(
            "SELECT start_folder, last_cwd FROM session WHERE session_id = 's-notes'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("session row");
    let notes_str = common::path_str(&notes).to_owned();
    assert_eq!(start_folder.as_deref(), Some(notes_str.as_str()));
    assert_eq!(last_cwd.as_deref(), Some(notes_str.as_str()));
}
