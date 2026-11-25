//! Seam 2: attribution windows. A held `FILE_MUTATION` is attributed to a
//! Harness session only inside one of that session's windows on the same
//! Project, otherwise it is manual work.
//!
//! Windows compare event timestamps with the mutation's timestamp (its
//! first observation, on the wall clock), so every test sets event
//! timestamps explicitly relative to the wall clock: a window close is
//! stamped well after the edit it must cover, and an edit that must fall
//! outside a window is made only once the wall clock is well past the close.

mod common;

use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{MutationRow, TestDaemon, fast_watcher, fixtures, valid_event_value};

/// Slack between an edit and the timestamp of the event that must still
/// cover it (or must not), for slow file-system notification delivery.
const SLACK_MS: u64 = 1500;

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// Sleeps until the wall clock is past `ts_ms` by a safe margin.
fn wait_past(ts_ms: u64) {
    let target = ts_ms + 300;
    let now = now_ms();
    if now < target {
        std::thread::sleep(Duration::from_millis(target - now));
    }
}

/// One Adapter's session: who it is and how it reports.
#[derive(Clone, Copy)]
struct Session {
    harness: &'static str,
    session: &'static str,
    source: &'static str,
    network: &'static str,
}

const HOOK: Session = Session {
    harness: "claude-code",
    session: "hook-session",
    source: "harness_hook",
    network: "cloud_api",
};
const LOG: Session = Session {
    harness: "codex-cli",
    session: "log-session",
    source: "harness_log",
    network: "cloud_api",
};
const POLL: Session = Session {
    harness: "opencode",
    session: "poll-session",
    source: "harness_db_poll",
    network: "local_model",
};

/// Sends one event of `session` with an explicit timestamp; `n` must be
/// unique per test (it makes the `event_id`).
fn send(daemon: &TestDaemon, n: u32, s: Session, event_type: &str, cwd: &Path, ts_ms: u64) {
    let mut event = valid_event_value(n);
    event["project_id"] = serde_json::Value::Null;
    event["timestamp_ms"] = serde_json::json!(ts_ms);
    event["harness_id"] = serde_json::json!(s.harness);
    event["session_id"] = serde_json::json!(s.session);
    event["attribution_source"] = serde_json::json!(s.source);
    event["network_class"] = serde_json::json!(s.network);
    event["event_type"] = serde_json::json!(event_type);
    event["cwd"] = serde_json::json!(common::path_str(cwd));
    if event_type != "TURN_END" {
        event["end_reason"] = serde_json::Value::Null;
    }
    let response = daemon.caller().post_event(&event.to_string());
    assert_eq!(response.status, 202, "{}", response.body);
}

fn register(daemon: &TestDaemon, root: &Path) -> String {
    let response = daemon.caller().register_project(root);
    assert_eq!(response.status, 200, "{}", response.body);
    // Let the new watch settle, so the first edit is not at the attach
    // boundary (FSEvents latency and its pre-attach replay filter).
    std::thread::sleep(Duration::from_millis(500));
    serde_json::from_str::<serde_json::Value>(&response.body).unwrap()["project_id"]
        .as_str()
        .unwrap()
        .to_owned()
}

fn assert_attributed(row: &MutationRow, s: Session, project_id: &str) {
    assert_eq!(row.project_id.as_deref(), Some(project_id), "{row:?}");
    assert_eq!(row.harness_id.as_deref(), Some(s.harness), "{row:?}");
    assert_eq!(row.session_id.as_deref(), Some(s.session), "{row:?}");
    assert_eq!(row.network_class, s.network, "{row:?}");
    assert_eq!(row.attribution_source, "file_watcher", "{row:?}");
}

fn assert_manual(row: &MutationRow, project_id: &str) {
    assert_eq!(row.project_id.as_deref(), Some(project_id), "{row:?}");
    assert_eq!(row.harness_id, None, "{row:?}");
    assert_eq!(row.session_id, None, "{row:?}");
    assert_eq!(row.network_class, "manual", "{row:?}");
    assert_eq!(row.attribution_source, "file_watcher", "{row:?}");
}

