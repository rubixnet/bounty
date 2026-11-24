//! Seam 2: an authenticated Traction Event reaches SQLite through a real
//! in-process daemon (loopback HTTP, token file, read-only database).

mod common;

use std::net::Ipv4Addr;

use common::{TestDaemon, event_id, stored_event_count, valid_event};

#[test]
fn accepted_event_returns_202_and_is_stored_after_shutdown() {
    let mut daemon = TestDaemon::start();

    let response = daemon.client().post_event(&valid_event(1));

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
    let client = daemon.client();

    for n in 0..25 {
        assert_eq!(client.post_event(&valid_event(n)).status, 202, "event {n}");
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
    assert_eq!(schema_version, 1);
}

#[test]
fn stored_row_holds_the_validated_fields_as_typed_columns() {
    let mut daemon = TestDaemon::start();
    assert_eq!(daemon.client().post_event(&valid_event(7)).status, 202);

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
                        row.get::<_, String>(8)?,
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
            "git:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".to_owned(),
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
        .client()
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
    let client = daemon.client();
    let token = client.token().to_owned();
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
        let response = client
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
