//! Seam: Visual Proof Mode S1 (spec "Testing seams" row S1).
//!
//! - Linux (Xvfb) and Windows: a real scheduled capture through the real
//!   daemon, child, Vault, writer and finalizer. The `.bin` decrypts under
//!   `vault.key`, its bytes and salt hash to the sealed record, and the
//!   window's link carries the matching `frame_digest`.
//! - Every OS (the macOS CI path, deviation 3): everything after the grab on
//!   committed WebP fixtures, then recorded and sealed.
//! - Duplicates stop before encoding; a hung child is killed and leaves no
//!   record, and its leftovers go at the next start.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::TestDaemon;
use rusqlite::Connection;
use traction_core::{
    FRAME_DIGEST_NONE, FrameRecord, FrameTrigger, LinkPayload, TRACTION_RULES_1, frame_digest,
    frame_hash_hex,
};
use traction_daemon::{
    Bgra, ChildArgs, ChildReport, DB_FILE, FrameRow, VaultKey, enable_visual_proof,
    finalize_settled, frame_path, read_frame, record_frame, run_capture_child, store_frame,
    verify_database,
};

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

const BIN: &str = env!("CARGO_BIN_EXE_traction-daemon");
const WINDOW: u64 = 120_000;

/// Real WebP frames used after the grab (decoded, then re-run through the
/// child's pipeline). Shared with the server's Jev transcode tests.
const FIXTURES: [&[u8]; 2] = [
    include_bytes!("../../traction-server/tests/data/frames/large_2400x1500.webp"),
    include_bytes!("../../traction-server/tests/data/frames/normal_1280x800.webp"),
];

fn bgra_from_webp(bytes: &[u8]) -> Bgra {
    let image = webp::Decoder::new(bytes).decode().expect("fixture decodes");
    let channels = if image.is_alpha() { 4 } else { 3 };
    let pixels = image
        .chunks_exact(channels)
        .flat_map(|p| [p[2], p[1], p[0], 0xFF])
        .collect();
    Bgra {
        width: image.width(),
        height: image.height(),
        pixels,
    }
}

/// One stored FILE_MUTATION (a Project is active).
fn seed_event(dir: &Path, timestamp_ms: u64) {
    let db = Connection::open(dir.join(DB_FILE)).unwrap();
    common::chain::event(&db, 1, common::chain::ALPHA, timestamp_ms);
}

fn hash_bytes(hex: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).unwrap();
    }
    out
}

fn stored_frames(dir: &Path) -> Vec<(FrameRecord, String)> {
    let db = Connection::open(dir.join(DB_FILE)).unwrap();
    let mut stmt = db
        .prepare(
            "SELECT frame_id, frame_hash, captured_at_ms, trigger, scheduled_at_ms, nonce_id,
                    pixels_state
             FROM frame ORDER BY frame_id",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok((
            FrameRecord {
                frame_id: r.get(0)?,
                frame_hash: r.get(1)?,
                captured_at_ms: r.get::<_, i64>(2)? as u64,
                trigger: FrameTrigger::parse(&r.get::<_, String>(3)?).unwrap(),
                scheduled_at_ms: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
                nonce_id: r.get(5)?,
            },
            r.get(6)?,
        ))
    })
    .unwrap()
    .map(Result::unwrap)
    .collect()
}

fn link_payload(dir: &Path, window_start_ms: u64) -> Option<LinkPayload> {
    let db = Connection::open(dir.join(DB_FILE)).unwrap();
    db.query_row(
        "SELECT payload_json FROM chain_link WHERE window_start_ms = ?1",
        [window_start_ms as i64],
        |r| r.get::<_, String>(0),
    )
    .ok()
    .map(|json| serde_json::from_str(&json).unwrap())
}

