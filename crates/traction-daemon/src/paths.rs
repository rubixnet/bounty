//! Canonical paths for the Project Router.
//!
//! Every path the Router compares or stores goes through [`canonicalize`]:
//! symlinks resolved by the OS and, on Windows, the `\\?\` verbatim prefix
//! stripped and the drive letter uppercased. Comparison ([`is_within`]) is
//! component-wise and, on Windows, case-insensitive. macOS case-insensitive
//! volumes are handled by the case the OS returns from canonicalization.

use std::path::{Component, Path, PathBuf};

/// The canonical form of an existing `path`, or `None` when it does not
/// exist, cannot be resolved or is not UTF-8 (Project IDs hash UTF-8).
pub fn canonicalize(path: &Path) -> Option<PathBuf> {
    let canon = std::fs::canonicalize(path).ok()?;
    let text = canon.to_str()?;
    Some(PathBuf::from(normalize(text)))
}

/// [`canonicalize`] when the path exists, else the path made lexically
/// clean (for configured paths that may not exist yet).
pub fn canonical_or_lexical(path: &Path) -> PathBuf {
    canonicalize(path).unwrap_or_else(|| lexical(path))
}

/// `path` without `.` components and with `..` applied lexically.
fn lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(windows)]
fn normalize(text: &str) -> String {
    let text = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        text.strip_prefix(r"\\?\").unwrap_or(text).to_owned()
    };
    let mut bytes = text.into_bytes();
    if bytes.get(1) == Some(&b':') {
        bytes[0] = bytes[0].to_ascii_uppercase();
    }
    // Only an ASCII byte changed, so this stays valid UTF-8.
    String::from_utf8(bytes).unwrap_or_default()
}

#[cfg(not(windows))]
fn normalize(text: &str) -> String {
    text.to_owned()
}

/// `path` is `base` or inside it (component-wise; case-insensitive on
/// Windows).
pub fn is_within(path: &Path, base: &Path) -> bool {
    #[cfg(windows)]
    {
        let fold = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        fold(path).starts_with(fold(base))
    }
    #[cfg(not(windows))]
    {
        path.starts_with(base)
    }
}

/// `a` and `b` name the same folder.
pub fn same(a: &Path, b: &Path) -> bool {
    is_within(a, b) && is_within(b, a)
}

/// The components of `path` below `base` (which must contain it).
pub fn components_below<'a>(
    path: &'a Path,
    base: &Path,
) -> impl Iterator<Item = Component<'a>> + 'a {
    path.components().skip(base.components().count())
}

/// The components of `path` below its filesystem root (prefix and root
/// dir dropped).
pub fn components_below_root(path: &Path) -> impl Iterator<Item = Component<'_>> {
    path.components()
        .filter(|c| matches!(c, Component::Normal(_)))
}

/// `name` equals `wanted`: case-insensitively on Windows and macOS (whose
/// default volumes are case-insensitive), exactly elsewhere.
pub fn name_matches(name: &str, wanted: &str) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        name.eq_ignore_ascii_case(wanted)
    } else {
        name == wanted
    }
}
