//! Seam 2: the Local Ingestion Bus check order (`Host` -> `Origin` -> token
//! -> route -> body size -> validation -> enqueue) and its error responses,
//! through a real in-process daemon.

mod common;

use common::{
    BusClient, BusResponse, TestDaemon, stored_event_count, valid_event, valid_event_value,
};
use traction_daemon::MAX_BODY_BYTES;

fn own_host(client: &BusClient, name: &str) -> String {
    format!("{name}:{}", client.port())
}

fn assert_rejected(response: &BusResponse, status: u16, code: &str) {
    assert_eq!(response.status, status, "body: {}", response.body);
    assert_eq!(response.body, format!("{{\"error\":\"{code}\"}}"));
}

// --- Host (DNS rebinding) ---------------------------------------------------

#[test]
fn foreign_host_is_refused_with_403_even_with_a_valid_token() {
    let mut daemon = TestDaemon::start();
    let client = daemon.client();
    let port = client.port();

    for host in [
        format!("evil.example:{port}"),
        "127.0.0.1:9999".to_owned(),
        "127.0.0.1".to_owned(),
        "localhost".to_owned(),
        format!("[::1]:{port}"),
        format!("127.0.0.2:{port}"),
        format!("127.0.0.1.evil.example:{port}"),
    ] {
        let response = client
            .request("POST", "/v1/event")
            .header("Host", &host)
            .body(valid_event(1))
            .send();
        assert_rejected(&response, 403, "forbidden_host");
    }

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
}

#[test]
fn missing_host_is_refused_with_403() {
    let daemon = TestDaemon::start();

    let response = daemon
        .client()
        .request("POST", "/v1/event")
        .without_header("Host")
        .body(valid_event(1))
        .send();

    assert_rejected(&response, 403, "forbidden_host");
}

#[test]
fn both_loopback_host_spellings_on_the_bound_port_are_accepted() {
    let mut daemon = TestDaemon::start();
    let client = daemon.client();

    for (n, name) in ["127.0.0.1", "localhost", "LocalHost"]
        .into_iter()
        .enumerate()
    {
        let response = client
            .request("POST", "/v1/event")
            .header("Host", &own_host(&client, name))
            .body(valid_event(n as u32))
            .send();
        assert_eq!(response.status, 202, "Host {name}: {}", response.body);
    }

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 3);
}

#[test]
fn host_is_checked_before_the_token() {
    let daemon = TestDaemon::start();

    let response = daemon
        .client()
        .request("POST", "/v1/event")
        .header("Host", "evil.example")
        .without_header("Authorization")
        .body(valid_event(1))
        .send();

    assert_rejected(&response, 403, "forbidden_host");
}

// --- Origin (browser CSRF) --------------------------------------------------

#[test]
fn foreign_origin_is_refused_with_403_even_with_a_valid_token() {
    let mut daemon = TestDaemon::start();
    let client = daemon.client();
    let port = client.port();

    for origin in [
        "https://evil.example".to_owned(),
        "null".to_owned(),
        format!("https://127.0.0.1:{port}"),
        "http://127.0.0.1:9999".to_owned(),
        "http://127.0.0.1".to_owned(),
        format!("http://evil.example:{port}"),
        String::new(),
    ] {
        let response = client
            .request("POST", "/v1/event")
            .header("Origin", &origin)
            .body(valid_event(1))
            .send();
        assert_rejected(&response, 403, "forbidden_origin");
    }

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
}

#[test]
fn the_daemons_own_origin_and_no_origin_are_allowed() {
    let mut daemon = TestDaemon::start();
    let client = daemon.client();
    let port = client.port();

    let no_origin = client.post_event(&valid_event(0));
    assert_eq!(no_origin.status, 202, "{}", no_origin.body);
    for (n, origin) in [
        format!("http://127.0.0.1:{port}"),
        format!("http://localhost:{port}"),
    ]
    .into_iter()
    .enumerate()
    {
        let response = client
            .request("POST", "/v1/event")
            .header("Origin", &origin)
            .body(valid_event(n as u32 + 1))
            .send();
        assert_eq!(response.status, 202, "Origin {origin}: {}", response.body);
    }

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 3);
}

#[test]
fn origin_is_checked_before_the_token_and_after_the_host() {
    let daemon = TestDaemon::start();
    let client = daemon.client();

    let bad_origin_no_token = client
        .request("POST", "/v1/event")
        .header("Origin", "https://evil.example")
        .without_header("Authorization")
        .body(valid_event(1))
        .send();
    assert_rejected(&bad_origin_no_token, 403, "forbidden_origin");

    let bad_host_and_origin = client
        .request("POST", "/v1/event")
        .header("Host", "evil.example")
        .header("Origin", "https://evil.example")
        .body(valid_event(1))
        .send();
    assert_rejected(&bad_host_and_origin, 403, "forbidden_host");
}

// --- Routes: only after Host, Origin and token ------------------------------

#[test]
fn unknown_path_returns_404_only_once_authorized() {
    let daemon = TestDaemon::start();
    let client = daemon.client();

    let unauthenticated = client
        .request("POST", "/admin")
        .without_header("Authorization")
        .send();
    assert_rejected(&unauthenticated, 401, "unauthorized");

    let foreign_host = client
        .request("POST", "/admin")
        .header("Host", "evil.example")
        .send();
    assert_rejected(&foreign_host, 403, "forbidden_host");

    let authorized = client.request("POST", "/admin").send();
    assert_rejected(&authorized, 404, "not_found");
}

