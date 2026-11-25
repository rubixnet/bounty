//! Seam 2: folders that never become Projects. `$HOME`, OS folders and
//! dependency folders are never registered, a dotfile repo at `$HOME`
//! captures nothing, and a folder in the global ignore file drops events.

mod common;

use std::path::PathBuf;

use common::fixtures;
use common::{
    TestDaemon, path_str, registered_projects, routed_event, stored_event_count, stored_project_id,
};
use traction_daemon::GLOBAL_IGNORE_FILE;

/// A folder from this OS's OS-folder list that exists on every runner.
fn os_folder() -> PathBuf {
    #[cfg(windows)]
    {
        PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
    }
    #[cfg(not(windows))]
    {
        PathBuf::from("/usr")
    }
}

#[test]
fn home_and_os_folders_get_no_project_and_node_modules_routes_to_its_project() {
    let mut daemon = TestDaemon::start();
    let home = daemon.home();
    let web = fixtures::folder(&home.join("code/web"), &["package.json"]);
    let pkg = fixtures::folder(&web.join("node_modules/left-pad"), &["package.json"]);
    let os = os_folder();

    let caller = daemon.caller();
    // Each session starts where its event is, so the start-folder fallback
    // has nothing else to offer.
    let events = [
        routed_event(1, "SESSION_START", "s-home", Some(&home)),
        routed_event(2, "SESSION_START", "s-os", Some(&os)),
        routed_event(3, "SESSION_START", "s-pkg", Some(&pkg)),
        routed_event(4, "PROMPT_SUBMIT", "s-home", None),
    ];
    for event in &events {
        assert_eq!(caller.post_event(event).status, 202);
    }

    let web_id = daemon.dir_id(&web);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_project_id(&db, 1), None, "$HOME");
    assert_eq!(stored_project_id(&db, 2), None, "OS folder");
    assert_eq!(
        stored_project_id(&db, 3),
        Some(web_id.clone()),
        "node_modules"
    );
    assert_eq!(stored_project_id(&db, 4), None, "$HOME via last cwd");
    let ids: Vec<_> = registered_projects(&db)
        .into_iter()
        .map(|p| p.project_id)
        .collect();
    assert_eq!(ids, vec![web_id], "only the enclosing Project");
}

#[test]
fn a_dotfile_repo_at_home_does_not_capture_an_unmarked_subfolder() {
    let mut daemon = TestDaemon::start();
    let home = daemon.home();
    fixtures::git(&home, &["init", "-q"]);
    std::fs::write(home.join(".bashrc"), "# dotfiles\n").unwrap();
    fixtures::git(&home, &["add", ".bashrc"]);
    fixtures::git(&home, &["commit", "-q", "-m", "dotfiles"]);
    let notes = fixtures::folder(&home.join("notes"), &[]);

    let caller = daemon.caller();
    assert_eq!(
        caller
            .post_event(&routed_event(1, "SESSION_START", "s", Some(&home)))
            .status,
        202
    );
    assert_eq!(
        caller
            .post_event(&routed_event(2, "CWD_CHANGE", "s", Some(&notes)))
            .status,
        202
    );

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_project_id(&db, 1), None);
    assert_eq!(stored_project_id(&db, 2), None);
    assert!(registered_projects(&db).is_empty());
}

#[test]
fn a_folder_in_the_global_ignore_file_gets_202_and_nothing_is_stored() {
    let home = tempfile::tempdir().unwrap();
    let home = common::canonical(home.path());
    let secret = fixtures::folder(&home.join("secret-client"), &["package.json"]);
    let private = fixtures::repo(&home.join("private/diary"), None);
    let kept = fixtures::folder(&home.join("kept"), &["package.json"]);

    let ignore = format!(
        "# folders Traction never tracks\n\n~/secret-client\n{}\nnot/absolute\n",
        path_str(&home.join("private"))
    );
    let home_for_config = home.clone();
    let mut daemon = TestDaemon::start_with(move |config| {
        config.home_dir = Some(home_for_config);
        std::fs::create_dir_all(&config.data_dir).unwrap();
        std::fs::write(config.data_dir.join(GLOBAL_IGNORE_FILE), ignore).unwrap();
    });

    let caller = daemon.caller();
    let events = [
        routed_event(1, "SESSION_START", "s1", Some(&secret.join("."))),
        routed_event(2, "PROMPT_SUBMIT", "s1", None),
        routed_event(3, "SESSION_START", "s2", Some(&private.join("src"))),
        routed_event(4, "SESSION_START", "s3", Some(&kept)),
    ];
    for event in &events {
        assert_eq!(caller.post_event(event).status, 202);
    }

    let kept_id = daemon.dir_id(&kept);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 1, "only the kept folder's event");
    assert_eq!(stored_project_id(&db, 4), Some(kept_id.clone()));
    let ids: Vec<_> = registered_projects(&db)
        .into_iter()
        .map(|p| p.project_id)
        .collect();
    assert_eq!(ids, vec![kept_id]);
}
