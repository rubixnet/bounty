//! The GitHub App client (spec S3b, Q16). The App has read-only `metadata`
//! and `contents` permissions on the repos a user selects when installing it.
//!
//! - A user-to-server token proves **push access** before a link is made.
//! - **Installation tokens** (minted from an App JWT) fetch commits.
//! - Only `oid`, `authored_at`, `committed_at` and whether the author's login
//!   matches the user's linked GitHub login are kept. The wire structs below
//!   deliberately have no field for a message or a diff, so neither can be
//!   stored by construction.
//!
//! The client is blocking (`ureq`); handlers call it inside
//! `spawn_blocking`. The API base URL and the App private key (env or file
//! only) come from `[github]`.

use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::sha2::{Digest, Sha256};
use rsa::{Pkcs1v15Sign, PrivateKey};
use serde::Deserialize;
use serde_json::json;

use crate::config::{ConfigError, Github};

/// Why a GitHub call failed. Values never echo a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GithubError {
    /// `403`/`429` with an exhausted rate limit: retry after this many seconds.
    RateLimited { retry_after_sec: u64 },
    /// The token was refused (`401`).
    Unauthorized,
    /// `404`, or `403` without a rate limit (no access or no installation).
    NotFound,
    /// A transport failure or an unexpected status.
    Http(String),
    /// A reply that was not the documented shape.
    Malformed,
    /// `[github]` is not configured (no `app_id` or private key).
    NotConfigured,
}

impl GithubError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::RateLimited { .. } => "github_rate_limited",
            Self::Unauthorized => "github_unauthorized",
            Self::NotFound => "github_not_found",
            Self::Http(_) => "github_unavailable",
            Self::Malformed => "github_malformed",
            Self::NotConfigured => "github_not_configured",
        }
    }
}

/// The authenticated GitHub user behind a user-to-server token.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct GithubUser {
    pub id: i64,
    pub login: String,
}

/// A repo as the user's token sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoAccess {
    pub id: i64,
    pub full_name: String,
    pub push: bool,
}

/// One commit, reduced to what is stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRecord {
    pub oid: String,
    pub authored_at_ms: i64,
    pub committed_at_ms: i64,
    /// The commit's GitHub author login, if the author is a GitHub account.
    pub author_login: Option<String>,
}

#[derive(Deserialize)]
struct RepoWire {
    id: i64,
    full_name: String,
    #[serde(default)]
    permissions: Option<PermissionsWire>,
}

#[derive(Deserialize)]
struct PermissionsWire {
    #[serde(default)]
    push: bool,
}

#[derive(Deserialize)]
struct InstallationWire {
    id: i64,
}

#[derive(Deserialize)]
struct TokenWire {
    token: String,
}

// The commit list shape. There is intentionally no `message` field here:
// serde skips unknown fields, so a commit message is dropped at the parser.
#[derive(Deserialize)]
struct CommitWire {
    sha: String,
    commit: CommitInnerWire,
    author: Option<LoginWire>,
}

#[derive(Deserialize)]
struct CommitInnerWire {
    author: Option<SignatureWire>,
    committer: Option<SignatureWire>,
}

#[derive(Deserialize)]
struct SignatureWire {
    date: String,
}

#[derive(Deserialize)]
struct LoginWire {
    login: String,
}

/// Parses an RFC 3339 timestamp (`2012-03-06T23:06:50Z`, or with a numeric
/// offset) into Unix milliseconds.
pub fn parse_rfc3339_ms(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |from: usize, to: usize| -> Option<i64> {
        let part = text.get(from..to)?;
        if !part.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        part.parse().ok()
    };
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, minute, second) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        rest = &fraction[digits..];
    }
    let offset_sec = match rest {
        "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if rest.len() != 6 || rest.as_bytes()[3] != b':' {
                return None;
            }
            let h: i64 = rest.get(1..3)?.parse().ok()?;
            let m: i64 = rest.get(4..6)?.parse().ok()?;
            sign * (h * 3600 + m * 60)
        }
    };
    // Days from the civil date (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset_sec) * 1000)
}

