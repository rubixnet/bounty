//! Seam 2: an authenticated Traction Event reaches SQLite through a real
//! in-process daemon (loopback HTTP, token file, read-only database).

mod common;

use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream, UdpSocket};
use std::time::Duration;

use common::{TestDaemon, event_id, stored_event_count, valid_event};

#[test]
fn accepted_event_returns_202_and_is_stored_after_shutdown() {
    let mut daemon = TestDaemon::start();

    let response = daemon.caller().post_event(&valid_event(1));

    assert_eq!(response.status, 202);
    let db = daemon.shutdown_and_open_db();
    let stored: String = db
        .query_row("SELECT event_id FROM traction_event", [], |row| row.get(0))
        .expect("one stored event");
    assert_eq!(stored, event_id(1));
    assert_eq!(stored_event_count(&db), 1);
}

#[test]
fn every_accepted_event_is_stored_and_the_database_is_in_wal_mode() {
    let mut daemon = TestDaemon::start();
    let caller = daemon.caller();

    for n in 0..25 {
        assert_eq!(caller.post_event(&valid_event(n)).status, 202, "event {n}");
    }

    let db = daemon.shutdown_and_open_db();
    assert_eq!(stored_event_count(&db), 25);
    let journal_mode: String = db
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("journal mode");
    assert_eq!(journal_mode, "wal");
    let schema_version: i64 = db
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .expect("user_version");
    assert_eq!(schema_version, 3);
}

#[test]
fn stored_row_holds_the_validated_fields_as_typed_columns() {
    let mut daemon = TestDaemon::start();
    assert_eq!(daemon.caller().post_event(&valid_event(7)).status, 202);

    let db = daemon.shutdown_and_open_db();
    let row = db
        .query_row(
            "SELECT schema_version, timestamp_ms, harness_id, session_id,
                    parent_orchestrator_id, event_type, end_reason, cwd, project_id,
                    model_id, network_class, prompt_char_len, repo_symbol_hits,
                    has_stacktrace, tool_name, exit_code, diff_lines_delta,
                    typeof(timestamp_ms), typeof(has_stacktrace)
             FROM traction_event WHERE event_id = ?1",
            [event_id(7)],
            |row| {
                Ok((
                    (
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<String>>(6)?,
                        row.get::<_, String>(7)?,
                        row.get::<_, Option<String>>(8)?,
                    ),
                    (
                        row.get::<_, Option<String>>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, Option<i64>>(11)?,
                        row.get::<_, Option<i64>>(12)?,
                        row.get::<_, Option<bool>>(13)?,
                        row.get::<_, Option<String>>(14)?,
                        row.get::<_, Option<i64>>(15)?,
                        row.get::<_, Option<i64>>(16)?,
                        row.get::<_, String>(17)?,
                        row.get::<_, String>(18)?,
                    ),
                ))
            },
        )
        .expect("stored event");

    assert_eq!(
        row.0,
        (
            1,
            1_795_432_800_007,
            "claude-code".to_owned(),
            "session-1".to_owned(),
            None,
            "TURN_END".to_owned(),
            Some("completed".to_owned()),
            "/abs/path/to/working/dir".to_owned(),
            // The Router replaced the Adapter's ID; this `cwd` routes nowhere.
            None,
        )
    );
    assert_eq!(
        row.1,
        (
            Some("optional-model-string".to_owned()),
            "cloud_api".to_owned(),
            Some(142),
            Some(3),
            Some(true),
            Some("Bash".to_owned()),
            Some(1),
            Some(24),
            "integer".to_owned(),
            "integer".to_owned(),
        )
    );
}

#[test]
fn missing_authorization_is_401_and_nothing_is_stored() {
    let mut daemon = TestDaemon::start();

    let response = daemon
        .caller()
        .request("POST", "/v1/event")
        .without_header("Authorization")
        .body(valid_event(1))
        .send();

    assert_eq!(response.status, 401);
    assert_eq!(stored_event_count(&daemon.shutdown_and_open_db()), 0);
}

#[test]
fn wrong_token_is_401_and_nothing_is_stored() {
    let mut daemon = TestDaemon::start();
    let caller = daemon.caller();
    let token = caller.token().to_owned();
    let last = token.chars().last().expect("non-empty token");
    let flipped = if last == '0' { '1' } else { '0' };
    let off_by_last_char = format!("{}{flipped}", &token[..token.len() - 1]);

    let wrong_tokens = [
        format!("Bearer {off_by_last_char}"),
        format!("Bearer {}", &token[..token.len() - 1]),
        format!("Bearer {token}0"),
        "Bearer ".to_owned(),
        format!("Basic {token}"),
        token.clone(),
    ];
    for authorization in &wrong_tokens {
        let response = caller
            .request("POST", "/v1/event")
            .header("Authorization", authorization)
            .body(valid_event(1))
            .send();
        assert_eq!(response.status, 401, "Authorization: {authorization}");
        assert!(!response.body.contains(&token), "401 body echoes the token");
    }

    assert_eq!(stored_event_count(&daemon.shutdown_and_open_db()), 0);
}

#[test]
fn listener_is_bound_to_ipv4_loopback_only() {
    let daemon = TestDaemon::start();

    assert_eq!(daemon.addr().ip(), Ipv4Addr::LOCALHOST);
    assert_ne!(daemon.addr().port(), 0);
}

/// This host's outbound IPv4 address, if it has one that is not loopback.
/// `connect` on a UDP socket only picks a route and a source address; no
/// packet is sent. The target is TEST-NET-1 (RFC 5737), never a real host.
fn non_loopback_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok()?;
    socket.connect((Ipv4Addr::new(192, 0, 2, 1), 9)).ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(ip) if !ip.is_loopback() && !ip.is_unspecified() => Some(ip),
        _ => None,
    }
}

#[test]
fn listener_is_not_reachable_on_a_non_loopback_address() {
    let Some(ip) = non_loopback_ipv4() else {
        eprintln!("skipped: this host has no non-loopback IPv4 address");
        return;
    };
    let daemon = TestDaemon::start();
    let target = SocketAddr::from((ip, daemon.addr().port()));

    // Nothing listens on this address, so the host itself refuses at once.
    // The timeout only stops a misconfigured firewall from hanging the test.
    let connected = TcpStream::connect_timeout(&target, Duration::from_secs(2));

    assert!(
        connected.is_err(),
        "the bus answered on non-loopback {target}"
    );
}
