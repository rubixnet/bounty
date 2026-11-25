//! Seam A: hook coverage (the dedup rule) between Claude Code hooks and the
//! transcript tailer, through a real in-process daemon. A session delivered
//! both ways yields one set of events; spans the hooks missed are filled in
//! from the transcript; a hook-reported `transcript_path` outside the
//! Claude `projects/` folder is never opened; tailed records inside a
//! paused span are not stored.
//!
//! Transcript lines here are minimal `user` records stamped relative to the
//! daemon's clock, because hook timestamps are the daemon's receive times.

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use common::{BusResponse, TestDaemon, fixtures, path_str, poll_until};
use rusqlite::Connection;
use serde_json::{Value, json};
use traction_daemon::{CLAUDE_CODE_HOOK_PATH, DaemonConfig, RESUME_TRACK, STOP_TRACK};

const WAIT: Duration = Duration::from_secs(30);

fn fast(config: &mut DaemonConfig) {
    config.writer.commit_interval = Duration::from_millis(50);
}

fn now_ms() -> i64 {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_millis();
    i64::try_from(ms).expect("fits")
}

/// RFC 3339 UTC text of Unix milliseconds (civil-from-days).
fn iso(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let in_day = ms.rem_euclid(86_400_000);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        in_day / 3_600_000,
        in_day / 60_000 % 60,
        in_day / 1000 % 60,
        in_day % 1000
    )
}

/// A `user` record with prompt text at `ts_ms`.
fn prompt_line(session: &str, n: u32, ts_ms: i64, cwd: &Path) -> String {
    json!({
        "type": "user",
        "message": { "role": "user", "content": "please do the thing" },
        "uuid": format!("00000000-0000-4000-8000-{n:012}"),
        "timestamp": iso(ts_ms),
        "cwd": path_str(cwd),
        "sessionId": session,
    })
    .to_string()
}

fn claude_projects(home: &Path) -> PathBuf {
    let dir = home.join(".claude").join("projects");
    fs::create_dir_all(&dir).expect("projects folder");
    dir
}

/// `<projects>/<slug>/<session>.jsonl` (the folder is created).
fn transcript_path(home: &Path, session: &str) -> PathBuf {
    let folder = claude_projects(home).join("-code-app");
    fs::create_dir_all(&folder).expect("slug folder");
    folder.join(format!("{session}.jsonl"))
}

fn append_lines(path: &Path, lines: &[String]) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open transcript");
    for line in lines {
        writeln!(file, "{line}").expect("append");
    }
}

fn post_hook(daemon: &TestDaemon, payload: &Value) -> BusResponse {
    daemon
        .caller()
        .request("POST", CLAUDE_CODE_HOOK_PATH)
        .body(payload.to_string())
        .send()
}

fn hook(name: &str, session: &str, cwd: &Path, extra: Value) -> Value {
    let mut payload = json!({
        "session_id": session,
        "transcript_path": format!("/nowhere/{session}.jsonl"),
        "cwd": path_str(cwd),
        "hook_event_name": name,
    });
    for (key, value) in extra.as_object().expect("object").clone() {
        payload[key] = value;
    }
    payload
}

fn send(daemon: &TestDaemon, payload: &Value) {
    let response = post_hook(daemon, payload);
    assert_eq!(response.status, 200, "{}", response.body);
}

fn session_start(daemon: &TestDaemon, session: &str, cwd: &Path) {
    send(daemon, &hook("SessionStart", session, cwd, json!({})));
}

fn user_prompt(daemon: &TestDaemon, session: &str, cwd: &Path) {
    let extra = json!({ "prompt": "please do the thing" });
    send(daemon, &hook("UserPromptSubmit", session, cwd, extra));
}

fn repo_under(home: &Path) -> PathBuf {
    fixtures::repo(&home.join("code/app"), None)
}

