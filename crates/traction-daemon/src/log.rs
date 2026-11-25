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

/// Logs `message` with a running count of how often it happened, on the
/// 1st, 2nd, 4th, 8th, ... occurrence so a flood cannot flood stderr.
pub fn warn_counted(message: &'static str, count: u64) {
    if count.is_power_of_two() {
        eprintln!("traction-daemon: warning: {message} (total so far: {count})");
    }
}

/// `error` must come from the OS or SQLite, never from request input.
pub fn error(context: &'static str, error: &dyn Display) {
    eprintln!("traction-daemon: error: {context}: {error}");
}
