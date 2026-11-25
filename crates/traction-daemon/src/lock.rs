//! Single-instance lock: at most one daemon per user and data folder.
//!
//! - Unix: an exclusive advisory `flock` on `<data dir>/daemon.lock`.
//! - Windows: a named mutex in the session namespace whose name includes the
//!   user name and a hash of the data folder, so test instances never collide
//!   with a real daemon.
//!
//! Either way the OS releases the lock when the process dies (even on a
//! crash), so a restart never needs manual cleanup. The lock file itself is
//! never deleted: removing it would let two daemons lock different inodes.
//!
//! [`InstanceLock`] can only be obtained through [`InstanceLock::acquire`].
//! Code that must only run while this daemon owns the data folder (e.g.
//! replacing a stale Unix socket) takes `&InstanceLock` as proof.

use std::io;
use std::path::Path;

/// The lock file in the data folder (Unix only; Windows uses a named mutex).
pub const LOCK_FILE: &str = "daemon.lock";

/// Held for the daemon's lifetime; dropping it releases the lock.
#[must_use = "the lock is released when this is dropped"]
pub struct InstanceLock {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(windows)]
    _mutex: windows::Mutex,
}

#[derive(Debug)]
pub enum LockError {
    /// Another live daemon holds the lock on this data folder.
    AlreadyRunning,
    /// The lock could not be created or checked.
    Io(io::Error),
}

impl InstanceLock {
    /// Takes the lock for `data_dir`, which must already exist.
    pub fn acquire(data_dir: &Path) -> Result<Self, LockError> {
        #[cfg(unix)]
        {
            unix_acquire(data_dir)
        }
        #[cfg(windows)]
        {
            windows::Mutex::create(data_dir).map(|mutex| Self { _mutex: mutex })
        }
    }
}

#[cfg(unix)]
fn unix_acquire(data_dir: &Path) -> Result<InstanceLock, LockError> {
    use std::fs::{OpenOptions, TryLockError};
    use std::os::unix::fs::OpenOptionsExt;

    // Never truncate: the file has no content, only an flock on its inode.
    // std opens with O_CLOEXEC, so child processes never inherit the lock.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(data_dir.join(LOCK_FILE))
        .map_err(LockError::Io)?;
    match file.try_lock() {
        Ok(()) => Ok(InstanceLock { _file: file }),
        Err(TryLockError::WouldBlock) => Err(LockError::AlreadyRunning),
        Err(TryLockError::Error(error)) => Err(LockError::Io(error)),
    }
}

#[cfg(windows)]
mod windows {
    use std::io;
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    use windows_sys::Win32::System::WindowsProgramming::GetUserNameW;

    use super::LockError;

    pub struct Mutex(HANDLE);

    // SAFETY: a mutex HANDLE is a process-wide kernel object reference; it
    // is only closed once, in `Drop`.
    unsafe impl Send for Mutex {}
    unsafe impl Sync for Mutex {}

    impl Mutex {
        pub fn create(data_dir: &Path) -> Result<Self, LockError> {
            let name = mutex_name(&user_name().map_err(LockError::Io)?, data_dir)
                .map_err(LockError::Io)?;
            let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
            // SAFETY: `wide` is NUL-terminated and outlives the call; default
            // security attributes; we do not request initial ownership because
            // existence of the named object is the lock.
            let handle = unsafe { CreateMutexW(std::ptr::null(), 0, wide.as_ptr()) };
            // Read before any other call can overwrite the thread's last error.
            let last_error = unsafe { GetLastError() };
            if handle.is_null() {
                return Err(LockError::Io(io::Error::from_raw_os_error(
                    last_error as i32,
                )));
            }
            if last_error == ERROR_ALREADY_EXISTS {
                // SAFETY: `handle` is a valid handle we own.
                unsafe { CloseHandle(handle) };
                return Err(LockError::AlreadyRunning);
            }
            Ok(Self(handle))
        }
    }

    impl Drop for Mutex {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a valid handle owned by this value.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// `Local\traction-daemon-<user>-<fnv1a64 of the data folder>`. `Local\`
    /// is the caller's session namespace, which other users' sessions cannot
    /// squat. Backslashes are not allowed after the namespace prefix.
    fn mutex_name(user: &str, data_dir: &Path) -> io::Result<String> {
        let canonical = std::fs::canonicalize(data_dir)?;
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        // Windows paths are case-insensitive: hash the lowercased path.
        let lowered = canonical.as_os_str().to_string_lossy().to_lowercase();
        for unit in std::ffi::OsStr::new(&lowered).encode_wide() {
            for byte in unit.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        }
        let user: String = user
            .chars()
            .map(|c| if c.is_alphanumeric() { c } else { '_' })
            .collect();
        Ok(format!("Local\\traction-daemon-{user}-{hash:016x}"))
    }

    fn user_name() -> io::Result<String> {
        // UNLEN (256) + 1.
        let mut buffer = [0u16; 257];
        let mut len = buffer.len() as u32;
        // SAFETY: `buffer` holds `len` u16s; `len` is updated on return.
        let ok = unsafe { GetUserNameW(buffer.as_mut_ptr(), &mut len) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        // `len` includes the terminating NUL.
        let len = (len as usize).saturating_sub(1).min(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..len]))
    }
}
