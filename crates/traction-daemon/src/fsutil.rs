//! Small file-system helpers shared by the data-folder secrets.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

/// Writes `contents` to `<dir>/<name>` through a fresh temp file (mode `0600`
/// from creation on Unix; Windows relies on the user-profile ACL), synced and
/// then renamed over the target, so the file is never observed half-written
/// or with wider permissions.
pub fn write_atomically(dir: &Path, name: &str, contents: &[u8]) -> io::Result<()> {
    let path = dir.join(name);
    let temp = dir.join(format!("{name}.tmp-{}", std::process::id()));
    let _ = fs::remove_file(&temp);
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        fs::rename(&temp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}
