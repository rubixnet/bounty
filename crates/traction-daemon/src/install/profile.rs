//! The `TRACTION_TOKEN` block in the user's shell profiles.
//!
//! The block sits between [`BLOCK_START`] and [`BLOCK_END`] and, at shell
//! start, exports `TRACTION_TOKEN` by reading the daemon's token file, so the
//! token value itself is never copied into a profile. Each shell gets its own
//! syntax, chosen from the profile's file name: `*.ps1` is PowerShell,
//! `*.fish` is fish, anything else is POSIX `sh` (zsh, bash). Rerunning the
//! installer replaces the block in place instead of adding another.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::files::replace_file;

pub const BLOCK_START: &str = "# >>> traction >>>";
pub const BLOCK_END: &str = "# <<< traction <<<";
const BLOCK_NOTE: &str = "# Added by `traction-daemon install claude-code`: reads the Traction \
     bus token at shell start (the token itself is never stored here).";

/// The shell syntax a profile is written in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Syntax {
    /// zsh, bash and other POSIX shells.
    Posix,
    Fish,
    PowerShell,
}

impl Syntax {
    /// The syntax of the profile at `path`, from its file name.
    pub fn of(path: &Path) -> Self {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("ps1") => Self::PowerShell,
            Some(ext) if ext.eq_ignore_ascii_case("fish") => Self::Fish,
            _ => Self::Posix,
        }
    }
}

/// The user's existing zsh, bash, fish and PowerShell profiles (missing
/// ones are skipped, never created).
pub fn default_profiles(home: &Path) -> Vec<PathBuf> {
    let env_dir = |name: &str| {
        std::env::var_os(name)
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from)
            .filter(|dir| dir.is_absolute())
    };
    let zdotdir = env_dir("ZDOTDIR").unwrap_or_else(|| home.to_path_buf());
    let config_home = env_dir("XDG_CONFIG_HOME").unwrap_or_else(|| home.join(".config"));
    let mut candidates = vec![
        zdotdir.join(".zshrc"),
        home.join(".bashrc"),
        home.join(".bash_profile"),
        config_home.join("fish").join("config.fish"),
    ];
    const PS_PROFILE: &str = "Microsoft.PowerShell_profile.ps1";
    if cfg!(windows) {
        // PowerShell 7+ and Windows PowerShell 5.1. (A Documents folder
        // redirected elsewhere is covered by the per-user env var.)
        let documents = home.join("Documents");
        candidates.push(documents.join("PowerShell").join(PS_PROFILE));
        candidates.push(documents.join("WindowsPowerShell").join(PS_PROFILE));
    } else {
        candidates.push(config_home.join("powershell").join(PS_PROFILE));
    }
    candidates.retain(|path| path.is_file());
    candidates
}

/// Why a profile was not written.
#[derive(Debug)]
pub enum ProfileError {
    /// A start marker without its end marker: the profile is left untouched
    /// rather than guessing where the block ends.
    UnclosedBlock,
    Io(io::Error),
}

/// Adds (or replaces) the block reading `token_file` in the profile at
/// `path`, creating the profile if missing. Returns whether it changed.
pub fn install(path: &Path, token_file: &Path) -> Result<bool, ProfileError> {
    let original = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(ProfileError::Io(e)),
    };
    let updated = with_block(&original, &block(Syntax::of(path), token_file))?;
    if updated == original {
        return Ok(false);
    }
    replace_file(path, updated.as_bytes()).map_err(ProfileError::Io)?;
    Ok(true)
}

