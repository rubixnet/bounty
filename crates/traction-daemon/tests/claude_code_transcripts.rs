//! Seam A: the Claude Code transcript tailer through a real in-process
//! daemon. Transcripts are the real ones captured from `claude -p` in
//! `tests/data/claude-code/` (placeholders swapped for temp paths), written
//! into a temp Claude config folder under the test's fake home, and the
//! stored rows are read from SQLite.

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use common::{TestDaemon, fixtures, path_str, poll_until};
use rusqlite::Connection;
use serde_json::Value;
use traction_daemon::DaemonConfig;

const WAIT: Duration = Duration::from_secs(30);

/// The tailer's test config: frequent commits so rows appear while the
/// daemon runs.
fn fast(config: &mut DaemonConfig) {
    config.writer.commit_interval = Duration::from_millis(50);
}

/// The captured transcript lines of `scenario`, placeholders replaced:
/// `<REPO>` by `repo`, the rest by folders under `home`.
fn transcript_lines(scenario: &str, repo: &Path, home: &Path) -> Vec<String> {
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/claude-code")
        .join(scenario)
        .join("transcript.jsonl");
    let substitutions = [
        ("<REPO>", path_str(repo).to_owned()),
        (
            "<CLAUDE_CONFIG_DIR>",
            path_str(&home.join(".claude")).to_owned(),
        ),
        (
            "<CLAUDE_TMP>",
            path_str(&home.join("claude-tmp")).to_owned(),
        ),
        ("<HOME>", path_str(home).to_owned()),
        ("<TMP>", path_str(&home.join("tmp")).to_owned()),
        ("<HOST>", "host".to_owned()),
        ("<USER>", "user".to_owned()),
    ];
    fs::read_to_string(file)
        .expect("fixture transcript")
        .lines()
        .map(|line| {
            let mut value: Value = serde_json::from_str(line).expect("fixture line is JSON");
            substitute(&mut value, &substitutions);
            value.to_string()
        })
        .collect()
}

fn substitute(value: &mut Value, substitutions: &[(&str, String)]) {
    match value {
        Value::String(text) => {
            for (placeholder, replacement) in substitutions {
                *text = text.replace(placeholder, replacement);
            }
        }
        Value::Array(items) => items
            .iter_mut()
            .for_each(|item| substitute(item, substitutions)),
        Value::Object(map) => map
            .values_mut()
            .for_each(|item| substitute(item, substitutions)),
        _ => {}
    }
}

/// The session ID of a transcript's lines.
fn session_of(lines: &[String]) -> String {
    lines
        .iter()
        .find_map(|line| {
            let value: Value = serde_json::from_str(line).ok()?;
            value["sessionId"].as_str().map(str::to_owned)
        })
        .expect("a line with a sessionId")
}

/// A Claude config folder (with `projects/`) under the daemon's fake home.
fn claude_dir(home: &Path) -> PathBuf {
    let dir = home.join(".claude");
    fs::create_dir_all(dir.join("projects")).expect("projects folder");
    dir
}

/// `<claude dir>/projects/<slug>/<session>.jsonl` for `repo`.
fn transcript_path(claude: &Path, repo: &Path, session: &str) -> PathBuf {
    let slug: String = path_str(repo)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let folder = claude.join("projects").join(slug);
    fs::create_dir_all(&folder).expect("project slug folder");
    folder.join(format!("{session}.jsonl"))
}

/// Appends raw bytes to `path` (creating it), closing the file afterwards
/// as Claude Code does.
fn append(path: &Path, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open transcript");
    file.write_all(bytes).expect("append");
}

fn append_lines(path: &Path, lines: &[String]) {
    let mut bytes = Vec::new();
    for line in lines {
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
    }
    append(path, &bytes);
}

/// A tailed event, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tailed {
    event_id: String,
    timestamp_ms: i64,
    session_id: Option<String>,
    event_type: String,
    cwd: Option<String>,
    project_id: Option<String>,
    attribution_source: String,
    network_class: String,
    prompt_char_len: Option<i64>,
    tool_name: Option<String>,
}

/// Every stored non-mutation event, in timestamp order (a synthesized
/// `SESSION_START` shares its first record's timestamp and comes first).
fn tailed(db: &Connection) -> Vec<Tailed> {
    let mut stmt = db
        .prepare(
            "SELECT event_id, timestamp_ms, session_id, event_type, cwd, project_id,
                    attribution_source, network_class, prompt_char_len, tool_name
             FROM traction_event WHERE event_type <> 'FILE_MUTATION'
             ORDER BY timestamp_ms, event_type <> 'SESSION_START', event_id",
        )
        .expect("prepare");
    stmt.query_map([], |row| {
        Ok(Tailed {
            event_id: row.get(0)?,
            timestamp_ms: row.get(1)?,
            session_id: row.get(2)?,
            event_type: row.get(3)?,
            cwd: row.get(4)?,
            project_id: row.get(5)?,
            attribution_source: row.get(6)?,
            network_class: row.get(7)?,
            prompt_char_len: row.get(8)?,
            tool_name: row.get(9)?,
        })
    })
    .expect("query")
    .map(|row| row.expect("row"))
    .collect()
}

