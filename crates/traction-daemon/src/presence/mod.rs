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
//! part-way is dropped. After a stall (a suspended machine) the minute it
//! interrupted is written with only the sub-slots actually observed, and a
//! minute nothing was observed in has no row, so nothing is written for
//! time the daemon was not watching. Presence is device-wide: tray and session
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

use traction_core::{AppCategory, PresenceMinute, SUBSLOT_SEC, SUBSLOTS_PER_MINUTE, TelemetryTier};

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

/// The sensor's clock-free state machine: sub-slots close on a fixed grid
/// from the minute start, whatever time the caller wakes up at, and a stall
/// closes the sub-slots and minutes it covered instead of dropping them.
struct Tracker {
    slot_ms: u64,
    minute_ms: u64,
    minute_start: u64,
    /// Index (0 to 5) of the next sub-slot to close.
    next_slot: usize,
    /// The daemon joined this minute late: it never writes a row.
    partial: bool,
    agg: Aggregator,
}

impl Tracker {
    fn new(
        slot_ms: u64,
        now: u64,
        previous: Option<AppCategory>,
        backend: &dyn Backend,
    ) -> Self {
        let minute_ms = slot_ms * u64::from(SUBSLOTS_PER_MINUTE);
        let minute_start = now / minute_ms * minute_ms;
        let into = now - minute_start;
        Self {
            slot_ms,
            minute_ms,
            minute_start,
            next_slot: usize::try_from(into / slot_ms).unwrap_or(0),
            // Joined more than a sub-slot late: a partial minute.
            partial: into > slot_ms,
            agg: Aggregator::new(minute_start, slot_ms, backend.tier(), previous),
        }
    }

    /// The end of the next sub-slot to close.
    fn deadline(&self) -> u64 {
        self.minute_start + (self.next_slot as u64 + 1) * self.slot_ms
    }

    /// Input older than the open minute belongs to a minute already closed.
    fn input(&mut self, t_ms: u64, kind: InputKind, injected: bool) {
        if t_ms >= self.minute_start {
            self.agg.input(t_ms, kind, injected);
        }
    }