#[test]
fn a_hook_window_attributes_and_edits_after_tool_post_or_turn_end_are_manual() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let root = fixtures::folder(&daemon.home().join("game"), &[]);
    let project = register(&daemon, &root);

    let t0 = now_ms();
    send(&daemon, 1, HOOK, "PROMPT_SUBMIT", &root, t0);
    send(&daemon, 2, HOOK, "TOOL_PRE", &root, t0);
    fs::write(root.join("a.txt"), "by the agent").unwrap();
    let post = now_ms() + SLACK_MS;
    send(&daemon, 3, HOOK, "TOOL_POST", &root, post);
    let rows = daemon.wait_for_mutations(1);
    assert_attributed(&rows[0], HOOK, &project);

    // After TOOL_POST, still inside the open turn: manual.
    wait_past(post);
    fs::write(root.join("b.txt"), "by hand").unwrap();
    let rows = daemon.wait_for_mutations(2);
    assert_manual(&rows[1], &project);

    // A new tool window, closed by TURN_END instead of TOOL_POST.
    send(&daemon, 4, HOOK, "TOOL_PRE", &root, now_ms());
    fs::write(root.join("c.txt"), "by the agent").unwrap();
    let end = now_ms() + SLACK_MS;
    send(&daemon, 5, HOOK, "TURN_END", &root, end);
    let rows = daemon.wait_for_mutations(3);
    assert_attributed(&rows[2], HOOK, &project);

    // After TURN_END a TOOL_PRE opens nothing: no turn is open.
    wait_past(end);
    send(&daemon, 6, HOOK, "TOOL_PRE", &root, now_ms());
    fs::write(root.join("d.txt"), "by hand").unwrap();
    let rows = daemon.wait_for_mutations(4);
    assert_manual(&rows[3], &project);
}

#[test]
fn a_log_tailed_turn_attributes_until_turn_end() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let root = fixtures::folder(&daemon.home().join("cli"), &[]);
    let project = register(&daemon, &root);

    send(&daemon, 1, LOG, "PROMPT_SUBMIT", &root, now_ms());
    fs::write(root.join("a.txt"), "by the agent").unwrap();
    let end = now_ms() + SLACK_MS;
    send(&daemon, 2, LOG, "TURN_END", &root, end);
    let rows = daemon.wait_for_mutations(1);
    assert_attributed(&rows[0], LOG, &project);

    wait_past(end);
    fs::write(root.join("b.txt"), "by hand").unwrap();
    let rows = daemon.wait_for_mutations(2);
    assert_manual(&rows[1], &project);
}

#[test]
fn a_late_prompt_submit_claims_an_edit_within_the_hold_but_not_after_it() {
    let mut daemon = TestDaemon::start_with(|config| {
        fast_watcher(config);
        config.watcher.hold = Duration::from_secs(3);
    });
    let root = fixtures::folder(&daemon.home().join("late"), &[]);
    let project = register(&daemon, &root);

    // The PROMPT_SUBMIT precedes the edit but is delivered 1 s after it,
    // inside the 3 s hold.
    let before = now_ms();
    fs::write(root.join("a.txt"), "by the agent").unwrap();
    std::thread::sleep(Duration::from_secs(1));
    send(&daemon, 1, LOG, "PROMPT_SUBMIT", &root, before - 500);
    let end = now_ms() + SLACK_MS;
    send(&daemon, 2, LOG, "TURN_END", &root, end);
    let rows = daemon.wait_for_mutations(1);
    assert_attributed(&rows[0], LOG, &project);

    // Delivered only after the edit was decided: too late.
    wait_past(end);
    let before = now_ms();
    fs::write(root.join("b.txt"), "by hand").unwrap();
    let rows = daemon.wait_for_mutations(2);
    assert_manual(&rows[1], &project);
    let late = Session {
        session: "late-session",
        ..LOG
    };
    send(&daemon, 3, late, "PROMPT_SUBMIT", &root, before - 500);

    let db = daemon.shutdown_and_open_db();
    let rows = common::stored_mutations(&db);
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_manual(&rows[1], &project);
}

#[test]
fn the_trailing_fallback_covers_a_db_poll_session_without_turn_boundaries() {
    // Production: 30 s window, an edit 10 s after the last event is
    // attributed. Kept at the same ratio: 3 s window, edit 1 s after.
    let daemon = TestDaemon::start_with(|config| {
        fast_watcher(config);
        config.watcher.trailing_window = Duration::from_secs(3);
    });
    let root = fixtures::folder(&daemon.home().join("poll"), &[]);
    let project = register(&daemon, &root);

    let last = now_ms() - 1000;
    send(&daemon, 1, POLL, "TOOL_POST", &root, last);
    fs::write(root.join("a.txt"), "by the agent").unwrap();
    let rows = daemon.wait_for_mutations(1);
    assert_attributed(&rows[0], POLL, &project);

    wait_past(last + 3000);
    fs::write(root.join("b.txt"), "by hand").unwrap();
    let rows = daemon.wait_for_mutations(2);
    assert_manual(&rows[1], &project);
}