/// Parses one page of the commit list. Entries with an unreadable date are
/// skipped rather than failing the page.
pub fn parse_commit_page(body: &[u8]) -> Result<Vec<CommitRecord>, GithubError> {
    let items: Vec<CommitWire> =
        serde_json::from_slice(body).map_err(|_| GithubError::Malformed)?;
    Ok(items
        .into_iter()
        .filter_map(|c| {
            let authored = parse_rfc3339_ms(&c.commit.author.as_ref()?.date)?;
            let committed = c
                .commit
                .committer
                .as_ref()
                .and_then(|s| parse_rfc3339_ms(&s.date))
                .unwrap_or(authored);
            Some(CommitRecord {
                oid: c.sha,
                authored_at_ms: authored,
                committed_at_ms: committed,
                author_login: c.author.map(|a| a.login),
            })
        })
        .collect())
}

/// The URL of the `rel="next"` page in a `Link` header, if any.
pub fn next_link(header: &str) -> Option<String> {
    header.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        let is_next = params
            .split(';')
            .any(|p| p.trim().replace(' ', "") == "rel=\"next\"");
        let url = url.trim().strip_prefix('<')?.strip_suffix('>')?;
        is_next.then(|| url.to_owned())
    })
}

/// Whether `owner/name` is a plausible GitHub repo name.
pub fn valid_repo_name(text: &str) -> bool {
    let ok = |part: &str| {
        !part.is_empty()
            && part.len() <= 100
            && part != "."
            && part != ".."
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    matches!(text.split_once('/'), Some((o, n)) if ok(o) && ok(n) && !n.contains('/'))
}

pub struct GithubClient {
    base: String,
    app_id: String,
    key: PrivateKey,
    per_page: u32,
    max_pages: u32,
    agent: ureq::Agent,
}

impl GithubClient {
    /// Builds the client from `[github]`. The private key comes only from
    /// the configured environment variable or file.
    pub fn from_config(
        config: &Github,
        env: impl Fn(&str) -> Option<String>,
    ) -> Result<Self, GithubError> {
        if config.app_id.is_empty() {
            return Err(GithubError::NotConfigured);
        }
        let pem = config.private_key(env).map_err(|e| match e {
            ConfigError::MissingSecret(_) => GithubError::NotConfigured,
            _ => GithubError::Malformed,
        })?;
        Self::new(config, &pem)
    }

    pub fn new(config: &Github, private_key_pem: &str) -> Result<Self, GithubError> {
        let key = PrivateKey::from_pkcs1_pem(private_key_pem)
            .or_else(|_| PrivateKey::from_pkcs8_pem(private_key_pem))
            .map_err(|_| GithubError::Malformed)?;
        let base = config.api_base_url.trim_end_matches('/').to_owned();
        if !traction_core::secure_http_url(&base) {
            return Err(GithubError::Malformed);
        }
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .into();
        Ok(Self {
            base,
            app_id: config.app_id.clone(),
            key,
            per_page: config.per_page.clamp(1, 100),
            max_pages: config.max_pages.max(1),
            agent,
        })
    }

    /// The RS256 App JWT: valid for 9 minutes, issued 60 s in the past to
    /// tolerate clock drift (GitHub allows 10 minutes at most).
    pub fn app_jwt(&self, now_sec: u64) -> Result<String, GithubError> {
        let b64 = |bytes: &[u8]| URL_SAFE_NO_PAD.encode(bytes);
        let header = b64(br#"{"alg":"RS256","typ":"JWT"}"#);
        let claims = json!({
            "iat": now_sec.saturating_sub(60),
            "exp": now_sec + 9 * 60,
            "iss": self.app_id,
        });
        let input = format!("{header}.{}", b64(claims.to_string().as_bytes()));
        let digest = Sha256::digest(input.as_bytes());
        let signature = self
            .key
            .sign(Pkcs1v15Sign::new::<Sha256>(), &digest)
            .map_err(|_| GithubError::Malformed)?;
        Ok(format!("{input}.{}", b64(&signature)))
    }

    /// One request; maps the documented failure statuses.
    fn call(
        &self,
        method: &'static str,
        url: &str,
        bearer: &str,
        now_sec: u64,
    ) -> Result<(Vec<u8>, Option<String>), GithubError> {
        let auth = format!("Bearer {bearer}");
        let mut response = if method == "GET" {
            self.agent
                .get(url)
                .header("authorization", &auth)
                .header("accept", "application/vnd.github+json")
                .header("x-github-api-version", "2022-11-28")
                .header("user-agent", "traction-server")
                .call()
        } else {
            self.agent
                .post(url)
                .header("authorization", &auth)
                .header("accept", "application/vnd.github+json")
                .header("x-github-api-version", "2022-11-28")
                .header("user-agent", "traction-server")
                .send_empty()
        }
        .map_err(|e| GithubError::Http(e.to_string()))?;
        let status = response.status().as_u16();
        let header = |name: &str| {
            response
                .headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        match status {
            200 | 201 => {
                let link = header("link");
                let body = response
                    .body_mut()
                    .with_config()
                    .limit(8 * 1024 * 1024)
                    .read_to_vec()
                    .map_err(|e| GithubError::Http(e.to_string()))?;
                Ok((body, link))
            }
            401 => Err(GithubError::Unauthorized),
            403 | 429 => {
                let remaining = header("x-ratelimit-remaining");
                let retry_after = header("retry-after").and_then(|v| v.parse::<u64>().ok());
                let reset = header("x-ratelimit-reset").and_then(|v| v.parse::<u64>().ok());
                let limited =
                    status == 429 || retry_after.is_some() || remaining.as_deref() == Some("0");
                if limited {
                    let wait = retry_after
                        .or_else(|| reset.map(|at| at.saturating_sub(now_sec)))
                        .unwrap_or(60);
                    Err(GithubError::RateLimited {
                        retry_after_sec: wait.clamp(1, 3600),
                    })
                } else {
                    Err(GithubError::NotFound)
                }
            }
            404 => Err(GithubError::NotFound),
            other => Err(GithubError::Http(format!("unexpected status {other}"))),
        }
    }

    /// `GET /user` with a user-to-server token.
    pub fn user(&self, user_token: &str, now_sec: u64) -> Result<GithubUser, GithubError> {
        let (body, _) = self.call("GET", &format!("{}/user", self.base), user_token, now_sec)?;
        serde_json::from_slice(&body).map_err(|_| GithubError::Malformed)
    }

    /// `GET /repos/{owner}/{name}` with the user's token: whether it has
    /// push access. A repo the token cannot see is `NotFound`.
    pub fn repo_access(
        &self,
        user_token: &str,
        full_name: &str,
        now_sec: u64,
    ) -> Result<RepoAccess, GithubError> {
        let url = format!("{}/repos/{full_name}", self.base);
        let (body, _) = self.call("GET", &url, user_token, now_sec)?;
        let repo: RepoWire = serde_json::from_slice(&body).map_err(|_| GithubError::Malformed)?;
        Ok(RepoAccess {
            id: repo.id,
            full_name: repo.full_name,
            push: repo.permissions.is_some_and(|p| p.push),
        })
    }

    /// `GET /repos/{owner}/{name}/installation` with the App JWT: the App's
    /// installation on that repo, if the user installed it there.
    pub fn installation_for_repo(&self, full_name: &str, now_sec: u64) -> Result<i64, GithubError> {
        let jwt = self.app_jwt(now_sec)?;
        let url = format!("{}/repos/{full_name}/installation", self.base);
        let (body, _) = self.call("GET", &url, &jwt, now_sec)?;
        serde_json::from_slice::<InstallationWire>(&body)
            .map(|i| i.id)
            .map_err(|_| GithubError::Malformed)
    }

    /// `POST /app/installations/{id}/access_tokens` with the App JWT.
    pub fn installation_token(
        &self,
        installation: i64,
        now_sec: u64,
    ) -> Result<String, GithubError> {
        let jwt = self.app_jwt(now_sec)?;
        let url = format!(
            "{}/app/installations/{installation}/access_tokens",
            self.base
        );
        let (body, _) = self.call("POST", &url, &jwt, now_sec)?;
        serde_json::from_slice::<TokenWire>(&body)
            .map(|t| t.token)
            .map_err(|_| GithubError::Malformed)
    }

    /// The repo's commits (newest first) with an installation token,
    /// following pagination up to `max_pages`. `since_ms` limits the fetch to
    /// newer commits. A rate limit mid-way is returned as an error: the
    /// caller stores nothing and retries (upserts are idempotent).
    pub fn fetch_commits(
        &self,
        installation_token: &str,
        full_name: &str,
        since_ms: Option<i64>,
        now_sec: u64,
    ) -> Result<Vec<CommitRecord>, GithubError> {
        let mut url = format!(
            "{}/repos/{full_name}/commits?per_page={}",
            self.base, self.per_page
        );
        if let Some(since) = since_ms {
            url.push_str(&format!("&since={}", iso_utc(since)));
        }
        let mut all = Vec::new();
        for _ in 0..self.max_pages {
            let (body, link) = self.call("GET", &url, installation_token, now_sec)?;
            all.extend(parse_commit_page(&body)?);
            match link.as_deref().and_then(next_link) {
                // Only ever follow a link on the configured API's origin
                // (scheme, host and port; a prefix match would accept
                // `https://api.github.com.evil.tld`).
                Some(next) if traction_core::same_origin(&self.base, &next) => url = next,
                _ => break,
            }
        }
        Ok(all)
    }
}

/// `2012-03-06T23:06:50Z` for a Unix-millisecond time.
fn iso_utc(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_timestamps_parse_to_unix_milliseconds() {
        // Independent known values (checked with `date -u -d`).
        assert_eq!(parse_rfc3339_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_ms("2012-03-06T23:06:50Z"),
            Some(1_331_075_210_000)
        );
        assert_eq!(
            parse_rfc3339_ms("2024-02-29T12:00:00Z"),
            Some(1_709_208_000_000)
        );
        // A numeric offset is subtracted; fractions are ignored.
        assert_eq!(
            parse_rfc3339_ms("2012-03-07T00:06:50.123+01:00"),
            Some(1_331_075_210_000)
        );
        for bad in [
            "",
            "2012-03-06",
            "2012-13-06T23:06:50Z",
            "2012-03-06T23:06:50",
            "x",
        ] {
            assert_eq!(parse_rfc3339_ms(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_since_filter_round_trips_through_the_iso_formatter() {
        for ms in [0, 1_331_075_210_000, 1_709_208_000_000, 1_800_000_000_000] {
            assert_eq!(parse_rfc3339_ms(&iso_utc(ms)), Some(ms));
        }
    }

    #[test]
    fn the_next_page_is_taken_from_the_link_header() {
        let header = "<https://api.github.com/r?page=2>; rel=\"next\", \
                      <https://api.github.com/r?page=9>; rel=\"last\"";
        assert_eq!(
            next_link(header).as_deref(),
            Some("https://api.github.com/r?page=2")
        );
        let last = "<https://api.github.com/r?page=1>; rel=\"first\", <https://api.github.com/r?page=1>; rel=\"prev\"";
        assert_eq!(next_link(last), None);
    }

    #[test]
    fn repo_names_are_validated_before_they_reach_a_url() {
        assert!(valid_repo_name("octocat/Hello-World"));
        assert!(valid_repo_name("a/b.c_d"));
        for bad in [
            "", "a", "a/", "/b", "a/b/c", "a/../b", "a b/c", "a/b?x=1", "../x",
        ] {
            assert!(!valid_repo_name(bad), "{bad}");
        }
    }

    #[test]
    fn a_commit_message_has_nowhere_to_go() {
        let page = br#"[{"sha":"abc","commit":{"message":"secret subject","author":{"name":"n","email":"e","date":"2012-03-06T23:06:50Z"},"committer":{"date":"2012-03-07T00:00:00Z"}},"author":{"login":"octocat"},"files":[{"patch":"+secret diff"}]}]"#;
        let records = parse_commit_page(page).unwrap();
        assert_eq!(records.len(), 1);
        let debug = format!("{records:?}");
        assert!(!debug.contains("secret"));
        assert_eq!(records[0].author_login.as_deref(), Some("octocat"));
        assert_eq!(
            records[0].committed_at_ms - records[0].authored_at_ms,
            3_190_000
        );
    }
}