    /// Closes every sub-slot whose end is at or before `now`. Only a
    /// sub-slot closed within one slot of its end is sampled (the backend
    /// reflects now); one the sensor was too late for stays unobserved, so a
    /// stall adds nothing it did not see. A completed minute with any
    /// observation is pushed to `out`; one nothing was observed in has no
    /// row (the rollup treats it as `none`) and is logged.
    fn tick(&mut self, now: u64, backend: &mut dyn Backend, out: &mut Vec<PresenceMinute>) {
        let mut unobserved_minutes = 0u64;
        while self.deadline() <= now {
            let timely = now - self.deadline() < self.slot_ms;
            let sample = if timely {
                backend.sample()
            } else {
                Sample::default()
            };
            self.agg.slot_end(self.next_slot, sample);
            self.next_slot += 1;
            if self.next_slot == usize::from(SUBSLOTS_PER_MINUTE) {
                let next_start = self.minute_start + self.minute_ms;
                let finished = std::mem::replace(
                    &mut self.agg,
                    Aggregator::new(next_start, self.slot_ms, backend.tier(), None),
                );
                let observed = finished.observed();
                let (minute, last) = finished.finish();
                if !self.partial {
                    if observed {
                        out.push(minute);
                    } else {
                        unobserved_minutes += 1;
                    }
                }
                self.agg = Aggregator::new(next_start, self.slot_ms, backend.tier(), last);
                self.minute_start = next_start;
                self.next_slot = 0;
                self.partial = false;
            }
        }
        if unobserved_minutes > 0 {
            crate::log::warn(
                "presence sensor was stalled; minutes it could not observe have no row",
            );
        }
    }
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
    let handle = thread::Builder::new()
        .name("traction-presence".to_owned())
        .spawn(move || {
            let mut tracker = Tracker::new(slot_ms, now_ms(), None, backend.as_ref());
            let mut ready = Vec::new();
            loop {
                let now = now_ms();
                tracker.tick(now, backend.as_mut(), &mut ready);
                for minute in ready.drain(..) {
                    if writer.send(WriteOp::Presence(minute)).is_err() {
                        return;
                    }
                }
                let deadline = tracker.deadline();
                let wait = deadline.saturating_sub(now_ms()).max(1);
                match rx.recv_timeout(Duration::from_millis(wait)) {
                    Ok(Msg::Input {
                        t_ms,
                        kind,
                        injected,
                    }) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                        tracker.input(t_ms, kind, injected);
                    }
                    Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {
                        counter.fetch_add(1, Ordering::Relaxed);
                        // The timer fired for `deadline`: count it as reached
                        // even if the wall clock reads a hair early.
                        tracker.tick(now_ms().max(deadline), backend.as_mut(), &mut ready);
                        for minute in ready.drain(..) {
                            if writer.send(WriteOp::Presence(minute)).is_err() {
                                return;
                            }
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

    /// Waits (bounded) until `count` presence rows are stored: a positive
    /// barrier instead of a guessed sleep.
    fn wait_for_rows(db: &Connection, count: usize) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while minutes(db).len() < count {
            assert!(std::time::Instant::now() < deadline, "rows never arrived");
            thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn one_row_per_wall_clock_minute_and_no_busy_polling() {
        let (dir, db, sender, writer_thread) = open();
        let (backend, reads, _) = fake(TelemetryTier::Reduced);
        let slot = Duration::from_millis(40);
        let started = now_ms();
        let sensor = spawn(Box::new(backend), sender, slot).unwrap();
        wait_for_rows(&db, 3);
        let wakeups = sensor.wakeups();
        let elapsed_slots = (now_ms() - started) / 40;
        sensor.stop().unwrap();
        writer_thread.join().unwrap();

        // No input: at most one wakeup per sub-slot (plus one for the
        // boundary the test started inside).
        assert!(wakeups <= elapsed_slots + 1, "{wakeups} > {elapsed_slots}");
        assert!(reads.load(Ordering::Relaxed) <= elapsed_slots + 1);

        let rows = minutes(&db);
        for pair in rows.windows(2) {
            assert_eq!(pair[1].0 - pair[0].0, 240, "contiguous wall-clock minutes");
        }
        for (start, tier) in &rows {
            assert_eq!(start % 240, 0);
            assert_eq!(*tier, TelemetryTier::Reduced);
        }
        drop(dir);
    }

    const SLOT: u64 = 10;
    const MINUTE: u64 = 6 * SLOT;
    /// A minute-aligned origin for the injected clock.
    const START: u64 = 1_000_000 * MINUTE;

    fn tracker(now: u64, backend: &Fake) -> Tracker {
        Tracker::new(SLOT, now, None, backend)
    }

    #[test]
    fn sub_slots_close_on_a_fixed_grid_whatever_time_each_wakeup_is_late() {
        let (mut backend, reads, _) = fake(TelemetryTier::Reduced);
        let mut t = tracker(START, &backend);
        let mut out = Vec::new();
        // Every wakeup is 0 to 9 ms late (less than a sub-slot): three
        // minutes still close 18 sub-slots on the grid, none skipped.
        for i in 0..18u64 {
            assert_eq!(t.deadline(), START + (i + 1) * SLOT);
            t.tick(t.deadline() + (i * 7) % 10, &mut backend, &mut out);
        }
        assert_eq!(reads.load(Ordering::Relaxed), 18);
        let starts: Vec<u64> = out.iter().map(|m| m.minute_start_ms).collect();
        assert_eq!(starts, [START, START + MINUTE, START + 2 * MINUTE]);
        for m in &out {
            assert_eq!(m.category_sec.unwrap().ide, 0, "10 ms slots are 0 s");
            assert_eq!(m.active_subslots, Some(6));
        }
    }

    #[test]
    fn a_stall_closes_the_minute_it_interrupted_with_what_was_observed() {
        let (mut backend, reads, _) = fake(TelemetryTier::Reduced);
        let mut t = tracker(START, &backend);
        let mut out = Vec::new();
        t.tick(START + SLOT, &mut backend, &mut out); // slot 0 observed
        assert!(out.is_empty());
        // The machine sleeps through 2.5 minutes, then wakes.
        t.tick(START + 3 * MINUTE + 4 * SLOT + 3, &mut backend, &mut out);
        // The interrupted minute is written, not dropped, with one observed
        // sub-slot; the two minutes nothing was observed in have no row.
        assert_eq!(out.len(), 1, "{out:?}");
        assert_eq!(out[0].minute_start_ms, START);
        assert_eq!(out[0].active_subslots, Some(1));
        assert_eq!(out[0].telemetry_tier, TelemetryTier::Reduced);
        // Only the first slot and the one the wakeup closed were sampled.
        assert_eq!(reads.load(Ordering::Relaxed), 2);
        // The sensor is back on the grid inside the fourth minute.
        assert_eq!(t.deadline(), START + 3 * MINUTE + 5 * SLOT);
        t.tick(START + 4 * MINUTE, &mut backend, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].minute_start_ms, START + 3 * MINUTE);
        assert_eq!(out[1].active_subslots, Some(2));
    }

    #[test]
    fn input_stamped_before_the_open_minute_is_not_counted_in_it() {
        let (mut backend, _, _) = fake(TelemetryTier::Full);
        let mut t = tracker(START, &backend);
        let mut out = Vec::new();
        t.tick(START + MINUTE, &mut backend, &mut out);
        // Delivered late, stamped in the minute that just closed.
        t.input(START + 5, InputKind::Key, false);
        t.input(START + MINUTE + 5, InputKind::Key, false);
        t.tick(START + 2 * MINUTE, &mut backend, &mut out);
        assert_eq!(out[1].key_events, Some(1));
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
        // Every pointer event here was injected, and counts include those.
        assert_eq!(pointers, injected);
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
