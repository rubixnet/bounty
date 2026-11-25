//! Daemon configuration. The binary uses [`DaemonConfig::default_for_user`];
//! tests build one with [`DaemonConfig::new`] on a temp data folder and port `0`.

#[cfg(unix)]
use std::ffi::OsStr;
use std::fmt;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

/// The production Local Ingestion Bus port (`docs/protocol.md` section 1).
pub const DEFAULT_HTTP_PORT: u16 = 41937;
/// How often the writer commits a batch when it is not full.
pub const DEFAULT_COMMIT_INTERVAL: Duration = Duration::from_secs(5);
/// The writer commits early once a batch holds this many events.
pub const DEFAULT_MAX_BATCH: usize = 256;
/// Events the bus may queue for the writer before it answers "busy".
pub const DEFAULT_WRITER_QUEUE_CAPACITY: usize = 1024;
/// The `git` program the Project Router runs (looked up on `PATH`).
pub const DEFAULT_GIT_PROGRAM: &str = "git";
/// The global ignore file inside the data folder: one folder per line
/// (absolute or `~/`), `#` comments. Events in a listed folder are dropped.
pub const GLOBAL_IGNORE_FILE: &str = "ignored-folders";
/// How long the file watcher holds each observed mutation before deciding
/// it (`docs/protocol.md` section 2): a delivery-lag buffer, so late
/// log-tailed events can still claim the edit. Not an attribution window.
pub const LOG_TAIL_HOLD: Duration = Duration::from_secs(5);
/// Files the watcher may hold per Project at once; past this, the oldest
/// held mutation of that Project is decided early (bounded memory).
pub const DEFAULT_WATCHER_PENDING_CAP: usize = 1024;
/// The bus socket's file name inside `$XDG_RUNTIME_DIR`.
#[cfg(unix)]
pub const RUNTIME_SOCKET_FILE: &str = "traction.sock";
/// The bus socket's file name inside the data folder (no `$XDG_RUNTIME_DIR`).
#[cfg(unix)]
pub const DATA_DIR_SOCKET_FILE: &str = "daemon.sock";

/// Everything `start` needs. Fields are public so tests can adjust them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DaemonConfig {
    /// Holds `daemon.token` and `traction.db`. Default `~/.traction`.
    pub data_dir: PathBuf,
    /// Loopback HTTP port. `0` picks an ephemeral port (tests only).
    pub http_port: u16,
    /// SQLite writer tuning.
    pub writer: WriterSettings,
    /// The user's home folder: never a Project Root, the base of
    /// `home_relative_path`, and where OS/cache exclusions stop applying.
    /// Default: the OS home. Tests inject a temp folder.
    pub home_dir: Option<PathBuf>,
    /// The `git` binary the hardened wrapper runs. Default
    /// [`DEFAULT_GIT_PROGRAM`] from `PATH`; tests name a missing program to
    /// stand for "no `git` on `PATH`".
    pub git_program: PathBuf,
    /// File watcher tuning.
    pub watcher: WatcherSettings,
    /// The Local Ingestion Bus Unix socket (macOS/Linux only). Default from
    /// [`default_socket_path`].
    #[cfg(unix)]
    pub socket_path: PathBuf,
}

impl DaemonConfig {
    /// Production defaults on the given data folder.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            #[cfg(unix)]
            socket_path: default_socket_path(
                std::env::var_os("XDG_RUNTIME_DIR").as_deref(),
                &data_dir,
            ),
            data_dir,
            http_port: DEFAULT_HTTP_PORT,
            writer: WriterSettings::default(),
            home_dir: std::env::home_dir().filter(|home| home.is_absolute()),
            git_program: PathBuf::from(DEFAULT_GIT_PROGRAM),
            watcher: WatcherSettings::default(),
        }
    }

    /// Production defaults with the data folder at `~/.traction`.
    pub fn default_for_user() -> Result<Self, ConfigError> {
        let home = std::env::home_dir()
            .filter(|home| home.is_absolute())
            .ok_or(ConfigError::NoHomeDir)?;
        Ok(Self::new(home.join(".traction")))
    }
}

/// SQLite writer tuning. Fields are public so tests can adjust them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WriterSettings {
    /// A non-empty batch is committed at least this often.
    pub commit_interval: Duration,
    /// A batch this large is committed immediately.
    pub max_batch: usize,
    /// Bounded writer channel capacity.
    pub queue_capacity: usize,
}

impl Default for WriterSettings {
    fn default() -> Self {
        Self {
            commit_interval: DEFAULT_COMMIT_INTERVAL,
            max_batch: DEFAULT_MAX_BATCH,
            queue_capacity: DEFAULT_WRITER_QUEUE_CAPACITY,
        }
    }
}

/// File watcher tuning. Production always uses the defaults; the fields are
/// public only so tests can shorten the hold or shrink the cap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct WatcherSettings {
    /// Test-only override of [`LOG_TAIL_HOLD`].
    pub hold: Duration,
    /// Held files per Project before the oldest is decided early.
    pub pending_cap: usize,
}

impl Default for WatcherSettings {
    fn default() -> Self {
        Self {
            hold: LOG_TAIL_HOLD,
            pending_cap: DEFAULT_WATCHER_PENDING_CAP,
        }
    }
}

/// The default bus socket: `$XDG_RUNTIME_DIR/traction.sock`, else
/// `<data dir>/daemon.sock`. Takes the variable's value instead of reading the
/// environment, so it is testable. An empty or relative value counts as
/// unset, as the XDG Base Directory spec requires.
#[cfg(unix)]
pub fn default_socket_path(xdg_runtime_dir: Option<&OsStr>, data_dir: &Path) -> PathBuf {
    match xdg_runtime_dir.map(Path::new) {
        Some(runtime_dir) if runtime_dir.is_absolute() => runtime_dir.join(RUNTIME_SOCKET_FILE),
        _ => data_dir.join(DATA_DIR_SOCKET_FILE),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// The user's home folder could not be determined.
    NoHomeDir,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoHomeDir => f.write_str("cannot determine the user's home folder"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn the_socket_lives_in_xdg_runtime_dir_when_it_is_set() {
        let path = default_socket_path(
            Some(OsStr::new("/run/user/1000")),
            Path::new("/home/u/.traction"),
        );
        assert_eq!(path, Path::new("/run/user/1000/traction.sock"));
    }

    #[test]
    fn the_socket_falls_back_to_the_data_folder_without_xdg_runtime_dir() {
        let path = default_socket_path(None, Path::new("/home/u/.traction"));
        assert_eq!(path, Path::new("/home/u/.traction/daemon.sock"));
    }

    #[test]
    fn an_empty_or_relative_xdg_runtime_dir_is_ignored() {
        for value in ["", "run/user/1000"] {
            let path = default_socket_path(Some(OsStr::new(value)), Path::new("/data"));
            assert_eq!(
                path,
                Path::new("/data/daemon.sock"),
                "XDG_RUNTIME_DIR={value:?}"
            );
        }
    }
}
