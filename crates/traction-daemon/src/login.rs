//! `traction-daemon login --server <url>`: Pairing (`docs/protocol.md`
//! section 3).
//!
//! 1. Send the Device Signing Key's public key; get a pairing code and a
//!    login URL.
//! 2. Open the URL in the browser (best effort) and print the code and URL.
//!    The user approves on the server's page; nothing here approves anything.
//! 3. Poll until the server hands over the device token, then store it at
//!    `<data dir>/server.token` through `write_owner_only` (`0600` on Unix, an
//!    owner-only DACL on Windows), and the server URL beside it.
//!
//! The token is never printed or logged.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use traction_core::sync::{
    PairingPollRequest, PairingPollResponse, PairingStartRequest, PairingStartResponse,
    PairingStatus,
};

use crate::fsutil::{write_atomically, write_owner_only};
use crate::signing_key;
use crate::sync::{SERVER_KEY_ID_FILE, SERVER_TOKEN_FILE, SERVER_URL_FILE, post_json};

/// How often the CLI asks whether the pairing was approved.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);
/// How long the CLI waits for the user (the server's pairing lifetime).
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Clone)]
pub struct LoginOptions {
    pub data_dir: PathBuf,
    pub server_url: String,
    pub open_browser: bool,
    pub poll_interval: Duration,
    pub timeout: Duration,
}

fn call<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    url: &str,
    body: &T,
) -> Result<R, String> {
    let bytes = serde_json::to_vec(body).map_err(|e| e.to_string())?;
    let reply = post_json(url, None, &bytes, Duration::from_secs(15))?;
    if reply.status != 200 {
        return Err(format!("the server answered {}", reply.status));
    }
    serde_json::from_slice(&reply.body).map_err(|_| "the server's answer was not understood".into())
}

/// Runs Pairing to the end. `open` receives the login URL.
pub fn login(
    options: &LoginOptions,
    out: &mut dyn Write,
    open: &dyn Fn(&str),
) -> Result<(), String> {
    let base = options.server_url.trim().trim_end_matches('/').to_owned();
    if !(base.starts_with("https://") || base.starts_with("http://")) {
        return Err("the server URL must start with https:// (or http:// for local tests)".into());
    }
    std::fs::create_dir_all(&options.data_dir)
        .map_err(|e| format!("cannot create the data folder: {e}"))?;
    let key = signing_key::load_or_create(&options.data_dir)
        .map_err(|e| format!("cannot read the signing key: {e}"))?;
    let key_id = key.key_id();
    let previous_key_id = std::fs::read_to_string(options.data_dir.join(SERVER_KEY_ID_FILE))
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|previous| !previous.is_empty() && previous != key_id.as_str());
    let label = ["HOSTNAME", "COMPUTERNAME", "HOST"]
        .iter()
        .find_map(|name| std::env::var(name).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "this device".to_owned());
    let started: PairingStartResponse = call(
        &format!("{base}/v1/pairings"),
        &PairingStartRequest {
            public_key: key.verifying_key().to_hex(),
            previous_key_id,
            device_label: label,
        },
    )?;
    let _ = writeln!(out, "Pairing code: {}", started.pair_code);
    let _ = writeln!(out, "Approve this device at: {}", started.login_url);
    if options.open_browser {
        open(&started.login_url);
    }
    let _ = out.flush();

    let deadline = Instant::now() + options.timeout;
    loop {
        let polled: PairingPollResponse = call(
            &format!("{base}/v1/pairings/poll"),
            &PairingPollRequest {
                pair_code: started.pair_code.clone(),
                poll_secret: started.poll_secret.clone(),
            },
        )?;
        match (polled.status, polled.device_token) {
            (PairingStatus::Approved, Some(token)) => {
                write_owner_only(&options.data_dir, SERVER_TOKEN_FILE, token.as_bytes())
                    .map_err(|e| format!("cannot store the device token: {e}"))?;
                write_atomically(&options.data_dir, SERVER_URL_FILE, base.as_bytes())
                    .map_err(|e| format!("cannot store the server URL: {e}"))?;
                write_atomically(
                    &options.data_dir,
                    SERVER_KEY_ID_FILE,
                    key_id.as_str().as_bytes(),
                )
                .map_err(|e| format!("cannot store the key id: {e}"))?;
                let _ = writeln!(out, "Device paired. Syncing starts within a few minutes.");
                return Ok(());
            }
            (PairingStatus::Approved, None) => {
                return Err("this pairing was already completed; run login again".into());
            }
            (PairingStatus::Expired, _) => {
                return Err("the pairing expired before it was approved; run login again".into());
            }
            (PairingStatus::Pending, _) => {}
        }
        if Instant::now() >= deadline {
            return Err("timed out waiting for the approval; run login again".into());
        }
        std::thread::sleep(options.poll_interval);
    }
}

/// Best effort: ask the OS to open `url` in the default browser.
pub fn open_in_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let spawned = std::process::Command::new("open").arg(url).spawn();
    #[cfg(target_os = "windows")]
    let spawned = std::process::Command::new("rundll32")
        .args(["url.dll,FileProtocolHandler", url])
        .spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let spawned = std::process::Command::new("xdg-open").arg(url).spawn();
    let _ = spawned;
}

/// `login --server <url> [--data-dir <path>] [--no-browser]`.
pub fn run_login(mut args: impl Iterator<Item = OsString>) -> Result<(), String> {
    const USAGE: &str =
        "usage: traction-daemon login --server <url> [--data-dir <path>] [--no-browser]";
    let mut data_dir: Option<PathBuf> = None;
    let mut server: Option<String> = None;
    let mut open_browser = true;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data-dir") => data_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            Some("--server") => {
                server = Some(
                    args.next()
                        .and_then(|v| v.into_string().ok())
                        .ok_or(USAGE)?,
                );
            }
            Some("--no-browser") => open_browser = false,
            _ => return Err(USAGE.to_owned()),
        }
    }
    let data_dir = match data_dir {
        Some(dir) => dir,
        None => {
            crate::config::DaemonConfig::default_for_user()
                .map_err(|e| e.to_string())?
                .data_dir
        }
    };
    let server_url = match server {
        Some(url) => url,
        None => read_server_url(&data_dir).ok_or(USAGE)?,
    };
    let options = LoginOptions {
        data_dir,
        server_url,
        open_browser,
        poll_interval: POLL_INTERVAL,
        timeout: LOGIN_TIMEOUT,
    };
    login(&options, &mut std::io::stdout(), &open_in_browser)
}

fn read_server_url(data_dir: &Path) -> Option<String> {
    std::fs::read_to_string(data_dir.join(SERVER_URL_FILE))
        .ok()
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty())
}
