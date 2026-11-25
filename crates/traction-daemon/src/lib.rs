//! The Traction daemon library: Local Ingestion Bus, single-instance lock and
//! SQLite WAL persistence. The binary in `main.rs` is a thin wrapper.
//!
//! Public surface (the integration-test seam): [`DaemonConfig`], [`start`]
//! and [`DaemonHandle`]. Everything else is internal.

mod bus;
mod config;
mod daemon;
mod exclusions;
mod fsutil;
mod git;
mod http;
mod lock;
mod log;
mod paths;
mod router;
mod secret;
#[cfg(unix)]
mod socket;
mod storage;
mod token;
mod watcher;
mod writer;

pub use bus::{EVENT_PATH, MAX_BODY_BYTES, PROJECT_PATH};
pub use config::{
    ConfigError, DEFAULT_COMMIT_INTERVAL, DEFAULT_GIT_PROGRAM, DEFAULT_HTTP_PORT,
    DEFAULT_MAX_BATCH, DEFAULT_WATCHER_PENDING_CAP, DEFAULT_WRITER_QUEUE_CAPACITY, DaemonConfig,
    GLOBAL_IGNORE_FILE, LOG_TAIL_HOLD, WatcherSettings, WriterSettings,
};
#[cfg(unix)]
pub use config::{DATA_DIR_SOCKET_FILE, RUNTIME_SOCKET_FILE, default_socket_path};
pub use daemon::{DaemonHandle, ShutdownError, StartError, start};
pub use exclusions::{DEPENDENCY_FOLDERS, PROJECT_MARKER_EXTENSION, PROJECT_MARKERS};
pub use lock::LOCK_FILE;
pub use router::RESOLUTION_TTL;
pub use secret::DEVICE_SECRET_FILE;
pub use storage::{DB_FILE, PROJECT_EVENT_VIEW};
pub use token::TOKEN_FILE;
