//! The Claude Code version check: `onFailure` on HTTP hooks needs Claude
//! Code `2.1.295` or newer. Older versions are already non-blocking by
//! default, so the installer only warns.

use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::git::wait_bounded;

/// The first Claude Code version that honours `"onFailure"` on hooks.
pub const MIN_ON_FAILURE_VERSION: Version = Version(2, 1, 295);

/// The hard timeout for `<claude-program> --version`; the child is killed
/// on expiry.
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(5);

/// A `major.minor.patch` version.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u32, pub u32, pub u32);

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// What `--version` told us.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionCheck {
    Supported(Version),
    TooOld(Version),
    /// The program is missing, failed, timed out or printed no version.
    Unknown,
}

impl VersionCheck {
    /// The warning to show, if any.
    pub fn warning(self) -> Option<String> {
        match self {
            Self::Supported(_) => None,
            Self::TooOld(found) => Some(format!(
                "Claude Code {found} is older than {MIN_ON_FAILURE_VERSION} and ignores \
                 \"onFailure\"; hooks are still non-blocking by default, so installing anyway"
            )),
            Self::Unknown => Some(
                "could not check Claude Code version; installing anyway \
                 (\"onFailure\" needs Claude Code 2.1.295 or newer)"
                    .to_owned(),
            ),
        }
    }
}

/// Runs `<program> --version` with [`VERSION_TIMEOUT`].
pub fn check(program: &Path) -> VersionCheck {
    let child = Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(child) = child else {
        return VersionCheck::Unknown;
    };
    let Ok(stdout) = wait_bounded(child, VERSION_TIMEOUT) else {
        return VersionCheck::Unknown;
    };
    match parse(&String::from_utf8_lossy(&stdout)) {
        Some(found) if found >= MIN_ON_FAILURE_VERSION => VersionCheck::Supported(found),
        Some(found) => VersionCheck::TooOld(found),
        None => VersionCheck::Unknown,
    }
}

/// The first `major.minor.patch` word in `output` (e.g. `2.1.332 (Claude
/// Code)`), allowing a leading `v` and a `-pre`/`+build` suffix.
fn parse(output: &str) -> Option<Version> {
    output.split_whitespace().find_map(|word| {
        let word = word.strip_prefix('v').unwrap_or(word);
        let core = word.split(['-', '+']).next()?;
        let mut parts = core.split('.');
        let mut next = || -> Option<u32> {
            let part = parts.next()?;
            (!part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()))
                .then(|| part.parse().ok())?
        };
        let version = Version(next()?, next()?, next()?);
        parts.next().is_none().then_some(version)
    })
}