/// `(event_type, tool_name)` of `rows`, in stored order.
fn shapes(rows: &[Tailed]) -> Vec<(String, Option<String>)> {
    rows.iter()
        .map(|row| (row.event_type.clone(), row.tool_name.clone()))
        .collect()
}

fn shape(event_type: &str, tool: Option<&str>) -> (String, Option<String>) {
    (event_type.to_owned(), tool.map(str::to_owned))
}

/// Waits until the running daemon has stored exactly `count` tailed events.
fn wait_for_events(daemon: &TestDaemon, count: usize) -> Vec<Tailed> {
    let db = daemon.live_db();
    poll_until(WAIT, "tailed events", || {
        let rows = tailed(&db);
        assert!(rows.len() <= count, "more events than expected: {rows:#?}");
        (rows.len() == count).then_some(rows)
    })
}

/// A fixture repo (a real Git repo with a commit) under the fake home.
fn repo_under(home: &Path) -> PathBuf {
    fixtures::repo(&home.join("code/app"), None)
}

/// The events `normal-multi-tool` maps to, in order.
fn normal_multi_tool_shapes() -> Vec<(String, Option<String>)> {
    vec![
        shape("SESSION_START", None),
        shape("PROMPT_SUBMIT", None),
        shape("TOOL_PRE", Some("Write")),
        shape("TOOL_POST", Some("Write")),
        shape("TOOL_PRE", Some("Read")),
        shape("TOOL_POST", Some("Read")),
        shape("TOOL_PRE", Some("Bash")),
        shape("TOOL_POST", Some("Bash")),
    ]
}

#[test]
fn a_tailer_only_session_is_fully_recorded() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let claude = claude_dir(&home);
    let repo = repo_under(&home);
    let lines = transcript_lines("normal-multi-tool", &repo, &home);
    let session = session_of(&lines);
    let path = transcript_path(&claude, &repo, &session);

    // Created after the tailer's first start: read from 0. No hook ever
    // authenticates for this session.
    append_lines(&path, &lines);

    let rows = wait_for_events(&daemon, 8);
    assert_eq!(shapes(&rows), normal_multi_tool_shapes());
    let repo_id = fixtures::git_id(&repo, None);
    for row in &rows {
        assert_eq!(row.attribution_source, "harness_log", "{row:?}");
        assert_eq!(row.network_class, "cloud_api", "{row:?}");
        assert_eq!(row.session_id.as_deref(), Some(session.as_str()));
        assert_eq!(row.cwd.as_deref(), Some(path_str(&repo)));
        assert_eq!(row.project_id.as_deref(), Some(repo_id.as_str()));
    }
    // Timestamps come from the records (2026-11-25T16:36:30.382Z is the
    // prompt), not from the daemon's clock.
    assert_eq!(rows[1].timestamp_ms, 1_795_624_590_382);
    assert_eq!(rows[0].timestamp_ms, rows[1].timestamp_ms);
    assert!(rows[1].prompt_char_len.is_some_and(|len| len > 0));
    daemon.shutdown();
}

/// The first index of a `user` line with prompt text in `lines`.
fn prompt_index(lines: &[String]) -> usize {
    lines
        .iter()
        .position(|line| {
            let value: Value = serde_json::from_str(line).expect("JSON");
            value["type"] == "user" && value["message"]["content"].is_string()
        })
        .expect("a prompt line")
}

