//! Library-first daemon lifecycle: [`start`] brings every part up and returns
//! a [`DaemonHandle`]; [`DaemonHandle::shutdown`] stops accepting, flushes
//! the writer and returns once everything accepted is committed.
//!
//! Threads: one runs a single-threaded tokio runtime serving the bus, one is
//! the Project Router, one is the SQLite writer. Startup order matters: data
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
use crate::config::{DaemonConfig, GLOBAL_IGNORE_FILE};
use crate::exclusions::Exclusions;
use crate::git::Git;
use crate::http::Listeners;
use crate::lock::{InstanceLock, LockError};
use crate::router::{self, Router, RouterThread};
use crate::secret::DeviceSecret;
#[cfg(unix)]
use crate::socket::{self, SocketError, SocketFile};
use crate::token::BusToken;
use crate::writer::{self, WriterThread};
use crate::{http, log, storage};

/// A running daemon. Dropping it performs the same graceful shutdown.
pub struct DaemonHandle {
    local_addr: SocketAddr,
    shutdown: Option<oneshot::Sender<()>>,
    server: Option<JoinHandle<()>>,
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
    let projects = storage::load_projects(&conn).map_err(StartError::Database)?;
    let aliases = storage::load_aliases(&conn).map_err(StartError::Database)?;
    let sessions = storage::load_sessions(&conn).map_err(StartError::Database)?;

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
        writer::spawn(conn, config.writer).map_err(StartError::Runtime)?;

    let exclusions = Exclusions::load(
        config.home_dir.as_deref(),
        &config.data_dir,
        &config.data_dir.join(GLOBAL_IGNORE_FILE),
    );
    let git = Git::with_program(&config.git_program, exclusions.home());
    let router = Router::new(
        git,
        exclusions,
        device_secret,
        writer_sender,
        projects,
        aliases,
        sessions,
    );
    let (router_sender, router_thread) = match router::spawn(router, config.writer.queue_capacity) {
        Ok(spawned) => spawned,
        Err(error) => {
            // The Router (and its writer sender) was dropped.
            let _ = writer_thread.join();
            return Err(StartError::Runtime(error));
        }
    };

    let bus = Arc::new(Bus::new(token, router_sender, local_addr.port()));
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
            // The closure (and its Router sender) was dropped; let the
            // Router, then the writer, exit.
            let _ = router_thread.join();
            let _ = writer_thread.join();
            return Err(StartError::Runtime(error));
        }
    };

    Ok(DaemonHandle {
        local_addr,
        shutdown: Some(shutdown_tx),
        server: Some(server),
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
    RouterPanicked,
    WriterPanicked,
}

impl fmt::Display for ShutdownError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ServerPanicked => f.write_str("the bus thread panicked"),
            Self::RouterPanicked => f.write_str("the Project Router thread panicked"),
            Self::WriterPanicked => f.write_str("the SQLite writer thread panicked"),
        }
    }
}

impl std::error::Error for ShutdownError {}
