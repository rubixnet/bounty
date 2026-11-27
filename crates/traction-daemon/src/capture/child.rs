//! `traction-daemon capture-frame`: the short-lived child that takes one
//! Frame (spec Q2, addition 2). It grabs, downscales, dHashes, WebP-encodes,
//! salts, hashes and encrypts, writes one Vault file, prints exactly one JSON
//! line and exits. It never opens SQLite and never prints free text.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use traction_core::{FRAME_SALT_LEN, FrameTrigger, frame_hash, frame_id};

use super::grab::{self, Bgra, GrabError};
use super::image::{self, hamming};
use super::vault::{self, VaultKey};
use super::{DEFAULT_WEBP_QUALITY, DHASH_SKIP_DISTANCE};
use crate::clock::now_ms;

/// The child's one line of output. Closed: nothing else is ever printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum ChildReport {
    Written {
        frame_id: String,
        frame_hash: String,
        /// 16 lowercase hex characters.
        dhash: String,
        captured_at_ms: u64,
        width: u32,
        height: u32,
    },
    Duplicate {
        dhash: String,
    },
    PermissionDenied,
    Unsupported,
    FocusChanged,
    Error,
}

impl ChildReport {
    /// Parses one report strictly: exactly the keys its status allows (serde
    /// alone lets unit variants carry extra keys) and a 16-hex `dhash`.
    pub fn parse(line: &str) -> Option<Self> {
        let value: serde_json::Value = serde_json::from_str(line).ok()?;
        let object = value.as_object()?;
        let keys: &[&str] = match object.get("status")?.as_str()? {
            "written" => &[
                "status",
                "frame_id",
                "frame_hash",
                "dhash",
                "captured_at_ms",
                "width",
                "height",
            ],
            "duplicate" => &["status", "dhash"],
            _ => &["status"],
        };
        if object.len() != keys.len() || !keys.iter().all(|k| object.contains_key(*k)) {
            return None;
        }
        let report: Self = serde_json::from_value(value).ok()?;
        match &report {
            ChildReport::Written { dhash, .. } | ChildReport::Duplicate { dhash }
                if dhash.len() != 16
                    || !dhash
                        .bytes()
                        .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) =>
            {
                None
            }
            _ => Some(report),
        }
    }
}

impl From<GrabError> for ChildReport {
    fn from(error: GrabError) -> Self {
        match error {
            GrabError::PermissionDenied => ChildReport::PermissionDenied,
            GrabError::Unsupported => ChildReport::Unsupported,
            GrabError::Error => ChildReport::Error,
        }
    }
}

/// The parsed arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildArgs {
    pub data_dir: PathBuf,
    pub display: String,
    pub prev_dhash: Option<u64>,
    pub trigger: FrameTrigger,
    pub scheduled_at_ms: Option<u64>,
    pub nonce_id: Option<String>,
    pub expect_focus: String,
    pub quality: f32,
}

impl ChildArgs {
    /// The command-line form the daemon passes (see [`ChildArgs::parse`]).
    pub fn to_args(&self) -> Vec<OsString> {
        let none = || "none".to_owned();
        let mut args: Vec<OsString> = vec!["capture-frame".into(), "--data-dir".into()];
        args.push(self.data_dir.clone().into_os_string());
        for (flag, value) in [
            ("--display", self.display.clone()),
            (
                "--prev-dhash",
                self.prev_dhash.map(dhash_hex).unwrap_or_else(none),
            ),
            ("--trigger", self.trigger.as_str().to_owned()),
            (
                "--scheduled-at-ms",
                self.scheduled_at_ms
                    .map(|v| v.to_string())
                    .unwrap_or_else(none),
            ),
            ("--nonce-id", self.nonce_id.clone().unwrap_or_else(none)),
            ("--expect-focus", self.expect_focus.clone()),
            ("--quality", self.quality.to_string()),
        ] {
            args.push(flag.into());
            args.push(value.into());
        }
        args
    }

