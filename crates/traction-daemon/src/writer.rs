//! The single SQLite writer: one dedicated thread owns the only read-write
//! connection and receives validated rows over a bounded channel.
//!
//! A batch is committed when it is `commit_interval` old or holds `max_batch`
//! rows, whichever comes first. When every [`WriterSender`] has been dropped,
//! the thread drains the channel, commits and exits; [`WriterThread::join`]
//! waits for that, so a graceful shutdown loses nothing already accepted.

use std::io;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender, TrySendError};
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

pub struct WriterThread(JoinHandle<()>);

impl WriterThread {
    /// Waits until all senders are gone and the final batch is committed.
    pub fn join(self) -> thread::Result<()> {
        self.0.join()
    }
}

pub fn spawn(
    conn: Connection,
    settings: WriterSettings,
) -> io::Result<(WriterSender, WriterThread)> {
    let (sender, receiver) = mpsc::sync_channel(settings.queue_capacity.max(1));
    let thread = thread::Builder::new()
        .name("traction-sqlite-writer".to_owned())
        .spawn(move || run(conn, receiver, settings))?;
    Ok((WriterSender(sender), WriterThread(thread)))
}

fn run(mut conn: Connection, receiver: mpsc::Receiver<Box<EventRow>>, settings: WriterSettings) {
    let max_batch = settings.max_batch.max(1);
    let mut batch: Vec<EventRow> = Vec::new();
    let mut deadline: Option<Instant> = None;
    loop {
        let received = match deadline {
            None => receiver.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(at) => receiver.recv_timeout(at.saturating_duration_since(Instant::now())),
        };
        match received {
            Ok(row) => {
                if batch.is_empty() {
                    deadline = Some(Instant::now() + settings.commit_interval);
                }
                batch.push(*row);
                if batch.len() >= max_batch {
                    commit(&mut conn, &mut batch, &mut deadline);
                }
            }
            Err(RecvTimeoutError::Timeout) => commit(&mut conn, &mut batch, &mut deadline),
            Err(RecvTimeoutError::Disconnected) => {
                commit(&mut conn, &mut batch, &mut deadline);
                return;
            }
        }
    }
}

fn commit(conn: &mut Connection, batch: &mut Vec<EventRow>, deadline: &mut Option<Instant>) {
    if !batch.is_empty()
        && let Err(error) = storage::insert_batch(conn, batch)
    {
        log::error("failed to commit a batch of Traction Events", &error);
    }
    batch.clear();
    *deadline = None;
}
