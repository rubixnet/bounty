//! Pure Project ID derivation (`docs/protocol.md` §2a).
//!
//! Only the daemon's Router calls these; Adapters never derive IDs. This
//! module does no file system or Git work: callers pass in the root commit,
//! the raw origin URL, the device secret and the canonical Project Root.

use sha2::{Digest, Sha256};

/// Length in bytes of the per-device secret that keys `dir:` IDs.
pub const DEVICE_SECRET_LEN: usize = 32;

/// Fixed separator byte between the two hashed fields of a Project ID.
const SEPARATOR: u8 = 0x00;

/// Derives a `dir:` Project ID for a non-Git Project Root (or a Git repo with
/// no commits yet).
///
/// `dir:` = `"dir:" || hex(SHA-256(device_secret || 0x00 || canonical_root))`,
/// where `canonical_root` is the UTF-8 canonical path the daemon resolved.
/// Keying by the device secret means an ID cannot be confirmed from a path
/// alone.
pub fn dir_project_id(device_secret: &[u8; DEVICE_SECRET_LEN], canonical_root: &str) -> String {
    prefixed_digest("dir:", device_secret, canonical_root.as_bytes())
}

/// Derives a `git:` Project ID for a Git Project Root with at least one commit.
///
/// `git:` = `"git:" || hex(SHA-256(lower(root_commit) || 0x00 ||
/// normalize_origin(origin)))`. A missing or blank origin hashes as the empty
/// string. The Router pins the result per repo, so a later `origin` change
/// does not change the ID.
pub fn git_project_id(root_commit: &str, origin: Option<&str>) -> String {
    let commit = root_commit.trim().to_ascii_lowercase();
    let origin = origin.map(normalize_origin).unwrap_or_default();
    prefixed_digest("git:", commit.as_bytes(), origin.as_bytes())
}

/// Normalizes a Git remote URL so every spelling of one remote compares equal.
///
/// Trims whitespace, strips the scheme, userinfo and port, lowercases the
/// host, rewrites scp-style `user@host:owner/repo` to `host/owner/repo`, and
/// strips any trailing `/` and `.git`. Path case is kept. Local paths
/// (including `file://` URLs and Windows drive paths) keep only their path.
///
/// `https://github.com/A/B.git`, `git@github.com:A/B` and
/// `ssh://git@github.com:22/A/B/` all normalize to `github.com/A/B`.
pub fn normalize_origin(origin: &str) -> String {
    let origin = origin.trim();
    let joined = if let Some((_scheme, rest)) = origin.split_once("://") {
        let (authority, path) = match rest.find('/') {
            Some(i) => rest.split_at(i),
            None => (rest, ""),
        };
        let host = normalize_host(authority, true);
        if host.is_empty() {
            path.to_owned()
        } else {
            format!("{host}{path}")
        }
    } else if let Some((authority, path)) = scp_split(origin) {
        let host = normalize_host(authority, false);
        format!("{host}/{}", path.trim_start_matches('/'))
    } else {
        origin.to_owned()
    };
    strip_trailing(&joined).to_owned()
}

/// Splits scp-style `[user@]host:path`. Returns `None` for local paths: no
/// `:` before the first `/`, or a single-letter Windows drive prefix.
fn scp_split(origin: &str) -> Option<(&str, &str)> {
    let colon = origin.find(':')?;
    if origin.find('/').is_some_and(|slash| slash < colon) {
        return None;
    }
    let (authority, path) = (&origin[..colon], &origin[colon + 1..]);
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if host.len() == 1 && host.as_bytes()[0].is_ascii_alphabetic() {
        return None;
    }
    Some((authority, path))
}

/// Drops userinfo and (when `strip_port`) a `:port`, then lowercases the host.
/// Bracketed IPv6 hosts keep their brackets.
fn normalize_host(authority: &str, strip_port: bool) -> String {
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host.starts_with('[') {
        host.find(']').map_or(host, |end| &host[..=end])
    } else if strip_port {
        host.split_once(':').map_or(host, |(h, _)| h)
    } else {
        host
    };
    host.to_ascii_lowercase()
}

