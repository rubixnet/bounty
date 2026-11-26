//! The Presence Sensor: content-free Presence Minutes, one OS backend per
//! platform chosen at compile time.
//!
//! A backend is read once per 10 s sub-slot (idle timer and foreground
//! category) and may push input events from an OS notification source (X11
//! raw events). The sensor thread sleeps in `recv_timeout` until the next
//! sub-slot boundary or an input notification, so it never polls: with no
//! input it wakes exactly once per sub-slot. Each wall-clock minute it
//! hands one [`PresenceMinute`] to the single SQLite writer.
//!
//! Nothing here reads key codes, characters, pointer positions, window
//! titles or URLs into any retained value. A minute the daemon only joined
//! part-way (or slept through) is dropped, so nothing is written for time
//! the daemon was not watching. Presence is device-wide: tray and session
//! pause do not touch it.

pub mod aggregate;
pub mod category;
mod platform;

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use traction_core::{AppCategory, SUBSLOT_SEC, SUBSLOTS_PER_MINUTE, TelemetryTier};

use crate::storage::WriteOp;
use crate::writer::WriterSender;
use aggregate::{Aggregator, InputKind, Sample};

pub use platform::detect;

/// The default sub-slot length ([`SUBSLOT_SEC`]).
pub const DEFAULT_SUBSLOT: Duration = Duration::from_secs(SUBSLOT_SEC as u64);

/// Presence sensor tuning. Production uses the defaults; tests shorten the
/// sub-slot or turn the sensor off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct PresenceSettings {
    pub enabled: bool,
    /// Test-only override of [`DEFAULT_SUBSLOT`]. A minute is six sub-slots.
    pub subslot: Duration,
}

impl Default for PresenceSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            subslot: DEFAULT_SUBSLOT,
        }
    }
}

/// What the sensor thread receives.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the X11 backend pushes input")
)]
pub enum Msg {
    Input {
        t_ms: u64,
        kind: InputKind,
        injected: bool,
    },
    Stop,
}

/// One OS backend.
pub trait Backend: Send + 'static {
    /// What this backend can currently observe. A backend that hits an error
    /// lowers this (and logs once); it never panics.
    fn tier(&self) -> TelemetryTier;
    /// Idle timer and foreground category, once per sub-slot.
    fn sample(&mut self) -> Sample;
    /// Starts an OS notification source that sends [`Msg::Input`]. The
    /// default has none.
    fn start_input(&mut self, _tx: Sender<Msg>) {}
}

pub struct PresenceSensor {
    tx: Sender<Msg>,
    handle: JoinHandle<()>,
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the wakeup-budget test"))]
    wakeups: Arc<AtomicU64>,
}

