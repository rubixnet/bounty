//! `traction-daemon`: a thin wrapper around the daemon library.
//!
//! Starts the daemon (by default on `~/.traction` and port 41937), prints
//! `traction-daemon: listening on <addr>` to stderr, then waits for
//! `SIGINT`/`SIGTERM` (Unix) or Ctrl+C, Ctrl+Break or console close
//! (Windows) and runs the graceful shutdown, which flushes every accepted
//! Traction Event before exit. Any start error (e.g. "already running" or a
//! taken port) is one stderr line and a non-zero exit.
//!
//! Options (mainly so tests stay hermetic): `--data-dir <path>`,
//! `--port <n>` (`0` = ephemeral).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use traction_daemon::DaemonConfig;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("traction-daemon: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let config = config_from_args(std::env::args_os().skip(1))?;

    // Signals are registered before `start` so a signal that arrives during
    // startup is not lost (and does not kill the process mid-start).
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .map_err(|e| format!("cannot start the signal runtime: {e}"))?;
    let signals = runtime
        .block_on(async { ShutdownSignals::register() })
        .map_err(|e| format!("cannot register shutdown signals: {e}"))?;

    let daemon = traction_daemon::start(config).map_err(|e| e.to_string())?;
    eprintln!("traction-daemon: listening on {}", daemon.local_addr());

    runtime.block_on(signals.recv());
    daemon.shutdown().map_err(|e| e.to_string())
}

fn config_from_args(mut args: impl Iterator<Item = OsString>) -> Result<DaemonConfig, String> {
    let mut data_dir: Option<PathBuf> = None;
    let mut port: Option<u16> = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data-dir") => {
                let value = args.next().ok_or("--data-dir needs a path")?;
                data_dir = Some(PathBuf::from(value));
            }
            Some("--port") => {
                let value = args.next().ok_or("--port needs a number")?;
                let value = value
                    .to_str()
                    .and_then(|v| v.parse().ok())
                    .ok_or("--port needs a number from 0 to 65535")?;
                port = Some(value);
            }
            _ => return Err("unknown argument (use --data-dir <path>, --port <n>)".to_owned()),
        }
    }
    let mut config = match data_dir {
        Some(dir) => DaemonConfig::new(dir),
        None => DaemonConfig::default_for_user().map_err(|e| e.to_string())?,
    };
    if let Some(port) = port {
        config.http_port = port;
    }
    Ok(config)
}

#[cfg(unix)]
struct ShutdownSignals {
    interrupt: tokio::signal::unix::Signal,
    terminate: tokio::signal::unix::Signal,
}

#[cfg(unix)]
impl ShutdownSignals {
    fn register() -> std::io::Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    async fn recv(mut self) {
        tokio::select! {
            _ = self.interrupt.recv() => {}
            _ = self.terminate.recv() => {}
        }
    }
}

#[cfg(windows)]
struct ShutdownSignals {
    ctrl_c: tokio::signal::windows::CtrlC,
    ctrl_break: tokio::signal::windows::CtrlBreak,
    ctrl_close: tokio::signal::windows::CtrlClose,
    ctrl_shutdown: tokio::signal::windows::CtrlShutdown,
}

#[cfg(windows)]
impl ShutdownSignals {
    fn register() -> std::io::Result<Self> {
        use tokio::signal::windows::{ctrl_break, ctrl_c, ctrl_close, ctrl_shutdown};
        Ok(Self {
            ctrl_c: ctrl_c()?,
            ctrl_break: ctrl_break()?,
            ctrl_close: ctrl_close()?,
            ctrl_shutdown: ctrl_shutdown()?,
        })
    }

    /// For console close and system shutdown, tokio keeps the OS handler
    /// thread parked, so Windows waits (up to its grace period) while the
    /// main thread flushes and returns.
    async fn recv(mut self) {
        tokio::select! {
            _ = self.ctrl_c.recv() => {}
            _ = self.ctrl_break.recv() => {}
            _ = self.ctrl_close.recv() => {}
            _ = self.ctrl_shutdown.recv() => {}
        }
    }
}
