//! Which folders may never become Project Roots, and which the user has
//! excluded entirely (the global ignore file).
//!
//! A candidate Project Root is excluded when it:
//! - is `$HOME` itself or a filesystem root;
//! - is under an OS folder or a cache/toolchain folder (per-OS lists,
//!   including the OS temp folder and the daemon's data folder);
//! - has a path component in [`DEPENDENCY_FOLDERS`], counted below `$HOME`
//!   (or below the filesystem root outside `$HOME`); or
//! - is under a user-excluded folder.
//!
//! A list entry that contains `$HOME` (for example `/var/folders/...` on a
//! macOS test home, or `/home` on Linux) never excludes `$HOME`'s
//! descendants, so homes under OS or temp folders keep working.

use std::fs;
use std::path::{Component, Path, PathBuf};

use crate::log;
use crate::paths::{self, canonical_or_lexical, is_within, name_matches, same};

/// The closed project-marker list: the nearest ancestor holding one of
/// these is a Project Root (Router step 2). `*.sln` is matched by extension.
pub const PROJECT_MARKERS: [&str; 12] = [
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "go.mod",
    ".traction-project",
    "deno.json",
    "pom.xml",
    "build.gradle",
    "Gemfile",
    "composer.json",
    "CMakeLists.txt",
    "Makefile",
];
/// The marker matched by extension (`*.sln`).
pub const PROJECT_MARKER_EXTENSION: &str = "sln";

/// A path component naming one of these makes a folder a dependency or
/// build folder, never a Project Root.
pub const DEPENDENCY_FOLDERS: [&str; 10] = [
    "node_modules",
    "target",
    ".git",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".gradle",
    ".next",
    "dist-newstyle",
];

/// OS folders (absolute).
#[cfg(target_os = "macos")]
const OS_FOLDERS: &[&str] = &[
    "/System",
    "/Library",
    "/Applications",
    "/private",
    "/usr",
    "/bin",
    "/sbin",
    "/etc",
    "/var",
    "/tmp",
    "/dev",
    "/opt",
    "/cores",
    "/nix",
];
#[cfg(all(unix, not(target_os = "macos")))]
const OS_FOLDERS: &[&str] = &[
    "/bin", "/sbin", "/usr", "/lib", "/lib32", "/lib64", "/libx32", "/etc", "/dev", "/proc",
    "/sys", "/run", "/boot", "/var", "/tmp", "/opt", "/snap", "/nix",
];

/// Cache and toolchain folders, relative to `$HOME`.
const HOME_CACHE_FOLDERS: &[&str] = &[
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".pnpm-store",
    ".yarn",
    ".nvm",
    ".pyenv",
    ".rbenv",
    ".gradle",
    ".m2",
    ".bun",
    ".deno",
    "go/pkg",
    ".local/share",
    ".Trash",
    #[cfg(target_os = "macos")]
    "Library/Caches",
    #[cfg(target_os = "macos")]
    "Library/Application Support",
    #[cfg(target_os = "macos")]
    "Library/Containers",
    #[cfg(target_os = "macos")]
    "Library/Developer",
    #[cfg(windows)]
    "AppData",
    #[cfg(windows)]
    "scoop",
];

/// Windows OS folders come from the environment (the system drive varies).
#[cfg(windows)]
const OS_FOLDER_VARS: &[&str] = &[
    "SystemRoot",
    "windir",
    "ProgramFiles",
    "ProgramFiles(x86)",
    "ProgramW6432",
    "ProgramData",
    "LOCALAPPDATA",
    "APPDATA",
    "TEMP",
    "TMP",
];

/// The folder rules for one daemon.
#[derive(Debug, Clone)]
pub struct Exclusions {
    /// Canonical `$HOME`, when known.
    home: Option<PathBuf>,
    /// OS, cache and toolchain folders (canonical when they exist).
    system: Vec<PathBuf>,
    /// The user's global ignore list (canonical when they exist).
    user: Vec<PathBuf>,
}

