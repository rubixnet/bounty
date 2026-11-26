//! Library-first daemon lifecycle: [`start`] brings every part up and returns
//! a [`DaemonHandle`]; [`DaemonHandle::shutdown`] stops accepting, flushes
//! the writer and returns once everything accepted is committed.
//!
//! Threads: one runs a single-threaded tokio runtime serving the bus, one is
//! the Project Router, one is the file watcher (plus the OS watch backend's
//! own), one is the SQLite writer. Startup order matters: data
//! folder, single-instance lock, token, device secret, database (and the
//! stored registry), listener, writer, Router, server. Nothing
//! in the data folder (token, device secret, database, later the socket) is
//! touched before the lock is held, and the lock is released only after
//! every thread has stopped. On macOS/Linux the bus socket is bound after the
//! TCP listener and removed on shutdown, before the lock is released.

use std::fmt;
use std::io;
use std::net::{Ipv4Addr, SocketAddr, TcpListener};
use std::path::Path;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use tokio::sync::oneshot;

use crate::bus::Bus;
use crate::claude_code::tailer::{self, TailerThread};
use crate::commits::{self, CommitObserver};
use crate::config::{DaemonConfig, GLOBAL_IGNORE_FILE};
use crate::exclusions::Exclusions;
use crate::finalizer::{self, FinalizerThread};
use crate::git::Git;
use crate::http::Listeners;
use crate::lock::{InstanceLock, LockError};
use crate::presence::{self, PresenceSensor};
use crate::router::{self, Router, RouterThread};
use crate::secret::DeviceSecret;
use crate::signing_key;
#[cfg(unix)]
use crate::socket::{self, SocketError, SocketFile};
use crate::storage::Sealer;
use crate::symbol_index::SymbolIndexes;
use crate::token::BusToken;
use crate::watcher::{self, WatcherThread, Watches};
use crate::writer::{self, WriterThread};
use crate::{http, log, storage};

/// A running daemon. Dropping it performs the same graceful shutdown.
pub struct DaemonHandle {
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<()>>,
    /// The Claude transcript tailer (absent without a Claude config dir).
    tailer: Option<TailerThread>,
    watcher: Option<WatcherThread>,
    /// The Presence Sensor (device-wide; unaffected by pause).
    presence: Option<PresenceSensor>,
    /// The commit observer (stat-only watch of each worktree's reflog).
    commits: Option<CommitObserver>,
    /// The Work Bucket finalizer.
    finalizer: Option<FinalizerThread>,
    router: Option<RouterThread>,
    writer: Option<WriterThread>,
    #[cfg(unix)]
    socket_path: PathBuf,
    /// Removes the socket file on drop. Declared before `_lock` so the file
    /// is gone before another daemon can take the lock and bind.
    #[cfg(unix)]
    _socket: SocketFile,
    /// Dropped last (after `stop` joined every thread), so a new daemon can
    /// only start once this one has flushed.
    _lock: InstanceLock,
}