    /// Parses `capture-frame`'s arguments (after the subcommand name).
    pub fn parse(mut args: impl Iterator<Item = OsString>) -> Option<Self> {
        let (mut data_dir, mut display, mut prev, mut trigger) = (None, None, None, None);
        let (mut scheduled, mut nonce, mut focus) = (None, None, None);
        let mut quality = DEFAULT_WEBP_QUALITY;
        while let Some(flag) = args.next() {
            let value = args.next()?;
            match flag.to_str()? {
                "--data-dir" => data_dir = Some(PathBuf::from(value)),
                text => {
                    let value = value.into_string().ok()?;
                    let optional = |v: &str| (v != "none").then(|| v.to_owned());
                    match text {
                        "--display" => display = Some(value),
                        "--prev-dhash" => {
                            prev = Some(match optional(&value) {
                                None => None,
                                Some(hex) if hex.len() == 16 => {
                                    Some(u64::from_str_radix(&hex, 16).ok()?)
                                }
                                Some(_) => return None,
                            })
                        }
                        "--trigger" => trigger = Some(FrameTrigger::parse(&value)?),
                        "--scheduled-at-ms" => {
                            scheduled = Some(match optional(&value) {
                                None => None,
                                Some(ms) => Some(ms.parse().ok()?),
                            })
                        }
                        "--nonce-id" => nonce = Some(optional(&value)),
                        "--expect-focus" => focus = Some(value),
                        "--quality" => {
                            quality = value.parse().ok().filter(|q| (0.0..=100.0).contains(q))?
                        }
                        _ => return None,
                    }
                }
            }
        }
        Some(Self {
            data_dir: data_dir?,
            display: display?,
            prev_dhash: prev?,
            trigger: trigger?,
            scheduled_at_ms: scheduled?,
            nonce_id: nonce?,
            expect_focus: focus?,
            quality,
        })
    }
}

/// `{:016x}`.
pub fn dhash_hex(dhash: u64) -> String {
    format!("{dhash:016x}")
}

/// Runs the child: one JSON line on stdout, always exit 0 once arguments
/// parsed (the status is the result).
pub fn run(args: impl Iterator<Item = OsString>) -> Result<crate::audit::Outcome, String> {
    let Some(args) = ChildArgs::parse(args) else {
        return Ok(line(&ChildReport::Error, 2));
    };
    Ok(line(&capture(&args), 0))
}

