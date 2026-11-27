//! The one outbound-URL policy shared by the daemon and the server.
//!
//! - [`secure_http_url`]: `https://` on any host, plain `http://` only on a
//!   loopback host (`localhost`, `127.0.0.1`, `[::1]`; local tests and
//!   self-hosted dev). Every other scheme (`file:`, `javascript:`, …) is
//!   refused, as is anything this strict parser cannot read.
//! - [`same_origin`]: compares the parsed (scheme, host, port), never a string
//!   prefix, so `https://api.github.com.evil.tld` is not
//!   `https://api.github.com`.
//!
//! The parser is deliberately narrow: no userinfo (`user@`), no backslashes,
//! no whitespace or control characters, hostnames limited to
//! `[A-Za-z0-9.-]` or a bracketed IPv6 literal.

const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

#[derive(Debug, PartialEq, Eq)]
struct Origin {
    https: bool,
    host: String,
    port: u16,
}

fn parse_origin(url: &str) -> Option<Origin> {
    if url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '\\')
    {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    let https = if scheme.eq_ignore_ascii_case("https") {
        true
    } else if scheme.eq_ignore_ascii_case("http") {
        false
    } else {
        return None;
    };
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    if authority.contains('@') {
        return None;
    }
    let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
        let (inner, after) = v6.split_once(']')?;
        if inner.is_empty()
            || !inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
        {
            return None;
        }
        (format!("[{inner}]"), after)
    } else {
        let split = authority.find(':').unwrap_or(authority.len());
        let host = &authority[..split];
        if host.is_empty()
            || !host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        {
            return None;
        }
        (host.to_owned(), &authority[split..])
    };
    let port = match port {
        "" => {
            if https {
                443
            } else {
                80
            }
        }
        p => {
            let digits = p.strip_prefix(':')?;
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            digits.parse().ok()?
        }
    };
    Some(Origin {
        https,
        host: host.to_ascii_lowercase(),
        port,
    })
}

/// `https://` anywhere, plain `http://` only on loopback, nothing else.
pub fn secure_http_url(url: &str) -> bool {
    parse_origin(url).is_some_and(|o| o.https || LOOPBACK_HOSTS.contains(&o.host.as_str()))
}

/// Whether `a` and `b` share scheme, host and port (default ports filled in).
/// An unparseable URL matches nothing.
pub fn same_origin(a: &str, b: &str) -> bool {
    match (parse_origin(a), parse_origin(b)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}