#[test]
fn a_window_in_one_project_never_claims_an_edit_in_another() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let a = fixtures::folder(&daemon.home().join("alpha"), &[]);
    let b = fixtures::folder(&daemon.home().join("beta"), &[]);
    let a_id = register(&daemon, &a);
    let b_id = register(&daemon, &b);

    let t0 = now_ms();
    send(&daemon, 1, HOOK, "PROMPT_SUBMIT", &a, t0);
    send(&daemon, 2, HOOK, "TOOL_PRE", &a, t0);
    send(&daemon, 3, LOG, "PROMPT_SUBMIT", &a, t0);
    send(&daemon, 4, POLL, "TOOL_POST", &a, t0);
    fs::write(b.join("b.txt"), "by hand").unwrap();
    let rows = daemon.wait_for_mutations(1);
    assert_manual(&rows[0], &b_id);

    // Control: the same windows do claim an edit in their own Project.
    fs::write(a.join("a.txt"), "by the agent").unwrap();
    let rows = daemon.wait_for_mutations(2);
    let in_a: Vec<_> = rows
        .iter()
        .filter(|r| r.project_id.as_deref() == Some(&a_id))
        .collect();
    assert_eq!(in_a.len(), 1, "{rows:?}");
    assert_attributed(in_a[0], HOOK, &a_id);
}

#[test]
fn hook_beats_log_turn_beats_trailing_and_the_latest_opened_wins() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let root = fixtures::folder(&daemon.home().join("arena"), &[]);
    let project = register(&daemon, &root);

    // Trailing fallback and a log-tailed turn: the turn wins.
    let t0 = now_ms();
    send(&daemon, 1, POLL, "TOOL_POST", &root, t0);
    send(&daemon, 2, LOG, "PROMPT_SUBMIT", &root, t0);
    fs::write(root.join("a.txt"), "1").unwrap();
    let rows = daemon.wait_for_mutations(1);
    assert_attributed(&rows[0], LOG, &project);

    // Two hook windows join: the later opened wins over both.
    let first = HOOK;
    let second = Session {
        session: "hook-session-2",
        network: "local_model",
        ..HOOK
    };
    let t1 = now_ms();
    send(&daemon, 3, first, "PROMPT_SUBMIT", &root, t1);
    send(&daemon, 4, first, "TOOL_PRE", &root, t1);
    send(&daemon, 5, second, "PROMPT_SUBMIT", &root, t1 + 1);
    send(&daemon, 6, second, "TOOL_PRE", &root, t1 + 1);
    fs::write(root.join("b.txt"), "2").unwrap();
    let close = now_ms() + SLACK_MS;
    let rows = daemon.wait_for_mutations(2);
    assert_attributed(&rows[1], second, &project);

    // Close the hook windows and the log turn: the trailing fallback is
    // all that is left.
    send(&daemon, 7, first, "TOOL_POST", &root, close);
    send(&daemon, 8, second, "TOOL_POST", &root, close);
    send(&daemon, 9, LOG, "TURN_END", &root, close);
    wait_past(close);
    send(&daemon, 10, POLL, "TOOL_POST", &root, now_ms());
    fs::write(root.join("c.txt"), "3").unwrap();
    let rows = daemon.wait_for_mutations(3);
    assert_attributed(&rows[2], POLL, &project);
}

#[test]
fn windows_are_live_only_and_do_not_survive_a_restart() {
    let mut daemon = TestDaemon::start_with(fast_watcher);
    let root = fixtures::folder(&daemon.home().join("restart"), &[]);
    fs::write(root.join("down.txt"), "before").unwrap();
    let project = register(&daemon, &root);

    let t0 = now_ms();
    send(&daemon, 1, HOOK, "PROMPT_SUBMIT", &root, t0);
    send(&daemon, 2, HOOK, "TOOL_PRE", &root, t0);
    daemon.shutdown();
    fs::write(root.join("down.txt"), "edited while down").unwrap();
    let daemon = daemon.restart_with(fast_watcher);

    // The window opened before the restart is gone: the control is manual,
    // and the downtime edit produced nothing.
    fs::write(root.join("after.txt"), "after restart").unwrap();
    let rows = daemon.wait_for_mutations(1);
    assert_manual(&rows[0], &project);
    assert_eq!(rows[0].mutation_kind.as_deref(), Some("created"));
}
