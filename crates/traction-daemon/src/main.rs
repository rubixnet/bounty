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
//! `--port <n>` (`0` = ephemeral) and, on macOS/Linux, `--socket-path <path>`
//! (default `$XDG_RUNTIME_DIR/traction.sock`, else `<data dir>/daemon.sock`).
//!
//! Subcommands:
//! - `export-bundle --project <project_id> --out <file> [--data-dir <path>]`
//!   writes that Project's frame-less Audit Bundle (read-only; safe while the
//!   daemon runs).
//! - `submit --project <project_id> [--data-dir <path>]` exports that Project's
//!   bundle and sends it to the paired server (presigned upload, then
//!   `complete`); the server verifies and triages it.
//! - `verify <bundle> [--trusted-key <hex>]... [--expect-tip <key_id>:<seq>:<hash>]`
//!   or `verify --data-dir <path> ...` checks a bundle or the local database
//!   offline. Exit 0 = verified (one chain), 2 = verified with more than one
//!   chain (key discontinuity), 1 = failed or unreadable.
//! - `login --server <url> [--data-dir <path>] [--no-browser]` pairs this
//!   device with the server (opens the approval page, then stores the device
//!   token in `<data dir>/server.token`, owner-only).
//! - `install claude-code [--settings <path>] [--claude-program <path>]
//!   [--port <n>] [--profile <path>]...` merges Traction's hooks into Claude
//!   Code's `settings.json` (default `$CLAUDE_CONFIG_DIR/settings.json`, else
//!   `~/.claude/settings.json`), writes the `/stop-track` and `/resume-track`
//!   command files next to it, adds the `TRACTION_TOKEN` block to each shell
//!   profile (default: the existing zsh, bash, fish and PowerShell ones; any
//!   `--profile` replaces that list), prints a summary to stdout and exits.
//!   On Windows it also sets `TRACTION_TOKEN` under `HKCU\Environment`
//!   (the test-only `--windows-env-key <subkey>` redirects that to a scratch
//!   key; it is deliberately left out of the usage text).

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;

use traction_daemon::{ClaudeCodeInstall, DaemonConfig};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) if message.starts_with("exit:") => ExitCode::from(
            message
                .strip_prefix("exit:")
                .and_then(|code| code.parse().ok())
                .unwrap_or(1),
        ),
        Err(message) => {
            eprintln!("traction-daemon: {message}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = std::env::args_os().skip(1).peekable();
    match args.peek().and_then(|arg| arg.to_str()) {
        Some("install") => {
            args.next();
            return install(args);
        }
        Some("login") => {
            args.next();
            return traction_daemon::run_login(args);
        }
        Some("export-bundle") => {
            args.next();
            return finish(traction_daemon::run_export(args)?);
        }
        Some("submit") => {
            args.next();
            return finish(traction_daemon::run_submit(args)?);
        }
        Some("verify") => {
            args.next();
            return finish(traction_daemon::run_verify(args)?);
        }
        _ => {}
    }
    let config = config_from_args(args)?;

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
    #[cfg(unix)]
    let mut socket_path: Option<PathBuf> = None;
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
            #[cfg(unix)]
            Some("--socket-path") => {
                let value = args.next().ok_or("--socket-path needs a path")?;
                socket_path = Some(PathBuf::from(value));
            }
            _ => return Err(USAGE.to_owned()),
        }
    }
    let mut config = match data_dir {
        Some(dir) => DaemonConfig::new(dir),
        None => DaemonConfig::default_for_user().map_err(|e| e.to_string())?,
    };
    if let Some(port) = port {
        config.http_port = port;
    }
    #[cfg(unix)]
    if let Some(socket_path) = socket_path {
        config.socket_path = socket_path;
    }
    Ok(config)
}

/// Prints a finished `export-bundle`/`verify` run. A non-zero code travels as
/// the internal `exit:<n>` error so `main` returns it without printing more.
fn finish(outcome: traction_daemon::Outcome) -> Result<(), String> {
    print!("{}", outcome.stdout);
    match outcome.code {
        0 => Ok(()),
        code => Err(format!("exit:{code}")),
    }
}

/// `install <harness> [options]`.
fn install(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    match args.next().as_ref().and_then(|arg| arg.to_str()) {
        Some("claude-code") => {}
        _ => return Err(INSTALL_USAGE.to_owned()),
    }
    let mut install = ClaudeCodeInstall::for_user().map_err(|e| e.to_string())?;
    let mut profiles: Vec<PathBuf> = Vec::new();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--settings") => {
                install.settings = PathBuf::from(args.next().ok_or("--settings needs a path")?);
            }
            Some("--claude-program") => {
                install.claude_program =
                    PathBuf::from(args.next().ok_or("--claude-program needs a path")?);
            }
            Some("--port") => {
                install.port = args
                    .next()
                    .as_ref()
                    .and_then(|v| v.to_str())
                    .and_then(|v| v.parse().ok())
                    .filter(|&port| port != 0)
                    .ok_or("--port needs a number from 1 to 65535")?;
            }
            Some("--profile") => {
                profiles.push(PathBuf::from(args.next().ok_or("--profile needs a path")?));
            }
            // Test-only (the Seam B tests' scratch `HKCU` key): left out of
            // INSTALL_USAGE so users never see or rely on it.
            #[cfg(windows)]
            Some("--windows-env-key") => {
                install.windows_env_key = args
                    .next()
                    .and_then(|v| v.into_string().ok())
                    .filter(|v| !v.is_empty())
                    .ok_or("--windows-env-key needs an HKCU subkey")?;
            }
            _ => return Err(INSTALL_USAGE.to_owned()),
        }
    }
    if !profiles.is_empty() {
        install.profiles = profiles;
    }
    let report = install.run().map_err(|e| e.to_string())?;
    for warning in &report.warnings {
        eprintln!("traction-daemon: warning: {warning}");
    }
    print!("{report}");
    Ok(())
}

const INSTALL_USAGE: &str = "usage: traction-daemon install claude-code \
    [--settings <path>] [--claude-program <path>] [--port <n>] [--profile <path>]...";

#[cfg(unix)]
const USAGE: &str = "unknown argument (use --data-dir <path>, --port <n>, --socket-path <path>)";
#[cfg(not(unix))]
const USAGE: &str = "unknown argument (use --data-dir <path>, --port <n>)";

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
