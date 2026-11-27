//! S1 scheduling: while Visual Proof Mode is on and a Project is active,
//! one scheduled Frame every 60–90 s from the OS CSPRNG (local jitter,
//! `nonce_id: null`). S2 replaces the policy and adds the Capture Nonce.
//!
//! The scheduler marks the window of each child it runs as in flight
//! ([`InFlight`]); the finalizer does not seal that window until the child
//! has reported or been killed.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rusqlite::{Connection, OpenFlags};
use traction_core::{FrameRecord, FrameTrigger, TRACTION_RULES_1};

use super::child::{ChildArgs, ChildReport};
use super::settings::VisualProof;
use super::{CAPTURE_ACTIVE_WINDOW, CAPTURE_CHILD_TIMEOUT, grab, local_jitter_interval_ms, runner};
use crate::clock::now_ms;
use crate::log;
use crate::storage::{DB_FILE, FrameRow, WriteOp};
use crate::writer::WriterSender;

/// The window start of the capture child now running, if any.
#[derive(Debug, Clone, Default)]
pub struct InFlight(Arc<AtomicU64>);

const NONE_IN_FLIGHT: u64 = u64::MAX;

impl InFlight {
    pub fn new() -> Self {
        Self(Arc::new(AtomicU64::new(NONE_IN_FLIGHT)))
    }

    /// The in-flight child's window start.
    pub fn window(&self) -> Option<u64> {
        let w = self.0.load(Ordering::SeqCst);
        (w != NONE_IN_FLIGHT).then_some(w)
    }

    fn set(&self, window: Option<u64>) {
        self.0
            .store(window.unwrap_or(NONE_IN_FLIGHT), Ordering::SeqCst);
    }

    /// Whether a batch ending at `through_ms` may be sealed now.
    pub fn allows_sealing_through(&self, through_ms: u64) -> bool {
        self.window().is_none_or(|w| through_ms <= w)
    }
}

/// Scheduler tuning. Production uses the defaults; tests shorten them.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CaptureSettings {
    pub enabled: bool,
    /// The `traction-daemon` binary run as `capture-frame`. `None` uses
    /// this process's own executable.
    pub program: Option<PathBuf>,
    /// Test-only fixed interval instead of local jitter (and no wait for the
    /// first window boundary).
    pub interval_override: Option<Duration>,
    pub active_window: Duration,
    pub child_timeout: Duration,
}

impl Default for CaptureSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            program: None,
            interval_override: None,
            active_window: CAPTURE_ACTIVE_WINDOW,
            child_timeout: CAPTURE_CHILD_TIMEOUT,
        }
    }
}

pub struct CaptureThread {
    stop: Sender<()>,
    handle: JoinHandle<()>,
}

impl CaptureThread {
    pub fn stop(self) -> thread::Result<()> {
        let _ = self.stop.send(());
        self.handle.join()
    }
}

