//! Seam 2: the per-user bus token in `<data dir>/daemon.token`.

mod common;

use common::{TestDaemon, valid_event};

fn assert_is_token(value: &str) {
    assert_eq!(value.len(), 64, "32 bytes hex-encoded");
    assert!(
        value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "lowercase hex"
    );
}

#[test]
fn first_start_writes_a_32_byte_hex_token_that_authenticates() {
    let daemon = TestDaemon::start();

    assert_is_token(&daemon.token());
    assert_eq!(daemon.caller().post_event(&valid_event(1)).status, 202);
}

#[test]
fn each_data_folder_gets_its_own_random_token() {
    let first = TestDaemon::start();
    let second = TestDaemon::start();

    assert_ne!(first.token(), second.token());
}

#[cfg(unix)]
#[test]
fn token_file_is_readable_only_by_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = TestDaemon::start();

    let mode = std::fs::metadata(daemon.token_path())
        .expect("token metadata")
        .permissions()
        .mode();

    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn restart_reuses_the_same_token() {
    let daemon = TestDaemon::start();
    let before = daemon.token();

    let daemon = daemon.restart();

    assert_eq!(daemon.token(), before);
    assert_eq!(daemon.caller().post_event(&valid_event(1)).status, 202);
}

#[test]
fn malformed_token_file_is_regenerated_on_start() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    std::fs::write(daemon.token_path(), "not-a-token\n").expect("corrupt token");

    let daemon = daemon.restart();

    let regenerated = daemon.token();
    assert_is_token(&regenerated);
    assert_eq!(daemon.caller().post_event(&valid_event(1)).status, 202);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(daemon.token_path())
            .expect("token metadata")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
