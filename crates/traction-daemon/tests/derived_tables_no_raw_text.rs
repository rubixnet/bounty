//! Seam: the "no raw text" gate for everything Step 2 derives, with canary
//! text planted where real input carries it (a hook prompt, a file name and
//! its contents, a commit message) and run through a real daemon (hooks,
//! watcher, commit observer, presence sensor) and the finalizer. None of it
//! may appear in any byte of the data folder, WAL included.
//!
//! The daemon's log lines are `&'static str` by construction (`log.rs`), so
//! no input text can reach them; this test checks the stored bytes.

mod common;

use std::time::Duration;

use common::fixtures::{self, git};
use common::{TestDaemon, fast_watcher, path_str, poll_until, stored_mutations};
use serde_json::json;

const PROMPT: &str = "CANARY-PROMPT-b81e";
const FILE_STEM: &str = "canaryfile_7d2e";
const FILE_BODY: &str = "CANARY-FILE-BODY-4c90";
const COMMIT_MSG: &str = "CANARY-COMMIT-MSG-a23f";

fn count(db: &rusqlite::Connection, table: &str) -> i64 {
    db.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
        .unwrap()
}

fn data_folder_bytes(dir: &std::path::Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            bytes.extend(std::fs::read(&path).unwrap_or_default());
        }
    }
    bytes
}

fn has(haystack: &[u8], needle: &str) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle.as_bytes())
}

#[test]
fn canary_text_never_reaches_the_derived_tables() {
    let mut daemon = TestDaemon::start_with(|config| {
        fast_watcher(config);
        config.presence.subslot = Duration::from_millis(60);
        config.commits.poll = Duration::from_millis(25);
        config.finalizer.enabled = false;
    });
    let repo = fixtures::repo(&daemon.home().join("code").join("alpha"), None);
    let response = daemon.caller().register_project(&repo);
    assert_eq!(response.status, 200, "{response:?}");
    // The commit observer takes its baseline when it handles the watch.
    std::thread::sleep(Duration::from_millis(300));

    // A hook prompt naming the file and carrying a stack trace.
    let prompt = json!({
        "session_id": "canary-session",
        "transcript_path": "/nowhere/canary.jsonl",
        "cwd": path_str(&repo),
        "prompt_id": "6cc0ac7e-854d-4139-8e0f-e0b52fd93a1d",
        "permission_mode": "acceptEdits",
        "hook_event_name": "UserPromptSubmit",
        "prompt": format!("fix {FILE_STEM}.rs {PROMPT}\n  at f (a.js:1:2)\n  at g (b.js:3:4)"),
    });
    let sent = daemon
        .caller()
        .request("POST", "/v1/hooks/claude-code")
        .body(prompt.to_string())
        .send();
    assert_eq!(sent.status, 200, "{}", sent.body);

    // A real file mutation and a real commit with a canary message.
    std::fs::write(repo.join(format!("{FILE_STEM}.rs")), FILE_BODY).unwrap();
    let live = daemon.live_db();
    poll_until(Duration::from_secs(30), "the file mutation", || {
        (!stored_mutations(&live).is_empty()).then_some(())
    });
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", COMMIT_MSG]);
    poll_until(Duration::from_secs(10), "the commit observation", || {
        (count(&live, "commit_observation") > 0).then_some(())
    });
    poll_until(Duration::from_secs(10), "a presence minute", || {
        (count(&live, "presence_minute") > 0).then_some(())
    });
    drop(live);
    daemon.shutdown();

    let now = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    let batches = traction_daemon::finalize_settled(daemon.data_dir(), now + 3_600_000).unwrap();
    assert!(batches > 0, "nothing was finalized");

    let db = daemon.live_db();
    for table in ["presence_minute", "commit_observation", "work_bucket"] {
        assert!(count(&db, table) > 0, "{table} was not populated");
    }
    drop(db);
    let bytes = data_folder_bytes(daemon.data_dir());
    for canary in [PROMPT, FILE_STEM, FILE_BODY, COMMIT_MSG] {
        assert!(!has(&bytes, canary), "{canary} reached the data folder");
    }
}
