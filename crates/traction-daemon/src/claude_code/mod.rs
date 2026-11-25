//! The Claude Code Adapter.
//!
//! - [`hook`]: Claude Code's native HTTP hook payloads, mapped to at most
//!   one content-free Traction Event each. The hook URL path and the hook
//!   set live here, shared by the bus route and the installer.
//! - [`transcript`]: one session transcript record, mapped to zero or more
//!   content-free Traction Events.
//! - [`tailer`]: discovers, reads and resumes transcripts, then hands the
//!   mapped events to the Router.

pub mod hook;
pub mod tailer;
pub mod transcript;
