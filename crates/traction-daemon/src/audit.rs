//! The offline half of Step 3a: `export-bundle` writes one Project's
//! frame-less Audit Bundle and `verify` checks a bundle or a local data
//! folder with the same `traction-core` verifier the server will use. Neither
//! needs the daemon to run or a network.
//!
//! Both open SQLite `mode=ro` with `PRAGMA query_only = ON` and never write
//! to the data folder.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use traction_core::{
    AuditBundle, BundleChain, BundleError, BundleInput, BundlePolicy, ChainError, ChainReport,
    KeyId, PayloadPart, VerifyPolicy, VerifyingKeyHex, build_bundle, verify_bundle,
};

use crate::clock::now_ms;
use crate::storage::{self, VerifyDatabaseError};
use crate::{fsutil, signing_key};

/// Why a bundle could not be exported. Each message is one line.
#[derive(Debug)]
pub enum ExportError {
    UnknownProject,
    NoChains,
    SigningKey(io::Error),
    KeyIsNotCurrent,
    Database(rusqlite::Error),
    Write(io::Error),
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::UnknownProject => f.write_str("unknown project id"),
            ExportError::NoChains => {
                f.write_str("nothing is sealed yet: the data folder holds no Chain Link")
            }
            ExportError::SigningKey(e) => write!(f, "cannot read signing.key: {e}"),
            ExportError::KeyIsNotCurrent => f.write_str(
                "signing.key is not the key of the newest chain; start the daemon once so it \
                 seals a first link, then export again",
            ),
            ExportError::Database(e) => write!(f, "cannot read the database: {e}"),
            ExportError::Write(e) => write!(f, "cannot write the bundle: {e}"),
        }
    }
}

impl std::error::Error for ExportError {}

/// Writes `project_id`'s bundle to `out`, stamped `exported_at_ms`.
pub fn export_bundle(
    data_dir: &Path,
    project_id: &str,
    out: &Path,
    exported_at_ms: u64,
) -> Result<(), ExportError> {
    let conn = storage::open_read_only(data_dir).map_err(ExportError::Database)?;
    let known: bool = conn
        .query_row(
            "SELECT EXISTS (SELECT 1 FROM project WHERE project_id = ?1)
                 OR EXISTS (SELECT 1 FROM work_bucket WHERE project_id = ?1)",
            [project_id],
            |row| row.get(0),
        )
        .map_err(ExportError::Database)?;
    if !known {
        return Err(ExportError::UnknownProject);
    }
    let stored = storage::load_chains(&conn).map_err(ExportError::Database)?;
    let first_window = stored
        .chains
        .first()
        .and_then(|chain| chain.links.first())
        .map(|link| link.header.window_start_ms)
        .ok_or(ExportError::NoChains)?;
    let rows = storage::load_sealed_rows(&conn, first_window, u64::try_from(i64::MAX).unwrap_or(0))
        .map_err(ExportError::Database)?;
    let key = signing_key::load_existing(data_dir).map_err(ExportError::SigningKey)?;
    let chains = stored
        .chains
        .into_iter()
        .map(|chain| BundleChain {
            public_key: chain.key.to_hex(),
            links: chain.links,
        })
        .collect();
    let bundle = build_bundle(
        &key,
        BundleInput {
            project_id: project_id.to_owned(),
            chains,
            work_buckets: rows.work_buckets,
            commits: rows.commits,
            rollups: rows.rollups,
            exported_at_ms,
        },
    )
    .map_err(|e| match e {
        traction_core::BundleBuildError::NoChains => ExportError::NoChains,
        _ => ExportError::KeyIsNotCurrent,
    })?;
    fsutil::replace_file(out, &bundle.to_canonical_bytes()).map_err(ExportError::Write)
}

/// What a CLI run prints and exits with.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub code: u8,
}

const EXPORT_USAGE: &str =
    "usage: traction-daemon export-bundle --project <project_id> --out <file> [--data-dir <path>]";
const VERIFY_USAGE: &str = "usage: traction-daemon verify <bundle> [--trusted-key <hex>]... \
    [--expect-tip <key_id>:<seq>:<entry_hash>]\n       traction-daemon verify --data-dir <path> \
    [--trusted-key <hex>]... [--expect-tip <key_id>:<seq>:<entry_hash>]";

fn text(arg: Option<OsString>, usage: &'static str) -> Result<String, String> {
    arg.and_then(|a| a.into_string().ok())
        .ok_or_else(|| usage.to_owned())
}

