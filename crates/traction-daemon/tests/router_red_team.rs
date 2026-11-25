//! Seam 2 red team for the Project Router: routing events into a hostile
//! repo (and a planted bare repo) through the running daemon never runs
//! repo code. Also: no `git` at all, and a `git` that hangs, leave the
//! daemon healthy and the bus answering at once.

mod common;

use std::path::Path;
#[cfg(unix)]
use std::time::{Duration, Instant};

use common::fixtures::{self, git_id};
use common::{TestDaemon, routed_event, stored_project_id};

const EVIL_ORIGIN: &str = "https://example.invalid/evil/repo.git";

#[test]
fn control_the_hostile_fixture_runs_code_under_plain_git() {
    let tmp = tempfile::tempdir().unwrap();
    let sentinels = tmp.path().join("sentinels");
    let repo = fixtures::hostile_repo(&tmp.path().join("evil"), &sentinels);
    let _ = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("status")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    assert!(
        fixtures::sentinels_fired(&sentinels).contains(&"fsmonitor".to_owned()),
        "the fixture must be able to run code, or the red team proves nothing"
    );
}

#[test]
fn red_team_routing_into_a_hostile_repo_and_a_planted_bare_repo_runs_no_repo_code() {
    let mut daemon = TestDaemon::start();
    let home = daemon.home();
    let sentinels = home.join("sentinels");
    let evil = fixtures::hostile_repo(&home.join("code/evil"), &sentinels);
    let bare = fixtures::hostile_bare_repo(&evil.join("vendor/bare.git"), &sentinels);
    let evil_id = git_id(&evil, Some(EVIL_ORIGIN));

    let caller = daemon.caller();
    let cwds: [&Path; 5] = [
        &evil,
        &evil.join("src"),
        &bare,
        &bare.join("objects"),
        &evil.join(".git"),
    ];
    for (n, cwd) in (1..).zip(cwds) {
        let event_type = if n == 1 {
            "SESSION_START"
        } else {
            "CWD_CHANGE"
        };
        let response = caller.post_event(&routed_event(n, event_type, "s-evil", Some(cwd)));
        assert_eq!(response.status, 202, "{}", cwd.display());
    }
    for folder in [evil.join("src"), bare.clone()] {
        let response = caller.register_project(&folder);
        assert_eq!(response.status, 200, "{}", folder.display());
        assert_eq!(response.body, format!(r#"{{"project_id":"{evil_id}"}}"#));
    }

    let db = daemon.shutdown_and_open_db();
    for n in 1..=5 {
        // A `cwd` inside the bare repo is never treated as a Git repo: it
        // routes to the enclosing Project.
        assert_eq!(
            stored_project_id(&db, n),
            Some(evil_id.clone()),
            "event {n}"
        );
    }
    assert_eq!(fixtures::sentinels_fired(&sentinels), Vec::<String>::new());
    assert_eq!(
        fixtures::dev_null_paths(&home),
        Vec::<std::path::PathBuf>::new()
    );
    let cwd = std::env::current_dir().unwrap();
    assert!(
        !cwd.join("dev").join("null").exists(),
        "no dev/null in the test's working folder"
    );
}

#[test]
fn with_no_git_on_path_a_repo_folder_routes_as_a_dir_project() {
    let mut daemon = TestDaemon::start_with(|config| {
        config.git_program = "traction-test-no-such-git".into();
    });
    let repo = fixtures::repo(&daemon.home().join("code/app"), None);

    let caller = daemon.caller();
    let start = routed_event(1, "SESSION_START", "s", Some(&repo));
    assert_eq!(caller.post_event(&start).status, 202);
    let later = routed_event(2, "PROMPT_SUBMIT", "s", Some(&repo.join("src")));
    assert_eq!(caller.post_event(&later).status, 202);
    let registered = caller.register_project(&repo);

    let dir_id = daemon.dir_id(&repo);
    assert_eq!(registered.status, 200, "daemon stays healthy");
    assert_eq!(registered.body, format!(r#"{{"project_id":"{dir_id}"}}"#));
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_project_id(&db, 1), Some(dir_id.clone()));
    assert_eq!(stored_project_id(&db, 2), Some(dir_id));
}

/// A `git` that hangs past every bus deadline before failing.
#[cfg(unix)]
fn hanging_git(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("slow-git");
    std::fs::write(&script, "#!/bin/sh\nsleep 2\nexit 1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(unix)]
#[test]
fn a_hanging_git_never_delays_the_bus_response() {
    let scripts = tempfile::tempdir().unwrap();
    let slow = hanging_git(scripts.path());
    let mut daemon = TestDaemon::start_with(move |config| config.git_program = slow);
    let repo = fixtures::repo(&daemon.home().join("code/app"), None);

    let caller = daemon.caller();
    for n in 1..=3 {
        let started = Instant::now();
        let response = caller.post_event(&routed_event(n, "TOOL_PRE", "s", Some(&repo)));
        assert_eq!(response.status, 202);
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "event {n} waited {:?} for routing",
            started.elapsed()
        );
    }

    // Routing still finishes: Git failed, so the repo routes as `dir:`.
    let dir_id = daemon.dir_id(&repo);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_project_id(&db, 3), Some(dir_id));
}