fn strip_trailing(mut s: &str) -> &str {
    loop {
        let next = s.trim_end_matches('/');
        let next = next.strip_suffix(".git").unwrap_or(next);
        if next.len() == s.len() {
            return s;
        }
        s = next;
    }
}

fn prefixed_digest(prefix: &str, first: &[u8], second: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(first);
    hasher.update([SEPARATOR]);
    hasher.update(second);
    let digest = hasher.finalize();
    let mut out = String::with_capacity(prefix.len() + 64);
    out.push_str(prefix);
    for byte in digest.iter() {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

const HEX: &[u8; 16] = b"0123456789abcdef";

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET_7: [u8; 32] = [0x07; 32];
    const SECRET_8: [u8; 32] = [0x08; 32];

    // Goldens computed independently (Python hashlib) from the §2a formulas.
    #[test]
    fn dir_project_id_matches_golden() {
        assert_eq!(
            dir_project_id(&SECRET_7, "/home/u/code/game"),
            "dir:311c11cf82721c59488160b16cecdda94389c49c18c46721aa3706465f964a82"
        );
    }

    const ROOT: &str = "1111111111111111111111111111111111111111";
    const GOLDEN_AB: &str = "git:55faad61db8823c281009a7e42ba10b4f839c02fd987e4aad6df36cc7a0cfd12";

    #[test]
    fn git_project_id_matches_golden() {
        assert_eq!(
            git_project_id(ROOT, Some("https://github.com/A/B.git")),
            GOLDEN_AB
        );
    }

    #[test]
    fn origin_spellings_of_one_repo_give_one_id() {
        for origin in [
            "https://github.com/A/B.git",
            "git@github.com:A/B",
            "ssh://git@github.com:22/A/B/",
            "HTTPS://user:pw@GitHub.COM:443/A/B.git/",
            "  git://github.com/A/B.git\n",
        ] {
            assert_eq!(git_project_id(ROOT, Some(origin)), GOLDEN_AB, "{origin}");
        }
    }

    #[test]
    fn root_commit_hex_case_is_ignored() {
        let upper = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let lower = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        assert_eq!(git_project_id(upper, None), git_project_id(lower, None));
    }

    #[test]
    fn missing_or_blank_origin_hashes_as_empty() {
        let golden = "git:8c3a6353b8e154f160ad598957988becab4e0f8722514d1cb07d85b529a7a35e";
        assert_eq!(git_project_id(ROOT, None), golden);
        assert_eq!(git_project_id(ROOT, Some("")), golden);
        assert_eq!(git_project_id(ROOT, Some("  \n")), golden);
    }

    #[test]
    fn normalize_origin_goldens() {
        for (raw, want) in [
            ("https://github.com/A/B.git", "github.com/A/B"),
            ("git@github.com:A/B", "github.com/A/B"),
            ("ssh://git@github.com:22/A/B/", "github.com/A/B"),
            (
                "git@GitLab.Example.COM:Group/Sub/Repo.git",
                "gitlab.example.com/Group/Sub/Repo",
            ),
            ("http://[::1]:8080/a/b.git", "[::1]/a/b"),
            ("ssh://host/path/repo.git/", "host/path/repo"),
            ("/srv/git/repo.git", "/srv/git/repo"),
            ("file:///srv/git/repo.git", "/srv/git/repo"),
            ("C:/repos/thing.git", "C:/repos/thing"),
            ("", ""),
        ] {
            assert_eq!(normalize_origin(raw), want, "{raw}");
        }
    }

    #[test]
    fn dir_project_id_differs_by_secret_and_path() {
        assert_eq!(
            dir_project_id(&SECRET_8, "/home/u/code/game"),
            "dir:be0e29abe1e4391de457240466e800278f80259f20a738c3455104f1fd35d984"
        );
        assert_eq!(
            dir_project_id(&SECRET_7, "/home/u/code/other"),
            "dir:fd91765f5505f675c4212eb08b9530b9c1d4518a52b191d4993d4188b8e8e981"
        );
    }
}