/// `(event_type, attribution_source, timestamp_ms)` of `session`'s stored
/// events, oldest first (a `SESSION_START` before its sibling).
fn stored(db: &Connection, session: &str) -> Vec<(String, String, i64)> {
    let mut stmt = db
        .prepare(
            "SELECT event_type, attribution_source, timestamp_ms FROM traction_event
             WHERE session_id = ?1 AND event_type <> 'FILE_MUTATION'
             ORDER BY timestamp_ms, event_type <> 'SESSION_START', event_id",
        )
        .expect("prepare");
    stmt.query_map([session], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .map(|row| row.expect("row"))
        .collect()
}

fn row(event_type: &str, source: &str, ts: i64) -> (String, String, i64) {
    (event_type.to_owned(), source.to_owned(), ts)
}

/// Waits until the tailer has committed the offset at the end of `path`
/// (events dropped by the dedup rule still advance it).
fn wait_for_tailer(db: &Connection, path: &Path) {
    let len = i64::try_from(fs::metadata(path).expect("transcript").len()).expect("fits");
    poll_until(WAIT, "the tailer to reach the end of the file", || {
        let offset: Option<i64> = db
            .query_row(
                "SELECT \"offset\" FROM transcript_offset WHERE path = ?1",
                [path_str(path)],
                |row| row.get(0),
            )
            .ok();
        (offset == Some(len)).then_some(())
    });
}

/// Waits until `session` has `count` stored events.
fn wait_for_stored(db: &Connection, session: &str, count: usize) -> Vec<(String, String, i64)> {
    poll_until(WAIT, "stored events", || {
        let rows = stored(db, session);
        assert!(rows.len() <= count, "more events than expected: {rows:#?}");
        (rows.len() == count).then_some(rows)
    })
}

#[test]
fn hooks_and_a_transcript_for_one_session_store_one_set_of_events() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let repo = repo_under(&home);
    let session = "2f1c8a54-0000-4000-8000-000000000001";

    session_start(&daemon, session, &repo);
    user_prompt(&daemon, session, &repo);
    let db = daemon.live_db();
    let hooked = wait_for_stored(&db, session, 2);

    // The same prompt as Claude wrote it, and a later one in the same run:
    // hooks that authenticated are assumed to keep delivering.
    let path = transcript_path(&home, session);
    let t = hooked[1].2;
    append_lines(
        &path,
        &[
            prompt_line(session, 1, t + 20, &repo),
            prompt_line(session, 2, t + 2_000, &repo),
        ],
    );
    wait_for_tailer(&db, &path);

    assert_eq!(
        stored(&db, session),
        vec![
            row("SESSION_START", "harness_hook", hooked[0].2),
            row("PROMPT_SUBMIT", "harness_hook", hooked[1].2),
        ]
    );
    daemon.shutdown();
}

#[test]
fn a_span_the_daemon_was_down_for_is_filled_in_from_the_transcript() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let repo = repo_under(&home);
    let session = "2f1c8a54-0000-4000-8000-000000000002";

    session_start(&daemon, session, &repo);
    user_prompt(&daemon, session, &repo);
    let hooked = wait_for_stored(&daemon.live_db(), session, 2);
    let seen_by_hooks = now_ms();
    daemon.shutdown();
    let after_stop = now_ms();

    // While the daemon is down Claude keeps writing: the prompt the hooks
    // saw, and one three seconds past the moment the daemon stopped.
    let path = transcript_path(&home, session);
    let missed = after_stop + 3_000;
    append_lines(
        &path,
        &[
            prompt_line(session, 1, seen_by_hooks, &repo),
            prompt_line(session, 2, missed, &repo),
        ],
    );
    let mut daemon = daemon.restart_with(fast);
    let db = daemon.live_db();
    wait_for_tailer(&db, &path);

    assert_eq!(
        stored(&db, session),
        vec![
            row("SESSION_START", "harness_hook", hooked[0].2),
            row("PROMPT_SUBMIT", "harness_hook", hooked[1].2),
            row("PROMPT_SUBMIT", "harness_log", missed),
        ]
    );
    // One row per run, and the first run's hook coverage for the session.
    let runs: Vec<(i64, i64)> = db
        .prepare("SELECT started_ms, last_alive_ms FROM daemon_run ORDER BY run_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(runs.len(), 2);
    assert!(runs[0].1 >= hooked[1].2, "last_alive_ms covers the hooks");
    let first_hooks: Vec<i64> = db
        .prepare("SELECT first_hook_ms FROM session_hook_run WHERE session_id = ?1")
        .unwrap()
        .query_map([session], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(first_hooks, vec![hooked[0].2]);
    daemon.shutdown();
}

#[test]
fn a_record_within_clock_skew_of_the_first_hook_is_a_duplicate() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let repo = repo_under(&home);
    let session = "2f1c8a54-0000-4000-8000-000000000003";

    let before = now_ms();
    session_start(&daemon, session, &repo);
    let db = daemon.live_db();
    let hooked = wait_for_stored(&db, session, 1);

    // Half a second before the first hook: Claude's own write of the same
    // moment, stamped a little earlier than the hook was received. Three
    // seconds before: genuinely earlier, and not covered.
    let path = transcript_path(&home, session);
    append_lines(
        &path,
        &[
            prompt_line(session, 1, before - 3_000, &repo),
            prompt_line(session, 2, before - 500, &repo),
        ],
    );
    wait_for_tailer(&db, &path);

    assert_eq!(
        stored(&db, session),
        vec![
            row("PROMPT_SUBMIT", "harness_log", before - 3_000),
            row("SESSION_START", "harness_hook", hooked[0].2),
        ]
    );
    daemon.shutdown();
}