/// `export-bundle --project <id> --out <file> [--data-dir <path>]`.
pub fn run_export(mut args: impl Iterator<Item = OsString>) -> Result<Outcome, String> {
    let (mut project, mut out, mut data_dir) = (None, None, None);
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--project") => project = Some(text(args.next(), EXPORT_USAGE)?),
            Some("--out") => out = Some(PathBuf::from(args.next().ok_or(EXPORT_USAGE)?)),
            Some("--data-dir") => data_dir = Some(PathBuf::from(args.next().ok_or(EXPORT_USAGE)?)),
            _ => return Err(EXPORT_USAGE.to_owned()),
        }
    }
    let (Some(project), Some(out)) = (project, out) else {
        return Err(EXPORT_USAGE.to_owned());
    };
    let data_dir = resolve_data_dir(data_dir)?;
    export_bundle(&data_dir, &project, &out, now_ms()).map_err(|e| e.to_string())?;
    Ok(Outcome {
        stdout: format!("exported {project} to {}\n", out.display()),
        code: 0,
    })
}

fn resolve_data_dir(data_dir: Option<PathBuf>) -> Result<PathBuf, String> {
    match data_dir {
        Some(dir) => Ok(dir),
        None => crate::DaemonConfig::default_for_user()
            .map(|config| config.data_dir)
            .map_err(|e| e.to_string()),
    }
}

/// `verify <bundle> | --data-dir <path>` with optional pins.
pub fn run_verify(mut args: impl Iterator<Item = OsString>) -> Result<Outcome, String> {
    let (mut bundle, mut data_dir) = (None::<PathBuf>, None::<PathBuf>);
    let mut trusted: Vec<VerifyingKeyHex> = Vec::new();
    let mut expect_tip: Option<(KeyId, u64, String)> = None;
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--data-dir") => data_dir = Some(PathBuf::from(args.next().ok_or(VERIFY_USAGE)?)),
            Some("--trusted-key") => {
                let key = text(args.next(), VERIFY_USAGE)?;
                trusted.push(
                    VerifyingKeyHex::parse(&key)
                        .map_err(|_| "--trusted-key is not a valid public key".to_owned())?,
                );
            }
            Some("--expect-tip") => {
                expect_tip = Some(parse_tip(&text(args.next(), VERIFY_USAGE)?)?);
            }
            Some(flag) if flag.starts_with("--") => return Err(VERIFY_USAGE.to_owned()),
            _ if bundle.is_none() => bundle = Some(PathBuf::from(arg)),
            _ => return Err(VERIFY_USAGE.to_owned()),
        }
    }
    let trusted_keys = (!trusted.is_empty()).then_some(trusted);
    match (bundle, data_dir) {
        (Some(path), None) => {
            let bytes =
                std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
            Ok(verify_bundle_bytes(&bytes, trusted_keys, expect_tip))
        }
        (None, Some(dir)) => Ok(verify_data_dir(&dir, trusted_keys, expect_tip)?),
        _ => Err(VERIFY_USAGE.to_owned()),
    }
}

fn parse_tip(text: &str) -> Result<(KeyId, u64, String), String> {
    let bad = || "--expect-tip is <key_id>:<seq>:<entry_hash>".to_owned();
    // A key id itself holds a colon (`ed25519:<hex>`), so split from the right.
    let (rest, hash) = text.rsplit_once(':').ok_or_else(bad)?;
    let (key_id, seq) = rest.rsplit_once(':').ok_or_else(bad)?;
    let seq: u64 = seq.parse().map_err(|_| bad())?;
    let key_id: KeyId =
        serde_json::from_value(serde_json::Value::String(key_id.to_owned())).map_err(|_| bad())?;
    Ok((key_id, seq, hash.to_owned()))
}

fn verify_bundle_bytes(
    bytes: &[u8],
    trusted_keys: Option<Vec<VerifyingKeyHex>>,
    expect_tip: Option<(KeyId, u64, String)>,
) -> Outcome {
    let policy = BundlePolicy {
        trusted_keys,
        expected_tip: expect_tip,
    };
    match verify_bundle(bytes, &policy) {
        Ok(report) => render_report(&report),
        Err(error) => {
            let links: Vec<(KeyId, u64)> = serde_json::from_slice::<AuditBundle>(bytes)
                .map(|b| {
                    b.chains
                        .iter()
                        .flat_map(|c| {
                            c.links
                                .iter()
                                .map(|l| (l.header.key_id.clone(), l.header.seq))
                        })
                        .collect()
                })
                .unwrap_or_default();
            match error {
                BundleError::Chain(error) => render_failure(&error, &links),
                _ => failure_line(error.code(), "-", None, ""),
            }
        }
    }
}

