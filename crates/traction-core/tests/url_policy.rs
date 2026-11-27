//! Seam: the shared outbound-URL policy. `https://` anywhere; plain `http://`
//! only on loopback (`localhost`, `127.0.0.1`, `[::1]`); nothing else (no
//! `file:`, no other scheme). Plus the origin comparison used to pin
//! followed links to a configured base.

use traction_core::{same_origin, secure_http_url};

#[test]
fn https_is_allowed_anywhere() {
    for url in [
        "https://traction.example",
        "https://traction.example/",
        "https://traction.example:8443/auth/login?pair=ABCD-EFGH",
        "HTTPS://Traction.Example/x",
    ] {
        assert!(secure_http_url(url), "{url}");
    }
}

#[test]
fn plain_http_is_allowed_only_on_loopback() {
    for url in [
        "http://127.0.0.1",
        "http://127.0.0.1:41937/x",
        "http://localhost",
        "http://localhost:9/v1",
        "http://[::1]:8080/",
    ] {
        assert!(secure_http_url(url), "{url}");
    }
    for url in [
        "http://traction.example",
        "http://example.invalid/auth/login",
        "http://127.0.0.1.evil.example/x",
        "http://localhost.evil.example",
        "http://127.0.0.1@evil.example/",
        "http://evil.example#@127.0.0.1",
        "http://10.0.0.1:41937",
    ] {
        assert!(!secure_http_url(url), "{url}");
    }
}

#[test]
fn non_http_schemes_and_malformed_urls_are_refused() {
    for url in [
        "",
        "example.com",
        "file:///etc/passwd",
        "file://localhost/etc/passwd",
        "javascript:alert(1)",
        "ftp://example.com",
        "https://",
        "https:///path",
        "https://exa mple.com",
        "https://example.com:notaport/",
        "https://example.com:99999/",
        "https://user:pass@example.com/",
        "https://example.com\\@evil.example/",
        " https://example.com",
    ] {
        assert!(!secure_http_url(url), "{url:?}");
    }
}

#[test]
fn same_origin_compares_scheme_host_and_port_not_a_prefix() {
    let base = "https://api.github.com";
    assert!(same_origin(
        base,
        "https://api.github.com/repositories/1/commits?page=2"
    ));
    assert!(same_origin(base, "https://API.github.com:443/x"));
    for next in [
        "https://api.github.com.evil.tld/repositories/1/commits?page=2",
        "https://api.github.com@evil.tld/x",
        "http://api.github.com/x",
        "https://api.github.com:8443/x",
        "https://evil.tld/https://api.github.com/x",
        "file:///api.github.com",
    ] {
        assert!(!same_origin(base, next), "{next}");
    }
    let local = "http://127.0.0.1:41000";
    assert!(same_origin(
        local,
        "http://127.0.0.1:41000/repositories/1?page=2"
    ));
    assert!(!same_origin(local, "http://127.0.0.1:410001/x"));
    assert!(!same_origin(local, "http://127.0.0.1:41000@evil.invalid/x"));
}
