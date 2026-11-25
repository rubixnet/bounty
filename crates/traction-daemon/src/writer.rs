//! The single SQLite writer: one dedicated thread owns the only read-write
//! connection and receives [`WriteOp`]s (validated event rows, Project
//! registrations, session upserts) over a bounded channel, applied in order.
//!
//! A batch is committed when it is `commit_interval` old or holds `max_batch`
//! events (registry and session writes ride along and are not counted)
//! whichever comes first. When every [`WriterSender`] has been dropped,
//! the thread drains the channel, commits and exits; [`WriterThread::join`]
//! waits for that, so a graceful shutdown loses nothing already accepted.
//!
//! A failed commit (SQLite busy past its timeout, disk full, I/O error) keeps
//! the batch and retries it `commit_interval` later. Inserts ignore a repeated
//! `event_id` (and registrations a repeated `project_id`), so a retry never
//! double-stores. While a failed batch is full, the writer stops taking
//! writes: the channel fills, the Router blocks and the bus answers `503`,
//! so memory stays bounded by `max_batch` events (each with at most one
//! session and one registration write) plus `queue_capacity`. Only the final
//! commit at shutdown gives up, logging that the batch was dropped.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::config::WriterSettings;
use crate::log;
use crate::storage::{self, WriteOp};

/// The producer side of the channel (the Project Router). Cheap to clone.
#[derive(Clone)]
pub struct WriterSender(SyncSender<WriteOp>);

/// The writer has stopped (shutdown); the write was not taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterGone;

impl WriterSender {
    /// Queues `op`, waiting while the queue is full. Only the Router's own
    /// thread calls this, so waiting never blocks a bus response.
    pub fn send(&self, op: WriteOp) -> Result<(), WriterGone> {
        self.0.send(op).map_err(|_| WriterGone)
    }
}

pub struct WriterThread {
    handle: JoinHandle<()>,
    stopping: Arc<AtomicBool>,
}

impl WriterThread {
    /// Tells a writer stuck retrying a full, failed batch to give up, so a
    /// producer blocked in [`WriterSender::send`] is released at shutdown.
    pub fn request_stop(&self) {
        self.stopping.store(true, Ordering::Release);
        self.handle.thread().unpark();
    }

    /// Waits until the final batch is committed. Call it after every sender
    /// is gone; it also wakes a writer that is waiting to retry a full batch.
    pub fn join(self) -> thread::Result<()> {
        self.request_stop();
        self.handle.join()
    }
}

pub fn spawn(
    conn: Connection,
    settings: WriterSettings,
) -> io::Result<(WriterSender, WriterThread)> {
    let (sender, receiver) = mpsc::sync_channel(settings.queue_capacity.max(1));
    let stopping = Arc::new(AtomicBool::new(false));
    let writer = Writer {
        conn,
        receiver,
        commit_interval: settings.commit_interval,
        max_batch: settings.max_batch.max(1),
        stopping: Arc::clone(&stopping),
        batch: Vec::new(),
        batch_events: 0,
        deadline: None,
    };
    let handle = thread::Builder::new()
        .name("traction-sqlite-writer".to_owned())
        .spawn(move || writer.run())?;
    Ok((WriterSender(sender), WriterThread { handle, stopping }))
}

struct Writer {
    conn: Connection,
    receiver: Receiver<WriteOp>,
    commit_interval: Duration,
    max_batch: usize,
    stopping: Arc<AtomicBool>,
    batch: Vec<WriteOp>,
    /// How many of `batch` are events (the `max_batch` count).
    batch_events: usize,
    /// When the pending batch is committed (or a failed one retried).
    deadline: Option<Instant>,
}

impl Writer {
    fn run(mut self) {
        loop {
            if self.batch_events >= self.max_batch {
                // Only a failed batch stays full: wait for its retry without
                // taking rows, so the bus sees a full queue.
                if self.stopping.load(Ordering::Acquire) {
                    return self.finish();
                }
                if let Some(at) = self.deadline {
                    thread::park_timeout(at.saturating_duration_since(Instant::now()));
                }
                if self.deadline.is_some_and(|at| Instant::now() >= at) {
                    self.commit();
                }
                continue;
            }
            let received = match self.deadline {
                None => self
                    .receiver
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
                Some(at) => self
                    .receiver
                    .recv_timeout(at.saturating_duration_since(Instant::now())),
            };
            match received {
                Ok(op) => {
                    if self.batch.is_empty() {
                        self.deadline = Some(Instant::now() + self.commit_interval);
                    }
                    if matches!(op, WriteOp::Event(_)) {
                        self.batch_events += 1;
                    }
                    self.batch.push(op);
                    if self.batch_events >= self.max_batch {
                        self.commit();
                    }
                }
                Err(RecvTimeoutError::Timeout) => self.commit(),
                Err(RecvTimeoutError::Disconnected) => return self.finish(),
            }
        }
    }

    /// Commits the batch. On failure keeps it and schedules a retry.
    fn commit(&mut self) {
        if self.batch.is_empty() {
            self.deadline = None;
            return;
        }
        match storage::apply_batch(&mut self.conn, &self.batch) {
            Ok(()) => {
                self.batch.clear();
                self.batch_events = 0;
                self.deadline = None;
            }
            Err(error) => {
                log::error("failed to commit a batch of writes, will retry", &error);
                self.deadline = Some(Instant::now() + self.commit_interval);
            }
        }
    }

    /// Shutdown: every sender is gone. Takes what is left in the channel and
    /// makes one last commit attempt.
    fn finish(mut self) {
        self.batch.extend(self.receiver.try_iter());
        if !self.batch.is_empty()
            && let Err(error) = storage::apply_batch(&mut self.conn, &self.batch)
        {
            log::error(
                "failed to commit writes at shutdown, they were dropped",
                &error,
            );
        }
    }
}