/// Starts the daemon described by `config`.
pub fn start(config: DaemonConfig) -> Result<DaemonHandle, StartError> {
    create_data_dir(&config.data_dir).map_err(StartError::DataDir)?;
    let lock = InstanceLock::acquire(&config.data_dir).map_err(|error| match error {
        LockError::AlreadyRunning => StartError::AlreadyRunning,
        LockError::Io(error) => StartError::Lock(error),
    })?;
    let token = BusToken::load_or_create(&config.data_dir).map_err(StartError::Token)?;
    let device_secret =
        DeviceSecret::load_or_create(&config.data_dir).map_err(StartError::DeviceSecret)?;
    let conn = storage::open(&config.data_dir).map_err(StartError::Database)?;
    let signing_key =
        signing_key::load_or_create(&config.data_dir).map_err(StartError::SigningKey)?;
    let sealer = Sealer::open(&conn, signing_key).map_err(StartError::Database)?;
    let registry = storage::load_registry(&conn).map_err(StartError::Database)?;
    let tailer_state = storage::load_tailer_state(&conn).map_err(StartError::Database)?;

    // Loopback IPv4 only: never `0.0.0.0` or `::`. No fallback port.
    let listener =
        TcpListener::bind((Ipv4Addr::LOCALHOST, config.http_port)).map_err(|source| {
            if source.kind() == io::ErrorKind::AddrInUse {
                StartError::PortInUse {
                    port: config.http_port,
                }
            } else {
                StartError::Bind {
                    port: config.http_port,
                    source,
                }
            }
        })?;
    listener
        .set_nonblocking(true)
        .map_err(StartError::Runtime)?;
    let local_addr = listener.local_addr().map_err(StartError::Runtime)?;

    #[cfg(unix)]
    let (unix_listener, socket_file) = socket::bind(&config.socket_path, &lock)
        .map_err(|error| socket_start_error(error, &config.socket_path))?;

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .map_err(StartError::Runtime)?;

    let (writer_sender, writer_thread) =
        writer::spawn(conn, config.writer, sealer).map_err(StartError::Runtime)?;

    let exclusions = Exclusions::load(
        config.home_dir.as_deref(),
        &config.data_dir,
        &config.data_dir.join(GLOBAL_IGNORE_FILE),
    );
    let git = Git::with_program(&config.git_program, exclusions.home());
    let observer_git = Arc::new(Git::with_program(&config.git_program, exclusions.home()));
    let symbols = SymbolIndexes::default();
    let (watches, notifications) = Watches::new(exclusions.clone());
    watches.set_symbol_index(symbols.clone());
    let presence_writer = writer_sender.clone();
    let commits_writer = writer_sender.clone();
    let finalizer_writer = writer_sender.clone();
    let mut router = Router::new(git, exclusions, device_secret, writer_sender, registry);
    router.set_attribution_timing(&config.watcher);
    // Re-attach every stored (non-merged) Project before anything can edit
    // unobserved; later registrations attach on the Router thread before
    // they are answered.
    for project in router.projects() {
        watches.attach(project);
    }
    // Commit Observations: Git Projects (and every folder resolved in one,
    // which is how linked worktrees are found) go to the observer.
    let commit_observer = if config.commits.enabled {
        match commits::spawn(
            &traction_core::TRACTION_RULES_1,
            observer_git,
            commits_writer,
            config.commits,
        ) {
            Ok(observer) => Some(observer),
            Err(error) => {
                log::error("failed to start the commit observer", &error);
                None
            }
        }
    } else {
        None
    };
    if let Some(observer) = &commit_observer {
        let watch = observer.watch_handle();
        for project in router.projects() {
            if project.kind == storage::ProjectKind::Git {
                watch.watch(&project.project_id, &project.root);
            }
        }
        let hook_watch = watch.clone();
        router.add_project_hook(Box::new(move |project| {
            if project.merged_into.is_none() && project.kind == storage::ProjectKind::Git {
                hook_watch.watch(&project.project_id, &project.root);
            }
        }));
        router.set_folder_hook(Box::new(move |project_id, folder| {
            watch.watch(project_id, folder);
        }));
    }
    let hook_watches = watches.clone();
    router.add_project_hook(Box::new(move |project| match &project.merged_into {
        Some(into) => hook_watches.merge(&project.project_id, into),
        None => hook_watches.attach(project),
    }));
    let (router_sender, router_thread) = match router::spawn(router, config.writer.queue_capacity) {
        Ok(spawned) => spawned,
        Err(error) => {
            // The Router (and its writer sender) was dropped.
            let _ = writer_thread.join();
            return Err(StartError::Runtime(error));
        }
    };
    let watcher_thread = match watcher::spawn(
        watches,
        notifications,
        router_sender.clone(),
        config.watcher,
    ) {
        Ok(thread) => thread,
        Err(error) => {
            drop(router_sender);
            let _ = router_thread.join();
            let _ = writer_thread.join();
            return Err(StartError::Runtime(error));
        }
    };
    let tailer_thread = match &config.claude_config_dir {
        None => None,
        Some(dir) => match tailer::start(dir, tailer_state, router_sender.clone(), symbols.clone())
        {
            Ok(thread) => Some(thread),
            Err(error) => {
                drop(router_sender);
                let _ = watcher_thread.stop();
                let _ = router_thread.join();
                let _ = writer_thread.join();
                return Err(StartError::Runtime(error));
            }
        },
    };

    let hints = tailer_thread.as_ref().map(TailerThread::hints);
    let bus = Arc::new(
        Bus::new(token, router_sender, local_addr.port())
            .with_transcript_hints(hints)
            .with_symbols(symbols),
    );
    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let server = thread::Builder::new()
        .name("traction-bus".to_owned())
        .spawn(move || {
            runtime.block_on(async move {
                match register(
                    listener,
                    #[cfg(unix)]
                    unix_listener,
                ) {
                    Ok(listeners) => http::serve(listeners, bus, shutdown_rx).await,
                    Err(error) => log::error("failed to register the bus listener", &error),
                }
            });
            // Dropping the runtime cancels leftover connection tasks, which
            // drops the last writer senders and lets the writer drain.
            drop(runtime);
        });
    let server = match server {
        Ok(server) => server,
        Err(error) => {
            // The closure (and its Router sender) was dropped; flush the
            // watcher, then let the Router, then the writer, exit.
            if let Some(tailer) = tailer_thread {
                let _ = tailer.stop();
            }
            let _ = watcher_thread.stop();
            let _ = router_thread.join();
            let _ = writer_thread.join();
            return Err(StartError::Runtime(error));
        }
    };

    // Presence is best effort: a sensor that cannot start never stops the
    // daemon (the backend degrades its own tier instead).
    let presence = if config.presence.enabled {
        match presence::spawn(
            &traction_core::TRACTION_RULES_1,
            presence::detect(),
            presence_writer,
            config.presence.subslot,
        ) {
            Ok(sensor) => Some(sensor),
            Err(error) => {
                log::error("failed to start the presence sensor", &error);
                None
            }
        }
    } else {
        None
    };

    let finalizer = if config.finalizer.enabled {
        match finalizer::spawn(&config.data_dir, finalizer_writer, config.finalizer) {
            Ok(thread) => Some(thread),
            Err(error) => {
                log::error("failed to start the finalizer", &error);
                None
            }
        }
    } else {
        None
    };

    Ok(DaemonHandle {
        local_addr,
        shutdown: Some(shutdown_tx),
        server: Some(server),
        tailer: tailer_thread,
        watcher: Some(watcher_thread),
        presence,
        commits: commit_observer,
        finalizer,
        router: Some(router_thread),
        writer: Some(writer_thread),
        #[cfg(unix)]
        socket_path: config.socket_path,
        #[cfg(unix)]
        _socket: socket_file,
        _lock: lock,
    })
}

