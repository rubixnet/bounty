//! The Traction daemon library: Local Ingestion Bus, single-instance lock and
//! SQLite WAL persistence. The binary in `main.rs` is a thin wrapper.
//!
//! Public surface (the integration-test seam): [`DaemonConfig`], [`start`]
//! and [`DaemonHandle`]. Everything else is internal.

mod attribution;
mod audit;
mod bus;
mod claude_code;
mod clock;
mod commits;
mod config;
mod content_metrics;
mod coverage;
mod daemon;
mod event_id;
mod exclusions;
mod finalizer;
mod fsutil;
mod git;
mod http;
mod install;
mod lock;
mod log;
mod login;
#[cfg(windows)]
mod owner_only;
mod paths;
mod pause;
mod presence;
mod router;
mod secret;
mod signing_key;
#[cfg(unix)]
mod socket;
mod storage;
mod submit;
mod symbol_index;
mod sync;
mod token;
mod watcher;
mod writer;

#[doc(hidden)]
pub use audit::{ExportError, Outcome, export_bundle, run_export, run_verify};
pub use bus::{EVENT_PATH, MAX_BODY_BYTES, PROJECT_PATH};
pub use claude_code::config_dir::{CLAUDE_CONFIG_DIR_ENV, claude_config_dir};
pub use claude_code::hook::{HOOK_PATH as CLAUDE_CODE_HOOK_PATH, MAX_HOOK_BODY_BYTES};
pub use claude_code::pause::{Command as PauseCommand, PauseAction, RESUME_TRACK, STOP_TRACK};
pub use commits::CommitSettings;
pub use config::{
    ConfigError, DEFAULT_COMMIT_INTERVAL, DEFAULT_GIT_PROGRAM, DEFAULT_HTTP_PORT,
    DEFAULT_MAX_BATCH, DEFAULT_WATCHER_PENDING_CAP, DEFAULT_WRITER_QUEUE_CAPACITY, DaemonConfig,
    GLOBAL_IGNORE_FILE, HOOK_COVERAGE_SKEW, LOG_TAIL_HOLD, LOG_TAIL_TRAILING_WINDOW,
    STALE_TURN_TIMEOUT, WINDOW_CLOSE_GRACE, WatcherSettings, WriterSettings,
};
#[cfg(unix)]
pub use config::{DATA_DIR_SOCKET_FILE, RUNTIME_SOCKET_FILE, default_socket_path};
pub use daemon::{DaemonHandle, ShutdownError, StartError, start};
pub use exclusions::{DEPENDENCY_FOLDERS, PROJECT_MARKER_EXTENSION, PROJECT_MARKERS};
pub use finalizer::FinalizerSettings;
#[doc(hidden)]
pub use finalizer::finalize_settled;
pub use install::claude_code::{ClaudeCodeInstall, InstallError, InstallReport};
pub use install::{BACKUP_SUFFIX, MIN_ON_FAILURE_VERSION, VERSION_TIMEOUT, Version};
pub use lock::LOCK_FILE;
#[doc(hidden)]
pub use login::{LoginOptions, login, run_login};
pub use presence::{DEFAULT_SUBSLOT, PresenceSettings};
pub use router::RESOLUTION_TTL;
pub use secret::DEVICE_SECRET_FILE;
pub use signing_key::{SIGNING_KEY_FILE, SIGNING_PUB_FILE};
pub use storage::{DB_FILE, PROJECT_EVENT_VIEW};
#[doc(hidden)]
pub use storage::{VerifyDatabaseError, verify_database, verify_database_with};
pub use submit::{run_submit, submit_project};
pub use sync::{
    BACKOFF_BASE, BACKOFF_MAX, MAX_LINKS_PER_REQUEST, MAX_RETRY_AFTER, OFFLINE_BUFFERED_AFTER,
    REQUEST_TIMEOUT, SERVER_KEY_ID_FILE, SERVER_TOKEN_FILE, SERVER_URL_FILE, SYNC_INTERVAL,
    SyncSettings, backoff_delay, sync_mode_for, wait_after_failure,
};
pub use token::TOKEN_FILE;
