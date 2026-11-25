//! The Claude Code Adapter.
//!
//! - [`hook`]: Claude Code's native HTTP hook payloads, mapped to at most
//!   one content-free Traction Event each. The hook URL path and the hook
//!   set live here, shared by the bus route and the installer.
//! - [`config_dir`]: the Claude config folder rule (`$CLAUDE_CONFIG_DIR`,
//!   else `~/.claude`).
//! - [`pause`]: the `/stop-track` / `/resume-track` commands (Session
//!   Pause), shared by the hook endpoint and the installer.
//! - [`transcript`]: one session transcript record, mapped to zero or more
//!   content-free Traction Events.
//! - [`tailer`]: discovers, reads and resumes transcripts, then hands the
//!   mapped events to the Router.

pub mod config_dir;
pub mod hook;
pub mod pause;
pub mod tailer;
pub mod transcript;
