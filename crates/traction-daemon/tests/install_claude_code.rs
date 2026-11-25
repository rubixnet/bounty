//! `traction-daemon install claude-code` (Seam B): the real binary merges
//! Traction's HTTP hooks into a temp `settings.json`, writes the command
//! files next to it and the `TRACTION_TOKEN` block into temp shell
//! profiles, with a fake `claude` program printing chosen versions.
//!
//! Nothing here touches the user's real config: every run passes
//! `--settings` and `--claude-program` (or a temp `CLAUDE_CONFIG_DIR`),
//! points `HOME`/`USERPROFILE`/`XDG_CONFIG_HOME` at a temp home so default
//! profiles resolve inside it, and on Windows redirects the per-user env var
//! to a scratch `HKCU` key (`--windows-env-key`) that is deleted afterwards.

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use tempfile::TempDir;
use traction_daemon::{CLAUDE_CODE_HOOK_PATH, PauseCommand, RESUME_TRACK, STOP_TRACK};

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

/// A temp Claude config folder and home plus a fake `claude` program.
struct Setup {
    dir: TempDir,
    claude: PathBuf,
    /// The scratch `HKCU` subkey standing in for `HKCU\Environment`.
    #[cfg(windows)]
    env_key: String,
}

#[cfg(windows)]
impl Drop for Setup {
    fn drop(&mut self) {
        let _ = Command::new("reg")
            .args(["delete", &format!("HKCU\\{}", self.env_key), "/f"])
            .output();
    }
}

impl Setup {
    fn new() -> Self {
        Self::with_claude_output("2.1.332 (Claude Code)")
    }

    /// The fake `claude --version` prints `output` and exits 0.
    fn with_claude_output(output: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let claude = fake_program(dir.path(), "claude", output);
        fs::create_dir_all(dir.path().join("home")).unwrap();
        Self {
            #[cfg(windows)]
            env_key: format!(
                "Software\\TractionTest\\{}",
                dir.path().file_name().unwrap().to_string_lossy()
            ),
            dir,
            claude,
        }
    }

    /// The temp home every run sees as `HOME`/`USERPROFILE`.
    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    /// The token file the profile block must read.
    fn token_file(&self) -> PathBuf {
        self.home().join(".traction").join("daemon.token")
    }

    fn commands_dir(&self) -> PathBuf {
        self.dir.path().join("claude").join("commands")
    }

    /// A command pointed at the temp home (and scratch registry key), with
    /// no `--settings`.
    fn bare_command(&self) -> Command {
        let mut command = Command::new(BIN);
        command
            .args(["install", "claude-code"])
            .env("HOME", self.home())
            .env("USERPROFILE", self.home())
            .env("XDG_CONFIG_HOME", self.home().join(".config"))
            .env_remove("ZDOTDIR")
            .stdin(Stdio::null());
        #[cfg(windows)]
        command.args(["--windows-env-key", &self.env_key]);
        command
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
        let mut command = self.bare_command();
        command
            .arg("--settings")
            .arg(self.settings())
            .arg("--claude-program")
            .arg(&self.claude);
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

/// A dotfile manager's symlinked `settings.json` stays a link: the merge
/// lands in its target, which keeps its permissions.
#[cfg(unix)]
#[test]
fn a_symlinked_settings_file_stays_a_link_and_its_target_gets_the_merge() {
    use std::os::unix::fs::PermissionsExt;

    let setup = Setup::new();
    let target = setup.dir.path().join("dotfiles").join("settings.json");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, USER_SETTINGS).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o640)).unwrap();
    fs::create_dir_all(setup.settings().parent().unwrap()).unwrap();
    std::os::unix::fs::symlink(&target, setup.settings()).unwrap();

    setup.install_ok();

    let link = fs::symlink_metadata(setup.settings()).unwrap();
    assert!(link.file_type().is_symlink(), "settings.json is still a link");
    assert_eq!(fs::read_link(setup.settings()).unwrap(), target);
    let merged: Value = serde_json::from_str(&fs::read_to_string(&target).unwrap()).unwrap();
    assert_eq!(traction_entries(&merged, "SessionStart").len(), 1);
    assert_eq!(merged["model"], "opus");
    let mode = fs::metadata(&target).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640, "the target keeps its permissions");
    assert_eq!(fs::read_to_string(setup.backup()).unwrap(), USER_SETTINGS);
}

