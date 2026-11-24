//! The daemon's only logging path (stderr).
//!
//! Logs never include request bodies, token values or Traction Event field
//! contents beyond `event_type`, `harness_id` and `event_id`. To keep that
//! structural, messages are `&'static str`; the only dynamic part allowed is
//! an OS or SQLite error, which never carries request data.

use std::fmt::Display;

pub fn warn(message: &'static str) {
    eprintln!("traction-daemon: warning: {message}");
}

/// `error` must come from the OS or SQLite, never from request input.
pub fn error(context: &'static str, error: &dyn Display) {
    eprintln!("traction-daemon: error: {context}: {error}");
}
