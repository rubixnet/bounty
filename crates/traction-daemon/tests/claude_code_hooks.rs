//! Seam A: the Claude Code hook endpoint (`POST /v1/hooks/claude-code`)
//! through a real in-process daemon. Hook payloads are the real ones
//! captured from `claude -p` in `tests/data/claude-code/` (placeholders
//! swapped for temp paths), plus payloads shaped like them for hooks `-p`
//! never delivers (`SessionStart`, `DirectoryAdded`).

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use common::{
    BusResponse, MutationRow, TestDaemon, canonical_string, fast_watcher, fixtures, path_str,
    stored_event_count,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use traction_daemon::{CLAUDE_CODE_HOOK_PATH, DB_FILE, MAX_HOOK_BODY_BYTES, WINDOW_CLOSE_GRACE};

const HOOK_PATH: &str = "/v1/hooks/claude-code";

fn now_ms() -> u64 {
    u64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

/// The captured hook payloads of `scenario`, in arrival order, with every
/// scrub placeholder replaced: `<REPO>` by `repo`, the rest by folders
/// under `home`.
fn captured_hooks(scenario: &str, repo: &Path, home: &Path) -> Vec<Value> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/claude-code")
        .join(scenario)
        .join("hooks");
    let mut files: Vec<PathBuf> = fs::read_dir(&dir)
        .expect("fixture folder")
        .map(|entry| entry.expect("fixture entry").path())
        .collect();
    files.sort();
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
    files
        .iter()
        .map(|file| {
            let mut value: Value =
                serde_json::from_slice(&fs::read(file).expect("read fixture")).expect("JSON");
            substitute(&mut value, &substitutions);
            value
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

/// `POST /v1/hooks/claude-code` with a valid `Host` and token.
fn post_hook(daemon: &TestDaemon, body: impl AsRef<[u8]>) -> BusResponse {
    daemon.caller().request("POST", HOOK_PATH).body(body).send()
}

/// Sends a hook and asserts the only answer a Claude hook ever gets.
fn send_hook(daemon: &TestDaemon, payload: &Value) {
    let response = post_hook(daemon, payload.to_string());
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, "{}");
}

/// A payload shaped like the captured ones (common fields first).
fn hook(name: &str, session: &str, cwd: &Path, extra: Value) -> Value {
    let mut payload = json!({
        "session_id": session,
        "transcript_path": format!("/nowhere/{session}.jsonl"),
        "cwd": path_str(cwd),
        "prompt_id": "6cc0ac7e-854d-4139-8e0f-e0b52fd93a1d",
        "permission_mode": "acceptEdits",
        "hook_event_name": name,
    });
    for (key, value) in extra.as_object().expect("object").clone() {
        payload[key] = value;
    }
    payload
}

/// One stored Traction Event, as the hook endpoint must have written it.
#[derive(Debug, Clone)]
struct Row {
    event_id: String,
    timestamp_ms: i64,
    harness_id: Option<String>,
    session_id: Option<String>,
    parent_orchestrator_id: Option<String>,
    event_type: String,
    end_reason: Option<String>,
    cwd: Option<String>,
    project_id: Option<String>,
    model_id: Option<String>,
    network_class: String,
    attribution_source: String,
    prompt_char_len: Option<i64>,
    repo_symbol_hits: Option<i64>,
    has_stacktrace: Option<i64>,
    tool_name: Option<String>,
    exit_code: Option<i64>,
    diff_lines_delta: Option<i64>,
}

/// Every stored non-mutation event, in timestamp order.
fn stored_hook_events(db: &Connection) -> Vec<Row> {
    let mut stmt = db
        .prepare(
            "SELECT event_id, timestamp_ms, harness_id, session_id, parent_orchestrator_id,
                    event_type, end_reason, cwd, project_id, model_id, network_class,
                    attribution_source, prompt_char_len, repo_symbol_hits, has_stacktrace,
                    tool_name, exit_code, diff_lines_delta
             FROM traction_event WHERE event_type <> 'FILE_MUTATION'
             ORDER BY timestamp_ms, event_id",
        )
        .expect("prepare");
    stmt.query_map([], |row| {
        Ok(Row {
            event_id: row.get(0)?,
            timestamp_ms: row.get(1)?,
            harness_id: row.get(2)?,
            session_id: row.get(3)?,
            parent_orchestrator_id: row.get(4)?,
            event_type: row.get(5)?,
            end_reason: row.get(6)?,
            cwd: row.get(7)?,
            project_id: row.get(8)?,
            model_id: row.get(9)?,
            network_class: row.get(10)?,
            attribution_source: row.get(11)?,
            prompt_char_len: row.get(12)?,
            repo_symbol_hits: row.get(13)?,
            has_stacktrace: row.get(14)?,
            tool_name: row.get(15)?,
            exit_code: row.get(16)?,
            diff_lines_delta: row.get(17)?,
        })
    })
    .expect("query")
    .map(|row| row.expect("row"))
    .collect()
}

/// `(event_type, tool_name)` of `rows`, sorted (hooks sent within one
/// millisecond have no defined storage order).
fn shapes(rows: &[Row]) -> Vec<(String, Option<String>)> {
    let mut shapes: Vec<_> = rows
        .iter()
        .map(|row| (row.event_type.clone(), row.tool_name.clone()))
        .collect();
    shapes.sort();
    shapes
}

fn shape(event_type: &str, tool: Option<&str>) -> (String, Option<String>) {
    (event_type.to_owned(), tool.map(str::to_owned))
}

/// A hyphenated UUIDv7.
fn is_uuid_v7(id: &str) -> bool {
    let groups: Vec<&str> = id.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, len)| g.len() == len && g.bytes().all(|b| b.is_ascii_hexdigit()))
        && groups[2].starts_with('7')
}

/// The rules every hook-mapped event follows, whatever its hook.
fn assert_hook_event(row: &Row, session: &str, sent_from_ms: u64, sent_to_ms: u64) {
    assert!(is_uuid_v7(&row.event_id), "{row:?}");
    let ts = u64::try_from(row.timestamp_ms).unwrap();
    assert!(
        (sent_from_ms..=sent_to_ms).contains(&ts),
        "timestamp_ms is the receive time: {row:?}"
    );
    assert_eq!(row.harness_id.as_deref(), Some("claude-code"), "{row:?}");
    assert_eq!(row.session_id.as_deref(), Some(session), "{row:?}");
    assert_eq!(row.attribution_source, "harness_hook", "{row:?}");
    assert_eq!(row.network_class, "cloud_api", "{row:?}");
    assert_eq!(row.parent_orchestrator_id, None, "{row:?}");
    assert_eq!(row.repo_symbol_hits, None, "Step 2 field: {row:?}");
    assert_eq!(row.exit_code, None, "Step 2 field: {row:?}");
    assert_eq!(row.diff_lines_delta, None, "Step 2 field: {row:?}");
    let prompt = row.event_type == "PROMPT_SUBMIT";
    assert_eq!(row.prompt_char_len.is_some(), prompt, "{row:?}");
    assert_eq!(row.has_stacktrace.is_some(), prompt, "{row:?}");
    assert_eq!(row.end_reason.is_some(), row.event_type == "TURN_END");
}

/// Replays a captured scenario into a fresh daemon on a real repo and
/// returns the stored events, after checking the per-event rules.
fn replay(scenario: &str) -> (Vec<Row>, String, PathBuf) {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    let hooks = captured_hooks(scenario, &repo, &daemon.home());
    let session = hooks[0]["session_id"].as_str().unwrap().to_owned();
    let from = now_ms();
    for payload in &hooks {
        send_hook(&daemon, payload);
    }
    let to = now_ms();
    let db = daemon.shutdown_and_open_db();
    let rows = stored_hook_events(&db);
    for row in &rows {
        assert_hook_event(row, &session, from, to);
        assert_eq!(
            row.project_id.as_deref(),
            Some(fixtures::git_id(&repo, None).as_str())
        );
    }
    (rows, session, repo)
}

/// `PROMPT_SUBMIT`s a tailer-only daemon stores from `scenario`'s
/// transcript.
fn transcript_prompt_count(scenario: &str) -> usize {
    let mut daemon = TestDaemon::start_with(|config| {
        config.writer.commit_interval = Duration::from_millis(50);
    });
    let folder = daemon.home().join(".claude/projects/-repo");
    fs::create_dir_all(&folder).expect("projects folder");
    let file = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/claude-code")
        .join(scenario)
        .join("transcript.jsonl");
    let repo = serde_json::to_string(common::path_str(&daemon.home())).expect("JSON");
    let text = fs::read_to_string(file)
        .expect("fixture transcript")
        .replace("<REPO>", repo.trim_matches('"'));
    fs::write(folder.join("parity.jsonl"), &text).expect("transcript");
    // The last record that maps is stored once the whole file was read.
    let db = daemon.live_db();
    common::poll_until(Duration::from_secs(30), "tailed rows", || {
        let tools: i64 = db
            .query_row(
                "SELECT count(*) FROM traction_event WHERE event_type = 'TOOL_POST'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        (tools > 0).then_some(())
    });
    let db = daemon.shutdown_and_open_db();
    db.query_row(
        "SELECT count(*) FROM traction_event WHERE event_type = 'PROMPT_SUBMIT'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .expect("count")
    .try_into()
    .expect("count fits")
}

// --- Mapping, with the captured payloads ----------------------------------

#[test]
fn a_normal_session_maps_prompt_tool_calls_turn_end_and_session_end() {
    let (rows, _, _) = replay("normal-multi-tool");
    let mut expected = vec![
        shape("PROMPT_SUBMIT", None),
        shape("TOOL_PRE", Some("Write")),
        shape("TOOL_POST", Some("Write")),
        shape("TOOL_PRE", Some("Read")),
        shape("TOOL_POST", Some("Read")),
        shape("TOOL_PRE", Some("Bash")),
        shape("TOOL_POST", Some("Bash")),
        shape("TURN_END", None),
        shape("SESSION_END", None),
    ];
    expected.sort();
    assert_eq!(shapes(&rows), expected);
    let turn_end = rows.iter().find(|r| r.event_type == "TURN_END").unwrap();
    assert_eq!(turn_end.end_reason.as_deref(), Some("completed"));
    let prompt = rows
        .iter()
        .find(|r| r.event_type == "PROMPT_SUBMIT")
        .unwrap();
    // The captured prompt is 188 characters long and holds no stack trace.
    assert_eq!(prompt.prompt_char_len, Some(188));
    assert_eq!(prompt.has_stacktrace, Some(0));
}

#[test]
fn a_failed_tool_call_maps_to_tool_fail() {
    let (rows, _, _) = replay("tool-failure");
    let mut expected = vec![
        shape("PROMPT_SUBMIT", None),
        shape("TOOL_PRE", Some("Read")),
        shape("TOOL_FAIL", Some("Read")),
        shape("TOOL_PRE", Some("Bash")),
        shape("TOOL_FAIL", Some("Bash")),
        shape("TURN_END", None),
        shape("SESSION_END", None),
    ];
    expected.sort();
    assert_eq!(shapes(&rows), expected);
}

#[test]
fn a_permission_request_keeps_its_tool_name() {
    let (rows, _, _) = replay("permission-request");
    let mut expected = vec![
        shape("PROMPT_SUBMIT", None),
        shape("TOOL_PRE", Some("Bash")),
        shape("PERMISSION_REQUEST", Some("Bash")),
        shape("TURN_END", None),
        shape("SESSION_END", None),
    ];
    expected.sort();
    assert_eq!(shapes(&rows), expected);
}

#[test]
fn subagent_hooks_map_to_subagent_start_and_stop() {
    let (rows, _, _) = replay("subagent");
    let mut expected = vec![
        shape("PROMPT_SUBMIT", None),
        shape("TOOL_PRE", Some("Agent")),
        shape("SUBAGENT_START", None),
        shape("TOOL_POST", Some("Agent")),
        shape("TOOL_PRE", Some("Read")),
        shape("TOOL_POST", Some("Read")),
        shape("TURN_END", None),
        shape("SUBAGENT_STOP", None),
        shape("TURN_END", None),
        shape("SESSION_END", None),
    ];
    expected.sort();
    assert_eq!(shapes(&rows), expected);
    // Parity with the transcript of the same session: the user typed one
    // prompt; the task notification is none on either path.
    assert_eq!(transcript_prompt_count("subagent"), 1);
    let hook_prompts = rows.iter().filter(|r| r.event_type == "PROMPT_SUBMIT");
    assert_eq!(hook_prompts.count(), 1);
}

#[test]
fn an_api_error_stop_maps_to_stop_failure() {
    let (rows, _, _) = replay("api-error-stop");
    let mut expected = vec![
        shape("PROMPT_SUBMIT", None),
        shape("STOP_FAILURE", None),
        shape("SESSION_END", None),
    ];
    expected.sort();
    assert_eq!(shapes(&rows), expected);
}

#[test]
fn an_interrupted_turn_maps_like_a_completed_one() {
    let (rows, _, _) = replay("interrupted-turn");
    let mut expected = vec![
        shape("PROMPT_SUBMIT", None),
        shape("TOOL_PRE", Some("Bash")),
        shape("TOOL_POST", Some("Bash")),
        shape("TURN_END", None),
        shape("SESSION_END", None),
    ];
    expected.sort();
    assert_eq!(shapes(&rows), expected);
}

#[test]
fn cwd_changed_maps_to_cwd_change_and_moves_later_events_to_the_new_project() {
    let mut daemon = TestDaemon::start();
    // Two folder Projects: the session starts in `repo` and `cd`s into
    // `repo/sub`, which has its own marker.
    let repo = fixtures::folder(&daemon.home().join("repo"), &["package.json"]);
    let sub = fixtures::folder(&repo.join("sub"), &["Cargo.toml"]);
    let hooks = captured_hooks("cwd-change", &repo, &daemon.home());
    let mut sent = Vec::new();
    for payload in &hooks {
        send_hook(&daemon, payload);
        sent.push(payload["hook_event_name"].as_str().unwrap().to_owned());
        // Distinct receive times, so the stored order is the hook order.
        std::thread::sleep(Duration::from_millis(3));
    }
    let repo_id = daemon.dir_id(&repo);
    let sub_id = daemon.dir_id(&sub);
    let db = daemon.shutdown_and_open_db();
    let rows = stored_hook_events(&db);

    let got: Vec<(&str, Option<&str>)> = rows
        .iter()
        .map(|r| (r.event_type.as_str(), r.project_id.as_deref()))
        .collect();
    let (r, s) = (Some(repo_id.as_str()), Some(sub_id.as_str()));
    assert_eq!(
        got,
        vec![
            ("PROMPT_SUBMIT", r),
            ("TOOL_PRE", r),
            ("CWD_CHANGE", s),
            ("TOOL_POST", s),
            ("TOOL_PRE", s),
            ("TOOL_POST", s),
            ("TOOL_PRE", s),
            ("TOOL_POST", s),
            ("TURN_END", s),
            ("SESSION_END", s),
        ],
        "hooks sent: {sent:?}"
    );
    let change = rows.iter().find(|r| r.event_type == "CWD_CHANGE").unwrap();
    // Compared as paths: the reported string is kept, and the captured
    // `<REPO>/sub` mixes separators on Windows.
    assert_eq!(
        change.cwd.as_deref().map(Path::new),
        Some(&*repo.join("sub"))
    );
}

#[test]
fn session_start_maps_with_its_cwd_and_model_id() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    let from = now_ms();
    send_hook(
        &daemon,
        &hook(
            "SessionStart",
            "start-1",
            &repo,
            json!({"source": "startup", "model": "claude-sonnet-4-5-20250929"}),
        ),
    );
    // A model ID that is not a safe identifier is dropped, not the event.
    send_hook(
        &daemon,
        &hook(
            "SessionStart",
            "start-2",
            &repo,
            json!({"source": "resume", "model": "a model with spaces"}),
        ),
    );
    let to = now_ms();
    let db = daemon.shutdown_and_open_db();
    let rows = stored_hook_events(&db);
    assert_eq!(rows.len(), 2, "{rows:?}");
    let first = rows
        .iter()
        .find(|r| r.session_id.as_deref() == Some("start-1"))
        .unwrap();
    assert_hook_event(first, "start-1", from, to);
    assert_eq!(first.event_type, "SESSION_START");
    assert_eq!(first.cwd.as_deref(), Some(path_str(&repo)));
    assert_eq!(
        first.model_id.as_deref(),
        Some("claude-sonnet-4-5-20250929")
    );
    let second = rows
        .iter()
        .find(|r| r.session_id.as_deref() == Some("start-2"))
        .unwrap();
    assert_eq!(second.event_type, "SESSION_START");
    assert_eq!(second.model_id, None);
}

#[test]
fn a_prompt_keeps_only_its_length_and_stack_trace_flag() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    // 16 characters (the `é` is two bytes).
    send_hook(
        &daemon,
        &hook(
            "UserPromptSubmit",
            "plain",
            &repo,
            json!({"prompt": "café is closed??"}),
        ),
    );
    let trace = "why does this fail?\nTraceback (most recent call last):\n  File \"app.py\", line 3, in <module>\n    main()\nValueError: boom";
    send_hook(
        &daemon,
        &hook("UserPromptSubmit", "trace", &repo, json!({"prompt": trace})),
    );
    let db = daemon.shutdown_and_open_db();
    let rows = stored_hook_events(&db);
    let by = |s: &str| {
        rows.iter()
            .find(|r| r.session_id.as_deref() == Some(s))
            .unwrap()
    };
    assert_eq!(by("plain").prompt_char_len, Some(16));
    assert_eq!(by("plain").has_stacktrace, Some(0));
    assert_eq!(by("trace").prompt_char_len, Some(119));
    assert_eq!(by("trace").has_stacktrace, Some(1));
}

#[test]
fn a_long_tool_name_is_capped_and_an_unsafe_one_dropped() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    let long = format!("mcp__server__{}", "x".repeat(200));
    send_hook(
        &daemon,
        &hook(
            "PreToolUse",
            "long",
            &repo,
            json!({"tool_name": long, "tool_input": {}}),
        ),
    );
    send_hook(
        &daemon,
        &hook(
            "PreToolUse",
            "unsafe",
            &repo,
            json!({"tool_name": "rm -rf /", "tool_input": {}}),
        ),
    );
    let db = daemon.shutdown_and_open_db();
    let rows = stored_hook_events(&db);
    let by = |s: &str| {
        rows.iter()
            .find(|r| r.session_id.as_deref() == Some(s))
            .unwrap()
    };
    let capped = format!("mcp__server__{}", "x".repeat(64 - "mcp__server__".len()));
    assert_eq!(by("long").tool_name.as_deref(), Some(capped.as_str()));
    assert_eq!(by("unsafe").event_type, "TOOL_PRE");
    assert_eq!(by("unsafe").tool_name, None);
}

