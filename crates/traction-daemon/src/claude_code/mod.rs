//! The Claude Code Adapter.
//!
//! - [`hook`]: Claude Code's native HTTP hook payloads, mapped to at most
//!   one content-free Traction Event each. The hook URL path and the hook
//!   set live here, shared by the bus route and the installer.
//! - [`pause`]: the `/stop-track` / `/resume-track` commands (Session
//!   Pause), shared by the hook endpoint and the installer.

pub mod hook;
pub mod pause;