fn line(report: &ChildReport, code: u8) -> crate::audit::Outcome {
    let mut stdout =
        serde_json::to_string(report).unwrap_or_else(|_| r#"{"status":"error"}"#.into());
    stdout.push('\n');
    crate::audit::Outcome { stdout, code }
}

/// Grab, re-check focus, then [`store`].
fn capture(args: &ChildArgs) -> ChildReport {
    let Ok(Some(key)) = VaultKey::load(&args.data_dir) else {
        // Visual Proof Mode was never enabled here: no key, no Frame.
        return ChildReport::Error;
    };
    hang_for_tests(&args.data_dir);
    let image = match grab::grab(&args.display) {
        Ok(image) => image,
        Err(error) => return error.into(),
    };
    let captured_at_ms = now_ms();
    // Right after the grab: a focus change during it discards the Frame.
    if grab::focus().map(|f| f.token).as_deref() != Some(args.expect_focus.as_str()) {
        return ChildReport::FocusChanged;
    }
    store(
        &args.data_dir,
        &key,
        &image,
        args.prev_dhash,
        captured_at_ms,
        args.quality,
    )
}

/// A `capture-hang.test` file in the data folder (debug builds only): leave a
/// partial temp file in the Vault and hang, so tests can prove a killed
/// child leaves no record and its leftovers are removed at startup.
fn hang_for_tests(data_dir: &Path) {
    if cfg!(debug_assertions) && data_dir.join("capture-hang.test").exists() {
        let dir = vault::vault_dir(data_dir);
        let _ = std::fs::write(
            dir.join(format!(
                "{}-00.bin.traction-tmp-{}",
                now_ms(),
                std::process::id()
            )),
            b"partial",
        );
        std::thread::sleep(std::time::Duration::from_secs(600));
    }
}

/// Everything after the grab: downscale, dHash (a near-duplicate stops
/// before encoding), lossy WebP, salt, `frame_hash`, then the encrypted
/// Vault file. The pixels are dropped when this returns.
pub fn store(
    data_dir: &Path,
    key: &VaultKey,
    image: &Bgra,
    prev_dhash: Option<u64>,
    captured_at_ms: u64,
    quality: f32,
) -> ChildReport {
    let scaled = image::downscale(image);
    let dhash = image::dhash(&scaled);
    if prev_dhash.is_some_and(|prev| hamming(prev, dhash) < DHASH_SKIP_DISTANCE) {
        return ChildReport::Duplicate {
            dhash: dhash_hex(dhash),
        };
    }
    let webp = webp::Encoder::from_rgb(&scaled.pixels, scaled.width, scaled.height).encode(quality);
    drop(scaled);
    let mut salt = [0u8; FRAME_SALT_LEN];
    if getrandom::fill(&mut salt).is_err() {
        return ChildReport::Error;
    }
    let hash = frame_hash(&webp, &salt);
    // The counter keeps ids unique when two Frames share a millisecond.
    let Some(id) = (0..100)
        .filter_map(|n| frame_id(captured_at_ms, n))
        .find(|id| !vault::frame_path(data_dir, id).exists())
    else {
        return ChildReport::Error;
    };
    if std::fs::create_dir_all(vault::vault_dir(data_dir)).is_err()
        || vault::write_frame(data_dir, key, &id, &hash, &salt, &webp).is_err()
    {
        return ChildReport::Error;
    }
    let (width, height) = image::target_size(image.width, image.height);
    ChildReport::Written {
        frame_id: id,
        frame_hash: traction_core::frame_hash_hex(&webp, &salt),
        dhash: dhash_hex(dhash),
        captured_at_ms,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> ChildArgs {
        ChildArgs {
            data_dir: PathBuf::from("/data"),
            display: "0".into(),
            prev_dhash: Some(0x0123_4567_89ab_cdef),
            trigger: FrameTrigger::Scheduled,
            scheduled_at_ms: Some(120_000),
            nonce_id: None,
            expect_focus: "x11:2a".into(),
            quality: 60.0,
        }
    }

    #[test]
    fn arguments_round_trip_and_reject_anything_else() {
        let a = args();
        let parsed = ChildArgs::parse(a.to_args().into_iter().skip(1));
        assert_eq!(parsed, Some(a.clone()));
        let none = ChildArgs {
            prev_dhash: None,
            scheduled_at_ms: None,
            nonce_id: Some("n1".into()),
            trigger: FrameTrigger::FocusShift,
            ..a
        };
        assert_eq!(
            ChildArgs::parse(none.to_args().into_iter().skip(1)),
            Some(none)
        );
        let bad = |from: &str, to: &str| {
            let list: Vec<OsString> = args()
                .to_args()
                .into_iter()
                .skip(1)
                .map(|v| if v == from { to.into() } else { v })
                .collect();
            ChildArgs::parse(list.into_iter())
        };
        assert_eq!(bad("0123456789abcdef", "0123"), None);
        assert_eq!(bad("scheduled", "other"), None);
        assert_eq!(bad("--display", "--title"), None);
        assert_eq!(bad("60", "101"), None);
    }

    #[test]
    fn every_report_is_one_closed_json_object() {
        let cases = [
            (
                ChildReport::PermissionDenied,
                r#"{"status":"permission_denied"}"#,
            ),
            (ChildReport::Unsupported, r#"{"status":"unsupported"}"#),
            (ChildReport::FocusChanged, r#"{"status":"focus_changed"}"#),
            (ChildReport::Error, r#"{"status":"error"}"#),
            (
                ChildReport::Duplicate {
                    dhash: "00000000000000ff".into(),
                },
                r#"{"status":"duplicate","dhash":"00000000000000ff"}"#,
            ),
        ];
        for (report, text) in cases {
            assert_eq!(serde_json::to_string(&report).unwrap(), text);
            assert_eq!(ChildReport::parse(text), Some(report));
        }
        for bad in [
            r#"{"status":"error","why":"x"}"#,
            r#"{"status":"ok"}"#,
            r#"{"status":"duplicate","dhash":"FF"}"#,
            r#"{"status":"duplicate","dhash":"00000000000000FF"}"#,
            r#"{"status":"duplicate"}"#,
            r#"["error"]"#,
        ] {
            assert_eq!(ChildReport::parse(bad), None, "{bad}");
        }
    }
}