fn verify_data_dir(
    data_dir: &Path,
    trusted_keys: Option<Vec<VerifyingKeyHex>>,
    expect_tip: Option<(KeyId, u64, String)>,
) -> Result<Outcome, String> {
    let policy = VerifyPolicy {
        trusted_keys,
        expected_tip: expect_tip
            .as_ref()
            .map(|(_, seq, hash)| (*seq, hash.clone())),
        ..VerifyPolicy::default()
    };
    match storage::verify_database_with(data_dir, &policy) {
        Ok((report, _)) => {
            if let (Some((key_id, seq, hash)), Some(last)) = (&expect_tip, report.chains.last())
                && (*key_id != last.key_id || *seq != last.tip.0 || *hash != last.tip.1)
            {
                let error = ChainError::TipMismatch {
                    expected_seq: *seq,
                    found_seq: last.tip.0,
                };
                return Ok(render_failure(&error, &[]));
            }
            Ok(render_report(&report))
        }
        Err((VerifyDatabaseError::Chain(error), stored)) => {
            let links: Vec<(KeyId, u64)> = stored
                .iter()
                .flat_map(|s| s.chains.iter().map(|c| &c.links))
                .flatten()
                .map(|l| (l.header.key_id.clone(), l.header.seq))
                .collect();
            Ok(render_failure(&error, &links))
        }
        Err((VerifyDatabaseError::Database(error), _)) => {
            Err(format!("cannot read the database: {error}"))
        }
    }
}

/// `OK ...`, the discontinuity warnings and the trust note; exit 0 for one
/// chain, 2 for more.
fn render_report(report: &ChainReport) -> Outcome {
    let mut out = String::new();
    let links: usize = report.chains.iter().map(|c| c.links).sum();
    let tip = report.chains.last().map_or_else(
        || "-".to_owned(),
        |c| format!("{}:{}:{}", c.key_id, c.tip.0, c.tip.1),
    );
    out.push_str(&format!(
        "OK chains={} links={links} tip={tip} pinned={} clock_regressions={} pending={} legacy={}\n",
        report.chains.len(),
        report.key_pinned,
        report.clock_regressions,
        report.pending_rows_after_tip,
        report.legacy_rows_before_genesis,
    ));
    for d in &report.key_discontinuities {
        out.push_str(&format!(
            "WARN key_discontinuity from={} to={} at_window={} linked={}\n",
            d.from_key_id, d.to_key_id, d.at_window_ms, d.linked
        ));
    }
    if !report.key_pinned {
        out.push_str(
            "NOTE keys are self-asserted by the input (trust on first use); \
             pass --trusted-key to pin the device's public key\n",
        );
    }
    Outcome {
        stdout: out,
        code: if report.chains.len() > 1 { 2 } else { 0 },
    }
}

fn render_failure(error: &ChainError, links: &[(KeyId, u64)]) -> Outcome {
    let seq = error.seq();
    let key = error
        .key_id()
        .cloned()
        .or_else(|| {
            let seq = seq?;
            links
                .iter()
                .find(|(_, s)| *s == seq)
                .map(|(k, _)| k.clone())
        })
        .map_or_else(|| "-".to_owned(), |k| k.to_string());
    let extra = match error {
        ChainError::PayloadHashMismatch { part, .. } => match part {
            PayloadPart::Payload => " part=payload".to_owned(),
            PayloadPart::DeviceRows => " part=device_rows".to_owned(),
            PayloadPart::Project(id) => format!(" part=project:{id}"),
        },
        ChainError::UnanchoredRow { window_start_ms } => format!(" window={window_start_ms}"),
        _ => String::new(),
    };
    failure_line(error.code(), &key, seq, &extra)
}

fn failure_line(code: &str, key: &str, seq: Option<u64>, extra: &str) -> Outcome {
    let seq = seq.map_or_else(|| "-".to_owned(), |s| s.to_string());
    Outcome {
        stdout: format!("FAIL {code} key={key} seq={seq}{extra}\n"),
        code: 1,
    }
}
