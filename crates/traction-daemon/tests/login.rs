//! Seam: `traction login` (Pairing) against a local test HTTP server built
//! from `traction_core::sync`: the public key goes up, the user approves on
//! the server's page (never here), and the device token is stored owner-only
//! at `<data dir>/server.token`.

mod common;

use std::cell::RefCell;
use std::time::Duration;

use common::http_server::{HttpServer, Request, Response};
use traction_core::sync::{
    PairingPollRequest, PairingPollResponse, PairingStartRequest, PairingStartResponse,
    PairingStatus,
};
use traction_daemon::{
    LoginOptions, SERVER_KEY_ID_FILE, SERVER_TOKEN_FILE, SERVER_URL_FILE, SIGNING_KEY_FILE,
    SIGNING_PUB_FILE, login,
};

const TOKEN: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn pairing_server(approve_after_polls: usize) -> HttpServer {
    HttpServer::start(move |req: &Request, n| match req.path.as_str() {
        "/v1/pairings" => Response::json(
            200,
            &PairingStartResponse {
                pair_code: "ABCD-EFGH".into(),
                poll_secret: "s".repeat(64),
                login_url: "http://example.invalid/auth/login?pair=ABCD-EFGH".into(),
                expires_in_sec: 600,
            },
        ),
        "/v1/pairings/poll" => {
            let poll: PairingPollRequest = req.json();
            assert_eq!(poll.pair_code, "ABCD-EFGH");
            let approved = n > approve_after_polls;
            Response::json(
                200,
                &PairingPollResponse {
                    status: if approved {
                        PairingStatus::Approved
                    } else {
                        PairingStatus::Pending
                    },
                    device_token: approved.then(|| TOKEN.to_owned()),
                    device_id: approved.then(|| "dev1".to_owned()),
                },
            )
        }
        _ => Response::json(404, &serde_json::json!({})),
    })
}

fn options(dir: &std::path::Path, server: &HttpServer) -> LoginOptions {
    LoginOptions {
        data_dir: dir.to_path_buf(),
        server_url: server.url(),
        open_browser: true,
        poll_interval: Duration::from_millis(20),
        timeout: Duration::from_secs(20),
    }
}

#[test]
fn login_pins_the_key_waits_for_approval_and_stores_the_token_owner_only() {
    let dir = tempfile::tempdir().unwrap();
    let server = pairing_server(2);
    let opened = RefCell::new(Vec::new());
    let mut out = Vec::new();

    login(&options(dir.path(), &server), &mut out, &|url| {
        opened.borrow_mut().push(url.to_owned());
    })
    .expect("login succeeds");

    let token_path = dir.path().join(SERVER_TOKEN_FILE);
    assert_eq!(std::fs::read_to_string(&token_path).unwrap(), TOKEN);
    assert_eq!(
        std::fs::read_to_string(dir.path().join(SERVER_URL_FILE)).unwrap(),
        server.url()
    );
    assert_eq!(
        *opened.borrow(),
        ["http://example.invalid/auth/login?pair=ABCD-EFGH"]
    );

    // The key that went up is the Device Signing Key in the data folder.
    let public = std::fs::read_to_string(dir.path().join(SIGNING_PUB_FILE)).unwrap();
    let start: PairingStartRequest = server.requests()[0].json();
    assert_eq!(start.public_key, public.trim());
    assert_eq!(start.previous_key_id, None);

    // The printed output shows the code and URL, never the token.
    let printed = String::from_utf8(out).unwrap();
    assert!(printed.contains("ABCD-EFGH") && printed.contains("/auth/login?pair="));
    assert!(!printed.contains(TOKEN));
    assert_owner_only(&token_path);
}

#[cfg(unix)]
fn assert_owner_only(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

/// `server.token` is written through the same `owner_only` code as
/// `signing.key`: one protected ACE for the current user.
#[cfg(windows)]
fn assert_owner_only(path: &std::path::Path) {
    use std::ffi::c_void;
    use std::os::windows::ffi::OsStrExt;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        ACL, ACL_SIZE_INFORMATION, AclSizeInformation, DACL_SECURITY_INFORMATION,
        GetAclInformation, GetSecurityDescriptorControl, PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED,
    };

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

#[test]
fn re_login_after_a_key_change_names_the_previous_key() {
    let dir = tempfile::tempdir().unwrap();
    let server = pairing_server(0);
    let mut out = Vec::new();
    login(&options(dir.path(), &server), &mut out, &|_| {}).unwrap();
    let first_key_id = std::fs::read_to_string(dir.path().join(SERVER_KEY_ID_FILE)).unwrap();

    // Honest key loss: a new key, then log in again.
    std::fs::remove_file(dir.path().join(SIGNING_KEY_FILE)).unwrap();
    std::fs::remove_file(dir.path().join(SIGNING_PUB_FILE)).unwrap();
    login(&options(dir.path(), &server), &mut out, &|_| {}).unwrap();

    let starts: Vec<PairingStartRequest> = server
        .requests()
        .iter()
        .filter(|r| r.path == "/v1/pairings")
        .map(Request::json)
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(
        starts[1].previous_key_id.as_deref(),
        Some(first_key_id.trim())
    );
    assert_ne!(starts[0].public_key, starts[1].public_key);
}

#[test]
fn a_pairing_that_is_never_approved_times_out_and_stores_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let server = pairing_server(usize::MAX - 1);
    let mut options = options(dir.path(), &server);
    options.timeout = Duration::from_millis(200);
    let error = login(&options, &mut Vec::new(), &|_| {}).unwrap_err();
    assert!(error.contains("timed out"), "{error}");
    assert!(!dir.path().join(SERVER_TOKEN_FILE).exists());
}

#[test]
fn a_server_url_without_a_scheme_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let server = pairing_server(0);
    let mut options = options(dir.path(), &server);
    options.server_url = "example.com".into();
    assert!(login(&options, &mut Vec::new(), &|_| {}).is_err());
    assert_eq!(server.count(), 0);
}
