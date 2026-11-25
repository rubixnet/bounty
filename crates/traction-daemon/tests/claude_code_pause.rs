//! Seam A: `/stop-track` and `/resume-track` (Session Pause) through the
//! Claude Code hook endpoint of a real in-process daemon.
//!
//! The command arrives as a `UserPromptSubmit` whose prompt is the literal
//! command (ticket 01's finding with Claude Code 2.1.332), or, should a
//! Claude Code version expand it, the command file's body text.

mod common;

use std::fs;
use std::path::Path;
use std::time::Duration;

use common::{BusResponse, MutationRow, TestDaemon, fast_watcher, fixtures, path_str};
use rusqlite::Connection;
use serde_json::{Value, json};
use traction_daemon::{CLAUDE_CODE_HOOK_PATH, RESUME_TRACK, STOP_TRACK, WINDOW_CLOSE_GRACE};

const PAUSED: &str =
    r#"{"decision":"block","reason":"Traction tracking paused for this session."}"#;
const RESUMED: &str =
    r#"{"decision":"block","reason":"Traction tracking resumed for this session."}"#;

fn post_hook(daemon: &TestDaemon, payload: &Value) -> BusResponse {
    daemon
        .caller()
        .request("POST", CLAUDE_CODE_HOOK_PATH)
        .body(payload.to_string())
        .send()
}

/// Sends a hook that must get the ordinary non-blocking answer.
fn send_hook(daemon: &TestDaemon, payload: &Value) {
    let response = post_hook(daemon, payload);
    assert_eq!((response.status, response.body.as_str()), (200, "{}"));
}

/// Sends a prompt that must get the block answer `expected`.
fn send_command(daemon: &TestDaemon, session: &str, cwd: &Path, prompt: &str, expected: &str) {
    let response = post_hook(daemon, &prompt_hook(session, cwd, prompt));
    assert_eq!(
        (response.status, response.body.as_str()),
        (200, expected),
        "prompt {prompt:?}"
    );
}

/// A payload shaped like the captured ones.
fn hook(name: &str, session: &str, cwd: &Path, extra: Value) -> Value {
    let mut payload = json!({
        "session_id": session,
        "transcript_path": format!("/nowhere/{session}.jsonl"),
        "cwd": path_str(cwd),
        "permission_mode": "acceptEdits",
        "hook_event_name": name,
    });
    for (key, value) in extra.as_object().expect("object").clone() {
        payload[key] = value;
    }
    payload
}

fn prompt_hook(session: &str, cwd: &Path, prompt: &str) -> Value {
    hook(
        "UserPromptSubmit",
        session,
        cwd,
        json!({ "prompt": prompt }),
    )
}

/// `(session_id, event_type)` of every stored non-mutation event, sorted.
fn stored(db: &Connection) -> Vec<(String, String)> {
    let mut stmt = db
        .prepare(
            "SELECT session_id, event_type FROM traction_event
             WHERE event_type <> 'FILE_MUTATION' ORDER BY session_id, event_type",
        )
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn pair(session: &str, event_type: &str) -> (String, String) {
    (session.to_owned(), event_type.to_owned())
}

/// `(session_id, closed)` of every stored pause span, by session then start.
fn spans(db: &Connection) -> Vec<(String, bool)> {
    let mut stmt = db
        .prepare(
            "SELECT session_id, to_ms IS NOT NULL FROM session_pause
             WHERE harness_id = 'claude-code' ORDER BY session_id, from_ms",
        )
        .unwrap();
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

#[test]
fn the_literal_stop_track_command_is_blocked_with_the_paused_answer_and_not_stored() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    send_command(&daemon, "s", &repo, "/stop-track", PAUSED);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored(&db), vec![]);
    assert_eq!(spans(&db), vec![("s".to_owned(), false)]);
}

#[test]
fn surrounding_whitespace_is_trimmed_and_the_command_body_text_also_matches() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    send_command(&daemon, "a", &repo, "  /stop-track\n", PAUSED);
    send_command(&daemon, "b", &repo, STOP_TRACK.body, PAUSED);
    send_command(
        &daemon,
        "b",
        &repo,
        &format!("\n{}\n", RESUME_TRACK.body),
        RESUMED,
    );
    send_command(&daemon, "a", &repo, "/resume-track", RESUMED);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored(&db), vec![]);
    assert_eq!(
        spans(&db),
        vec![("a".to_owned(), true), ("b".to_owned(), true)]
    );
}

#[test]
fn only_the_exact_command_pauses_and_other_prompts_are_normal() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    for prompt in [
        "what does /stop-track do?",
        "/stop-track please",
        "/STOP-TRACK",
        "/stop-tracking",
        "stop-track",
        "/resume-track now",
    ] {
        send_hook(&daemon, &prompt_hook("s", &repo, prompt));
    }
    // A command-looking field on any other hook never pauses either.
    send_hook(
        &daemon,
        &hook(
            "PreToolUse",
            "s",
            &repo,
            json!({"tool_name": "Bash", "prompt": "/stop-track"}),
        ),
    );
    send_hook(
        &daemon,
        &hook("Stop", "s", &repo, json!({"prompt": "/stop-track"})),
    );
    let db = daemon.shutdown_and_open_db();
    let mut expected = vec![pair("s", "PROMPT_SUBMIT"); 6];
    expected.push(pair("s", "TOOL_PRE"));
    expected.push(pair("s", "TURN_END"));
    expected.sort();
    assert_eq!(stored(&db), expected);
    assert_eq!(spans(&db), vec![]);
}