/// The [`StartError`] for a failed bind of the bus socket at `path`.
#[cfg(unix)]
fn socket_start_error(error: SocketError, path: &Path) -> StartError {
    let path = path.to_path_buf();
    match error {
        SocketError::InUse => StartError::SocketInUse { path },
        SocketError::NotASocket => StartError::Socket {
            path,
            source: io::Error::new(
                io::ErrorKind::AlreadyExists,
                "the path exists and is not a socket",
            ),
        },
        SocketError::Io(source) => StartError::Socket { path, source },
    }
}

/// Hands the bound std listeners to the tokio runtime (inside it).
fn register(
    tcp: TcpListener,
    #[cfg(unix)] unix: std::os::unix::net::UnixListener,
) -> io::Result<Listeners> {
    Ok(Listeners {
        tcp: tokio::net::TcpListener::from_std(tcp)?,
        #[cfg(unix)]
        unix: tokio::net::UnixListener::from_std(unix)?,
    })
}

impl DaemonHandle {
    /// The bound loopback HTTP address (useful when the port was `0`).
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The bound Local Ingestion Bus Unix socket (macOS/Linux only).
    #[cfg(unix)]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// How many Git worktrees the commit observer has a baseline for; a
    /// commit made in a worktree before then is never observed. Zero when
    /// the observer is disabled.
    pub fn commit_baselined_worktrees(&self) -> usize {
        self.commits
            .as_ref()
            .map_or(0, CommitObserver::baselined_worktrees)
    }

    /// Stops accepting requests, commits every accepted Traction Event and
    /// waits for all daemon threads to finish.
    pub fn shutdown(mut self) -> Result<(), ShutdownError> {
        self.stop()
    }

