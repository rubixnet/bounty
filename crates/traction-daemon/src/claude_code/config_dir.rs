//! Where Claude Code keeps its config: `$CLAUDE_CONFIG_DIR` if set (and not
//! empty), else `~/.claude`. The installer and the transcript tailer share
//! this rule.

use std::path::PathBuf;

/// The env var Claude Code reads to relocate its config folder.
pub const CLAUDE_CONFIG_DIR_ENV: &str = "CLAUDE_CONFIG_DIR";

/// The Claude config folder, or `None` when neither `$CLAUDE_CONFIG_DIR`
/// nor the user's home folder is known.
pub fn claude_config_dir() -> Option<PathBuf> {
    match std::env::var_os(CLAUDE_CONFIG_DIR_ENV) {
        Some(dir) if !dir.is_empty() => Some(PathBuf::from(dir)),
        _ => std::env::home_dir()
            .filter(|home| home.is_absolute())
            .map(|home| home.join(".claude")),
    }
}
