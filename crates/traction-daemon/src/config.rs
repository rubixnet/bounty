//! Daemon configuration. The binary uses [`DaemonConfig::default_for_user`];
//! tests build one with [`DaemonConfig::new`] on a temp data folder and port `0`.

use std::fmt;
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

/// Everything `start` needs. Fields are public so tests can adjust them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DaemonConfig {
    /// Holds `daemon.token` and `traction.db`. Default `~/.traction`.
    pub data_dir: PathBuf,
    /// Loopback HTTP port. `0` picks an ephemeral port (tests only).
    pub http_port: u16,
    /// Writer timer: a non-empty batch is committed at least this often.
    pub commit_interval: Duration,
    /// Writer size threshold: a batch this large is committed immediately.
    pub max_batch: usize,
    /// Bounded writer channel capacity.
    pub writer_queue_capacity: usize,
}

impl DaemonConfig {
    /// Production defaults on the given data folder.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            http_port: DEFAULT_HTTP_PORT,
            commit_interval: DEFAULT_COMMIT_INTERVAL,
            max_batch: DEFAULT_MAX_BATCH,
            writer_queue_capacity: DEFAULT_WRITER_QUEUE_CAPACITY,
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