#[test]
fn the_default_settings_path_is_in_claude_config_dir() {
    let setup = Setup::new();
    let config_dir = setup.dir.path().join("custom-claude");
    let home = setup.home();

    let output = setup
        .bare_command()
        .arg("--claude-program")
        .arg(&setup.claude)
        .env("CLAUDE_CONFIG_DIR", &config_dir)
        .output()
        .unwrap();

    assert!(output.status.success(), "{}", stderr(&output));
    let settings: Value =
        serde_json::from_slice(&fs::read(config_dir.join("settings.json")).unwrap()).unwrap();
    assert_eq!(traction_entries(&settings, "Stop").len(), 1);
    assert!(!home.join(".claude").exists());
    assert!(config_dir.join("commands").join("stop-track.md").exists());
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

// ---- Command files ------------------------------------------------------

/// The frontmatter and body of a command file.
fn split_command_file(text: &str) -> (&str, &str) {
    let rest = text
        .strip_prefix("---\n")
        .expect("frontmatter opens the file");
    let end = rest.find("\n---\n").expect("frontmatter closes");
    (&rest[..end], &rest[end + "\n---\n".len()..])
}

#[test]
fn both_command_files_are_written_model_proof_with_the_shared_body() {
    let setup = Setup::new();

    setup.install_ok();

    for command in PauseCommand::ALL {
        let text = fs::read_to_string(setup.commands_dir().join(command.file_name)).unwrap();
        let (frontmatter, body) = split_command_file(&text);
        let lines: Vec<&str> = frontmatter.lines().collect();
        assert!(
            lines.contains(&format!("description: {}", command.description).as_str()),
            "{text}"
        );
        assert!(lines.contains(&"disable-model-invocation: true"), "{text}");
        assert!(
            lines
                .iter()
                .any(|line| line.starts_with('#') && line.contains("traction")),
            "a Traction marker comment in the frontmatter: {text}"
        );
        // Exactly the shared one-line body, nothing for the model to follow.
        assert_eq!(body, format!("{}\n", command.body), "{text}");
    }
}

#[test]
fn traction_command_files_are_rewritten_on_rerun() {
    let setup = Setup::new();
    setup.install_ok();
    let stop = setup.commands_dir().join("stop-track.md");
    let first = fs::read_to_string(&stop).unwrap();
    // An older Traction version's file: marked, different body.
    fs::write(&stop, first.replace(STOP_TRACK.body, "Traction: old body.")).unwrap();

    let output = setup.install_ok();

    assert_eq!(fs::read_to_string(&stop).unwrap(), first);
    assert!(!stderr(&output).contains("warning"), "{}", stderr(&output));
}

#[test]
fn a_user_written_command_file_is_left_untouched_with_a_warning() {
    const MINE: &str = "---\ndescription: my own stop-track\n---\nDo my thing.\n";
    let setup = Setup::new();
    fs::create_dir_all(setup.commands_dir()).unwrap();
    fs::write(setup.commands_dir().join("stop-track.md"), MINE).unwrap();

    let output = setup.install_ok();

    assert_eq!(
        fs::read_to_string(setup.commands_dir().join("stop-track.md")).unwrap(),
        MINE
    );
    let err = stderr(&output);
    assert!(
        err.contains("stop-track.md") && err.contains("literal-text match only"),
        "{err}"
    );
    // The other one is still Traction's.
    let resume = fs::read_to_string(setup.commands_dir().join("resume-track.md")).unwrap();
    assert!(resume.ends_with(&format!("{}\n", RESUME_TRACK.body)));
}

// ---- TRACTION_TOKEN profile block ---------------------------------------

const BLOCK_START: &str = "# >>> traction >>>";
const BLOCK_END: &str = "# <<< traction <<<";
/// A token value that must never be copied into any file the installer
/// writes, whether it sits in the token file or in the environment.
const CANARY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

impl Setup {
    /// A run with each of `profiles` passed as `--profile`.
    fn install_with_profiles(&self, profiles: &[PathBuf]) -> Output {
        let mut command = self.command();
        for profile in profiles {
            command.arg("--profile").arg(profile);
        }
        let output = command.env("TRACTION_TOKEN", CANARY).output().unwrap();
        assert!(output.status.success(), "{}", stderr(&output));
        output
    }

    fn write_token(&self) {
        fs::create_dir_all(self.token_file().parent().unwrap()).unwrap();
        fs::write(self.token_file(), CANARY).unwrap();
    }
}

/// The lines between the markers, markers included, and how many blocks.
fn traction_block(text: &str) -> (String, usize) {
    let count = text.lines().filter(|line| *line == BLOCK_START).count();
    let start = text.find(BLOCK_START).expect("block start");
    let end = text.find(BLOCK_END).expect("block end") + BLOCK_END.len();
    (text[start..end].to_owned(), count)
}

/// One profile per shell syntax.
fn profile_set(setup: &Setup) -> [PathBuf; 4] {
    let dir = setup.dir.path().join("profiles");
    [
        dir.join(".zshrc"),
        dir.join(".bashrc"),
        dir.join("config.fish"),
        dir.join("Microsoft.PowerShell_profile.ps1"),
    ]
}

#[test]
fn each_profile_gets_one_block_in_its_shell_syntax_reading_the_token_file() {
    let setup = Setup::new();
    setup.write_token();
    let profiles = profile_set(&setup);
    fs::create_dir_all(profiles[0].parent().unwrap()).unwrap();
    fs::write(&profiles[0], "alias ll='ls -l'\n").unwrap();

    setup.install_with_profiles(&profiles);

    let token_file = setup.token_file().to_string_lossy().into_owned();
    let expect = [
        // (profile, a line its shell understands)
        (&profiles[0], "export TRACTION_TOKEN"),
        (&profiles[1], "export TRACTION_TOKEN"),
        (&profiles[2], "set -gx TRACTION_TOKEN"),
        (&profiles[3], "$env:TRACTION_TOKEN ="),
    ];
    for (profile, line) in expect {
        let text = fs::read_to_string(profile).unwrap();
        let (block, count) = traction_block(&text);
        assert_eq!(count, 1, "{text}");
        assert!(block.contains(line), "{}: {block}", profile.display());
        assert!(block.contains(&token_file), "{block}");
        assert!(!text.contains(CANARY), "the token leaked into {text}");
    }
    let zshrc = fs::read_to_string(&profiles[0]).unwrap();
    assert!(zshrc.starts_with("alias ll='ls -l'\n"), "{zshrc}");
}

#[test]
fn rerunning_replaces_the_block_instead_of_adding_one() {
    let setup = Setup::new();
    let profiles = profile_set(&setup);
    setup.install_with_profiles(&profiles);
    let first: Vec<String> = profiles
        .iter()
        .map(|p| fs::read_to_string(p).unwrap())
        .collect();

    // Rerun: byte-identical.
    setup.install_with_profiles(&profiles);
    for (profile, first) in profiles.iter().zip(&first) {
        assert_eq!(&fs::read_to_string(profile).unwrap(), first);
    }

    // An older block, with user lines around it, is replaced in place.
    let zshrc = &profiles[0];
    fs::write(
        zshrc,
        format!("export A=1\n{BLOCK_START}\nexport TRACTION_TOKEN=old\n{BLOCK_END}\nexport B=2\n"),
    )
    .unwrap();
    setup.install_with_profiles(&profiles);
    let text = fs::read_to_string(zshrc).unwrap();
    let (block, count) = traction_block(&text);
    assert_eq!(count, 1);
    assert!(!block.contains("=old"), "{text}");
    assert!(text.starts_with("export A=1\n# >>> traction >>>"), "{text}");
    assert!(text.ends_with("# <<< traction <<<\nexport B=2\n"), "{text}");
}

/// The POSIX block really exports the token file's contents at shell start.
#[cfg(unix)]
#[test]
fn the_posix_block_exports_the_token_from_the_file() {
    let setup = Setup::new();
    setup.write_token();
    let profile = setup.dir.path().join("profiles").join(".bashrc");
    setup.install_with_profiles(std::slice::from_ref(&profile));

    let output = Command::new("sh")
        .arg("-c")
        .arg(r#". "$1"; printf %s "$TRACTION_TOKEN""#)
        .arg("sh")
        .arg(&profile)
        .env_remove("TRACTION_TOKEN")
        .output()
        .unwrap();

    assert_eq!(String::from_utf8_lossy(&output.stdout), CANARY);

    // Without a token file the shell still starts, with nothing exported.
    fs::remove_file(setup.token_file()).unwrap();
    let output = Command::new("sh")
        .arg("-c")
        .arg(r#". "$1"; printf %s "${TRACTION_TOKEN-unset}""#)
        .arg("sh")
        .arg(&profile)
        .env_remove("TRACTION_TOKEN")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "unset");
}

/// The PowerShell block, run by a real PowerShell where one is installed
/// (always on Windows CI).
#[test]
fn the_powershell_block_exports_the_token_from_the_file() {
    let setup = Setup::new();
    setup.write_token();
    let profile = setup
        .dir
        .path()
        .join("profiles")
        .join("Microsoft.PowerShell_profile.ps1");
    setup.install_with_profiles(std::slice::from_ref(&profile));

    let program = if cfg!(windows) { "powershell" } else { "pwsh" };
    let Ok(output) = Command::new(program)
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-Command",
        ])
        .arg(format!(
            ". '{}'; [Console]::Out.Write($env:TRACTION_TOKEN)",
            profile.display()
        ))
        .env_remove("TRACTION_TOKEN")
        .output()
    else {
        if cfg!(windows) {
            panic!("PowerShell must exist on Windows");
        }
        return;
    };
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        CANARY,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn default_profiles_are_the_existing_shell_profiles_in_home() {
    let setup = Setup::new();
    let home = setup.home();
    let zshrc = home.join(".zshrc");
    let fish = home.join(".config").join("fish").join("config.fish");
    fs::write(&zshrc, "# mine\n").unwrap();
    fs::create_dir_all(fish.parent().unwrap()).unwrap();
    fs::write(&fish, "# mine\n").unwrap();

    let output = setup.install_ok();

    for profile in [&zshrc, &fish] {
        let (_, count) = traction_block(&fs::read_to_string(profile).unwrap());
        assert_eq!(count, 1, "{}", profile.display());
    }
    assert!(
        !home.join(".bashrc").exists(),
        "missing profiles are not created"
    );
    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(summary.contains(&zshrc.display().to_string()), "{summary}");
    assert!(summary.contains(&fish.display().to_string()), "{summary}");
}

#[test]
fn no_existing_profile_is_named_in_the_summary() {
    let setup = Setup::new();

    let output = setup.install_ok();

    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(summary.contains("profiles: none found"), "{summary}");
    assert!(summary.contains("--profile"), "{summary}");
}

#[test]
fn the_summary_lists_everything_and_ends_with_the_restart_hint() {
    let setup = Setup::with_claude_output("2.1.294 (Claude Code)");
    let profiles = profile_set(&setup);

    let output = setup.install_with_profiles(&profiles);

    let summary = String::from_utf8_lossy(&output.stdout);
    assert!(
        summary.contains(&setup.settings().display().to_string()),
        "{summary}"
    );
    for hook in HOOKS {
        assert!(summary.contains(hook), "{hook}: {summary}");
    }
    assert!(summary.contains("older than 2.1.295"), "{summary}");
    for profile in &profiles {
        assert!(
            summary.contains(&profile.display().to_string()),
            "{summary}"
        );
    }
    assert!(
        summary
            .trim_end()
            .ends_with("open a new terminal (and restart Claude Code) so hooks can authenticate"),
        "{summary}"
    );
    assert!(!summary.contains(CANARY));
}

/// Every file under `root`, relative.
fn tree(root: &Path) -> BTreeSet<PathBuf> {
    let mut files = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                files.insert(path.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    files
}

#[test]
fn nothing_is_written_outside_the_named_files() {
    let setup = Setup::new();
    setup.write_token();
    let profiles = profile_set(&setup);
    // A default-location profile that must be ignored when --profile is given.
    fs::write(setup.home().join(".zshrc"), "# mine\n").unwrap();
    let before = tree(setup.dir.path());

    setup.install_with_profiles(&profiles);
    setup.install_with_profiles(&profiles);

    let added: BTreeSet<PathBuf> = tree(setup.dir.path())
        .difference(&before)
        .cloned()
        .collect();
    let expected: BTreeSet<PathBuf> = [
        Path::new("claude").join("settings.json"),
        Path::new("claude").join("commands").join("stop-track.md"),
        Path::new("claude").join("commands").join("resume-track.md"),
    ]
    .into_iter()
    .chain(
        profiles
            .iter()
            .map(|p| p.strip_prefix(setup.dir.path()).unwrap().to_path_buf()),
    )
    .collect();
    assert_eq!(added, expected);
    assert_eq!(
        fs::read_to_string(setup.home().join(".zshrc")).unwrap(),
        "# mine\n"
    );
    for file in tree(setup.dir.path()) {
        if file.starts_with("bin") || file == Path::new("home/.traction/daemon.token") {
            continue;
        }
        let text = fs::read(setup.dir.path().join(&file)).unwrap();
        assert!(
            !String::from_utf8_lossy(&text).contains(CANARY),
            "the token leaked into {}",
            file.display()
        );
    }
}

/// On Windows the token is also the per-user env var, under `HKCU` (here a
/// scratch key standing in for `HKCU\Environment`), so Claude Code started
/// from any terminal or IDE can authenticate.
#[cfg(windows)]
#[test]
fn windows_sets_the_per_user_env_var_from_the_token_file() {
    let setup = Setup::new();
    setup.write_token();

    let output = setup.install_with_profiles(&[]);

    let query = Command::new("reg")
        .args([
            "query",
            &format!("HKCU\\{}", setup.env_key),
            "/v",
            "TRACTION_TOKEN",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&query.stdout);
    assert!(text.contains("REG_SZ"), "{text}");
    assert!(text.contains(CANARY), "{text}");
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("TRACTION_TOKEN"),
        "the summary names the env var"
    );
}
