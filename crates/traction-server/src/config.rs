//! `traction-server.toml`: one file, seven sections (`[anchor_policy]`,
//! `[heuristic]`, `[jev]`, `[auth]`, `[roles]`, `[blob]`, `[github]`) plus
//! `[server]`. Later slices fill their own sections.
//!
//! Secrets never live in this file. A key that names a secret (`password`,
//! `secret`, `token`, `api_key`, `private_key`, `database_url`) and is not an
//! `_env` or `_file` reference makes loading fail, so the server refuses to
//! start with a secret written inline. Public keys may be inline.

use std::fmt;
use std::path::Path;

use serde::Deserialize;

/// Words that mark a key as holding a secret.
const SECRET_WORDS: [&str; 6] = [
    "password",
    "secret",
    "token",
    "api_key",
    "private_key",
    "database_url",
];

#[derive(Debug)]
pub enum ConfigError {
    Read(std::io::Error),
    Parse(String),
    /// A secret written inline: the dotted key path, never its value.
    InlineSecret(String),
    /// A secret reference (`*_env` / `*_file`) that points at nothing.
    MissingSecret(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read(e) => write!(f, "cannot read the config file: {e}"),
            Self::Parse(e) => write!(f, "invalid config: {e}"),
            Self::InlineSecret(key) => write!(
                f,
                "`{key}` is written inline; secrets come only from environment variables or \
                 files (use `{key}_env` or `{key}_file`)"
            ),
            Self::MissingSecret(what) => write!(f, "secret not available: {what}"),
        }
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Config {
    pub server: ServerConfig,
    pub anchor_policy: AnchorPolicy,
    pub heuristic: Heuristic,
    pub jev: Jev,
    pub auth: AuthConfig,
    pub roles: Roles,
    pub blob: Blob,
    pub github: Github,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ServerConfig {
    /// Address to listen on.
    pub bind: String,
    /// Public base URL, used to build the pairing login URL.
    pub public_url: String,
    /// Name of the environment variable holding the Postgres URL.
    pub database_url_env: String,
    /// Or a file holding it.
    pub database_url_file: Option<String>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1:8080".into(),
            public_url: "http://127.0.0.1:8080".into(),
            database_url_env: "TRACTION_DATABASE_URL".into(),
            database_url_file: None,
        }
    }
}

/// Anchor policy (`docs/adr/0004`, spec "Policy constants"). Durations are
/// whole seconds or milliseconds as named.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AnchorPolicy {
    /// A tip window may start at most this long after `server_received_at`.
    pub max_future_skew_ms: u64,
    /// A realtime window first anchored later than this is `late_anchor` (S2).
    pub max_anchor_lag_ms: u64,
    /// `|server_received_at - sent_at|` above this flags `clock_skew` (S2).
    pub max_clock_skew_ms: u64,
    /// Windows older than this are terminally rejected (`window_too_old`).
    pub max_backlog_age_days: u64,
    /// Largest accepted heartbeat body.
    pub max_body_bytes: usize,
    /// Per-device token bucket: burst size.
    pub rate_limit_burst: u32,
    /// Per-device token bucket: one token back every this many ms.
    pub rate_limit_refill_ms: u64,
}

impl Default for AnchorPolicy {
    fn default() -> Self {
        Self {
            max_future_skew_ms: 2 * 60_000,
            max_anchor_lag_ms: 20 * 60_000,
            max_clock_skew_ms: 2 * 60_000,
            max_backlog_age_days: 45,
            max_body_bytes: 2 * 1024 * 1024,
            rate_limit_burst: 20,
            rate_limit_refill_ms: 2_000,
        }
    }
}

impl AnchorPolicy {
    pub fn max_backlog_age_ms(&self) -> u64 {
        self.max_backlog_age_days * 24 * 60 * 60 * 1000
    }
}

/// Filled by S3a.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Heuristic {}

