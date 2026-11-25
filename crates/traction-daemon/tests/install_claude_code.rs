//! `traction-daemon install claude-code` (Seam B): the real binary merges
//! Traction's HTTP hooks into a temp `settings.json`, with a fake `claude`
//! program printing chosen versions. Nothing here touches the user's real
//! Claude Code config: every run passes `--settings` and `--claude-program`
//! (or a temp `CLAUDE_CONFIG_DIR`).

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;
use traction_daemon::CLAUDE_CODE_HOOK_PATH;

const BIN: &str = env!("CARGO_BIN_EXE_traction-daemon");

/// Every hook the installer must wire, in install order. Tool hooks sit in
/// an all-tools matcher group.
const HOOKS: [&str; 13] = [
    "SessionStart",
    "UserPromptSubmit",
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "SubagentStart",
    "SubagentStop",
    "CwdChanged",
    "DirectoryAdded",
    "Stop",
    "StopFailure",
    "SessionEnd",
];
const TOOL_HOOKS: [&str; 4] = [
    "PermissionRequest",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
];

/// A temp Claude config folder plus a fake `claude` program.
struct Setup {
    dir: TempDir,
    claude: PathBuf,
}

impl Setup {
    fn new() -> Self {
        Self::with_claude_output("2.1.332 (Claude Code)")
    }

    /// The fake `claude --version` prints `output` and exits 0.
    fn with_claude_output(output: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let claude = fake_program(dir.path(), "claude", output);
        Self { dir, claude }
    }

    fn settings(&self) -> PathBuf {
        self.dir.path().join("claude").join("settings.json")
    }

    fn backup(&self) -> PathBuf {
        self.dir
            .path()
            .join("claude")
            .join("settings.json.traction-backup")
    }

    fn write_settings(&self, contents: &str) {
        fs::create_dir_all(self.settings().parent().unwrap()).unwrap();
        fs::write(self.settings(), contents).unwrap();
    }

    fn read_settings(&self) -> String {
        fs::read_to_string(self.settings()).expect("settings.json exists")
    }

    fn settings_json(&self) -> Value {
        serde_json::from_str(&self.read_settings()).expect("settings.json is JSON")
    }

    fn command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(["install", "claude-code", "--settings"])
            .arg(self.settings())
            .arg("--claude-program")
            .arg(&self.claude)
            .stdin(Stdio::null());
        command
    }

    fn install(&self) -> Output {
        self.command().output().expect("run installer")
    }

    fn install_ok(&self) -> Output {
        let output = self.install();
        assert!(
            output.status.success(),
            "installer failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

/// A program at `<dir>/bin/<name>` that prints `output` and exits 0.
fn fake_program(dir: &Path, name: &str, output: &str) -> PathBuf {
    let dir = dir.join("bin");
    fs::create_dir_all(&dir).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        fs::write(&path, format!("#!/bin/sh\necho '{output}'\n")).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }
    #[cfg(windows)]
    {
        let path = dir.join(format!("{name}.cmd"));
        fs::write(&path, format!("@echo off\r\necho {output}\r\n")).unwrap();
        path
    }
}

fn hook_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CLAUDE_CODE_HOOK_PATH}")
}

/// The Traction entry the spec prescribes for `port`.
fn traction_entry(port: u16) -> Value {
    json!({
        "type": "http",
        "url": hook_url(port),
        "timeout": 1,
        "onFailure": "continue",
        "headers": {"Authorization": "Bearer $TRACTION_TOKEN"},
        "allowedEnvVars": ["TRACTION_TOKEN"],
    })
}

/// Every hook entry under `settings.hooks.<event>`, across matcher groups.
fn entries<'a>(settings: &'a Value, event: &str) -> Vec<&'a Value> {
    settings["hooks"][event]
        .as_array()
        .map(|groups| {
            groups
                .iter()
                .flat_map(|group| group["hooks"].as_array().into_iter().flatten())
                .collect()
        })
        .unwrap_or_default()
}

fn is_traction(entry: &Value) -> bool {
    entry["url"]
        .as_str()
        .is_some_and(|url| url.ends_with(CLAUDE_CODE_HOOK_PATH))
}

