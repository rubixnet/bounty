//! The Claude Code Adapter.
//!
//! - [`hook`]: Claude Code's native HTTP hook payloads, mapped to at most
//!   one content-free Traction Event each. The hook URL path and the hook
//!   set live here, shared by the bus route and the installer.

pub mod hook;