#[test]
fn a_paused_session_stores_nothing_until_resumed_and_other_sessions_are_unaffected() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    send_command(&daemon, "paused", &repo, "/stop-track", PAUSED);
    let turn = |session: &str| {
        send_hook(&daemon, &prompt_hook(session, &repo, "fix the build"));
        send_hook(
            &daemon,
            &hook("PreToolUse", session, &repo, json!({"tool_name": "Bash"})),
        );
        send_hook(
            &daemon,
            &hook("PostToolUse", session, &repo, json!({"tool_name": "Bash"})),
        );
        send_hook(&daemon, &hook("Stop", session, &repo, json!({})));
    };
    turn("paused");
    turn("other");
    send_command(&daemon, "paused", &repo, "/resume-track", RESUMED);
    send_hook(&daemon, &hook("SessionEnd", "paused", &repo, json!({})));
    let db = daemon.shutdown_and_open_db();
    let mut expected = vec![
        pair("other", "PROMPT_SUBMIT"),
        pair("other", "TOOL_PRE"),
        pair("other", "TOOL_POST"),
        pair("other", "TURN_END"),
        pair("paused", "SESSION_END"),
    ];
    expected.sort();
    assert_eq!(stored(&db), expected);
}

#[test]
fn repeating_a_command_is_idempotent_and_gives_the_same_answer() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    // Resuming a tracked session is harmless.
    send_command(&daemon, "s", &repo, "/resume-track", RESUMED);
    send_command(&daemon, "s", &repo, "/stop-track", PAUSED);
    send_command(&daemon, "s", &repo, "/stop-track", PAUSED);
    send_hook(&daemon, &prompt_hook("s", &repo, "still paused"));
    send_command(&daemon, "s", &repo, "/resume-track", RESUMED);
    send_command(&daemon, "s", &repo, "/resume-track", RESUMED);
    send_hook(&daemon, &prompt_hook("s", &repo, "tracked again"));
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored(&db), vec![pair("s", "PROMPT_SUBMIT")]);
    assert_eq!(spans(&db), vec![("s".to_owned(), true)]);
}

#[test]
fn a_pause_survives_a_daemon_restart() {
    let daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    send_command(&daemon, "s", &repo, "/stop-track", PAUSED);
    let daemon = daemon.restart();
    send_hook(&daemon, &prompt_hook("s", &repo, "after the restart"));
    send_hook(&daemon, &prompt_hook("t", &repo, "another session"));
    let mut daemon = daemon.restart();
    send_command(&daemon, "s", &repo, "/resume-track", RESUMED);
    send_hook(&daemon, &hook("Stop", "s", &repo, json!({})));
    let db = daemon.shutdown_and_open_db();
    assert_eq!(
        stored(&db),
        vec![pair("s", "TURN_END"), pair("t", "PROMPT_SUBMIT")]
    );
    assert_eq!(spans(&db), vec![("s".to_owned(), true)]);
}

// --- File mutations of a paused session ------------------------------------

fn wait_past_grace() {
    std::thread::sleep(WINDOW_CLOSE_GRACE + Duration::from_millis(300));
}

#[test]
fn edits_claimed_by_a_paused_session_are_dropped_and_edits_outside_every_window_stay_manual() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let root = fixtures::folder(&daemon.home().join("game"), &[]);
    let registered = daemon.caller().register_project(&root);
    assert_eq!(registered.status, 200, "{}", registered.body);
    std::thread::sleep(Duration::from_millis(500));

    send_command(&daemon, "s", &root, "/stop-track", PAUSED);
    send_hook(&daemon, &prompt_hook("s", &root, "write the notes"));
    send_hook(
        &daemon,
        &hook("PreToolUse", "s", &root, json!({"tool_name": "Write"})),
    );
    fs::write(root.join("claude.txt"), "written by Claude while paused").unwrap();
    send_hook(
        &daemon,
        &hook("PostToolUse", "s", &root, json!({"tool_name": "Write"})),
    );
    wait_past_grace();
    // The user's own edit, outside every window, is still Manual Work.
    fs::write(root.join("mine.txt"), "by hand").unwrap();
    let rows: Vec<MutationRow> = daemon.wait_for_mutations(1);
    assert_eq!(rows[0].harness_id, None, "{rows:?}");
    assert_eq!(rows[0].session_id, None, "{rows:?}");
    assert_eq!(rows[0].network_class, "manual", "{rows:?}");
    // And Claude's paused edit never shows up, even after its hold ended.
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(daemon.wait_for_mutations(1).len(), 1);
}