#[test]
fn directory_added_registers_the_folder_as_a_project_and_stores_no_event() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    let extra = fixtures::folder(&daemon.home().join("extra"), &[]);
    send_hook(
        &daemon,
        &hook(
            "DirectoryAdded",
            "add-dir",
            &repo,
            json!({"directory": path_str(&extra), "source": "slash_command"}),
        ),
    );
    let extra_id = daemon.dir_id(&extra);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
    let roots: Vec<(String, String)> = db
        .prepare("SELECT project_id, root_path FROM project")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(roots, vec![(extra_id, canonical_string(&extra))]);
}

#[test]
fn hooks_outside_the_mapping_store_nothing() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    for name in [
        "WorktreeCreate",
        "Notification",
        "PreCompact",
        "FileChanged",
        "InstructionsLoaded",
        "SomeFutureHook",
    ] {
        send_hook(&daemon, &hook(name, "other", &repo, json!({"name": "x"})));
    }
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
}

#[test]
fn unknown_fields_and_large_tool_output_are_accepted_and_reduced_to_metadata() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    // Well over the 16 KiB `/v1/event` cap, inside the 8 MiB hook cap.
    let output = "y".repeat(4 * 1024 * 1024);
    send_hook(
        &daemon,
        &hook(
            "PostToolUse",
            "big",
            &repo,
            json!({
                "tool_name": "Read",
                "tool_input": {"file_path": "/a"},
                "tool_response": {"stdout": output},
                "a_field_from_a_future_version": {"nested": [1, 2, 3]},
            }),
        ),
    );
    let db = daemon.shutdown_and_open_db();
    let rows = stored_hook_events(&db);
    assert_eq!(shapes(&rows), vec![shape("TOOL_POST", Some("Read"))]);
}

