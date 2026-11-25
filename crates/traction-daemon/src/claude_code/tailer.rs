//! The Claude Code transcript tailer: reads
//! `<claude config dir>/projects/<slug>/<session-id>.jsonl` as Claude Code
//! appends to it and hands the mapped events to the Router.
//!
//! - **Watch.** `projects/` is watched recursively with OS notifications
//!   (FSEvents, inotify, `ReadDirectoryChangesW`); the thread blocks with no
//!   timeout between them, so there is no idle polling. Until `projects/`
//!   exists, its nearest existing ancestor is watched (not recursively)
//!   until it appears. Only files exactly one folder below `projects/` are
//!   transcripts: subagent transcripts (`<session>/subagents/*.jsonl`) are
//!   never read (`SubagentStart`/`SubagentStop` hooks cover subagents).
//! - **Resume.** Each file is tracked by path, file identity (inode +
//!   device on Unix, volume serial + file index on Windows) and the byte
//!   offset just past its last complete line, persisted in
//!   `transcript_offset` in the same writer transaction as the events read
//!   up to it. A changed identity or a file shorter than its offset is
//!   reread from 0 (deterministic event IDs absorb the repeats).
//! - **First sight.** At the tailer's first-ever start (recorded once),
//!   every existing transcript starts at its end; any file seen later with
//!   no stored offset was created after that and starts at 0.
//! - **Lines.** Only complete lines (up to the last `\n`) are read. A
//!   malformed line is skipped and logged by reason code only; a line
//!   longer than [`MAX_LINE_BYTES`] is skipped without being buffered.
//!
//! Windows opens transcripts with `FILE_SHARE_READ | FILE_SHARE_WRITE |
//! FILE_SHARE_DELETE`, so Claude Code can keep appending, renaming or
//! deleting them while they are read. Transcript text never reaches a log
//! line, and paths only reach SQLite as `transcript_offset.path`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, Metadata};
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};
use std::time::{SystemTime, UNIX_EPOCH};

use notify::event::EventKind;
use notify::{Config, EventKindMask, RecommendedWatcher, RecursiveMode, Watcher as _};
use traction_core::TractionEvent;

use super::transcript::{self, LineError, ToolNames};
use crate::log;
use crate::paths;
use crate::router::RouterSender;
use crate::storage::{TailerStartRow, TailerState, TranscriptOffset};

/// The folder under the Claude config folder that holds transcripts.
pub const PROJECTS_DIR: &str = "projects";
/// Lines longer than this are skipped (never buffered whole).
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// Events handed to the Router per batch (each batch commits its offset).
const MAX_BATCH_EVENTS: usize = 256;
/// Notifications queued before new ones are dropped; a drop schedules a
/// full rescan, so nothing is missed.
const MAX_QUEUED_NOTIFICATIONS: usize = 4096;
/// Sessions remembered as announced (`SESSION_START` sent) before the set
/// is cleared; the Router drops repeats anyway.
const MAX_ANNOUNCED_SESSIONS: usize = 65_536;

enum Message {
    Changed(Vec<PathBuf>),
    Stop,
}

/// The tailer thread. [`TailerThread::stop`] joins it.
pub struct TailerThread {
    tx: Sender<Message>,
    handle: JoinHandle<()>,
}

impl TailerThread {
    pub fn stop(self) -> thread::Result<()> {
        let _ = self.tx.send(Message::Stop);
        self.handle.join()
    }
}

/// One transcript's read position.
struct FileState {
    identity: String,
    offset: u64,
    tools: ToolNames,
}

impl FileState {
    fn new(identity: String, offset: u64) -> Self {
        Self {
            identity,
            offset,
            tools: ToolNames::default(),
        }
    }
}

