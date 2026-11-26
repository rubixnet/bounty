//! Small file-system helpers: the one temp-file-and-rename write shared by
//! the data-folder secrets and the installer's edits of user files.

use std::fs::{self, OpenOptions, Permissions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Writes `contents` to `<dir>/<name>` through a fresh temp file (mode `0600`
/// from creation on Unix; Windows relies on the user-profile ACL), synced and
/// then renamed over the target, so the file is never observed half-written
/// or with wider permissions.
pub fn write_atomically(dir: &Path, name: &str, contents: &[u8]) -> io::Result<()> {
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut options = OpenOptions::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    write_through_temp(&dir.join(name), contents, &options, None, &create_new)
}

/// [`write_atomically`] for a secret that must be readable by the owner and
/// nobody else: mode `0600` on Unix, and on Windows a protected DACL holding
/// one full-control ACE for the current user (no inherited ACEs), set when the
/// temp file is created so it is never briefly open to anyone else.
pub fn write_owner_only(dir: &Path, name: &str, contents: &[u8]) -> io::Result<()> {
    #[cfg(windows)]
    {
        write_through_temp(
            &dir.join(name),
            contents,
            &OpenOptions::new(),
            None,
            &|_, path| crate::owner_only::create_new(path),
        )
    }
    #[cfg(not(windows))]
    {
        write_atomically(dir, name, contents)
    }
}

/// A fresh file at `path`, failing if it exists.
fn create_new(options: &OpenOptions, path: &Path) -> io::Result<fs::File> {
    options.clone().write(true).create_new(true).open(path)
}

/// Writes `contents` to a user's file at `path` (a settings file, a command
/// file, a shell profile) through a temp file and a rename in the same
/// folder, so the file is never seen half-written. A symlinked file (as
/// dotfile managers make) is replaced at its target, so the link survives,
/// and an existing file keeps its permissions; a new one gets the default
/// mode. Missing folders are created.
pub fn replace_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let target = resolve(path)?;
    if let Some(dir) = target.parent().filter(|dir| !dir.as_os_str().is_empty()) {
        fs::create_dir_all(dir)?;
    }
    let permissions = fs::metadata(&target).ok().map(|meta| meta.permissions());
    write_through_temp(
        &target,
        contents,
        &OpenOptions::new(),
        permissions,
        &create_new,
    )
}

/// The shared write: a fresh temp file next to `path`, opened with
/// `options` (plus write and create-new, unless `create` does it differently), given `permissions` if any,
/// synced, then renamed over `path`. The temp file is removed on failure.
fn write_through_temp(
    path: &Path,
    contents: &[u8],
    options: &OpenOptions,
    permissions: Option<Permissions>,
    create: &dyn Fn(&OpenOptions, &Path) -> io::Result<fs::File>,
) -> io::Result<()> {
    let name = path
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path has no file name"))?;
    let mut temp_name = name.to_os_string();
    temp_name.push(format!(".traction-tmp-{}", std::process::id()));
    let temp = path.with_file_name(temp_name);
    let _ = fs::remove_file(&temp);
    let result = (|| {
        let mut file = create(options, &temp)?;
        file.write_all(contents)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// `path`, or the file it links to (followed through every link).
fn resolve(path: &Path) -> io::Result<PathBuf> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => fs::canonicalize(path),
        Ok(_) => Ok(path.to_path_buf()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(e) => Err(e),
    }
}