/// Seals everything stored, then checks every Frame: the Vault file
/// decrypts, rehashes to its record, and its window's link commits to the
/// window's records.
fn seal_and_check(dir: &Path) -> Vec<FrameRecord> {
    let far = now_ms() + 24 * 3_600_000;
    finalize_settled(dir, far).expect("finalize and seal");
    verify_database(dir).expect("the database verifies");
    let key = VaultKey::load(dir).unwrap().expect("vault.key");
    let frames: Vec<FrameRecord> = stored_frames(dir).into_iter().map(|(r, _)| r).collect();
    assert!(!frames.is_empty());
    for record in &frames {
        let (salt, webp) = read_frame(dir, &key, &record.frame_id, &hash_bytes(&record.frame_hash))
            .expect("the Vault file decrypts under vault.key");
        assert_eq!(frame_hash_hex(&webp, &salt), record.frame_hash);
        assert_eq!(&webp[..4], b"RIFF");
        let window = record.captured_at_ms / WINDOW * WINDOW;
        let in_window: Vec<FrameRecord> = frames
            .iter()
            .filter(|r| r.captured_at_ms / WINDOW * WINDOW == window)
            .cloned()
            .collect();
        let payload = link_payload(dir, window).unwrap_or_else(|| {
            let db = Connection::open(dir.join(DB_FILE)).unwrap();
            let links: Vec<i64> = db
                .prepare("SELECT window_start_ms FROM chain_link")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .map(Result::unwrap)
                .collect();
            panic!("window {window} is not sealed; links: {links:?}")
        });
        assert_eq!(payload.frame_digest, frame_digest(&in_window));
        assert_ne!(payload.frame_digest, FRAME_DIGEST_NONE);
    }
    frames
}

fn record(dir: &Path, report: &ChildReport, trigger: FrameTrigger) -> bool {
    let ChildReport::Written {
        frame_id,
        frame_hash,
        captured_at_ms,
        width,
        height,
        ..
    } = report
    else {
        panic!("not written: {report:?}");
    };
    record_frame(
        dir,
        FrameRow {
            record: FrameRecord {
                frame_id: frame_id.clone(),
                frame_hash: frame_hash.clone(),
                captured_at_ms: *captured_at_ms,
                trigger,
                scheduled_at_ms: None,
                nonce_id: None,
            },
            width: *width,
            height: *height,
            vault_file: frame_path(dir, frame_id),
        },
    )
    .unwrap()
}

#[test]
fn after_the_grab_fixtures_are_encoded_encrypted_recorded_and_sealed() {
    // One stored event gives the finalizer its starting point.
    let mut daemon = TestDaemon::start();
    daemon.shutdown();
    let dir = daemon.data_dir();
    seed_event(dir, now_ms() - 60_000);
    enable_visual_proof(dir).unwrap();
    let key = VaultKey::load(dir).unwrap().unwrap();
    let now = now_ms();
    let mut sizes = Vec::new();
    for (n, fixture) in FIXTURES.iter().enumerate() {
        let image = bgra_from_webp(fixture);
        let report = store_frame(dir, &key, &image, None, now - 10_000 + n as u64, 60.0);
        let ChildReport::Written {
            width,
            height,
            frame_id,
            ..
        } = &report
        else {
            panic!("{report:?}");
        };
        assert!(*height <= 720 && (*height == 720 || *height == image.height));
        assert!(*width <= image.width);
        sizes.push(std::fs::metadata(frame_path(dir, frame_id)).unwrap().len());
        assert!(record(dir, &report, FrameTrigger::ToolFail));
    }
    eprintln!("fixture Vault file sizes at q60: {sizes:?}");
    // A schema-1 style database with Frames still verifies end to end.
    assert_eq!(seal_and_check(dir).len(), 2);
}

#[test]
fn an_identical_screen_is_a_duplicate_with_no_encode_and_no_file() {
    let dir = tempfile::tempdir().unwrap();
    enable_visual_proof(dir.path()).unwrap();
    let key = VaultKey::load(dir.path()).unwrap().unwrap();
    let image = bgra_from_webp(FIXTURES[1]);
    let first = store_frame(dir.path(), &key, &image, None, 1_000, 60.0);
    let ChildReport::Written { dhash, .. } = &first else {
        panic!("{first:?}")
    };
    let prev = u64::from_str_radix(dhash, 16).unwrap();
    let files = || std::fs::read_dir(dir.path().join("vault")).unwrap().count();
    let before = files();
    let again = store_frame(dir.path(), &key, &image, Some(prev), 2_000, 60.0);
    assert_eq!(
        again,
        ChildReport::Duplicate {
            dhash: dhash.clone()
        }
    );
    assert_eq!(files(), before, "no file for a duplicate");
}