impl PresenceSensor {
    /// Sensor thread wakeups so far (sub-slot timers plus input notifications).
    #[cfg_attr(not(test), allow(dead_code, reason = "read by the wakeup-budget test"))]
    pub fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Relaxed)
    }

    /// Stops the thread and waits for it (dropping its writer sender).
    pub fn stop(self) -> thread::Result<()> {
        let _ = self.tx.send(Msg::Stop);
        self.handle.join()
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

pub fn spawn(
    mut backend: Box<dyn Backend>,
    writer: WriterSender,
    subslot: Duration,
) -> io::Result<PresenceSensor> {
    let (tx, rx) = mpsc::channel();
    backend.start_input(tx.clone());
    let wakeups = Arc::new(AtomicU64::new(0));
    let counter = Arc::clone(&wakeups);
    let slot_ms = u64::try_from(subslot.as_millis())
        .unwrap_or(u64::MAX)
        .max(1);
    let minute_ms = slot_ms * u64::from(SUBSLOTS_PER_MINUTE);
    let handle = thread::Builder::new()
        .name("traction-presence".to_owned())
        .spawn(move || {
            let fresh = |now: u64, previous: Option<AppCategory>, backend: &dyn Backend| {
                let start = now / minute_ms * minute_ms;
                // Joined more than a sub-slot late: a partial minute.
                let partial = now - start > slot_ms;
                (
                    start,
                    partial,
                    Aggregator::new(start, slot_ms, backend.tier(), previous),
                )
            };
            let (mut minute_start, mut skip, mut agg) = fresh(now_ms(), None, backend.as_ref());
            loop {
                let now = now_ms();
                let boundary = (now / slot_ms + 1) * slot_ms;
                match rx.recv_timeout(Duration::from_millis(boundary - now)) {
                    Ok(Msg::Input {
                        t_ms,
                        kind,
                        injected,
                    }) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                        agg.input(t_ms, kind, injected);
                    }
                    Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                        let after = now_ms();
                        if after > boundary + slot_ms {
                            // Suspended or stalled: the minute is unreliable.
                            (minute_start, skip, agg) = fresh(after, None, backend.as_ref());
                            continue;
                        }
                        let slot = (boundary - slot_ms).saturating_sub(minute_start) / slot_ms;
                        agg.slot_end(usize::try_from(slot).unwrap_or(0), backend.sample());
                        if boundary >= minute_start + minute_ms {
                            let (minute, last) = agg.finish();
                            if !skip && writer.send(WriteOp::Presence(minute)).is_err() {
                                return;
                            }
                            (minute_start, skip, agg) = fresh(boundary, last, backend.as_ref());
                        }
                    }
                }
            }
        })?;
    Ok(PresenceSensor {
        tx,
        handle,
        wakeups,
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::config::WriterSettings;
    use crate::storage;
    use crate::writer;
    use rusqlite::Connection;

    /// A scripted backend: constant sample, counts how often it is read.
    struct Fake {
        tier: TelemetryTier,
        category: AppCategory,
        reads: Arc<AtomicU64>,
        input_tx: InputSlot,
    }

    impl Backend for Fake {
        fn tier(&self) -> TelemetryTier {
            self.tier
        }
        fn sample(&mut self) -> Sample {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Sample {
                idle_ms: Some(0),
                category: Some(self.category),
            }
        }
        fn start_input(&mut self, tx: Sender<Msg>) {
            *self.input_tx.lock().unwrap() = Some(tx);
        }
    }

    type InputSlot = Arc<Mutex<Option<Sender<Msg>>>>;

    fn fake(tier: TelemetryTier) -> (Fake, Arc<AtomicU64>, InputSlot) {
        let reads = Arc::new(AtomicU64::new(0));
        let input_tx = Arc::new(Mutex::new(None));
        (
            Fake {
                tier,
                category: AppCategory::Ide,
                reads: Arc::clone(&reads),
                input_tx: Arc::clone(&input_tx),
            },
            reads,
            input_tx,
        )
    }

    fn open() -> (
        tempfile::TempDir,
        Connection,
        writer::WriterSender,
        writer::WriterThread,
    ) {
        let dir = tempfile::tempdir().unwrap();
        let writer_conn = storage::open(dir.path()).unwrap();
        let (sender, thread) = writer::spawn(
            writer_conn,
            WriterSettings {
                commit_interval: Duration::from_millis(20),
                ..WriterSettings::default()
            },
        )
        .unwrap();
        let reader = Connection::open(dir.path().join(storage::DB_FILE)).unwrap();
        (dir, reader, sender, thread)
    }

    fn minutes(db: &Connection) -> Vec<(u64, TelemetryTier)> {
        let mut stmt = db
            .prepare("SELECT minute_start_ms, telemetry_tier FROM presence_minute ORDER BY 1")
            .unwrap();
        stmt.query_map([], |row| {
            let start: i64 = row.get(0)?;
            let tier: String = row.get(1)?;
            Ok((
                u64::try_from(start).unwrap(),
                serde_json::from_value(serde_json::Value::String(tier)).unwrap(),
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    #[test]
    fn one_row_per_wall_clock_minute_and_no_busy_polling() {
        let (dir, db, sender, writer_thread) = open();
        let (backend, reads, _) = fake(TelemetryTier::Reduced);
        let slot = Duration::from_millis(40);
        let started = now_ms();
        let sensor = spawn(Box::new(backend), sender, slot).unwrap();
        // About 3.5 minutes of 240 ms.
        thread::sleep(slot * 6 * 3 + slot * 3);
        let wakeups = sensor.wakeups();
        let elapsed_slots = (now_ms() - started) / 40;
        sensor.stop().unwrap();
        writer_thread.join().unwrap();

        // No input: at most one wakeup per sub-slot (plus one for the
        // boundary the test started inside).
        assert!(wakeups <= elapsed_slots + 1, "{wakeups} > {elapsed_slots}");
        assert!(reads.load(Ordering::Relaxed) <= elapsed_slots + 1);

        let rows = minutes(&db);
        assert!(
            (2..=4).contains(&rows.len()),
            "expected 2 to 4 minutes, got {rows:?}"
        );
        for pair in rows.windows(2) {
            assert_eq!(pair[1].0 - pair[0].0, 240, "contiguous wall-clock minutes");
        }
        for (start, tier) in &rows {
            assert_eq!(start % 240, 0);
            assert_eq!(*tier, TelemetryTier::Reduced);
        }
        drop(dir);
    }

    #[test]
    fn a_full_tier_minute_carries_the_input_counts_and_injection() {
        let (dir, db, sender, writer_thread) = open();
        let (backend, _, input_tx) = fake(TelemetryTier::Full);
        let slot = Duration::from_millis(50);
        let sensor = spawn(Box::new(backend), sender, slot).unwrap();
        let tx = input_tx.lock().unwrap().clone().unwrap();
        // Send input for ~2.5 minutes of 300 ms, alternating real/injected.
        let end = now_ms() + 750;
        while now_ms() < end {
            let t_ms = now_ms();
            let _ = tx.send(Msg::Input {
                t_ms,
                kind: InputKind::Key,
                injected: false,
            });
            let _ = tx.send(Msg::Input {
                t_ms,
                kind: InputKind::Pointer,
                injected: true,
            });
            thread::sleep(Duration::from_millis(5));
        }
        sensor.stop().unwrap();
        writer_thread.join().unwrap();
        // The newest complete minute.
        let (keys, pointers, injected, active): (
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        ) = db
            .query_row(
                "SELECT key_events, pointer_events, injected_events, active_subslots
                 FROM presence_minute ORDER BY minute_start_ms DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert!(keys.unwrap() > 0);
        assert_eq!(pointers, Some(0));
        assert!(injected.unwrap() > 0);
        assert!(active.unwrap() >= 5);
        drop(dir);
    }

    #[test]
    fn a_sensor_started_mid_minute_drops_the_partial_first_minute() {
        // 6 slots of 200 ms; start 3 slots into a minute.
        let (dir, db, sender, writer_thread) = open();
        let (backend, _, _) = fake(TelemetryTier::Reduced);
        let slot = Duration::from_millis(200);
        let minute = 1200;
        let into = now_ms() % minute;
        let wait = if into < 600 {
            600 - into
        } else {
            minute - into + 600
        };
        thread::sleep(Duration::from_millis(wait));
        let first_minute_start = now_ms() / minute * minute;
        let sensor = spawn(Box::new(backend), sender, slot).unwrap();
        thread::sleep(Duration::from_millis(minute + 300));
        sensor.stop().unwrap();
        writer_thread.join().unwrap();
        let rows = minutes(&db);
        assert!(
            rows.iter().all(|(start, _)| *start > first_minute_start),
            "{rows:?} vs {first_minute_start}"
        );
        drop(dir);
    }
}
