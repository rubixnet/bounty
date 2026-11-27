//! The Visual Proof Mode flag: `<data-dir>/visual_proof.json`, written by
//! `traction-daemon visual-proof enable|disable` and read by the scheduler
//! on every slot, so a running daemon follows it without a restart. Off by
//! default (no file). Enabling creates `vault.key` (S2 adds the macOS
//! permission request to `enable`).

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::DEFAULT_WEBP_QUALITY;
use super::vault::VaultKey;
use crate::audit::Outcome;
use crate::fsutil::write_atomically;

/// The flag file in the data folder.
pub const VISUAL_PROOF_FILE: &str = "visual_proof.json";

const USAGE: &str = "usage: traction-daemon visual-proof enable|disable|status [--data-dir <path>]";

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisualProof {
    pub enabled: bool,
    /// Lossy WebP quality (0–100).
    #[serde(default = "default_quality")]
    pub webp_quality: f32,
}

fn default_quality() -> f32 {
    DEFAULT_WEBP_QUALITY
}

impl Default for VisualProof {
    fn default() -> Self {
        Self {
            enabled: false,
            webp_quality: DEFAULT_WEBP_QUALITY,
        }
    }
}

impl VisualProof {
    /// The stored flag; missing or unreadable is off (fail closed).
    pub fn load(data_dir: &Path) -> Self {
        std::fs::read(data_dir.join(VISUAL_PROOF_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Self>(&bytes).ok())
            .filter(|s| (0.0..=100.0).contains(&s.webp_quality))
            .unwrap_or_default()
    }

    fn store(&self, data_dir: &Path) -> io::Result<()> {
        let text = serde_json::to_vec(self).map_err(io::Error::other)?;
        write_atomically(data_dir, VISUAL_PROOF_FILE, &text)
    }
}

/// Turns Visual Proof Mode on, creating the Vault and its key first.
pub fn enable(data_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    VaultKey::load_or_create(data_dir)?;
    VisualProof {
        enabled: true,
        ..VisualProof::load(data_dir)
    }
    .store(data_dir)
}

/// Turns Visual Proof Mode off. Stored Frames stay (their records are sealed).
pub fn disable(data_dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    VisualProof {
        enabled: false,
        ..VisualProof::load(data_dir)
    }
    .store(data_dir)
}

/// `traction-daemon visual-proof enable|disable|status [--data-dir <path>]`.
pub fn run(mut args: impl Iterator<Item = OsString>) -> Result<Outcome, String> {
    let action = args
        .next()
        .and_then(|a| a.into_string().ok())
        .ok_or(USAGE)?;
    let mut data_dir: Option<PathBuf> = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data-dir") => data_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            _ => return Err(USAGE.to_owned()),
        }
    }
    let data_dir = match data_dir {
        Some(dir) => dir,
        None => {
            crate::DaemonConfig::default_for_user()
                .map_err(|e| e.to_string())?
                .data_dir
        }
    };
    let fail = |e: io::Error| format!("cannot update {VISUAL_PROOF_FILE}: {e}");
    let stdout = match action.as_str() {
        "enable" => {
            enable(&data_dir).map_err(fail)?;
            "visual proof: on\n".to_owned()
        }
        "disable" => {
            disable(&data_dir).map_err(fail)?;
            "visual proof: off\n".to_owned()
        }
        "status" => {
            let on = VisualProof::load(&data_dir).enabled;
            format!("visual proof: {}\n", if on { "on" } else { "off" })
        }
        _ => return Err(USAGE.to_owned()),
    };
    Ok(Outcome { stdout, code: 0 })
}