// --- Malformed input, caps and refusals -----------------------------------

#[test]
fn a_body_that_is_not_a_usable_hook_gets_200_and_stores_nothing() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    let no_session = {
        let mut p = hook("PreToolUse", "s", &repo, json!({"tool_name": "Bash"}));
        p.as_object_mut().unwrap().remove("session_id");
        p.to_string()
    };
    let no_name = {
        let mut p = hook("PreToolUse", "s", &repo, json!({"tool_name": "Bash"}));
        p.as_object_mut().unwrap().remove("hook_event_name");
        p.to_string()
    };
    let bad_session = hook("PreToolUse", "has spaces", &repo, json!({})).to_string();
    for body in [
        "not json".to_owned(),
        String::new(),
        "[1,2]".to_owned(),
        "{\"session_id\": 5, \"hook_event_name\": \"Stop\"}".to_owned(),
        no_session,
        no_name,
        bad_session,
    ] {
        let response = post_hook(&daemon, &body);
        assert_eq!(response.status, 200, "{body}: {}", response.body);
        assert_eq!(response.body, "{}", "{body}");
    }
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
}

#[test]
fn a_body_over_the_hook_cap_is_refused_with_413() {
    assert_eq!(CLAUDE_CODE_HOOK_PATH, HOOK_PATH);
    assert_eq!(MAX_HOOK_BODY_BYTES, 8 * 1024 * 1024);
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    let mut payload = hook("PostToolUse", "huge", &repo, json!({"tool_name": "Read"}));
    let base = payload.to_string().len();
    payload["pad"] = json!("z".repeat(MAX_HOOK_BODY_BYTES - base));
    let body = payload.to_string();
    assert!(body.len() > MAX_HOOK_BODY_BYTES);
    let response = post_hook(&daemon, &body);
    assert_eq!(response.status, 413, "{}", response.body);
    assert_eq!(response.body, r#"{"error":"body_too_large"}"#);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
}

#[test]
fn the_hook_route_has_the_same_host_origin_and_token_checks_as_v1_event() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    let caller = daemon.caller();
    let port = caller.port();
    let body = hook("Stop", "refused", &repo, json!({})).to_string();
    let cases = [
        (
            caller
                .request("POST", HOOK_PATH)
                .header("Host", &format!("evil.example:{port}")),
            403,
            "forbidden_host",
        ),
        (
            caller
                .request("POST", HOOK_PATH)
                .header("Origin", "https://evil.example"),
            403,
            "forbidden_origin",
        ),
        (
            caller
                .request("POST", HOOK_PATH)
                .without_header("Authorization"),
            401,
            "unauthorized",
        ),
        (
            caller
                .request("POST", HOOK_PATH)
                .header("Authorization", "Bearer wrong"),
            401,
            "unauthorized",
        ),
        (caller.request("GET", HOOK_PATH), 405, "method_not_allowed"),
    ];
    for (request, status, code) in cases {
        let response = request.body(&body).send();
        assert_eq!(response.status, status, "{}", response.body);
        assert_eq!(response.body, format!("{{\"error\":\"{code}\"}}"));
    }
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
}

