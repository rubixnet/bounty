//! Seam: `repo_symbol_hits` through a real daemon. The per-Project file-name
//! index is built from the file watcher's view, a hook prompt is matched
//! against it in memory, and neither the prompt text nor a file name ever
//! reaches SQLite or any other file in the data folder.

mod common;

use std::fs;
use std::path::Path;

use common::{TestDaemon, assert_no_file_contains, fixtures, path_str, sentinel_hits_in_tables};
use serde_json::json;

const HOOK_PATH: &str = "/v1/hooks/claude-code";
const FILE_NAME: &str = "quasarfoxtrot_widget.rs";
const IGNORED_NAME: &str = "zebracanary_hidden.rs";
const PROMPT_CANARY: &str = "canaryprompt_zq91_wording";

fn prompt(daemon: &TestDaemon, cwd: &Path, text: &str) {
    let body = json!({
        "session_id": "sym-session",
        "hook_event_name": "UserPromptSubmit",
        "cwd": path_str(cwd),
        "prompt": text,
    });
    let response = daemon
        .caller()
        .request("POST", HOOK_PATH)
        .body(body.to_string())
        .send();
    assert_eq!(response.status, 200, "{}", response.body);
}

fn hits(db: &rusqlite::Connection) -> Vec<(i64, Option<i64>)> {
    let mut stmt = db
        .prepare(
            "SELECT prompt_char_len, repo_symbol_hits FROM traction_event
             WHERE event_type = 'PROMPT_SUBMIT' ORDER BY timestamp_ms, event_id",
        )
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn a_prompt_naming_repo_files_hits_the_index_and_nothing_textual_is_stored() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    fs::write(repo.join("src").join(FILE_NAME), "fn widget() {}\n").unwrap();
    // The Project's own ignore file hides this one from the index.
    fs::write(repo.join(".tractionignore"), format!("{IGNORED_NAME}\n")).unwrap();
    fs::write(repo.join("src").join(IGNORED_NAME), "secret\n").unwrap();
    assert_eq!(daemon.caller().register_project(&repo).status, 200);

    let named = format!("the failure is in {FILE_NAME} again, {PROMPT_CANARY}");
    let terse = "continue";
    let ignored = format!("is {IGNORED_NAME} the problem? {PROMPT_CANARY}");
    prompt(&daemon, &repo, &named);
    prompt(&daemon, &repo, terse);
    prompt(&daemon, &repo, &ignored);

    let db = daemon.shutdown_and_open_db();
    let rows = hits(&db);
    assert_eq!(rows.len(), 3, "{rows:?}");
    // Prompts sent back to back can share a millisecond and event ids are
    // random, so each row is found by its (distinct) prompt length.
    let hits_for = |text: &str| {
        let len = i64::try_from(text.chars().count()).unwrap();
        let matching: Vec<_> = rows.iter().filter(|(l, _)| *l == len).collect();
        assert_eq!(matching.len(), 1, "one row of length {len}: {rows:?}");
        matching[0].1
    };
    assert_eq!(
        hits_for(&named),
        Some(1),
        "a named repo file hits: {rows:?}"
    );
    assert_eq!(
        hits_for(terse),
        Some(0),
        "a terse prompt has no hits: {rows:?}"
    );
    assert_eq!(
        hits_for(&ignored),
        Some(0),
        "an ignored file is not indexed: {rows:?}"
    );

    // Canary: no prompt wording and no file name anywhere in the stored data.
    for needle in [PROMPT_CANARY, FILE_NAME, IGNORED_NAME, "quasarfoxtrot"] {
        assert_eq!(sentinel_hits_in_tables(&db, needle), 0, "{needle}");
    }
    drop(db);
    for needle in [PROMPT_CANARY, FILE_NAME, IGNORED_NAME] {
        assert_no_file_contains(daemon.data_dir(), needle);
    }
}
