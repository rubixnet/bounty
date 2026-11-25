//! Seam 2: `POST /v1/project` registers the Project Root a path resolves
//! to, refuses folders that may never be Projects with `422`, and runs the
//! same `Host`, `Origin` and token checks as `/v1/event`.

mod common;

use std::path::PathBuf;

use common::fixtures::{self, git_id};
use common::{TestDaemon, canonical_string, path_str, registered_projects};
use traction_daemon::GLOBAL_IGNORE_FILE;

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
fn a_repo_subfolder_registers_the_repo_root() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("code/app"), None);
    let expected = git_id(&repo, None);

    let response = daemon.caller().register_project(&repo.join("src"));

    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, format!(r#"{{"project_id":"{expected}"}}"#));
    // Registering again (or the root itself) answers the same Project.
    let again = daemon.caller().register_project(&repo);
    assert_eq!((again.status, again.body), (200, response.body));

    let db = daemon.shutdown_and_open_db();
    let projects = registered_projects(&db);
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].project_id, expected);
    assert_eq!(projects[0].root_path, canonical_string(&repo));
    assert_eq!(projects[0].kind, "git");
}

#[test]
fn an_unmarked_folder_registers_as_a_dir_project() {
    let mut daemon = TestDaemon::start();
    let sketches = fixtures::folder(&daemon.home().join("sketches"), &[]);

    let response = daemon.caller().register_project(&sketches);

    let expected = daemon.dir_id(&sketches);
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, format!(r#"{{"project_id":"{expected}"}}"#));
    let db = daemon.shutdown_and_open_db();
    assert_eq!(registered_projects(&db)[0].project_id, expected);
}

#[test]
fn folders_that_may_never_be_projects_get_422() {
    let home_dir = tempfile::tempdir().unwrap();
    let home = common::canonical(home_dir.path());
    let web = fixtures::folder(&home.join("code/web"), &["package.json"]);
    let modules = fixtures::folder(&web.join("node_modules"), &[]);
    let client = fixtures::folder(&home.join("client-work"), &["package.json"]);
    let file = web.join("package.json");
    let home_for_config = home.clone();
    let mut daemon = TestDaemon::start_with(move |config| {
        config.home_dir = Some(home_for_config);
        std::fs::create_dir_all(&config.data_dir).unwrap();
        std::fs::write(config.data_dir.join(GLOBAL_IGNORE_FILE), "~/client-work\n").unwrap();
    });

    let caller = daemon.caller();
    let cases = [
        (home.clone(), "excluded_path"),
        (os_folder(), "excluded_path"),
        (modules, "excluded_path"),
        (client, "user_excluded_path"),
        (home.join("missing"), "path_not_found"),
        (file, "not_a_folder"),
    ];
    for (path, code) in cases {
        let response = caller.register_project(&path);
        assert_eq!(response.status, 422, "{}", path.display());
        assert_eq!(
            response.body,
            format!(r#"{{"error":"{code}"}}"#),
            "{}",
            path.display()
        );
    }

    let db = daemon.shutdown_and_open_db();
    assert!(registered_projects(&db).is_empty());
}

#[test]
fn a_malformed_body_or_relative_path_gets_400() {
    let daemon = TestDaemon::start();
    let caller = daemon.caller();
    let cases = [
        ("not json".to_owned(), "malformed_registration"),
        (r#"{"path":1}"#.to_owned(), "malformed_registration"),
        (
            serde_json::json!({ "path": path_str(&daemon.home()), "extra": true }).to_string(),
            "malformed_registration",
        ),
        (r#"{"path":"relative/folder"}"#.to_owned(), "invalid_path"),
    ];
    for (body, code) in cases {
        let response = caller.request("POST", "/v1/project").body(&body).send();
        assert_eq!(response.status, 400, "{body}");
        assert_eq!(response.body, format!(r#"{{"error":"{code}"}}"#), "{body}");
    }
}

#[test]
fn auth_host_and_origin_failures_match_the_event_endpoint() {
    let mut daemon = TestDaemon::start();
    let folder = fixtures::folder(&daemon.home().join("app"), &["package.json"]);
    let caller = daemon.caller();
    let body = serde_json::json!({ "path": path_str(&folder) }).to_string();
    let port = caller.port();

    type Tweak = fn(common::BusRequest, u16) -> common::BusRequest;
    let tweaks: [(&str, Tweak); 4] = [
        ("bad token", |r, _| {
            r.header("Authorization", "Bearer wrong")
        }),
        ("no token", |r, _| r.without_header("Authorization")),
        ("foreign Host", |r, _| {
            r.header("Host", "evil.example:41937")
        }),
        ("foreign Origin", |r, port| {
            r.header("Origin", &format!("http://evil.example:{port}"))
        }),
    ];
    for (what, tweak) in tweaks {
        let event = tweak(caller.request("POST", "/v1/event"), port)
            .body(common::valid_event(1))
            .send();
        let project = tweak(caller.request("POST", "/v1/project"), port)
            .body(&body)
            .send();
        assert!(event.status == 401 || event.status == 403, "{what}");
        assert_eq!(
            (project.status, project.body),
            (event.status, event.body),
            "{what}"
        );
    }
    let get = caller.request("GET", "/v1/project").send();
    assert_eq!(get.status, 405);

    let db = daemon.shutdown_and_open_db();
    assert!(registered_projects(&db).is_empty());
    assert_eq!(common::stored_event_count(&db), 0);
}

#[cfg(unix)]
#[test]
fn the_unix_socket_registers_too() {
    let mut daemon = TestDaemon::start();
    let folder = fixtures::folder(&daemon.home().join("app"), &["Cargo.toml"]);

    let response = daemon.socket_caller().register_project(&folder);

    assert_eq!(response.status, 200, "{}", response.body);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(registered_projects(&db).len(), 1);
}