#[test]
fn a_frame_reported_after_its_window_was_sealed_is_discarded() {
    let dir = tempfile::tempdir().unwrap();
    enable_visual_proof(dir.path()).unwrap();
    let key = VaultKey::load(dir.path()).unwrap().unwrap();
    // Seal through a recent window first (one event makes it non-empty).
    let mut daemon = TestDaemon::start_with(|c| c.data_dir = dir.path().to_path_buf());
    daemon.shutdown();
    let now = now_ms();
    let old = now - 3_600_000;
    let db = Connection::open(dir.path().join(DB_FILE)).unwrap();
    db.execute(
        "INSERT INTO rollup_cursor (ruleset_id, finalized_through_ms) VALUES (?1, ?2)",
        rusqlite::params![TRACTION_RULES_1.ruleset_id, (now / WINDOW * WINDOW) as i64],
    )
    .unwrap();
    drop(db);
    let report = store_frame(
        dir.path(),
        &key,
        &bgra_from_webp(FIXTURES[0]),
        None,
        old,
        60.0,
    );
    let ChildReport::Written { frame_id, .. } = &report else {
        panic!()
    };
    assert!(frame_path(dir.path(), frame_id).exists());
    assert!(
        !record(dir.path(), &report, FrameTrigger::Scheduled),
        "discarded"
    );
    assert!(!frame_path(dir.path(), frame_id).exists(), "file deleted");
    assert!(stored_frames(dir.path()).is_empty());
}

#[test]
fn a_hung_child_is_killed_leaves_no_record_and_its_leftovers_go_at_start() {
    let mut daemon = TestDaemon::start();
    let dir = daemon.data_dir().to_path_buf();
    daemon.shutdown();
    enable_visual_proof(&dir).unwrap();
    let args = ChildArgs {
        data_dir: dir.clone(),
        display: "0".into(),
        prev_dhash: None,
        trigger: FrameTrigger::Scheduled,
        scheduled_at_ms: Some(0),
        nonce_id: None,
        expect_focus: "none".into(),
        quality: 60.0,
    };
    std::fs::write(dir.join("capture-hang.test"), b"").unwrap();
    let started = Instant::now();
    let report = run_capture_child(Path::new(BIN), &args, Duration::from_secs(1));
    std::fs::remove_file(dir.join("capture-hang.test")).unwrap();
    assert_eq!(report, None, "killed");
    assert!(started.elapsed() < Duration::from_secs(8));
    let leftovers = || {
        std::fs::read_dir(dir.join("vault"))
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name() != "vault.key")
            .count()
    };
    assert_eq!(leftovers(), 1, "the hung child left a temp file");
    assert!(stored_frames(&dir).is_empty(), "no record");
    let mut daemon = TestDaemon::start_with(|c| c.data_dir = dir.clone());
    daemon.shutdown();
    assert_eq!(leftovers(), 0, "removed at startup");
}

#[test]
fn the_child_prints_one_closed_status_line_and_never_opens_sqlite() {
    let dir = tempfile::tempdir().unwrap();
    // No vault.key yet: an error status, no database created.
    let out = std::process::Command::new(BIN)
        .args(["capture-frame", "--data-dir"])
        .arg(dir.path())
        .args([
            "--display",
            "0",
            "--prev-dhash",
            "none",
            "--trigger",
            "scheduled",
            "--scheduled-at-ms",
            "none",
            "--nonce-id",
            "none",
            "--expect-focus",
            "x",
        ])
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "{\"status\":\"error\"}\n"
    );
    assert!(out.stderr.is_empty());
    enable_visual_proof(dir.path()).unwrap();
    let out = std::process::Command::new(BIN)
        .args(["capture-frame", "--data-dir"])
        .arg(dir.path())
        .args([
            "--display",
            "0",
            "--prev-dhash",
            "none",
            "--trigger",
            "scheduled",
            "--scheduled-at-ms",
            "none",
            "--nonce-id",
            "none",
            "--expect-focus",
            "not-the-focus",
        ])
        .output()
        .unwrap();
    let line = String::from_utf8(out.stdout).unwrap();
    let report: ChildReport = serde_json::from_str(line.trim_end()).expect("one report line");
    assert!(
        matches!(
            report,
            ChildReport::FocusChanged
                | ChildReport::PermissionDenied
                | ChildReport::Unsupported
                | ChildReport::Error
        ),
        "{report:?}"
    );
    #[cfg(target_os = "macos")]
    if std::env::var_os("CI").is_some() {
        assert_eq!(
            report,
            ChildReport::PermissionDenied,
            "macOS CI has no Screen Recording"
        );
    }
    assert!(
        !dir.path().join(DB_FILE).exists(),
        "the child never opens SQLite"
    );
}