    fn stop(&mut self) -> Result<(), ShutdownError> {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        let server = self.server.take().map(JoinHandle::join);
        // Every batch the tailer handed over is already queued to the Router.
        let tailer = self.tailer.take().map(TailerThread::stop);
        // Held mutations are decided and handed to the Router while it (and
        // the writer) still run.
        let watcher = self.watcher.take().map(WatcherThread::stop);
        // Drops the sensor's writer sender before the writer drains.
        if let Some(Err(_)) = self.presence.take().map(PresenceSensor::stop) {
            log::warn("the presence sensor thread panicked");
        }
        if let Some(Err(_)) = self.finalizer.take().map(FinalizerThread::stop) {
            log::warn("the finalizer thread panicked");
        }
        if let Some(Err(_)) = self.commits.take().map(CommitObserver::stop) {
            log::warn("the commit observer thread panicked");
        }
        // The bus is gone, so the Router drains its queue and exits. A writer
        // stuck retrying a full failed batch is told to give up first, so a
        // Router blocked on it is released.
        if let Some(writer) = &self.writer {
            writer.request_stop();
        }
        let router = self.router.take().map(RouterThread::join);
        let writer = self.writer.take().map(WriterThread::join);
        if matches!(server, Some(Err(_))) {
            return Err(ShutdownError::ServerPanicked);
        }
        if matches!(tailer, Some(Err(_))) {
            return Err(ShutdownError::TailerPanicked);
        }
        if matches!(watcher, Some(Err(_))) {
            return Err(ShutdownError::WatcherPanicked);
        }
        if matches!(router, Some(Err(_))) {
            return Err(ShutdownError::RouterPanicked);
        }
        if matches!(writer, Some(Err(_))) {
            return Err(ShutdownError::WriterPanicked);
        }
        Ok(())
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

fn create_data_dir(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

#[derive(Debug)]
pub enum StartError {
    DataDir(io::Error),
    /// Another daemon already holds the single-instance lock on this data
    /// folder. Distinct so the binary can exit with a one-line message.
    AlreadyRunning,
    /// The single-instance lock could not be created or checked.
    Lock(io::Error),
    Token(io::Error),
    /// `device.secret` could not be read or created, or is malformed (it is
    /// never replaced, since that would change every `dir:` Project ID).
    DeviceSecret(io::Error),
    /// `signing.key` could not be read or created, or is malformed (it is
    /// never replaced: a new key would silently fork the chain's trust).
    SigningKey(io::Error),
    Database(rusqlite::Error),
    /// The configured port is taken. The daemon never falls back to another.
    PortInUse {
        port: u16,
    },
    Bind {
        port: u16,
        source: io::Error,
    },
    /// A live daemon already serves the configured socket path.
    #[cfg(unix)]
    SocketInUse {
        path: PathBuf,
    },
    /// The bus socket could not be created (or the path is not a socket).
    #[cfg(unix)]
    Socket {
        path: PathBuf,
        source: io::Error,
    },
    Runtime(io::Error),
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DataDir(e) => write!(f, "cannot create the data folder: {e}"),
            Self::AlreadyRunning => {
                f.write_str("another Traction daemon is already running for this data folder")
            }
            Self::Lock(e) => write!(f, "cannot take the single-instance lock: {e}"),
            Self::Token(e) => write!(f, "cannot read or create daemon.token: {e}"),
            Self::DeviceSecret(e) => write!(f, "cannot read or create device.secret: {e}"),
            Self::SigningKey(e) => write!(f, "cannot read or create signing.key: {e}"),
            Self::Database(e) => write!(f, "cannot open the database: {e}"),
            Self::PortInUse { port } => write!(
                f,
                "port {port} on 127.0.0.1 is already in use by another program; \
                 free it or configure another port (no automatic fallback)"
            ),
            Self::Bind { port, source } => {
                write!(f, "cannot listen on 127.0.0.1:{port}: {source}")
            }
            #[cfg(unix)]
            Self::SocketInUse { path } => write!(
                f,
                "another Traction daemon is already serving the socket {}",
                path.display()
            ),
            #[cfg(unix)]
            Self::Socket { path, source } => {
                write!(f, "cannot create the socket {}: {source}", path.display())
            }
            Self::Runtime(e) => write!(f, "cannot start the daemon runtime: {e}"),
        }
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::DataDir(e)
            | Self::Lock(e)
            | Self::Token(e)
            | Self::DeviceSecret(e)
            | Self::SigningKey(e)
            | Self::Runtime(e) => Some(e),
            Self::AlreadyRunning | Self::PortInUse { .. } => None,
            Self::Bind { source, .. } => Some(source),
            #[cfg(unix)]
            Self::SocketInUse { .. } => None,
            #[cfg(unix)]
            Self::Socket { source, .. } => Some(source),
            Self::Database(e) => Some(e),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownError {
    ServerPanicked,
    TailerPanicked,
    WatcherPanicked,
    RouterPanicked,
    WriterPanicked,
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServerPanicked => f.write_str("the bus thread panicked"),
            Self::TailerPanicked => f.write_str("the Claude transcript tailer thread panicked"),
            Self::WatcherPanicked => f.write_str("the file watcher thread panicked"),
            Self::RouterPanicked => f.write_str("the Project Router thread panicked"),
            Self::WriterPanicked => f.write_str("the SQLite writer thread panicked"),
        }
    }
}

impl std::error::Error for ShutdownError {}