/// The block for `shell`, one line per entry, without line endings.
pub fn block(shell: Syntax, token_file: &Path) -> Vec<String> {
    let path = token_file.to_string_lossy();
    let body = match shell {
        Syntax::Posix => {
            let path = posix_quote(&path);
            vec![
                format!("if [ -r {path} ]; then"),
                format!("  TRACTION_TOKEN=\"$(cat {path})\""),
                "  export TRACTION_TOKEN".to_owned(),
                "fi".to_owned(),
            ]
        }
        Syntax::Fish => {
            let path = fish_quote(&path);
            vec![
                format!("if test -r {path}"),
                format!("    set -gx TRACTION_TOKEN (cat {path})"),
                "end".to_owned(),
            ]
        }
        Syntax::PowerShell => {
            let path = powershell_quote(&path);
            vec![
                format!("if (Test-Path -LiteralPath {path} -PathType Leaf) {{"),
                format!("    $env:TRACTION_TOKEN = [System.IO.File]::ReadAllText({path}).Trim()"),
                "}".to_owned(),
            ]
        }
    };
    let mut lines = vec![BLOCK_START.to_owned(), BLOCK_NOTE.to_owned()];
    lines.extend(body);
    lines.push(BLOCK_END.to_owned());
    lines
}

/// `text` with its first Traction block replaced by `block` (and any later
/// copies removed), or `block` appended when there is none. The file's line
/// ending (`\r\n` or `\n`) is kept.
fn with_block(text: &str, block: &[String]) -> Result<String, ProfileError> {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
    let mut insert_at = None;
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim() == BLOCK_START {
            let end = lines[index..]
                .iter()
                .position(|line| line.trim() == BLOCK_END)
                .ok_or(ProfileError::UnclosedBlock)?;
            insert_at.get_or_insert(kept.len());
            index += end + 1;
        } else {
            kept.push(lines[index]);
            index += 1;
        }
    }

    let block: Vec<&str> = block.iter().map(String::as_str).collect();
    let mut out: Vec<&str> = Vec::with_capacity(kept.len() + block.len() + 1);
    match insert_at {
        Some(at) => {
            out.extend(&kept[..at]);
            out.extend(&block);
            out.extend(&kept[at..]);
        }
        None => {
            out.extend(&kept);
            if kept.last().is_some_and(|line| !line.trim().is_empty()) {
                out.push("");
            }
            out.extend(&block);
        }
    }
    let mut result = out.join(newline);
    result.push_str(newline);
    Ok(result)
}

/// A single-quoted POSIX word: `'` becomes `'\''`.
fn posix_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// A single-quoted fish word: `\` and `'` are backslash-escaped.
fn fish_quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', r"\\").replace('\'', r"\'"))
}

/// A single-quoted PowerShell string: `'` is doubled.
fn powershell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_quotes_and_backslashes() {
        assert_eq!(posix_quote("/a b/it's"), r"'/a b/it'\''s'");
        assert_eq!(fish_quote(r"C:\it's"), r"'C:\\it\'s'");
        assert_eq!(powershell_quote(r"C:\it's"), r"'C:\it''s'");
    }

    #[test]
    fn the_shell_comes_from_the_file_name() {
        assert_eq!(Syntax::of(Path::new("/h/.zshrc")), Syntax::Posix);
        assert_eq!(Syntax::of(Path::new("/h/.bashrc")), Syntax::Posix);
        assert_eq!(Syntax::of(Path::new("/h/config.fish")), Syntax::Fish);
        assert_eq!(
            Syntax::of(Path::new("/h/Microsoft.PowerShell_profile.PS1")),
            Syntax::PowerShell
        );
    }

    #[test]
    fn an_unclosed_block_is_refused() {
        let text = format!("alias x=y\n{BLOCK_START}\nexport A=1\n");
        assert!(matches!(
            with_block(&text, &[]),
            Err(ProfileError::UnclosedBlock)
        ));
    }

    #[test]
    fn crlf_profiles_keep_crlf() {
        let block = block(Syntax::PowerShell, Path::new(r"C:\t\daemon.token"));
        let out = with_block("Set-Alias g git\r\n", &block).unwrap();
        assert!(out.starts_with("Set-Alias g git\r\n\r\n# >>> traction >>>\r\n"));
        assert!(!out.replace("\r\n", "").contains('\n'));
        assert_eq!(with_block(&out, &block).unwrap(), out);
    }
}