#[test]
fn a_forged_transcript_path_is_never_opened() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let repo = repo_under(&home);
    let hooked_session = "2f1c8a54-0000-4000-8000-000000000004";
    let outside = "2f1c8a54-0000-4000-8000-000000000005";
    let escaping = "2f1c8a54-0000-4000-8000-000000000006";
    let legit = "2f1c8a54-0000-4000-8000-000000000007";

    // Valid transcripts that are not under `projects/`: one elsewhere, one
    // reached through `..`.
    let outside_file = home.join("elsewhere").join(format!("{outside}.jsonl"));
    fs::create_dir_all(outside_file.parent().unwrap()).unwrap();
    append_lines(&outside_file, &[prompt_line(outside, 1, now_ms(), &repo)]);
    claude_projects(&home);
    let escaping_file = home.join(".claude").join(format!("{escaping}.jsonl"));
    append_lines(&escaping_file, &[prompt_line(escaping, 1, now_ms(), &repo)]);
    let traversal = claude_projects(&home)
        .join("..")
        .join(format!("{escaping}.jsonl"));

    for forged in [&outside_file, &traversal] {
        let extra = json!({ "transcript_path": path_str(forged) });
        send(&daemon, &hook("SessionStart", hooked_session, &repo, extra));
    }

    // A real transcript afterwards: once it is stored, the forged hints
    // (queued earlier) have been handled.
    let db = daemon.live_db();
    let path = transcript_path(&home, legit);
    append_lines(&path, &[prompt_line(legit, 1, now_ms(), &repo)]);
    wait_for_stored(&db, legit, 2);
    wait_for_tailer(&db, &path);

    assert_eq!(stored(&db, outside), vec![]);
    assert_eq!(stored(&db, escaping), vec![]);
    daemon.shutdown();
}

#[test]
fn tailed_records_inside_a_paused_span_are_not_stored() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let repo = repo_under(&home);
    let session = "2f1c8a54-0000-4000-8000-000000000008";
    let command = |daemon: &TestDaemon, literal: &str| {
        let extra = json!({ "prompt": literal });
        let response = post_hook(daemon, &hook("UserPromptSubmit", session, &repo, extra));
        assert!(response.body.contains("block"), "{}", response.body);
    };

    command(&daemon, STOP_TRACK.literal);
    sleep(Duration::from_millis(150));
    let paused_at = now_ms();
    sleep(Duration::from_millis(150));
    command(&daemon, RESUME_TRACK.literal);
    sleep(Duration::from_millis(150));
    let resumed_at = now_ms();

    let path = transcript_path(&home, session);
    append_lines(
        &path,
        &[
            prompt_line(session, 1, paused_at, &repo),
            prompt_line(session, 2, resumed_at, &repo),
        ],
    );
    let db = daemon.live_db();
    wait_for_tailer(&db, &path);

    let prompts: Vec<i64> = stored(&db, session)
        .into_iter()
        .filter(|(kind, _, _)| kind == "PROMPT_SUBMIT")
        .map(|(_, _, ts)| ts)
        .collect();
    assert_eq!(prompts, vec![resumed_at]);
    daemon.shutdown();
}