impl Exclusions {
    /// The rules for `home` and the daemon's `data_dir`, with the user list
    /// read from `ignore_file` (missing file: no user exclusions).
    pub fn load(home: Option<&Path>, data_dir: &Path, ignore_file: &Path) -> Self {
        let home = home.map(canonical_or_lexical);
        let mut system: Vec<PathBuf> = Vec::new();
        #[cfg(unix)]
        system.extend(OS_FOLDERS.iter().map(PathBuf::from));
        #[cfg(windows)]
        {
            system.extend(
                OS_FOLDER_VARS
                    .iter()
                    .filter_map(std::env::var_os)
                    .map(PathBuf::from)
                    .filter(|p| p.is_absolute()),
            );
            if let Some(drive) = std::env::var_os("SystemDrive") {
                let root = PathBuf::from(drive).join("\\");
                system.push(root.join("$Recycle.Bin"));
                system.push(root.join("System Volume Information"));
                system.push(root.join("Recovery"));
            }
        }
        if let Some(home) = &home {
            system.extend(HOME_CACHE_FOLDERS.iter().map(|rel| home.join(rel)));
        }
        system.push(std::env::temp_dir());
        system.push(data_dir.to_path_buf());
        let system = system.iter().map(|p| canonical_or_lexical(p)).collect();
        let user = read_ignore_file(ignore_file, home.as_deref());
        Self { home, system, user }
    }

    /// Canonical `$HOME`, when known.
    pub fn home(&self) -> Option<&Path> {
        self.home.as_deref()
    }

    /// `path` (canonical) lies inside a user-excluded folder.
    pub fn is_user_excluded(&self, path: &Path) -> bool {
        self.user.iter().any(|folder| is_within(path, folder))
    }

    /// `path` (canonical) may never be a Project Root.
    pub fn is_excluded(&self, path: &Path) -> bool {
        if path.parent().is_none() {
            return true;
        }
        let under_home = self.home.as_deref().filter(|home| is_within(path, home));
        if let Some(home) = under_home
            && same(path, home)
        {
            return true;
        }
        let in_system_folder = self.system.iter().any(|folder| {
            is_within(path, folder)
                // A list entry containing `$HOME` never excludes its descendants.
                && !under_home.is_some_and(|home| is_within(home, folder))
        });
        in_system_folder
            || self.has_dependency_component(path, under_home)
            || self.is_user_excluded(path)
    }

    fn has_dependency_component(&self, path: &Path, under_home: Option<&Path>) -> bool {
        let is_dependency = |component: Component<'_>| {
            component.as_os_str().to_str().is_some_and(|name| {
                DEPENDENCY_FOLDERS
                    .iter()
                    .any(|wanted| name_matches(name, wanted))
            })
        };
        match under_home {
            Some(home) => paths::components_below(path, home).any(is_dependency),
            None => paths::components_below_root(path).any(is_dependency),
        }
    }
}

/// `dir` holds a project marker.
pub fn has_project_marker(dir: &Path) -> bool {
    if PROJECT_MARKERS.iter().any(|m| dir.join(m).exists()) {
        return true;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let path = entry.path();
        path.extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case(PROJECT_MARKER_EXTENSION))
            && entry.file_type().is_ok_and(|t| t.is_file())
    })
}

/// Parses the global ignore file: one folder per line, absolute or `~/`
/// (relative to `home`); blank lines and `#` comments skipped. Other lines
/// are ignored (logged once, without the line).
fn read_ignore_file(file: &Path, home: Option<&Path>) -> Vec<PathBuf> {
    let Ok(text) = fs::read_to_string(file) else {
        return Vec::new();
    };
    let mut skipped = false;
    let mut folders = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let home_relative = line.strip_prefix("~/").or_else(|| line.strip_prefix("~\\"));
        let folder = match (home_relative, home) {
            (Some(rest), Some(home)) => Some(home.join(rest)),
            (Some(_), None) => None,
            (None, _) => Some(PathBuf::from(line)).filter(|p| p.is_absolute()),
        };
        match folder {
            Some(folder) => folders.push(canonical_or_lexical(&folder)),
            None => skipped = true,
        }
    }
    if skipped {
        log::warn("ignored a line in the global ignore file that is not an absolute or ~/ path");
    }
    folders
}
