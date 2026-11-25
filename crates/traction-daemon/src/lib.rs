//! The Traction daemon library: Local Ingestion Bus, single-instance lock and
//! SQLite WAL persistence. The binary in `main.rs` is a thin wrapper.
//!
//! Public surface (the integration-test seam): [`DaemonConfig`], [`start`]
//! and [`DaemonHandle`]. Everything else is internal.

mod bus;
mod config;
mod daemon;
mod git;
mod http;
mod lock;
mod log;
#[cfg(unix)]
mod socket;
mod storage;
mod token;
mod writer;

pub use bus::{EVENT_PATH, MAX_BODY_BYTES};
pub use config::{
    ConfigError, DEFAULT_COMMIT_INTERVAL, DEFAULT_HTTP_PORT, DEFAULT_MAX_BATCH,
    DEFAULT_WRITER_QUEUE_CAPACITY, DaemonConfig, WriterSettings,
};
#[cfg(unix)]
pub use config::{DATA_DIR_SOCKET_FILE, RUNTIME_SOCKET_FILE, default_socket_path};
pub use daemon::{DaemonHandle, ShutdownError, StartError, start};
pub use lock::LOCK_FILE;
pub use storage::DB_FILE;
pub use token::TOKEN_FILE;
