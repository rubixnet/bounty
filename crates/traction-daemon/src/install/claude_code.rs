//! `traction-daemon install claude-code`: wires Claude Code into Traction.
//!
//! Steps, in order (each one a function [`ClaudeCodeInstall::run`] calls):
//! 1. the `claude --version` check (warns, never stops the install);
//! 2. the `settings.json` hook merge (stops the install on a file it cannot
//!    parse, leaving it untouched);
//! 3. the `/stop-track` and `/resume-track` command files in the `commands`
//!    folder next to `settings.json` (a user-written file of the same name is
//!    left untouched, with a warning);
//! 4. the `TRACTION_TOKEN` block in each shell profile (a profile that cannot
//!    be written is a warning, so the others still get theirs);
//! 5. on Windows, `TRACTION_TOKEN` under `HKCU\Environment` (a failure is a
//!    warning: the profiles still cover new terminals).

use std::fmt;
use std::path::PathBuf;

use super::commands;
use super::profile::{self, ProfileError};
use super::settings::{self, SettingsError};
use super::version;
use crate::claude_code::config_dir::claude_config_dir;
use crate::claude_code::hook::Hook;
use crate::config::{DEFAULT_HTTP_PORT, DaemonConfig};
use crate::token::TOKEN_FILE;

/// The Claude Code installer's inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeCodeInstall {
    /// The `settings.json` to merge hooks into.
    pub settings: PathBuf,
    /// The program run as `<claude_program> --version`.
    pub claude_program: PathBuf,
    /// The daemon's loopback HTTP port the hooks call.
    pub port: u16,
    /// The shell profiles that get the `TRACTION_TOKEN` block.
    pub profiles: Vec<PathBuf>,
    /// The daemon's token file the profile block reads at shell start.
    pub token_file: PathBuf,
    /// The `HKCU` subkey `TRACTION_TOKEN` is set under: `Environment`, or a
    /// scratch key in tests.
    #[cfg(windows)]
    pub windows_env_key: String,
}

impl ClaudeCodeInstall {
    /// The defaults: `<claude config dir>/settings.json`, `claude` on
    /// `PATH`, port 41937, the user's existing zsh, bash, fish and
    /// PowerShell profiles, and `~/.traction/daemon.token`.
    pub fn for_user() -> Result<Self, InstallError> {
        let config_dir = claude_config_dir().ok_or(InstallError::NoConfigDir)?;
        let data_dir = DaemonConfig::default_for_user()
            .map_err(|_| InstallError::NoHomeDir)?
            .data_dir;
        let home = data_dir.parent().ok_or(InstallError::NoHomeDir)?;
        Ok(Self {
            settings: config_dir.join("settings.json"),
            claude_program: PathBuf::from("claude"),
            port: DEFAULT_HTTP_PORT,
            profiles: profile::default_profiles(home),
            token_file: data_dir.join(TOKEN_FILE),
            #[cfg(windows)]
            windows_env_key: super::user_env::ENVIRONMENT_KEY.to_owned(),
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
            let command = path.file_stem().unwrap_or_default().to_string_lossy();
            warnings.push(format!(
                "{} was not written by Traction and was left untouched; \
                 /{command} will rely on the literal-text match only",
                path.display()
            ));
        }

        let mut profiles = Vec::new();
        for path in &self.profiles {
            match profile::install(path, &self.token_file) {
                Ok(_) => profiles.push(path.clone()),
                Err(ProfileError::UnclosedBlock) => warnings.push(format!(
                    "{} has a `{}` line without its `{}` line; it was left untouched",
                    path.display(),
                    profile::BLOCK_START,
                    profile::BLOCK_END
                )),
                Err(ProfileError::Io(error)) => {
                    warnings.push(format!("cannot update {}: {error}", path.display()))
                }
            }
        }

        #[cfg(windows)]
        let user_env = {
            let data_dir = self
                .token_file
                .parent()
                .unwrap_or(std::path::Path::new("."));
            match super::user_env::set_token(data_dir, &self.windows_env_key) {
                Ok(()) => Some(format!(
                    "HKCU\\{}\\{}",
                    self.windows_env_key,
                    super::user_env::TOKEN_VAR
                )),
                Err(error) => {
                    warnings.push(format!(
                        "cannot set TRACTION_TOKEN under HKCU\\{}: {error}",
                        self.windows_env_key
                    ));
                    None
                }
            }
        };
        #[cfg(not(windows))]
        let user_env = None;

        Ok(InstallReport {
            settings: self.settings.clone(),
            settings_changed: outcome.changed,
            backup: outcome.backup_written,
            hooks: Hook::ALL.iter().map(|hook| hook.name()).collect(),
            commands: commands.installed,
            profiles,
            user_env,
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
    /// The profiles holding the `TRACTION_TOKEN` block.
    pub profiles: Vec<PathBuf>,
    /// Where the per-user `TRACTION_TOKEN` was set (Windows only).
    pub user_env: Option<String>,
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
        for profile in &self.profiles {
            writeln!(f, "profile: {} (exports TRACTION_TOKEN)", profile.display())?;
        }
        if self.profiles.is_empty() {
            writeln!(
                f,
                "profiles: none found (pass --profile <path> so new terminals get TRACTION_TOKEN)"
            )?;
        }
        if let Some(user_env) = &self.user_env {
            writeln!(f, "user environment variable: {user_env}")?;
        }
        for warning in &self.warnings {
            writeln!(f, "warning: {warning}")?;
        }
        writeln!(
            f,
            "open a new terminal (and restart Claude Code) so hooks can authenticate"
        )
    }
}

/// Why the install stopped.
#[derive(Debug)]
pub enum InstallError {
    /// Neither `$CLAUDE_CONFIG_DIR` nor the home folder is known.
    NoConfigDir,
    /// The home folder (for `~/.traction` and the profiles) is unknown.
    NoHomeDir,
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
            Self::NoHomeDir => f.write_str("cannot find the home folder (set HOME)"),
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
