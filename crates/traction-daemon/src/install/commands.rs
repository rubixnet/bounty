//! The `/stop-track` and `/resume-track` command files in
//! `<claude config dir>/commands/`.
//!
//! Each file is YAML frontmatter (`description`, `disable-model-invocation:
//! true` so the model can never run it, and the Traction marker comment)
//! followed by exactly the shared one-line body from
//! [`crate::claude_code::pause`], which names the Traction action and gives
//! the model no instructions. The marker sits in the frontmatter, outside the
//! body, so a Claude Code version that hands the hook the expanded command
//! still sends text the hook matches.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::fsutil::replace_file;
use crate::claude_code::pause::Command;

/// The frontmatter comment that marks a command file as Traction's.
pub const COMMAND_MARKER: &str =
    "# traction: managed by `traction-daemon install claude-code`; rerunning it rewrites this file";

/// What writing the command files did.
#[derive(Debug, Default)]
pub struct CommandsOutcome {
    /// Traction's files, written or already current.
    pub installed: Vec<PathBuf>,
    /// Files of the same name the user wrote, left untouched.
    pub foreign: Vec<PathBuf>,
}

/// Writes both command files into `dir`. A file is (re)written only when it
/// is missing or carries [`COMMAND_MARKER`].
pub fn install(dir: &Path) -> io::Result<CommandsOutcome> {
    let mut outcome = CommandsOutcome::default();
    for command in Command::ALL {
        let path = dir.join(command.file_name);
        let contents = file_contents(command);
        match fs::read_to_string(&path) {
            Ok(existing) if existing == contents => {}
            Ok(existing) if !is_traction_file(&existing) => {
                outcome.foreign.push(path);
                continue;
            }
            Ok(_) => replace_file(&path, contents.as_bytes())?,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                replace_file(&path, contents.as_bytes())?
            }
            // Not UTF-8: not a file Traction wrote.
            Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                outcome.foreign.push(path);
                continue;
            }
            Err(e) => return Err(e),
        }
        outcome.installed.push(path);
    }
    Ok(outcome)
}

/// The full text of `command`'s file.
pub fn file_contents(command: Command) -> String {
    format!(
        "---\ndescription: {}\ndisable-model-invocation: true\n{COMMAND_MARKER}\n---\n{}\n",
        command.description, command.body
    )
}

fn is_traction_file(text: &str) -> bool {
    text.lines().any(|line| line.trim_end() == COMMAND_MARKER)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_code::pause::STOP_TRACK;

    #[test]
    fn the_marker_is_outside_the_body_and_recognized() {
        let text = file_contents(STOP_TRACK);
        assert!(is_traction_file(&text));
        assert!(text.ends_with(&format!("---\n{}\n", STOP_TRACK.body)));
        assert!(!is_traction_file("---\ndescription: mine\n---\nhi\n"));
    }
}