#[test]
fn records_map_to_tool_failures_api_errors_and_nothing_for_a_blocked_prompt() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let claude = claude_dir(&home);
    let repo = repo_under(&home);
    let mut sessions = Vec::new();
    for scenario in ["tool-failure", "api-error-stop", "stop-track", "subagent"] {
        let lines = transcript_lines(scenario, &repo, &home);
        let session = session_of(&lines);
        append_lines(&transcript_path(&claude, &repo, &session), &lines);
        sessions.push(session);
    }
    // A subagent transcript is never tailed.
    let subagent_file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/claude-code/subagent/subagents/agent-a0308422526856e05.jsonl");
    let subagents = transcript_path(&claude, &repo, &sessions[3])
        .with_extension("")
        .join("subagents");
    fs::create_dir_all(&subagents).expect("subagents folder");
    fs::copy(
        &subagent_file,
        subagents.join("agent-a0308422526856e05.jsonl"),
    )
    .expect("copy subagent transcript");

    let rows = wait_for_events(&daemon, 14);
    let of = |session: &str| -> Vec<(String, Option<String>)> {
        let rows: Vec<Tailed> = rows
            .iter()
            .filter(|row| row.session_id.as_deref() == Some(session))
            .cloned()
            .collect();
        shapes(&rows)
    };
    assert_eq!(
        of(&sessions[0]),
        vec![
            shape("SESSION_START", None),
            shape("PROMPT_SUBMIT", None),
            shape("TOOL_PRE", Some("Read")),
            shape("TOOL_FAIL", Some("Read")),
            shape("TOOL_PRE", Some("Bash")),
            shape("TOOL_FAIL", Some("Bash")),
        ]
    );
    // `isApiErrorMessage: true` is the API-error record.
    assert_eq!(
        of(&sessions[1]),
        vec![
            shape("SESSION_START", None),
            shape("PROMPT_SUBMIT", None),
            shape("STOP_FAILURE", None),
        ]
    );
    // A `/stop-track` block writes only `system` records: no prompt.
    assert_eq!(of(&sessions[2]), vec![]);
    // The main transcript only; the task notification is a prompt (its
    // `UserPromptSubmit` hook fires too).
    assert_eq!(
        of(&sessions[3]),
        vec![
            shape("SESSION_START", None),
            shape("PROMPT_SUBMIT", None),
            shape("TOOL_PRE", Some("Agent")),
            shape("TOOL_POST", Some("Agent")),
            shape("PROMPT_SUBMIT", None),
        ]
    );
    daemon.shutdown();
}

#[test]
fn only_complete_lines_are_read() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let claude = claude_dir(&home);
    let repo = repo_under(&home);
    let lines = transcript_lines("normal-multi-tool", &repo, &home);
    let path = transcript_path(&claude, &repo, &session_of(&lines));
    let prompt = prompt_index(&lines);

    append_lines(&path, &lines[..prompt]);
    let (head, tail) = lines[prompt].split_at(lines[prompt].len() / 2);
    append(&path, head.as_bytes());
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(tailed(&daemon.live_db()), vec![], "a partial line was read");

    append(&path, format!("{tail}\n").as_bytes());
    let rows = wait_for_events(&daemon, 2);
    assert_eq!(
        shapes(&rows),
        vec![shape("SESSION_START", None), shape("PROMPT_SUBMIT", None)]
    );
    append_lines(&path, &lines[prompt + 1..]);
    let rows = wait_for_events(&daemon, 8);
    assert_eq!(shapes(&rows), normal_multi_tool_shapes());
    daemon.shutdown();
}

#[test]
fn a_malformed_line_is_skipped_and_the_rest_is_read() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let claude = claude_dir(&home);
    let repo = repo_under(&home);
    let mut lines = transcript_lines("normal-multi-tool", &repo, &home);
    let path = transcript_path(&claude, &repo, &session_of(&lines));
    let prompt = prompt_index(&lines);
    lines.insert(
        prompt + 1,
        "{\"type\": \"user\", \"sessionId\": tru".to_owned(),
    );
    lines.insert(
        prompt + 1,
        "{\"type\": \"user\", \"sessionId\": 7}".to_owned(),
    );
    lines.insert(prompt + 1, String::new());

    append_lines(&path, &lines);

    let rows = wait_for_events(&daemon, 8);
    assert_eq!(shapes(&rows), normal_multi_tool_shapes());
    daemon.shutdown();
}

#[test]
fn a_replaced_file_is_reread_without_duplicates() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let claude = claude_dir(&home);
    let repo = repo_under(&home);
    let lines = transcript_lines("normal-multi-tool", &repo, &home);
    let path = transcript_path(&claude, &repo, &session_of(&lines));
    let prompt = prompt_index(&lines);
    append_lines(&path, &lines[..=prompt]);
    wait_for_events(&daemon, 2);

    // A new file (new identity) renamed over the old one: read from 0.
    let replacement = path.with_extension("tmp");
    append_lines(&replacement, &lines);
    fs::rename(&replacement, &path).expect("replace transcript");

    let rows = wait_for_events(&daemon, 8);
    assert_eq!(shapes(&rows), normal_multi_tool_shapes());
    daemon.shutdown();
}