#[cfg(unix)]
#[test]
fn the_hook_route_works_over_the_unix_socket_without_a_host() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    let response = daemon
        .socket_caller()
        .request("POST", HOOK_PATH)
        .without_header("Host")
        .body(hook("Stop", "via-socket", &repo, json!({})).to_string())
        .send();
    assert_eq!(response.status, 200, "{}", response.body);
    assert_eq!(response.body, "{}");
    let refused = daemon
        .socket_caller()
        .request("POST", HOOK_PATH)
        .without_header("Authorization")
        .body("{}")
        .send();
    assert_eq!(refused.status, 401);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(
        shapes(&stored_hook_events(&db)),
        vec![shape("TURN_END", None)]
    );
}

#[test]
fn a_full_router_queue_still_answers_200_at_once_and_drops_the_event() {
    let mut daemon = TestDaemon::start_with(|config| {
        config.writer.max_batch = 1;
        config.writer.queue_capacity = 1;
    });
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    // Another process holds SQLite's write lock, so the writer is stuck.
    let lock = Connection::open(daemon.data_dir().join(DB_FILE)).unwrap();
    lock.execute_batch("BEGIN IMMEDIATE").unwrap();

    let sent = 10;
    for n in 0..sent {
        let started = Instant::now();
        let payload = hook(
            "PreToolUse",
            &format!("s{n}"),
            &repo,
            json!({"tool_name": "Bash"}),
        );
        let response = post_hook(&daemon, payload.to_string());
        assert_eq!((response.status, response.body.as_str()), (200, "{}"));
        // A blocking bus would wait at least SQLite's 5 s busy timeout.
        assert!(started.elapsed() < Duration::from_secs(4));
    }
    drop(lock);
    let db = daemon.shutdown_and_open_db();
    let stored = stored_hook_events(&db).len();
    assert!(
        stored < sent,
        "a full queue must drop events (stored {stored})"
    );
}