/// Starts the tailer on `claude_dir`. Before returning it starts the
/// watch and, on the first-ever start, pins every existing transcript at
/// its end, so anything appended after `start` returns is read. Reading
/// (including catching up on stored files) happens on the tailer thread.
pub fn start(
    claude_dir: &Path,
    state: TailerState,
    router: RouterSender,
) -> io::Result<TailerThread> {
    let (tx, rx) = mpsc::channel();
    let queued = Arc::new(AtomicUsize::new(0));
    let overflowed = Arc::new(AtomicBool::new(false));
    let backend = start_backend(tx.clone(), Arc::clone(&queued), Arc::clone(&overflowed));
    let claude_dir = paths::canonical_or_lexical(claude_dir);
    let mut worker = Worker {
        projects: claude_dir.join(PROJECTS_DIR),
        claude_dir,
        backend,
        projects_watched: false,
        ancestor_watched: None,
        router,
        files: HashMap::new(),
        announced: HashSet::new(),
        queued,
        overflowed,
        malformed: 0,
        router_gone: false,
    };
    worker.watch();
    for stored in state.offsets {
        let Ok(offset) = u64::try_from(stored.offset) else {
            continue;
        };
        worker
            .files
            .insert(stored.path, FileState::new(stored.file_identity, offset));
    }
    if state.started_ms.is_none() {
        worker.pin_existing_at_end();
    }
    let handle = thread::Builder::new()
        .name("traction-claude-tailer".to_owned())
        .spawn(move || worker.run(&rx))?;
    Ok(TailerThread { tx, handle })
}

fn start_backend(
    tx: Sender<Message>,
    queued: Arc<AtomicUsize>,
    overflowed: Arc<AtomicBool>,
) -> Option<RecommendedWatcher> {
    let handler = move |result: notify::Result<notify::Event>| {
        // Backend errors carry paths; they are never logged.
        let Ok(event) = result else {
            overflowed.store(true, Ordering::Release);
            return;
        };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if queued.fetch_add(1, Ordering::AcqRel) >= MAX_QUEUED_NOTIFICATIONS {
            queued.fetch_sub(1, Ordering::AcqRel);
            overflowed.store(true, Ordering::Release);
            return;
        }
        if tx.send(Message::Changed(event.paths)).is_err() {
            queued.fetch_sub(1, Ordering::AcqRel);
        }
    };
    let config = Config::default().with_event_kinds(
        EventKindMask::CREATE
            | EventKindMask::REMOVE
            | EventKindMask::MODIFY_DATA
            | EventKindMask::MODIFY_NAME,
    );
    match RecommendedWatcher::new(handler, config) {
        Ok(backend) => Some(backend),
        Err(_) => {
            log::warn("the OS file watcher could not start; Claude transcripts are not tailed");
            None
        }
    }
}

struct Worker {
    /// Canonical Claude config folder.
    claude_dir: PathBuf,
    /// `<claude_dir>/projects`.
    projects: PathBuf,
    backend: Option<RecommendedWatcher>,
    projects_watched: bool,
    /// The existing ancestor watched while `projects/` does not exist.
    ancestor_watched: Option<PathBuf>,
    router: RouterSender,
    /// Read positions by transcript path.
    files: HashMap<String, FileState>,
    /// Sessions whose `SESSION_START` was sent this run.
    announced: HashSet<String>,
    queued: Arc<AtomicUsize>,
    overflowed: Arc<AtomicBool>,
    malformed: u64,
    router_gone: bool,
}

impl Worker {
    fn run(mut self, rx: &Receiver<Message>) {
        self.rescan();
        // Blocks with no timeout: no idle polling.
        while let Ok(Message::Changed(changed)) = rx.recv() {
            self.queued.fetch_sub(1, Ordering::AcqRel);
            if self.overflowed.swap(false, Ordering::AcqRel) {
                self.watch();
                self.rescan();
            } else {
                for path in changed {
                    self.changed(&path);
                }
            }
            if self.router_gone {
                break;
            }
        }
    }

    /// Watches `projects/` recursively once it exists, else its nearest
    /// existing ancestor (not recursively) for the next folder down to
    /// appear.
    fn watch(&mut self) {
        if self.projects_watched {
            return;
        }
        let Some(backend) = self.backend.as_mut() else {
            return;
        };
        if self.projects.is_dir() {
            if let Some(ancestor) = self.ancestor_watched.take() {
                let _ = backend.unwatch(&ancestor);
            }
            if backend
                .watch(&self.projects, RecursiveMode::Recursive)
                .is_ok()
            {
                self.projects_watched = true;
            } else {
                log::warn("cannot watch the Claude projects folder; transcripts are not tailed");
            }
            return;
        }
        let Some(ancestor) = self.claude_dir.ancestors().find(|dir| dir.is_dir()) else {
            return;
        };
        if self.ancestor_watched.as_deref() == Some(ancestor) {
            return;
        }
        if let Some(previous) = self.ancestor_watched.take() {
            let _ = backend.unwatch(&previous);
        }
        if backend
            .watch(ancestor, RecursiveMode::NonRecursive)
            .is_ok()
        {
            self.ancestor_watched = Some(ancestor.to_path_buf());
        }
    }

