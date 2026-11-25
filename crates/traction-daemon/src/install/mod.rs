//! Installers that wire a Harness into Traction (`traction-daemon install
//! <harness>`).
//!
//! - [`claude_code`]: the Claude Code installer and its report.
//! - `settings`: the `settings.json` hook merge.
//! - `version`: the `claude --version` check.
//! - `ordered_json`: a JSON value that keeps the user's key order.

pub mod claude_code;
mod ordered_json;
mod settings;
mod version;

pub use settings::BACKUP_SUFFIX;
pub use version::{MIN_ON_FAILURE_VERSION, VERSION_TIMEOUT, Version};