fn traction_entries<'a>(settings: &'a Value, event: &str) -> Vec<&'a Value> {
    entries(settings, event)
        .into_iter()
        .filter(|entry| is_traction(entry))
        .collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn a_missing_settings_file_is_created_with_one_traction_entry_per_hook() {
    let setup = Setup::new();

    setup.install_ok();

    let settings = setup.settings_json();
    for hook in HOOKS {
        let groups = settings["hooks"][hook].as_array().expect(hook);
        assert_eq!(groups.len(), 1, "{hook}: {groups:?}");
        let mut expected_group = json!({"hooks": [traction_entry(41937)]});
        if TOOL_HOOKS.contains(&hook) {
            expected_group["matcher"] = json!("*");
        }
        assert_eq!(groups[0], expected_group, "{hook}");
    }
    let installed: Vec<&str> = settings["hooks"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(installed.len(), HOOKS.len(), "{installed:?}");
    assert!(!setup.backup().exists(), "nothing to back up");
}

/// A user file with foreign hooks (one sharing a Traction event key),
/// foreign settings, and keys deliberately out of alphabetical order.
const USER_SETTINGS: &str = r#"{
  "permissions": {"deny": ["Read(./.env)"], "allow": ["Bash(ls:*)"]},
  "model": "opus",
  "hooks": {
    "Stop": [{"hooks": [{"type": "command", "command": "notify-send done"}]}],
    "PreToolUse": [
      {"matcher": "Bash", "hooks": [{"type": "command", "command": "./guard.sh", "timeout": 30}]}
    ],
    "Notification": [{"matcher": "", "hooks": [{"type": "command", "command": "say hi"}]}]
  },
  "env": {"ZED": "1", "ALPHA": "2"}
}
"#;

#[test]
fn foreign_hooks_and_settings_are_kept_in_file_order() {
    let setup = Setup::new();
    setup.write_settings(USER_SETTINGS);

    setup.install_ok();

    let text = setup.read_settings();
    let settings: Value = serde_json::from_str(&text).unwrap();
    let original: Value = serde_json::from_str(USER_SETTINGS).unwrap();
    for key in ["permissions", "model", "env"] {
        assert_eq!(settings[key], original[key], "{key}");
    }
    assert_eq!(
        settings["hooks"]["Notification"],
        original["hooks"]["Notification"]
    );
    // A Traction event key the user already uses keeps the user's group first.
    let stop = settings["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(stop[0], original["hooks"]["Stop"][0]);
    assert_eq!(stop[1], json!({"hooks": [traction_entry(41937)]}));
    let pre = settings["hooks"]["PreToolUse"].as_array().unwrap();
    assert_eq!(pre[0], original["hooks"]["PreToolUse"][0]);
    assert_eq!(
        pre[1],
        json!({"matcher": "*", "hooks": [traction_entry(41937)]})
    );

    // Keys stay where the user put them (serde_json's Map would sort them).
    let positions: Vec<usize> = [
        "\"permissions\"",
        "\"deny\"",
        "\"allow\"",
        "\"model\"",
        "\"hooks\"",
        "\"Stop\"",
        "\"PreToolUse\"",
        "\"Notification\"",
        "\"env\"",
        "\"ZED\"",
        "\"ALPHA\"",
    ]
    .iter()
    .map(|key| text.find(key).unwrap_or_else(|| panic!("{key} missing")))
    .collect();
    assert!(positions.is_sorted(), "key order changed:\n{text}");
}

#[test]
fn a_second_run_leaves_the_file_byte_identical() {
    let setup = Setup::new();
    setup.write_settings(USER_SETTINGS);

    setup.install_ok();
    let first = setup.read_settings();
    setup.install_ok();

    assert_eq!(setup.read_settings(), first);
    for hook in HOOKS {
        assert_eq!(
            traction_entries(&setup.settings_json(), hook).len(),
            1,
            "{hook}"
        );
    }
}

#[test]
fn an_upgrade_replaces_old_traction_entries_on_any_port() {
    let setup = Setup::new();
    // An older install on another port, with an older entry shape, sharing
    // a group with a foreign hook, plus an obsolete Traction-only event.
    setup.write_settings(&format!(
        r#"{{
  "hooks": {{
    "Stop": [{{"hooks": [
      {{"type": "command", "command": "notify-send done"}},
      {{"type": "http", "url": "{old}", "timeout": 5}}
    ]}}],
    "SessionStart": [{{"hooks": [{{"type": "http", "url": "{old}"}}]}}],
    "Notification": [{{"hooks": [{{"type": "http", "url": "{old}"}}]}}]
  }}
}}"#,
        old = hook_url(40000)
    ));

    setup.install_ok();

    let settings = setup.settings_json();
    for hook in HOOKS {
        assert_eq!(
            traction_entries(&settings, hook),
            vec![&traction_entry(41937)],
            "{hook}"
        );
    }
    let stop = settings["hooks"]["Stop"].as_array().unwrap();
    assert_eq!(
        stop[0],
        json!({"hooks": [{"type": "command", "command": "notify-send done"}]})
    );
    assert!(
        settings["hooks"].get("Notification").is_none(),
        "an event left with no entries is dropped: {settings}"
    );
}