pub fn spawn(
    data_dir: &Path,
    writer: WriterSender,
    in_flight: InFlight,
    settings: CaptureSettings,
) -> std::io::Result<CaptureThread> {
    let program = match &settings.program {
        Some(program) => program.clone(),
        None => std::env::current_exe()?,
    };
    let data_dir = data_dir.to_path_buf();
    let (stop, rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("traction-capture".to_owned())
        .spawn(move || {
            let mut prev_dhash: Option<u64> = None;
            let mut next_slot: Option<u64> = None;
            loop {
                let now = now_ms();
                let proof = VisualProof::load(&data_dir);
                if !proof.enabled {
                    // Off: no children. The schedule restarts at the first
                    // window boundary after it is turned on again.
                    next_slot = None;
                }
                let slot = *next_slot.get_or_insert_with(|| first_slot(now, &settings));
                if proof.enabled && now >= slot {
                    next_slot = Some(slot + interval_ms(&settings));
                    if active(&data_dir, now, settings.active_window) {
                        let report = capture_once(
                            &program,
                            &data_dir,
                            &in_flight,
                            &settings,
                            slot,
                            prev_dhash,
                            proof.webp_quality,
                        );
                        match report {
                            Some(ChildReport::Written { ref dhash, .. })
                            | Some(ChildReport::Duplicate { ref dhash }) => {
                                prev_dhash = u64::from_str_radix(dhash, 16).ok();
                            }
                            _ => {}
                        }
                        if let Some(ChildReport::Written { .. }) = &report
                            && let Some(op) = frame_op(&data_dir, report.as_ref(), slot)
                            && writer.send(op).is_err()
                        {
                            return;
                        }
                    }
                }
                let wait = match (proof.enabled, next_slot) {
                    (true, Some(slot)) => Duration::from_millis(slot.saturating_sub(now_ms())),
                    _ => Duration::from_secs(5),
                }
                .clamp(Duration::from_millis(10), Duration::from_secs(5));
                match rx.recv_timeout(wait) {
                    Err(RecvTimeoutError::Timeout) => {}
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })?;
    Ok(CaptureThread { stop, handle })
}

fn window_of(ms: u64) -> u64 {
    let width = TRACTION_RULES_1.activity_rollup_ms;
    ms / width * width
}

/// `t₀`: the first window boundary strictly after `now`.
fn first_slot(now: u64, settings: &CaptureSettings) -> u64 {
    match settings.interval_override {
        Some(_) => now,
        None => window_of(now) + TRACTION_RULES_1.activity_rollup_ms,
    }
}

fn interval_ms(settings: &CaptureSettings) -> u64 {
    if let Some(fixed) = settings.interval_override {
        return u64::try_from(fixed.as_millis()).unwrap_or(u64::MAX).max(1);
    }
    let mut r = [0u8; 32];
    if getrandom::fill(&mut r).is_err() {
        // No randomness: skip ahead the longest interval rather than guess.
        return super::SCHEDULE_BASE_MS + super::SCHEDULE_JITTER_MS - 1;
    }
    local_jitter_interval_ms(&r)
}

/// A Project is active: some Traction Event in the last `window`.
fn active(data_dir: &Path, now: u64, window: Duration) -> bool {
    let since = now.saturating_sub(u64::try_from(window.as_millis()).unwrap_or(u64::MAX));
    let read = || -> rusqlite::Result<bool> {
        let conn = Connection::open_with_flags(
            data_dir.join(DB_FILE),
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        conn.busy_timeout(Duration::from_secs(2))?;
        conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM traction_event WHERE timestamp_ms >= ?1)",
            [i64::try_from(since).unwrap_or(i64::MAX)],
            |row| row.get(0),
        )
    };
    read().unwrap_or(false)
}

fn capture_once(
    program: &Path,
    data_dir: &Path,
    in_flight: &InFlight,
    settings: &CaptureSettings,
    slot: u64,
    prev_dhash: Option<u64>,
    quality: f32,
) -> Option<ChildReport> {
    // No focus, no Frame (fail closed).
    let focus = grab::focus()?;
    let args = ChildArgs {
        data_dir: data_dir.to_path_buf(),
        display: focus.display,
        prev_dhash,
        trigger: FrameTrigger::Scheduled,
        scheduled_at_ms: Some(slot),
        nonce_id: None,
        expect_focus: focus.token,
        quality,
    };
    in_flight.set(Some(window_of(now_ms())));
    let report = runner::run_child(program, &args, settings.child_timeout);
    in_flight.set(None);
    if report.is_none() {
        log::warn("a capture child was killed or reported nothing; no Frame recorded");
    }
    report
}

/// The writer op for a `written` report.
fn frame_op(data_dir: &Path, report: Option<&ChildReport>, slot: u64) -> Option<WriteOp> {
    let Some(ChildReport::Written {
        frame_id,
        frame_hash,
        captured_at_ms,
        width,
        height,
        ..
    }) = report
    else {
        return None;
    };
    let record = FrameRecord {
        frame_id: frame_id.clone(),
        frame_hash: frame_hash.clone(),
        captured_at_ms: *captured_at_ms,
        trigger: FrameTrigger::Scheduled,
        scheduled_at_ms: Some(slot),
        nonce_id: None,
    };
    let vault_file = super::vault::frame_path(data_dir, frame_id);
    if record.validate().is_err() {
        let _ = std::fs::remove_file(&vault_file);
        return None;
    }
    Some(WriteOp::Frame(Box::new(FrameRow {
        record,
        width: *width,
        height: *height,
        vault_file,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_in_flight_window_holds_back_sealing_it() {
        let f = InFlight::new();
        assert!(f.allows_sealing_through(u64::MAX - 1));
        f.set(Some(240_000));
        assert!(f.allows_sealing_through(240_000));
        assert!(!f.allows_sealing_through(360_000));
        f.set(None);
        assert!(f.allows_sealing_through(360_000));
    }

    #[test]
    fn the_first_slot_is_the_next_window_boundary() {
        let s = CaptureSettings::default();
        assert_eq!(first_slot(0, &s), 120_000);
        assert_eq!(first_slot(119_999, &s), 120_000);
        assert_eq!(first_slot(120_000, &s), 240_000);
        for _ in 0..50 {
            assert!((60_000..90_000).contains(&interval_ms(&s)));
        }
    }
}
