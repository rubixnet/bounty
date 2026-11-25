//! `traction-daemon install claude-code`: wires Claude Code into Traction.
//!
//! Steps, in order (each one a function [`ClaudeCodeInstall::run`] calls):
//! 1. the `claude --version` check (warns, never stops the install);
//! 2. the `settings.json` hook merge (stops the install on a file it cannot
//!    parse, leaving it untouched);
//! 3. the `/stop-track` and `/resume-track` command files in the `commands`
//!    folder next to `settings.json` (a user-written file of the same name is
//!    left untouched, with a warning).

use std::fmt;
use std::path::PathBuf;

use super::commands;
use super::settings::{self, SettingsError};
use super::version;
use crate::claude_code::config_dir::claude_config_dir;
use crate::claude_code::hook::Hook;
use crate::config::DEFAULT_HTTP_PORT;

/// The Claude Code installer's inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeInstall {
    /// The `settings.json` to merge hooks into.
    pub settings: PathBuf,
    /// The program run as `<claude_program> --version`.
    pub claude_program: PathBuf,
    /// The daemon's loopback HTTP port the hooks call.
    pub port: u16,
}

impl ClaudeCodeInstall {
    /// The defaults: `<claude config dir>/settings.json`, `claude` on
    /// `PATH`, port 41937.
    pub fn for_user() -> Result<Self, InstallError> {
        let config_dir = claude_config_dir().ok_or(InstallError::NoConfigDir)?;
        Ok(Self {
            settings: config_dir.join("settings.json"),
            claude_program: PathBuf::from("claude"),
            port: DEFAULT_HTTP_PORT,
        })
    }

    /// Runs every install step.
    pub fn run(&self) -> Result<InstallReport, InstallError> {
        let version = version::check(&self.claude_program);
        let outcome = settings::install(&self.settings, self.port).map_err(|error| {
            InstallError::Settings {
                path: self.settings.clone(),
                error,
            }
        })?;
        let mut warnings: Vec<String> = version.warning().into_iter().collect();

        let commands_dir = self.commands_dir();
        let commands = commands::install(&commands_dir).map_err(|error| InstallError::Write {
            path: commands_dir.clone(),
            error,
        })?;
        for path in &commands.foreign {
            warnings.push(format!(
                "{} was not written by Traction and was left untouched; \
                 its command will rely on the hook's literal-text match only",
                path.display()
            ));
        }

        Ok(InstallReport {
            settings: self.settings.clone(),
            settings_changed: outcome.changed,
            backup: outcome.backup_written,
            hooks: Hook::ALL.iter().map(|hook| hook.name()).collect(),
            commands: commands.installed,
            warnings,
        })
    }

    /// The Claude `commands` folder: next to `settings.json`, which by
    /// default is `<claude config dir>/commands`.
    pub fn commands_dir(&self) -> PathBuf {
        match self.settings.parent() {
            Some(dir) if !dir.as_os_str().is_empty() => dir.join("commands"),
            _ => PathBuf::from("commands"),
        }
    }
}

/// What an install did, printed as the summary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    pub settings: PathBuf,
    /// False when `settings.json` already held exactly this hook set.
    pub settings_changed: bool,
    /// The one-time backup of the original, when this run wrote it.
    pub backup: Option<PathBuf>,
    /// The hook events Traction is installed under.
    pub hooks: Vec<&'static str>,
    /// Traction's command files (a user-written one is a warning instead).
    pub commands: Vec<PathBuf>,
    pub warnings: Vec<String>,
}

impl fmt::Display for InstallReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = if self.settings_changed {
            "updated"
        } else {
            "already up to date"
        };
        writeln!(f, "settings: {} ({state})", self.settings.display())?;
        if let Some(backup) = &self.backup {
            writeln!(f, "backup of the original: {}", backup.display())?;
        }
        writeln!(f, "hooks installed: {}", self.hooks.join(", "))?;
        for command in &self.commands {
            writeln!(f, "command: {}", command.display())?;
        }
        for warning in &self.warnings {
            writeln!(f, "warning: {warning}")?;
        }
        Ok(())
    }
}

/// Why the install stopped.
#[derive(Debug)]
pub enum InstallError {
    /// Neither `$CLAUDE_CONFIG_DIR` nor the home folder is known.
    NoConfigDir,
    /// `settings.json` was refused or could not be written; it is untouched.
    Settings { path: PathBuf, error: SettingsError },
    /// A command file or profile could not be written.
    Write {
        path: PathBuf,
        error: std::io::Error,
    },
}

impl fmt::Display for InstallError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoConfigDir => {
                f.write_str("cannot find the Claude config folder (set CLAUDE_CONFIG_DIR or HOME)")
            }
            Self::Settings { path, error } => write!(
                f,
                "{} {error}; it was left untouched, fix it and rerun the installer",
                path.display()
            ),
            Self::Write { path, error } => write!(f, "cannot write {}: {error}", path.display()),
        }
    }
}

impl std::error::Error for InstallError {}
