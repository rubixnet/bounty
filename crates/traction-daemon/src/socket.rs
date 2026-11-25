//! The Local Ingestion Bus Unix socket (macOS/Linux only).
//!
//! [`bind`] creates the socket with mode `0600` and no window in which
//! another user could connect: it binds inside a fresh private (`0700`)
//! folder next to the target, sets `0600`, then renames the socket onto the
//! configured path. `rename` is atomic, so it also replaces a stale socket
//! left by a dead daemon in one step.
//!
//! What may be replaced is narrow: only a socket that refuses connections
//! (its daemon is gone). A live socket (another daemon, e.g. one on another
//! data folder sharing `$XDG_RUNTIME_DIR`) and any non-socket file (or
//! symlink) are left untouched and fail startup. [`bind`] takes
//! `&InstanceLock` so it can only run while this daemon owns its data folder.

use std::fs::{self, DirBuilder, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use crate::lock::InstanceLock;

#[derive(Debug)]
pub enum SocketError {
    /// A daemon is accepting connections on the path already.
    InUse,
    /// The path exists but is not a socket (a file, folder or symlink).
    NotASocket,
    Io(io::Error),
}

impl From<io::Error> for SocketError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// The socket file this daemon created. Dropping it removes the file, but
/// only if the path still names the same inode (never another daemon's).
pub struct SocketFile {
    path: PathBuf,
    dev: u64,
    ino: u64,
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        if let Ok(meta) = fs::symlink_metadata(&self.path)
            && meta.dev() == self.dev
            && meta.ino() == self.ino
        {
            let _ = fs::remove_file(&self.path);
        }
    }
}

/// Binds the bus socket at `path` (mode `0600`), replacing a stale socket.
pub fn bind(path: &Path, _held: &InstanceLock) -> Result<(UnixListener, SocketFile), SocketError> {
    check_existing(path)?;
    let parent = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let private_dir = create_private_dir(parent)?;
    let staged = private_dir.join("s");
    let result = bind_staged(&staged, path);
    // The staged socket was renamed away on success; clean up either way.
    let _ = fs::remove_file(&staged);
    let _ = fs::remove_dir(&private_dir);
    let listener = result?;
    let meta = fs::symlink_metadata(path)?;
    Ok((
        listener,
        SocketFile {
            path: path.to_path_buf(),
            dev: meta.dev(),
            ino: meta.ino(),
        },
    ))
}

/// Allows a missing path or a dead socket; refuses everything else.
fn check_existing(path: &Path) -> Result<(), SocketError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !meta.file_type().is_socket() {
        return Err(SocketError::NotASocket);
    }
    match UnixStream::connect(path) {
        Ok(_) => Err(SocketError::InUse),
        // Nobody is listening: a stale socket from a dead daemon.
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error.into()),
    }
}

/// A new `0700` folder in `parent` with a short random name (`sun_path` is
/// about 104 bytes on macOS, so the staged path must stay short too).
fn create_private_dir(parent: &Path) -> io::Result<PathBuf> {
    let mut random = [0u8; 4];
    getrandom::fill(&mut random).map_err(io::Error::other)?;
    let name: String = random.iter().map(|b| format!("{b:02x}")).collect();
    let dir = parent.join(format!(".tr{name}"));
    // Not recursive: fails if the name exists, so the folder is ours.
    DirBuilder::new().mode(0o700).create(&dir)?;
    Ok(dir)
}

fn bind_staged(staged: &Path, path: &Path) -> io::Result<UnixListener> {
    let listener = UnixListener::bind(staged)?;
    fs::set_permissions(staged, Permissions::from_mode(0o600))?;
    fs::rename(staged, path)?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}