// --- Zero raw content ------------------------------------------------------

/// Every byte of every file the daemon keeps in its data folder.
fn data_folder_bytes(data_dir: &Path) -> Vec<u8> {
    let mut bytes = Vec::new();
    for entry in fs::read_dir(data_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            bytes.extend(fs::read(&path).unwrap());
        }
    }
    bytes
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[test]
fn canary_strings_in_hook_payloads_never_reach_sqlite() {
    let mut daemon = TestDaemon::start();
    let repo = fixtures::repo(&daemon.home().join("repo"), None);
    let session = "canary-session";
    let canaries = [
        "CANARY-PROMPT-7f3a",
        "CANARY-TOOL-INPUT-91bc",
        "CANARY-FILE-CONTENT-02de",
        "CANARY-TOOL-OUTPUT-c4e1",
        "CANARY-ASSISTANT-5a77",
        "CANARY-ERROR-e0f9",
    ];
    let payloads = [
        hook(
            "UserPromptSubmit",
            session,
            &repo,
            json!({"prompt": "fix CANARY-PROMPT-7f3a\n  at f (a.js:1:2)\n  at g (b.js:3:4)"}),
        ),
        hook(
            "PreToolUse",
            session,
            &repo,
            json!({"tool_name": "Write", "tool_input": {
                "file_path": "/x/CANARY-TOOL-INPUT-91bc",
                "content": "CANARY-FILE-CONTENT-02de"}}),
        ),
        hook(
            "PostToolUse",
            session,
            &repo,
            json!({"tool_name": "Write",
                "tool_input": {"content": "CANARY-FILE-CONTENT-02de"},
                "tool_response": {"stdout": "CANARY-TOOL-OUTPUT-c4e1", "stderr": "CANARY-TOOL-OUTPUT-c4e1"}}),
        ),
        hook(
            "PostToolUseFailure",
            session,
            &repo,
            json!({"tool_name": "Bash", "tool_input": {"command": "CANARY-TOOL-INPUT-91bc"},
                "error": "CANARY-ERROR-e0f9"}),
        ),
        hook(
            "PermissionRequest",
            session,
            &repo,
            json!({"tool_name": "Bash", "tool_input": {"command": "CANARY-TOOL-INPUT-91bc"}}),
        ),
        hook(
            "Stop",
            session,
            &repo,
            json!({"last_assistant_message": "CANARY-ASSISTANT-5a77"}),
        ),
        hook(
            "StopFailure",
            session,
            &repo,
            json!({"error": "CANARY-ERROR-e0f9", "last_assistant_message": "CANARY-ASSISTANT-5a77"}),
        ),
        hook(
            "SubagentStop",
            session,
            &repo,
            json!({"last_assistant_message": "CANARY-ASSISTANT-5a77"}),
        ),
    ];
    for payload in &payloads {
        send_hook(&daemon, payload);
    }
    // And every captured scenario, whose prompts carry TRACTION-CANARY-*.
    for scenario in [
        "normal-multi-tool",
        "tool-failure",
        "permission-request",
        "subagent",
        "cwd-change",
        "interrupted-turn",
        "api-error-stop",
    ] {
        for payload in captured_hooks(scenario, &repo, &daemon.home()) {
            send_hook(&daemon, &payload);
        }
    }

    let db = daemon.shutdown_and_open_db();
    assert!(stored_event_count(&db) > payloads.len() as i64);
    let ours = stored_hook_events(&db)
        .into_iter()
        .filter(|r| r.session_id.as_deref() == Some(session))
        .count();
    assert_eq!(ours, payloads.len());
    drop(db);
    let bytes = data_folder_bytes(daemon.data_dir());
    for canary in canaries.iter().copied().chain(["TRACTION-CANARY"]) {
        assert!(
            !contains(&bytes, canary),
            "{canary} reached the data folder"
        );
    }
}