#[test]
fn wrong_method_on_the_event_path_returns_405_only_once_authorized() {
    let daemon = TestDaemon::start();
    let client = daemon.client();

    let unauthenticated = client
        .request("GET", "/v1/event")
        .without_header("Authorization")
        .send();
    assert_rejected(&unauthenticated, 401, "unauthorized");

    let foreign_origin = client
        .request("PUT", "/v1/event")
        .header("Origin", "null")
        .send();
    assert_rejected(&foreign_origin, 403, "forbidden_origin");

    for method in ["GET", "PUT", "DELETE"] {
        let authorized = client.request(method, "/v1/event").send();
        assert_rejected(&authorized, 405, "method_not_allowed");
    }
}

// --- Body size --------------------------------------------------------------

/// A valid event padded with trailing whitespace to exactly `len` bytes.
fn padded_event(n: u32, len: usize) -> String {
    let mut body = valid_event(n);
    assert!(body.len() <= len);
    body.extend(std::iter::repeat_n(' ', len - body.len()));
    body
}

#[test]
fn body_over_the_cap_returns_413_and_is_not_stored() {
    let mut daemon = TestDaemon::start();

    let response = daemon
        .client()
        .post_event(&padded_event(1, MAX_BODY_BYTES + 1));

    assert_rejected(&response, 413, "body_too_large");
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
}

#[test]
fn body_exactly_at_the_cap_is_accepted() {
    let mut daemon = TestDaemon::start();

    let response = daemon.client().post_event(&padded_event(1, MAX_BODY_BYTES));

    assert_eq!(response.status, 202, "{}", response.body);
    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 1);
}

#[test]
fn oversized_body_without_a_token_returns_401() {
    let daemon = TestDaemon::start();

    let response = daemon
        .client()
        .request("POST", "/v1/event")
        .without_header("Authorization")
        .body(padded_event(1, MAX_BODY_BYTES + 1))
        .send();

    assert_rejected(&response, 401, "unauthorized");
}

/// Bodies up to this size are read and discarded after an early refusal.
/// Under the daemon's internal drain bound (64 KiB), with headroom.
const WITHIN_DRAIN_BOUND: usize = 48 * 1024;

/// An Adapter that sends its whole body before reading the reply must get the
/// refusal and a clean close. If the daemon closed with the body unread, the
/// kernel would reset the connection, and on Windows the caller would lose
/// the already-received response.
#[test]
fn early_refusal_reads_the_unread_body_and_closes_without_a_reset() {
    let daemon = TestDaemon::start();
    let client = daemon.client();
    let body = " ".repeat(WITHIN_DRAIN_BOUND);

    for _ in 0..3 {
        let no_token = client
            .request("POST", "/v1/event")
            .without_header("Authorization")
            .body(&body)
            .send_expecting_clean_close();
        assert_rejected(&no_token, 401, "unauthorized");

        let foreign_host = client
            .request("POST", "/v1/event")
            .header("Host", "evil.example")
            .body(&body)
            .send_expecting_clean_close();
        assert_rejected(&foreign_host, 403, "forbidden_host");

        let too_large = client
            .request("POST", "/v1/event")
            .body(&body)
            .send_expecting_clean_close();
        assert_rejected(&too_large, 413, "body_too_large");
    }
}

/// The drain after a refusal is bounded in time: a caller that declares a
/// body and never sends it still gets the refusal, promptly.
#[test]
fn early_refusal_answers_a_caller_that_withholds_its_body() {
    let daemon = TestDaemon::start();
    let started = std::time::Instant::now();

    let response = daemon
        .client()
        .request("POST", "/v1/event")
        .without_header("Authorization")
        .body(" ".repeat(WITHIN_DRAIN_BOUND))
        .send_withholding_body();

    assert_rejected(&response, 401, "unauthorized");
    // The helper's read timeout is 10 s; the daemon's drain window is far
    // shorter. A generous bound keeps this steady on loaded CI runners.
    assert!(started.elapsed() < std::time::Duration::from_secs(5));
}

// --- Validation: 400 with a reason code, never echoing input ----------------

const MARKER: &str = "SECRET-PROMPT-TEXT-do-not-echo";

#[test]
fn malformed_json_returns_400_without_echoing_the_input() {
    let daemon = TestDaemon::start();

    let response = daemon
        .client()
        .post_event(&format!("{{\"event_id\": \"{MARKER}\", oops"));

    assert_rejected(&response, 400, "malformed_event");
    assert!(!response.body.contains(MARKER));
}

#[test]
fn unknown_field_returns_400_without_echoing_the_input() {
    let daemon = TestDaemon::start();
    let mut event = valid_event_value(1);
    event["raw_prompt"] = serde_json::Value::String(MARKER.to_owned());

    let response = daemon.client().post_event(&event.to_string());

    assert_rejected(&response, 400, "malformed_event");
    assert!(!response.body.contains(MARKER));
}

#[test]
fn failed_validation_returns_400_with_its_reason_code_without_echoing_the_input() {
    let mut daemon = TestDaemon::start();
    let client = daemon.client();
    let cases = [
        ("cwd", format!("relative/{MARKER}"), "invalid_cwd"),
        (
            "session_id",
            format!("{MARKER} with spaces"),
            "invalid_session_id",
        ),
        ("event_id", MARKER.to_owned(), "invalid_event_id"),
    ];

    for (field, value, code) in cases {
        let mut event = valid_event_value(1);
        event[field] = serde_json::Value::String(value);
        let response = client.post_event(&event.to_string());
        assert_rejected(&response, 400, code);
        assert!(!response.body.contains(MARKER));
    }

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 0);
}
