//! Seam: the Device Signing Key in `<data dir>/signing.key` (Step 3a). A raw
//! 32-byte Ed25519 seed beside a public `signing.pub`, owner-only, never
//! regenerated when malformed.

mod common;

use common::TestDaemon;
use traction_core::VerifyingKeyHex;
use traction_daemon::{SIGNING_KEY_FILE, SIGNING_PUB_FILE, StartError};

fn key_bytes(daemon: &TestDaemon) -> Vec<u8> {
    std::fs::read(daemon.data_dir().join(SIGNING_KEY_FILE)).expect("signing key file")
}

fn pub_text(daemon: &TestDaemon) -> String {
    std::fs::read_to_string(daemon.data_dir().join(SIGNING_PUB_FILE)).expect("signing.pub")
}

#[test]
fn first_start_writes_a_32_byte_seed_and_its_public_key_as_hex() {
    let daemon = TestDaemon::start();

    let seed = key_bytes(&daemon);
    let public = pub_text(&daemon);

    assert_eq!(seed.len(), 32);
    assert_ne!(seed, vec![0u8; 32]);
    assert_eq!(public.len(), 65, "64 hex characters and a newline");
    assert!(public.ends_with('\n'));
    assert!(
        public
            .trim_end()
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    );
    VerifyingKeyHex::parse(public.trim_end()).expect("a valid, non-weak public key");
}

#[test]
fn the_signing_key_is_not_the_device_secret_or_the_token() {
    let daemon = TestDaemon::start();

    let seed = key_bytes(&daemon);

    assert_ne!(seed, daemon.device_secret().to_vec());
    assert_ne!(seed, daemon.token().into_bytes());
}

#[test]
fn restart_reuses_the_same_key_and_registers_it_in_the_database() {
    let daemon = TestDaemon::start();
    let before = key_bytes(&daemon);
    let public = pub_text(&daemon);

    let mut daemon = daemon.restart();
    daemon.shutdown();

    assert_eq!(key_bytes(&daemon), before);
    assert_eq!(pub_text(&daemon), public);
    let db = daemon.live_db();
    let rows: Vec<(String, String)> = db
        .prepare("SELECT key_id, public_key FROM chain_key")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let key = VerifyingKeyHex::parse(public.trim_end()).unwrap();
    assert_eq!(
        rows,
        vec![(key.key_id().to_string(), public.trim_end().to_owned())]
    );
}

#[cfg(unix)]
#[test]
fn the_key_file_is_owner_only_and_a_too_open_mode_is_tightened_on_start() {
    use std::os::unix::fs::PermissionsExt;
    let daemon = TestDaemon::start();
    let path = daemon.data_dir().join(SIGNING_KEY_FILE);
    let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);

    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let daemon = daemon.restart();

    assert_eq!(mode(&path), 0o600);
    drop(daemon);
}

#[test]
fn a_malformed_key_stops_startup_and_is_left_untouched() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let path = daemon.data_dir().join(SIGNING_KEY_FILE);
    std::fs::write(&path, b"short").unwrap();

    let result = common::try_start_on(daemon.data_dir(), |_| {});

    assert!(matches!(result, Err(StartError::SigningKey(_))));
    assert_eq!(std::fs::read(&path).unwrap(), b"short");
}

#[test]
fn a_tampered_public_file_is_rewritten_from_the_seed() {
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let pub_path = daemon.data_dir().join(SIGNING_PUB_FILE);
    let good = std::fs::read_to_string(&pub_path).unwrap();
    std::fs::write(&pub_path, "00\n").unwrap();

    let daemon = daemon.restart();

    assert_eq!(pub_text(&daemon), good);
}

/// The key file's DACL holds exactly one ACE and does not inherit.
#[cfg(windows)]
#[test]
fn the_key_file_has_one_protected_owner_ace() {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
        GetAclInformation, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };

    let daemon = TestDaemon::start();
    let path = daemon.data_dir().join(SIGNING_KEY_FILE);
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
    let mut dacl: *mut ACL = null_mut();
    let mut descriptor: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: valid NUL-terminated path and out pointers; freed below.
    let status = unsafe {
        GetNamedSecurityInfoW(
            wide.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut descriptor,
        )
    };
    assert_eq!(status, 0, "GetNamedSecurityInfoW");
    let mut info = ACL_SIZE_INFORMATION {
        AceCount: 0,
        AclBytesInUse: 0,
        AclBytesFree: 0,
    };
    // SAFETY: `dacl` came from the call above; `info` is the right size.
    let ok = unsafe {
        GetAclInformation(
            dacl,
            (&mut info as *mut ACL_SIZE_INFORMATION).cast::<c_void>(),
            size_of::<ACL_SIZE_INFORMATION>() as u32,
            AclSizeInformation,
        )
    };
    assert_ne!(ok, 0, "GetAclInformation");
    let mut control = 0u16;
    let mut revision = 0u32;
    // SAFETY: `descriptor` came from the call above.
    let ok = unsafe { GetSecurityDescriptorControl(descriptor, &mut control, &mut revision) };
    assert_ne!(ok, 0, "GetSecurityDescriptorControl");
    // SAFETY: allocated by GetNamedSecurityInfoW.
    unsafe { LocalFree(descriptor) };

    assert_eq!(info.AceCount, 1, "exactly one ACE");
    assert_ne!(control & SE_DACL_PROTECTED, 0, "no inherited ACEs");
}
