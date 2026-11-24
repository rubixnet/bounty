//! `traction-daemon`: starts the daemon with the user's defaults.
//!
//! Wiring OS signals (`SIGINT`/`SIGTERM`, Ctrl+C) to
//! `DaemonHandle::shutdown` lands in ticket 07; until then the process runs
//! until it is killed.

use std::process::ExitCode;

fn main() -> ExitCode {
    let config = match traction_daemon::DaemonConfig::default_for_user() {
        Ok(config) => config,
        Err(error) => {
            eprintln!("traction-daemon: {error}");
            return ExitCode::FAILURE;
        }
    };
    let _daemon = match traction_daemon::start(config) {
        Ok(daemon) => daemon,
        Err(error) => {
            eprintln!("traction-daemon: {error}");
            return ExitCode::FAILURE;
        }
    };
    loop {
        std::thread::park();
    }
}