#[test]
fn the_port_option_sets_the_hook_url() {
    let setup = Setup::new();

    let output = setup.command().args(["--port", "45000"]).output().unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    for hook in HOOKS {
        assert_eq!(
            traction_entries(&setup.settings_json(), hook),
            vec![&traction_entry(45000)]
        );
    }
}

#[test]
fn worktree_create_is_never_written_and_a_traction_entry_there_is_removed() {
    let setup = Setup::new();
    setup.write_settings(&format!(
        r#"{{"hooks": {{"WorktreeCreate": [
            {{"hooks": [{{"type": "http", "url": "{url}"}}]}},
            {{"hooks": [{{"type": "command", "command": "./my-worktree.sh"}}]}}
        ]}}}}"#,
        url = hook_url(41937)
    ));

    setup.install_ok();

    assert_eq!(
        setup.settings_json()["hooks"]["WorktreeCreate"],
        json!([{"hooks": [{"type": "command", "command": "./my-worktree.sh"}]}])
    );
    let fresh = Setup::new();
    fresh.install_ok();
    assert!(
        fresh.settings_json()["hooks"]
            .get("WorktreeCreate")
            .is_none()
    );
}

#[test]
fn a_file_it_cannot_merge_is_refused_and_left_untouched() {
    for contents in [
        "{ \"model\": \"opus\", ",
        "",
        "[]",
        "{\"hooks\": []}",
        "{\"hooks\": \"none\"}",
        "{\"hooks\": {\"Stop\": {\"command\": \"x\"}}}",
    ] {
        let setup = Setup::new();
        setup.write_settings(contents);

        let output = setup.install();

        assert!(!output.status.success(), "accepted {contents:?}");
        assert!(
            stderr(&output).contains("left untouched"),
            "{contents:?}: {}",
            stderr(&output)
        );
        assert_eq!(setup.read_settings(), contents);
        assert!(!setup.backup().exists(), "{contents:?}");
        let names: Vec<_> = fs::read_dir(setup.settings().parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "stray files for {contents:?}: {names:?}");
    }
}

#[test]
fn the_original_is_backed_up_once_and_never_overwritten() {
    let setup = Setup::new();
    setup.write_settings(USER_SETTINGS);

    setup.install_ok();
    assert_eq!(fs::read_to_string(setup.backup()).unwrap(), USER_SETTINGS);

    // The user edits the file later; a rerun must keep the first backup.
    let mut edited = setup.settings_json();
    edited["model"] = json!("sonnet");
    setup.write_settings(&serde_json::to_string(&edited).unwrap());
    setup.install_ok();

    assert_eq!(fs::read_to_string(setup.backup()).unwrap(), USER_SETTINGS);
    assert_eq!(setup.settings_json()["model"], "sonnet");
}

