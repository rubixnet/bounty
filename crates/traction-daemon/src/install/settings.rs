//! Merging Traction's HTTP hooks into Claude Code's `settings.json`.
//!
//! Ownership rule: a hook entry is Traction's when its `url` is the Traction
//! hook URL ([`HOOK_PATH`] on loopback) on any port. A merge removes every
//! Traction entry, under every event key (so a Traction-owned
//! `WorktreeCreate` entry goes too), then appends one entry per hook in
//! [`Hook::ALL`]. Everything else in the file is kept, keys in file order.
//! The result is idempotent, and an upgrade replaces old entries instead of
//! adding copies.

use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::ordered_json::Json;
use crate::claude_code::hook::{HOOK_PATH, Hook};
use crate::fsutil::replace_file;

/// The suffix of the one-time copy of the user's original file.
pub const BACKUP_SUFFIX: &str = ".traction-backup";

/// Why `settings.json` was left untouched.
#[derive(Debug)]
pub enum SettingsError {
    /// The file is not valid JSON, or not a JSON object.
    Malformed(String),
    /// `hooks` is present but is not an object.
    HooksNotObject,
    /// A hook event Traction installs under holds something other than an
    /// array of matcher groups.
    EventNotArray(&'static str),
    Io(io::Error),
}

impl fmt::Display for SettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(reason) => write!(f, "is not a valid JSON object ({reason})"),
            Self::HooksNotObject => f.write_str("has a \"hooks\" value that is not an object"),
            Self::EventNotArray(event) => {
                write!(f, "has a \"hooks\".\"{event}\" value that is not an array")
            }
            Self::Io(e) => write!(f, "cannot be read or written ({e})"),
        }
    }
}

/// What [`install`] did to the file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingsOutcome {
    /// Whether the file's bytes changed (false on an idempotent rerun).
    pub changed: bool,
    /// Where the original was copied, when this run made the copy.
    pub backup_written: Option<PathBuf>,
}

/// Merges Traction's hooks for `port` into the file at `path`: created when
/// missing, backed up once before the first edit, written through a temp
/// file and a rename (at a symlink's target, keeping its permissions), and
/// left untouched on any error.
pub fn install(path: &Path, port: u16) -> Result<SettingsOutcome, SettingsError> {
    let original = match fs::read(path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == io::ErrorKind::NotFound => None,
        Err(e) => return Err(SettingsError::Io(e)),
    };
    let merged = merge(original.as_deref(), port)?;
    if original.as_deref() == Some(merged.as_slice()) {
        return Ok(SettingsOutcome {
            changed: false,
            backup_written: None,
        });
    }

    let name = path
        .file_name()
        .ok_or_else(|| SettingsError::Malformed("the settings path has no file name".into()))?;

    let mut backup_written = None;
    if let Some(original) = &original {
        let mut backup_name = name.to_os_string();
        backup_name.push(BACKUP_SUFFIX);
        let backup = path.with_file_name(backup_name);
        if !backup.exists() {
            replace_file(&backup, original).map_err(SettingsError::Io)?;
            backup_written = Some(backup);
        }
    }
    // A symlinked settings file is written at its target, so the link and
    // the target's permissions survive.
    replace_file(path, &merged).map_err(SettingsError::Io)?;
    Ok(SettingsOutcome {
        changed: true,
        backup_written,
    })
}

/// The merged file contents (pretty JSON plus a trailing newline) for
/// `original` (`None` = no file yet).
pub fn merge(original: Option<&[u8]>, port: u16) -> Result<Vec<u8>, SettingsError> {
    let mut root = match original {
        Some(bytes) => serde_json::from_slice::<Json>(bytes)
            .map_err(|e| SettingsError::Malformed(e.to_string()))?,
        None => Json::Object(Vec::new()),
    };
    let Json::Object(root_entries) = &mut root else {
        return Err(SettingsError::Malformed(
            "the top level is not an object".into(),
        ));
    };
    if !root_entries.iter().any(|(key, _)| key == "hooks") {
        root_entries.push(("hooks".to_owned(), Json::Object(Vec::new())));
    }
    let hooks = root_entries
        .iter_mut()
        .find_map(|(key, value)| (key == "hooks").then_some(value))
        .expect("hooks was just ensured");
    let Json::Object(events) = hooks else {
        return Err(SettingsError::HooksNotObject);
    };

    remove_traction_entries(events);
    for hook in Hook::ALL {
        let group = matcher_group(hook, port);
        match events.iter_mut().find(|(key, _)| key == hook.name()) {
            Some((_, Json::Array(groups))) => groups.push(group),
            Some(_) => return Err(SettingsError::EventNotArray(hook.name())),
            None => events.push((hook.name().to_owned(), Json::Array(vec![group]))),
        }
    }

    let mut out = serde_json::to_vec_pretty(&root).expect("JSON values always serialize");
    out.push(b'\n');
    Ok(out)
}

/// Drops every Traction entry, then any matcher group or event key that
/// held only Traction entries. Groups and keys that were already empty, or
/// that hold anything else, are kept as the user wrote them.
fn remove_traction_entries(events: &mut Vec<(String, Json)>) {
    events.retain_mut(|(_, value)| {
        let Json::Array(groups) = value else {
            return true;
        };
        let before = groups.len();
        groups.retain_mut(|group| {
            let Json::Object(fields) = group else {
                return true;
            };
            let Some((_, Json::Array(entries))) = fields.iter_mut().find(|(k, _)| k == "hooks")
            else {
                return true;
            };
            let had = entries.len();
            entries.retain(|entry| !is_traction_entry(entry));
            !(entries.is_empty() && had > 0)
        });
        !(groups.is_empty() && before > 0)
    });
}

fn is_traction_entry(entry: &Json) -> bool {
    entry
        .get("url")
        .and_then(Json::as_str)
        .is_some_and(is_traction_url)
}

/// `http://127.0.0.1:<port>/v1/hooks/claude-code` (or `localhost`), any port.
fn is_traction_url(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("http://127.0.0.1:")
        .or_else(|| url.strip_prefix("http://localhost:"))
    else {
        return false;
    };
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    digits > 0 && &rest[digits..] == HOOK_PATH
}

/// The hook URL Traction installs for `port`.
pub fn hook_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}{HOOK_PATH}")
}

/// One Traction entry: a 1-second HTTP hook that fails open and sends the
/// token by env-var reference (`$TRACTION_TOKEN`), never by value.
fn traction_entry(port: u16) -> Json {
    Json::object([
        ("type", Json::string("http")),
        ("url", Json::String(hook_url(port))),
        ("timeout", Json::Number(1.into())),
        ("onFailure", Json::string("continue")),
        (
            "headers",
            Json::object([("Authorization", Json::string("Bearer $TRACTION_TOKEN"))]),
        ),
        (
            "allowedEnvVars",
            Json::Array(vec![Json::string("TRACTION_TOKEN")]),
        ),
    ])
}

/// The matcher group holding `hook`'s Traction entry. Tool hooks match
/// every tool.
fn matcher_group(hook: Hook, port: u16) -> Json {
    let entries = ("hooks", Json::Array(vec![traction_entry(port)]));
    if hook.is_tool_hook() {
        Json::object([("matcher", Json::string("*")), entries])
    } else {
        Json::object([entries])
    }
}