    /// Every transcript currently on disk.
    fn transcripts(&self) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let Ok(slugs) = fs::read_dir(&self.projects) else {
            return found;
        };
        for slug in slugs.flatten() {
            if slug.file_type().is_ok_and(|t| t.is_dir()) {
                found.extend(transcripts_in(&slug.path()));
            }
        }
        found
    }

    /// First-ever start: every existing transcript starts at its end, and
    /// that start is recorded with those offsets.
    fn pin_existing_at_end(&mut self) {
        let mut offsets = Vec::new();
        for path in self.transcripts() {
            let Some(key) = path_key(&path) else { continue };
            if self.files.contains_key(&key) {
                continue;
            }
            let Ok((_, meta, identity)) = open(&path) else {
                continue;
            };
            offsets.push(offset_row(&key, &identity, meta.len()));
            self.files
                .insert(key, FileState::new(identity, meta.len()));
        }
        let start = TailerStartRow {
            started_ms: now_ms(),
            offsets,
        };
        if self.router.send_tailer_start(start).is_err() {
            self.router_gone = true;
        }
    }

    /// Reads every transcript on disk from its position.
    fn rescan(&mut self) {
        for path in self.transcripts() {
            self.read(&path);
            if self.router_gone {
                return;
            }
        }
    }

    fn changed(&mut self, path: &Path) {
        if !self.projects_watched {
            // Only the config folder is watched: wait for `projects/`.
            self.watch();
            if self.projects_watched {
                self.rescan();
            }
            return;
        }
        let Some(depth) = self.depth_below_projects(path) else {
            return;
        };
        match depth {
            // A new project folder: its transcripts may predate its watch.
            1 if path.is_dir() => {
                for transcript in transcripts_in(path) {
                    self.read(&transcript);
                }
            }
            2 if is_transcript_name(path) => self.read(path),
            _ => {}
        }
    }

    /// How many components `path` lies below `projects/`.
    fn depth_below_projects(&self, path: &Path) -> Option<usize> {
        if let Ok(rest) = path.strip_prefix(&self.projects) {
            return Some(rest.components().count());
        }
        // The backend may report another spelling (a symlinked temp
        // folder): compare canonical parents.
        let parent = paths::canonicalize(path.parent()?)?;
        let rest = parent.strip_prefix(&self.projects).ok()?;
        Some(rest.components().count() + 1)
    }

    /// Reads `path` from its position to the last complete line, handing
    /// the events to the Router in batches, each with its offset.
    fn read(&mut self, path: &Path) {
        let Some(key) = path_key(path) else { return };
        let Ok((file, meta, identity)) = open(path) else {
            return;
        };
        let len = meta.len();
        let mut state = self
            .files
            .remove(&key)
            .unwrap_or_else(|| FileState::new(identity.clone(), 0));
        let mut dirty = false;
        if state.identity != identity || len < state.offset {
            state = FileState::new(identity, 0);
            dirty = true;
        }
        if len > state.offset || dirty {
            self.read_lines(&key, file, len, &mut state, dirty);
        }
        self.files.insert(key, state);
    }

    fn read_lines(&mut self, key: &str, mut file: File, len: u64, state: &mut FileState, dirty: bool) {
        if file.seek(SeekFrom::Start(state.offset)).is_err() {
            return;
        }
        let mut reader = BufReader::with_capacity(64 * 1024, file.take(len - state.offset));
        let mut pending: Vec<TractionEvent> = Vec::new();
        let mut line: Vec<u8> = Vec::new();
        let mut skipping = false;
        let mut position = state.offset;
        let mut committed = state.offset;
        let mut flushed = !dirty;
        loop {
            let buffer = match reader.fill_buf() {
                Ok([]) | Err(_) => break,
                Ok(buffer) => buffer,
            };
            let (chunk, complete) = match buffer.iter().position(|&b| b == b'\n') {
                Some(at) => (&buffer[..at], true),
                None => (buffer, false),
            };
            if !skipping {
                if line.len() + chunk.len() > MAX_LINE_BYTES {
                    skipping = true;
                    line = Vec::new();
                } else {
                    line.extend_from_slice(chunk);
                }
            }
            let used = chunk.len() + usize::from(complete);
            reader.consume(used);
            position += used as u64;
            if !complete {
                continue;
            }
            if skipping {
                self.skipped("skipped an over-long Claude transcript line (reason: line_too_long)");
            } else {
                self.map_line(&line, &mut state.tools, &mut pending);
            }
            line.clear();
            skipping = false;
            committed = position;
            flushed = false;
            if pending.len() >= MAX_BATCH_EVENTS {
                self.flush(key, &state.identity, committed, &mut pending);
                flushed = true;
            }
        }
        if !flushed {
            self.flush(key, &state.identity, committed, &mut pending);
        }
        state.offset = committed;
    }

    fn map_line(&mut self, line: &[u8], tools: &mut ToolNames, pending: &mut Vec<TractionEvent>) {
        if line.iter().all(u8::is_ascii_whitespace) {
            return;
        }
        match transcript::map_line(line, tools) {
            Ok(None) => {}
            Ok(Some(mapped)) => {
                let first = &mapped.events[0];
                let session = first.session_id.clone().unwrap_or_default();
                if !self.announced.contains(&session) {
                    if self.announced.len() >= MAX_ANNOUNCED_SESSIONS {
                        self.announced.clear();
                    }
                    pending.push(transcript::session_start(first, &mapped.record_uuid));
                    self.announced.insert(session);
                }
                pending.extend(mapped.events);
            }
            Err(error) => self.skipped(match error {
                LineError::InvalidJson => {
                    "skipped a malformed Claude transcript line (reason: invalid_json)"
                }
                LineError::UnexpectedShape => {
                    "skipped a malformed Claude transcript line (reason: unexpected_shape)"
                }
                LineError::BadTimestamp => {
                    "skipped a malformed Claude transcript line (reason: bad_timestamp)"
                }
            }),
        }
    }

    fn skipped(&mut self, message: &'static str) {
        self.malformed += 1;
        log::warn_counted(message, self.malformed);
    }

    fn flush(&mut self, key: &str, identity: &str, offset: u64, pending: &mut Vec<TractionEvent>) {
        let events = std::mem::take(pending);
        if self
            .router
            .send_tailed(events, offset_row(key, identity, offset))
            .is_err()
        {
            self.router_gone = true;
        }
    }
}