/// A real capture through the running daemon (Linux Xvfb and Windows).
#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_real_scheduled_capture_is_sealed_with_a_matching_frame_digest() {
    if cfg!(target_os = "linux") && std::env::var_os("DISPLAY").is_none() {
        assert!(std::env::var_os("CI").is_none(), "CI runs under Xvfb");
        eprintln!("SKIPPED (no X display)");
        return;
    }
    #[cfg(windows)]
    let _window = foreground::KnownWindow::show();
    let mut daemon = TestDaemon::start_with(|c| {
        c.capture.program = Some(BIN.into());
        c.capture.interval_override = Some(Duration::from_millis(300));
        c.capture.active_window = Duration::from_secs(100 * 365 * 86_400);
    });
    let dir = daemon.data_dir().to_path_buf();
    enable_visual_proof(&dir).unwrap();
    seed_event(&dir, now_ms());
    let deadline = Instant::now() + Duration::from_secs(60);
    let live = |dir: &Path| -> i64 {
        Connection::open(dir.join(DB_FILE))
            .and_then(|db| db.query_row("SELECT count(*) FROM frame", [], |r| r.get(0)))
            .unwrap_or(0)
    };
    while live(&dir) == 0 {
        assert!(Instant::now() < deadline, "no Frame was captured");
        std::thread::sleep(Duration::from_millis(200));
    }
    daemon.shutdown();
    let frames = seal_and_check(&dir);
    assert!(
        frames
            .iter()
            .all(|f| f.trigger == FrameTrigger::Scheduled && f.nonce_id.is_none())
    );
    assert!(frames.iter().all(|f| f.scheduled_at_ms.is_some()));
    // WebP size at the configured quality on this real screen.
    let key = VaultKey::load(&dir).unwrap().unwrap();
    let f = &frames[0];
    let (_, webp) = read_frame(&dir, &key, &f.frame_id, &hash_bytes(&f.frame_hash)).unwrap();
    eprintln!("real capture: {} WebP bytes", webp.len());
}

#[cfg(windows)]
mod foreground {
    //! A topmost, foreground window on a pumping thread, so the runner's
    //! session has a focused window to capture.

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, mpsc};
    use std::thread::JoinHandle;
    use std::time::Duration;

    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::Graphics::Gdi::CreateSolidBrush;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, MSG, PM_REMOVE,
        PeekMessageW, RegisterClassW, SetForegroundWindow, TranslateMessage, WNDCLASSW,
        WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
    };

    unsafe extern "system" fn proc_(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(h, m, w, l) }
    }

    pub struct KnownWindow {
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    impl KnownWindow {
        pub fn show() -> Self {
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let (ready, wait) = mpsc::channel();
            let thread = std::thread::spawn(move || unsafe {
                let class: Vec<u16> = "TractionVisualProof\0".encode_utf16().collect();
                let instance = GetModuleHandleW(std::ptr::null());
                let mut wc: WNDCLASSW = std::mem::zeroed();
                wc.lpfnWndProc = Some(proc_);
                wc.hInstance = instance;
                wc.lpszClassName = class.as_ptr();
                wc.hbrBackground = CreateSolidBrush(0x0033_99FF);
                RegisterClassW(&wc);
                let hwnd = CreateWindowExW(
                    WS_EX_TOPMOST,
                    class.as_ptr(),
                    class.as_ptr(),
                    WS_POPUP | WS_VISIBLE,
                    50,
                    50,
                    600,
                    400,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    instance,
                    std::ptr::null(),
                );
                SetForegroundWindow(hwnd);
                let _ = ready.send(());
                while !flag.load(Ordering::SeqCst) {
                    let mut msg: MSG = std::mem::zeroed();
                    while PeekMessageW(&mut msg, std::ptr::null_mut(), 0, 0, PM_REMOVE) != 0 {
                        TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                DestroyWindow(hwnd);
            });
            wait.recv().unwrap();
            std::thread::sleep(Duration::from_millis(500));
            Self {
                stop,
                thread: Some(thread),
            }
        }
    }

    impl Drop for KnownWindow {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            if let Some(t) = self.thread.take() {
                let _ = t.join();
            }
        }
    }
}