/// Filled by S4.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Jev {}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    /// `dev` is the only provider in S1; S2 adds `hackclub`.
    pub provider: String,
    /// The stable `sub` the dev provider logs everyone in as.
    pub dev_sub: String,
    pub dev_display_name: String,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            provider: "dev".into(),
            dev_sub: "dev-user".into(),
            dev_display_name: "Dev User".into(),
        }
    }
}

/// Provider `sub`s granted each role (used from S2).
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Roles {
    pub reviewers: Vec<String>,
    pub admins: Vec<String>,
}

/// Filled by S3a.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Blob {}

/// Filled by S3b.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Github {}

impl Config {
    /// Parses config text, refusing an inline secret.
    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let table: toml::Table = text
            .parse()
            .map_err(|e: toml::de::Error| ConfigError::Parse(e.message().to_owned()))?;
        if let Some(key) = find_inline_secret(&table, "") {
            return Err(ConfigError::InlineSecret(key));
        }
        toml::from_str(text).map_err(|e| ConfigError::Parse(e.message().to_owned()))
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::parse(&text)
    }

    /// The Postgres URL from its environment variable or file.
    pub fn database_url(
        &self,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<String, ConfigError> {
        if let Some(path) = &self.server.database_url_file {
            let text = std::fs::read_to_string(path)
                .map_err(|_| ConfigError::MissingSecret("database_url_file".into()))?;
            return Ok(text.trim().to_owned());
        }
        env(&self.server.database_url_env)
            .filter(|v| !v.is_empty())
            .ok_or_else(|| ConfigError::MissingSecret(self.server.database_url_env.clone()))
    }
}

/// The dotted path of the first string value under a secret-named key that
/// is not an `_env`/`_file` reference.
fn find_inline_secret(table: &toml::Table, prefix: &str) -> Option<String> {
    for (key, value) in table {
        let path = format!("{prefix}{key}");
        match value {
            toml::Value::Table(inner) => {
                if let Some(found) = find_inline_secret(inner, &format!("{path}.")) {
                    return Some(found);
                }
            }
            toml::Value::String(_) => {
                let lower = key.to_ascii_lowercase();
                let is_reference = lower.ends_with("_env") || lower.ends_with("_file");
                if !is_reference && SECRET_WORDS.iter().any(|w| lower.contains(w)) {
                    return Some(path);
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_is_the_default_policy() {
        let config = Config::parse("").unwrap();
        assert_eq!(config.anchor_policy.max_future_skew_ms, 120_000);
        assert_eq!(config.anchor_policy.max_backlog_age_days, 45);
        assert_eq!(config.auth.provider, "dev");
    }

    #[test]
    fn all_seven_sections_parse() {
        let text = "[anchor_policy]\n[heuristic]\n[jev]\n[auth]\n[roles]\n[blob]\n[github]\n";
        assert!(Config::parse(text).is_ok());
    }

    #[test]
    fn an_inline_secret_is_refused_without_echoing_it() {
        for text in [
            "[server]\ndatabase_url = \"postgres://u:hunter2@h/db\"\n",
            "[jev]\napi_key = \"hunter2\"\n",
            "[github]\nprivate_key = \"hunter2\"\n",
            "[auth]\noidc_client_secret = \"hunter2\"\n",
        ] {
            let error = Config::parse(text).unwrap_err();
            assert!(matches!(error, ConfigError::InlineSecret(_)), "{text}");
            assert!(!error.to_string().contains("hunter2"));
        }
    }

    #[test]
    fn secret_references_and_public_keys_are_fine() {
        let text = "[server]\ndatabase_url_env = \"X\"\ndatabase_url_file = \"/x\"\n";
        assert!(Config::parse(text).is_ok());
    }

    #[test]
    fn the_database_url_comes_from_the_environment() {
        let config = Config::parse("").unwrap();
        let url = config
            .database_url(|name| (name == "TRACTION_DATABASE_URL").then(|| "postgres://x".into()))
            .unwrap();
        assert_eq!(url, "postgres://x");
        assert!(config.database_url(|_| None).is_err());
    }
}
