//! `traction-daemon submit --project <id>`: sends one Project's Audit Bundle
//! to the server (Step 4, S3a, `docs/protocol.md` section 5).
//!
//! 1. Exports the bundle with the same read-only export as `export-bundle`.
//! 2. `POST /v1/bundles` (device token) returns `{bundle_id, put_url}`.
//! 3. `PUT`s the bytes to `put_url`: the presigned URL carries its own
//!    authority, so the device token is **never** sent to the blob store.
//! 4. `POST /v1/bundles/{id}/complete` (device token). The server verifies
//!    and triages; the submitter is only told the bundle was received.
//!
//! The token and the bundle bytes are never printed or logged.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use traction_core::sync::{BundleCompleteResponse, BundleStartRequest, BundleStartResponse};

use crate::audit::{Outcome, export_bundle};
use crate::clock::now_ms;
use crate::sync::{SERVER_TOKEN_FILE, SERVER_URL_FILE, post_json};

/// Network timeout of each step (the upload may carry up to 64 MB).
pub const SUBMIT_TIMEOUT: Duration = Duration::from_secs(120);

const USAGE: &str = "usage: traction-daemon submit --project <project_id> [--data-dir <path>]";

fn read_trimmed(dir: &Path, name: &str, what: &str) -> Result<String, String> {
    std::fs::read_to_string(dir.join(name))
        .map(|t| t.trim().to_owned())
        .ok()
        .filter(|t| !t.is_empty())
        .ok_or_else(|| format!("{what} is missing; run `traction-daemon login` first"))
}

/// `PUT` with no credentials and no redirects.
fn put_bytes(url: &str, bytes: &[u8]) -> Result<u16, String> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .max_redirects(0)
        .timeout_global(Some(SUBMIT_TIMEOUT))
        .build()
        .into();
    let response = agent
        .put(url)
        .header("content-type", "application/json")
        .send(bytes)
        .map_err(|e| e.to_string())?;
    Ok(response.status().as_u16())
}

/// Runs the whole submission; returns the bundle id.
pub fn submit_project(data_dir: &Path, project_id: &str) -> Result<String, String> {
    let base = read_trimmed(data_dir, SERVER_URL_FILE, "the server URL")?;
    let token = read_trimmed(data_dir, SERVER_TOKEN_FILE, "the device token")?;
    let base = base.trim_end_matches('/');

    let staging = data_dir.join(format!("submit-{}.tmp", std::process::id()));
    let exported = export_bundle(data_dir, project_id, &staging, now_ms());
    let bytes = exported
        .map_err(|e| e.to_string())
        .and_then(|()| std::fs::read(&staging).map_err(|e| format!("cannot read the bundle: {e}")));
    let _ = std::fs::remove_file(&staging);
    let bytes = bytes?;

    let request = serde_json::to_vec(&BundleStartRequest {
        project_id: project_id.to_owned(),
        size_bytes: bytes.len() as u64,
    })
    .map_err(|e| e.to_string())?;
    let started = post_json(
        &format!("{base}/v1/bundles"),
        Some(&token),
        &request,
        SUBMIT_TIMEOUT,
    )?;
    if started.status != 200 {
        return Err(format!(
            "the server refused the submission ({}){}",
            started.status,
            error_code(&started.body)
        ));
    }
    let started: BundleStartResponse = serde_json::from_slice(&started.body)
        .map_err(|_| "the server's answer was not understood".to_owned())?;

    let status = put_bytes(&started.put_url, &bytes)?;
    if status != 200 {
        return Err(format!("the upload was refused ({status})"));
    }

    let done = post_json(
        &format!("{base}/v1/bundles/{}/complete", started.bundle_id),
        Some(&token),
        b"",
        SUBMIT_TIMEOUT,
    )?;
    if done.status != 200 {
        return Err(format!(
            "the server could not complete the submission ({}){}",
            done.status,
            error_code(&done.body)
        ));
    }
    let done: BundleCompleteResponse = serde_json::from_slice(&done.body)
        .map_err(|_| "the server's answer was not understood".to_owned())?;
    Ok(done.bundle_id)
}

/// `: <code>` from an `{"error": "<code>"}` body, or nothing.
fn error_code(body: &[u8]) -> String {
    serde_json::from_slice::<traction_core::sync::ErrorBody>(body)
        .map(|e| format!(": {}", e.error))
        .unwrap_or_default()
}

/// `submit --project <id> [--data-dir <path>]`.
pub fn run_submit(mut args: impl Iterator<Item = OsString>) -> Result<Outcome, String> {
    let (mut project, mut data_dir) = (None::<String>, None::<PathBuf>);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--project") => {
                project = Some(
                    args.next()
                        .and_then(|v| v.into_string().ok())
                        .ok_or(USAGE)?,
                );
            }
            Some("--data-dir") => data_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let project = project.ok_or(USAGE)?;
    let data_dir = match data_dir {
        Some(dir) => dir,
        None => crate::DaemonConfig::default_for_user()
            .map(|config| config.data_dir)
            .map_err(|e| e.to_string())?,
    };
    let id = submit_project(&data_dir, &project)?;
    Ok(Outcome {
        stdout: format!("submitted {project} (bundle {id})\n"),
        code: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> impl Iterator<Item = OsString> {
        list.iter()
            .map(OsString::from)
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn the_project_flag_is_required_and_unknown_flags_are_refused() {
        assert_eq!(run_submit(args(&[])).unwrap_err(), USAGE);
        assert_eq!(run_submit(args(&["--project"])).unwrap_err(), USAGE);
        assert_eq!(run_submit(args(&["--bogus", "x"])).unwrap_err(), USAGE);
    }

    #[test]
    fn without_login_it_says_so_and_sends_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let error = submit_project(dir.path(), "dir:x").unwrap_err();
        assert!(error.contains("login"), "{error}");
    }

    #[test]
    fn an_error_body_surfaces_only_its_code() {
        assert_eq!(error_code(br#"{"error":"bad_length"}"#), ": bad_length");
        assert_eq!(error_code(b"<html>"), "");
    }
}
