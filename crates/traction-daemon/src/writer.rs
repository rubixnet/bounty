//! The single SQLite writer: one dedicated thread owns the only read-write
//! connection and receives validated rows over a bounded channel.
//!
//! A batch is committed when it is `commit_interval` old or holds `max_batch`
//! rows, whichever comes first. When every [`WriterSender`] has been dropped,
//! the thread drains the channel, commits and exits; [`WriterThread::join`]
//! waits for that, so a graceful shutdown loses nothing already accepted.
//!
//! A failed commit (SQLite busy past its timeout, disk full, I/O error) keeps
//! the batch and retries it `commit_interval` later. Inserts ignore a repeated
//! `event_id`, so a retry never double-stores. While a failed batch is full,
//! the writer stops taking rows: the channel fills and the bus answers `503`,
//! so memory stays bounded by `max_batch + queue_capacity`. Only the final
//! commit at shutdown gives up, logging that the batch was dropped.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::log;
use crate::storage::{self, EventRow};

/// Writer tuning, taken from `DaemonConfig`.
#[derive(Debug, Clone, Copy)]
pub struct WriterSettings {
    pub commit_interval: Duration,
    pub max_batch: usize,
    pub queue_capacity: usize,
}

/// The bus side of the channel. Cheap to clone; never blocks.
#[derive(Clone)]
pub struct WriterSender(SyncSender<Box<EventRow>>);

/// The writer cannot take the row right now (queue full or shutting down).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriterBusy;

impl WriterSender {
    pub fn try_enqueue(&self, row: EventRow) -> Result<(), WriterBusy> {
        self.0.try_send(Box::new(row)).map_err(|error| match error {
            TrySendError::Full(_) | TrySendError::Disconnected(_) => WriterBusy,
        })
    }
}

pub struct WriterThread {
    handle: JoinHandle<()>,
    stopping: Arc<AtomicBool>,
}

impl WriterThread {
    /// Waits until the final batch is committed. Call it after every sender
    /// is gone; it also wakes a writer that is waiting to retry a full batch.
    pub fn join(self) -> thread::Result<()> {
        self.stopping.store(true, Ordering::Release);
        self.handle.thread().unpark();
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
        deadline: None,
    };
    let handle = thread::Builder::new()
        .name("traction-sqlite-writer".to_owned())
        .spawn(move || writer.run())?;
    Ok((WriterSender(sender), WriterThread { handle, stopping }))
}

struct Writer {
    conn: Connection,
    receiver: Receiver<Box<EventRow>>,
    commit_interval: Duration,
    max_batch: usize,
    stopping: Arc<AtomicBool>,
    batch: Vec<EventRow>,
    /// When the pending batch is committed (or a failed one retried).
    deadline: Option<Instant>,
}

impl Writer {
    fn run(mut self) {
        loop {
            if self.batch.len() >= self.max_batch {
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
                Ok(row) => {
                    if self.batch.is_empty() {
                        self.deadline = Some(Instant::now() + self.commit_interval);
                    }
                    self.batch.push(*row);
                    if self.batch.len() >= self.max_batch {
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
        match storage::insert_batch(&mut self.conn, &self.batch) {
            Ok(()) => {
                self.batch.clear();
                self.deadline = None;
            }
            Err(error) => {
                log::error(
                    "failed to commit a batch of Traction Events, will retry",
                    &error,
                );
                self.deadline = Some(Instant::now() + self.commit_interval);
            }
        }
    }

    /// Shutdown: every sender is gone. Takes what is left in the channel and
    /// makes one last commit attempt.
    fn finish(mut self) {
        self.batch.extend(self.receiver.try_iter().map(|row| *row));
        if !self.batch.is_empty()
            && let Err(error) = storage::insert_batch(&mut self.conn, &self.batch)
        {
            log::error(
                "failed to commit Traction Events at shutdown, they were dropped",
                &error,
            );
        }
    }
}
