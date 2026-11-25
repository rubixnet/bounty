//! Installers that wire a Harness into Traction (`traction-daemon install
//! <harness>`).
//!
//! - [`claude_code`]: the Claude Code installer and its report.
//! - `settings`: the `settings.json` hook merge.
//! - `commands`: the `/stop-track` and `/resume-track` command files.
//! - `profile`: the `TRACTION_TOKEN` block in shell profiles.
//! - `user_env` (Windows): `TRACTION_TOKEN` under `HKCU\Environment`.
//! - `version`: the `claude --version` check.
//! - `ordered_json`: a JSON value that keeps the user's key order.

pub mod claude_code;
mod commands;
mod ordered_json;
mod profile;
mod settings;
#[cfg(windows)]
mod user_env;
mod version;

pub use settings::BACKUP_SUFFIX;
pub use version::{MIN_ON_FAILURE_VERSION, VERSION_TIMEOUT, Version};