#[test]
fn a_truncated_file_is_reread_from_the_start() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let claude = claude_dir(&home);
    let repo = repo_under(&home);
    let lines = transcript_lines("normal-multi-tool", &repo, &home);
    let path = transcript_path(&claude, &repo, &session_of(&lines));
    append_lines(&path, &lines);
    wait_for_events(&daemon, 8);

    // Same file, now shorter than the stored offset: one prompt of
    // another session.
    let other = transcript_lines("api-error-stop", &repo, &home);
    let other_prompt = other[prompt_index(&other)].clone();
    fs::write(&path, format!("{other_prompt}\n")).expect("truncate and rewrite");

    let rows = wait_for_events(&daemon, 10);
    let other_session = session_of(&other);
    let new: Vec<Tailed> = rows
        .into_iter()
        .filter(|row| row.session_id.as_deref() == Some(other_session.as_str()))
        .collect();
    assert_eq!(
        shapes(&new),
        vec![shape("SESSION_START", None), shape("PROMPT_SUBMIT", None)]
    );
    daemon.shutdown();
}

#[test]
fn a_file_that_existed_before_the_first_start_begins_at_its_end() {
    let fixture = std::sync::Mutex::new(None);
    let mut daemon = TestDaemon::start_with(|config| {
        fast(config);
        let home = config.home_dir.clone().expect("fake home");
        let claude = claude_dir(&home);
        let repo = repo_under(&home);
        let lines = transcript_lines("normal-multi-tool", &repo, &home);
        let path = transcript_path(&claude, &repo, &session_of(&lines));
        let prompt = prompt_index(&lines);
        // History from before Traction was installed.
        append_lines(&path, &lines[..=prompt]);
        *fixture.lock().unwrap() = Some((path, lines, prompt));
    });
    let (path, lines, prompt) = fixture.into_inner().unwrap().expect("fixture");

    append_lines(&path, &lines[prompt + 1..]);

    // Only what was appended after the start: the prompt came before it.
    let rows = wait_for_events(&daemon, 7);
    let mut expected = normal_multi_tool_shapes();
    expected.remove(1);
    assert_eq!(shapes(&rows), expected);
    daemon.shutdown();
}

#[test]
fn a_restart_resumes_from_saved_offsets_with_nothing_lost_or_duplicated() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let claude = claude_dir(&home);
    let repo = repo_under(&home);
    let lines = transcript_lines("normal-multi-tool", &repo, &home);
    let path = transcript_path(&claude, &repo, &session_of(&lines));
    // The prompt and the first tool call.
    let split = lines
        .iter()
        .position(|line| line.contains("\"tool_result\""))
        .expect("a tool result")
        + 1;
    append_lines(&path, &lines[..split]);
    wait_for_events(&daemon, 4);

    daemon.shutdown();
    // Written while the daemon is down.
    append_lines(&path, &lines[split..]);
    let daemon = daemon.restart_with(fast);

    let rows = wait_for_events(&daemon, 8);
    assert_eq!(shapes(&rows), normal_multi_tool_shapes());
    // A later restart reads nothing again.
    let mut daemon = daemon.restart_with(fast);
    std::thread::sleep(Duration::from_millis(500));
    let db = daemon.shutdown_and_open_db();
    assert_eq!(shapes(&tailed(&db)), normal_multi_tool_shapes());
    // The resume point is the stored offset, not a reread from 0 absorbed
    // by the deterministic IDs.
    let stored: Vec<(String, i64)> = db
        .prepare("SELECT path, \"offset\" FROM transcript_offset")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect()
        })
        .expect("offsets");
    let size = i64::try_from(fs::metadata(&path).expect("transcript").len()).unwrap();
    assert_eq!(stored, vec![(path_str(&path).to_owned(), size)]);
}

#[test]
fn no_transcript_text_reaches_sqlite() {
    let mut daemon = TestDaemon::start_with(fast);
    let home = daemon.home();
    let claude = claude_dir(&home);
    let repo = repo_under(&home);
    for scenario in [
        "normal-multi-tool",
        "tool-failure",
        "permission-request",
        "subagent",
        "cwd-change",
        "interrupted-turn",
        "api-error-stop",
        "stop-track",
    ] {
        let lines = transcript_lines(scenario, &repo, &home);
        append_lines(
            &transcript_path(&claude, &repo, &session_of(&lines)),
            &lines,
        );
    }
    wait_for_events(&daemon, 40);

    let db = daemon.shutdown_and_open_db();
    assert_eq!(
        common::sentinel_hits_in_tables(&db, "TRACTION-CANARY"),
        0,
        "a canary string reached SQLite"
    );
    drop(db);
    common::assert_no_file_contains(daemon.data_dir(), "TRACTION-CANARY");
}