#[test]
fn the_default_settings_path_is_in_claude_config_dir() {
    let setup = Setup::new();
    let config_dir = setup.dir.path().join("custom-claude");
    let home = setup.dir.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let output = Command::new(BIN)
        .args(["install", "claude-code", "--claude-program"])
        .arg(&setup.claude)
        .env("CLAUDE_CONFIG_DIR", &config_dir)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    let settings: Value =
        serde_json::from_slice(&fs::read(config_dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(traction_entries(&settings, "Stop").len(), 1);
    assert!(!home.join(".claude").exists());
}

#[test]
fn the_version_check_warns_but_always_installs() {
    let cases = [
        ("2.1.332 (Claude Code)", None),
        ("2.1.295 (Claude Code)", None),
        ("2.1.294 (Claude Code)", Some("older than 2.1.295")),
        ("1.0.88 (Claude Code)", Some("older than 2.1.295")),
        (
            "Claude Code, build unknown",
            Some("could not check Claude Code version"),
        ),
    ];
    for (version_output, warning) in cases {
        let setup = Setup::with_claude_output(version_output);

        let output = setup.install_ok();

        let err = stderr(&output);
        match warning {
            Some(warning) => assert!(err.contains(warning), "{version_output}: {err}"),
            None => assert!(!err.contains("warning"), "{version_output}: {err}"),
        }
        assert_eq!(traction_entries(&setup.settings_json(), "Stop").len(), 1);
    }
}

#[test]
fn a_missing_claude_program_warns_but_still_installs() {
    let mut setup = Setup::new();
    setup.claude = setup.dir.path().join("no-such-claude");

    let output = setup.install_ok();

    assert!(
        stderr(&output).contains("could not check Claude Code version"),
        "{}",
        stderr(&output)
    );
    assert_eq!(traction_entries(&setup.settings_json(), "Stop").len(), 1);
}

#[test]
fn no_token_value_is_ever_written() {
    const CANARY: &str = "canary-token-3f9a1c";
    let setup = Setup::new();
    setup.write_settings(USER_SETTINGS);

    let output = setup
        .command()
        .env("TRACTION_TOKEN", CANARY)
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    let text = setup.read_settings();
    assert!(!text.contains(CANARY));
    assert!(!String::from_utf8_lossy(&output.stdout).contains(CANARY));
}

#[test]
fn every_generated_entry_fails_open() {
    let setup = Setup::new();
    setup.write_settings(USER_SETTINGS);
    setup.install_ok();
    let settings = setup.settings_json();

    for hook in HOOKS {
        for entry in traction_entries(&settings, hook) {
            let timeout = entry["timeout"].as_u64().expect("numeric timeout");
            assert!(timeout <= 1, "{hook}: {entry}");
            assert_eq!(entry["onFailure"], "continue", "{hook}");
            assert_eq!(entry["type"], "http", "{hook}");
            assert_eq!(
                entry["headers"],
                json!({"Authorization": "Bearer $TRACTION_TOKEN"}),
                "{hook}"
            );
            assert_eq!(entry["allowedEnvVars"], json!(["TRACTION_TOKEN"]), "{hook}");
        }
    }
}

/// The request each entry describes, sent to a known-closed port with the
/// entry's headers and timeout, fails within that timeout plus a margin:
/// a stopped daemon can never hold up Claude Code longer than `timeout`.
#[test]
fn every_entry_request_to_a_stopped_daemon_fails_within_its_timeout() {
    const MARGIN: Duration = Duration::from_millis(750);
    let setup = Setup::new();
    setup.install_ok();
    let settings = setup.settings_json();
    let closed_port = {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.local_addr().unwrap().port()
    };

    for hook in HOOKS {
        for entry in traction_entries(&settings, hook) {
            let timeout = Duration::from_secs(entry["timeout"].as_u64().unwrap());
            let url = entry["url"].as_str().unwrap();
            let path = url
                .strip_prefix("http://127.0.0.1:41937")
                .expect("default-port loopback URL");
            let headers: Vec<(String, String)> = entry["headers"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(name, value)| {
                    let value = value.as_str().unwrap().replace("$TRACTION_TOKEN", "x");
                    (name.clone(), value)
                })
                .collect();

            let started = Instant::now();
            let result = send_hook_request(closed_port, path, &headers, timeout);
            let elapsed = started.elapsed();

            assert!(result.is_err(), "{hook}: a closed port answered");
            assert!(
                elapsed <= timeout + MARGIN,
                "{hook}: took {elapsed:?} with timeout {timeout:?}"
            );
        }
    }
}

/// POSTs `{}` to `127.0.0.1:<port><path>` and reads a status line, all
/// within `timeout` (as Claude Code bounds a hook request).
fn send_hook_request(
    port: u16,
    path: &str,
    headers: &[(String, String)],
    timeout: Duration,
) -> std::io::Result<String> {
    let deadline = Instant::now() + timeout;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|d| !d.is_zero())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::TimedOut))?;
    stream.set_read_timeout(Some(remaining))?;
    stream.set_write_timeout(Some(remaining))?;
    let mut request =
        format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 2\r\n");
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n{}");
    stream.write_all(request.as_bytes())?;
    let mut status = String::new();
    BufReader::new(stream).read_line(&mut status)?;
    if status.is_empty() {
        return Err(std::io::ErrorKind::UnexpectedEof.into());
    }
    Ok(status)
}
