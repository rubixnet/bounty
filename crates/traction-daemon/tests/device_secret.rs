//! Seam 2: the per-device secret in `<data dir>/device.secret`, which keys
//! `dir:` Project IDs. Separate from the bus token.

mod common;

use common::{TestDaemon, valid_event};
use traction_daemon::DEVICE_SECRET_FILE;

fn read_secret(daemon: &TestDaemon) -> Vec<u8> {
    std::fs::read(daemon.data_dir().join(DEVICE_SECRET_FILE)).expect("device secret file")
}

#[test]
fn first_start_writes_a_32_byte_device_secret() {
    let daemon = TestDaemon::start();

    let secret = read_secret(&daemon);

    assert_eq!(secret.len(), traction_core::DEVICE_SECRET_LEN);
    assert_ne!(secret, vec![0u8; 32], "not an all-zero secret");
}

#[test]
fn the_device_secret_is_not_the_bus_token() {
    let daemon = TestDaemon::start();

    let secret = read_secret(&daemon);
    let token = daemon.token();

    assert_ne!(secret, token.as_bytes());
    assert!(
        !token
            .as_bytes()
            .windows(secret.len())
            .any(|window| window == secret.as_slice())
    );
}

#[test]
fn each_data_folder_gets_its_own_random_secret() {
    let first = TestDaemon::start();
    let second = TestDaemon::start();

    assert_ne!(read_secret(&first), read_secret(&second));
}

#[test]
fn restart_reuses_the_same_secret() {
    let daemon = TestDaemon::start();
    let before = read_secret(&daemon);

    let daemon = daemon.restart();

    assert_eq!(read_secret(&daemon), before);
    assert_eq!(daemon.caller().post_event(&valid_event(1)).status, 202);
}

#[cfg(unix)]
#[test]
fn device_secret_file_is_readable_only_by_the_user() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = TestDaemon::start();

    let mode = std::fs::metadata(daemon.data_dir().join(DEVICE_SECRET_FILE))
        .expect("secret metadata")
        .permissions()
        .mode();

    assert_eq!(mode & 0o777, 0o600);
}

#[test]
fn a_malformed_secret_stops_startup_instead_of_forking_project_ids() {
    // Silently regenerating would give every `dir:` Project a new ID.
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let path = daemon.data_dir().join(DEVICE_SECRET_FILE);
    std::fs::write(&path, b"short").expect("corrupt secret");

    let result = common::try_start_on(daemon.data_dir(), |_| {});

    assert!(
        matches!(result, Err(traction_daemon::StartError::DeviceSecret(_))),
        "startup must refuse a malformed device secret"
    );
    assert_eq!(std::fs::read(&path).expect("secret kept"), b"short");
}