// --- Attribution through real hook windows ---------------------------------

fn wait_past(ts_ms: u64) {
    let grace = u64::try_from(WINDOW_CLOSE_GRACE.as_millis()).unwrap();
    let target = ts_ms + grace + 300;
    let now = now_ms();
    if now < target {
        std::thread::sleep(Duration::from_millis(target - now));
    }
}

fn assert_claude(row: &MutationRow, session: &str) {
    assert_eq!(row.harness_id.as_deref(), Some("claude-code"), "{row:?}");
    assert_eq!(row.session_id.as_deref(), Some(session), "{row:?}");
    assert_eq!(row.network_class, "cloud_api", "{row:?}");
}

fn assert_manual(row: &MutationRow) {
    assert_eq!(row.harness_id, None, "{row:?}");
    assert_eq!(row.session_id, None, "{row:?}");
    assert_eq!(row.network_class, "manual", "{row:?}");
}

#[test]
fn edits_inside_a_pre_to_post_tool_use_window_go_to_claude_and_between_calls_are_manual() {
    let daemon = TestDaemon::start_with(fast_watcher);
    let root = fixtures::folder(&daemon.home().join("game"), &[]);
    let registered = daemon.caller().register_project(&root);
    assert_eq!(registered.status, 200, "{}", registered.body);
    std::thread::sleep(Duration::from_millis(500));
    // UserPromptSubmit, PreToolUse(Write), PostToolUse(Write),
    // PreToolUse(Read), PostToolUse(Read), ...
    let hooks = captured_hooks("normal-multi-tool", &root, &daemon.home());
    let session = hooks[0]["session_id"].as_str().unwrap().to_owned();

    send_hook(&daemon, &hooks[0]);
    send_hook(&daemon, &hooks[1]);
    fs::write(root.join("notes.txt"), "written by Claude's Write tool").unwrap();
    send_hook(&daemon, &hooks[2]);
    let post = now_ms();
    let rows = daemon.wait_for_mutations(1);
    assert_claude(&rows[0], &session);

    // Between tool calls, with the turn still open: the user's own edit.
    wait_past(post);
    fs::write(root.join("mine.txt"), "by hand").unwrap();
    let rows = daemon.wait_for_mutations(2);
    assert_manual(&rows[1]);

    // An edit whose notification lands just after PostToolUse, inside
    // WINDOW_CLOSE_GRACE, is still Claude's.
    send_hook(&daemon, &hooks[3]);
    send_hook(&daemon, &hooks[4]);
    fs::write(root.join("late.txt"), "notified after the tool ended").unwrap();
    let rows = daemon.wait_for_mutations(3);
    assert_claude(&rows[2], &session);
}