/// The `*.jsonl` files directly in `folder`.
fn transcripts_in(folder: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(folder) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|t| t.is_file()))
        .map(|entry| entry.path())
        .filter(|path| is_transcript_name(path))
        .collect()
}

fn is_transcript_name(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| paths::name_matches(&ext.to_string_lossy(), "jsonl"))
}

/// The stored key of a transcript path (UTF-8 only).
fn path_key(path: &Path) -> Option<String> {
    path.to_str().map(str::to_owned)
}

fn offset_row(key: &str, identity: &str, offset: u64) -> TranscriptOffset {
    TranscriptOffset {
        path: key.to_owned(),
        file_identity: identity.to_owned(),
        offset: i64::try_from(offset).unwrap_or(i64::MAX),
        updated_ms: now_ms(),
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// Opens a transcript for reading without blocking its writer, with its
/// metadata and identity. Fails for anything that is not a regular file.
fn open(path: &Path) -> io::Result<(File, Metadata, String)> {
    let file = open_shared(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a file"));
    }
    let identity = file_identity(&file, &meta)?;
    Ok((file, meta, identity))
}

#[cfg(not(windows))]
fn open_shared(path: &Path) -> io::Result<File> {
    File::open(path)
}

/// Read access that never blocks Claude Code: it may keep writing,
/// rename or delete the file while it is open.
#[cfg(windows)]
fn open_shared(path: &Path) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    };
    fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(path)
}

/// Inode + device.
#[cfg(unix)]
fn file_identity(_file: &File, meta: &Metadata) -> io::Result<String> {
    use std::os::unix::fs::MetadataExt;
    Ok(format!("{}:{}", meta.dev(), meta.ino()))
}

/// Volume serial number + 64-bit file index.
#[cfg(windows)]
fn file_identity(file: &File, _meta: &Metadata) -> io::Result<String> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };
    // SAFETY: an all-zero BY_HANDLE_FILE_INFORMATION is a valid value of
    // this plain C struct, and the handle is open for the call's duration.
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    let ok = unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(format!(
        "{:08x}:{:08x}{:08x}",
        info.dwVolumeSerialNumber, info.nFileIndexHigh, info.nFileIndexLow
    ))
}
