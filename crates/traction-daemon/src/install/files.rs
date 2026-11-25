//! Replacing a user's file (a command file, a shell profile) in place.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// Writes `contents` to `path` through a temp file and a rename in the same
/// folder, so the file is never seen half-written. A symlinked file (as
/// dotfile managers make) is replaced at its target, so the link survives,
/// and an existing file keeps its permissions. Missing folders are created.
pub fn replace_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let target = resolve(path)?;
    let dir = match target.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "the path has no file name"))?;
    fs::create_dir_all(&dir)?;
    let permissions = fs::metadata(&target).ok().map(|meta| meta.permissions());

    let mut temp_name = name.to_os_string();
    temp_name.push(format!(".traction-tmp-{}", std::process::id()));
    let temp = dir.join(temp_name);
    let _ = fs::remove_file(&temp);
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(contents)?;
        if let Some(permissions) = permissions {
            file.set_permissions(permissions)?;
        }
        file.sync_all()?;
        drop(file);
        fs::rename(&temp, &target)
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
