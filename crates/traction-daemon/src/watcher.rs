//! The file watcher: live, metadata-only `FILE_MUTATION`s for every watched
//! Project.
//!
//! - **Attach.** [`Watches::attach`] runs synchronously (on the daemon's
//!   start-up thread for the Projects stored in SQLite, then on the Router
//!   thread through its new-Project hook), so a Project is watched before
//!   its registration is answered. macOS (FSEvents) and Windows
//!   (`ReadDirectoryChangesW`) watch each Project Root recursively; Linux
//!   (inotify) adds one non-recursive watch per folder, by a walk that prunes
//!   excluded and ignored folders, and extends it when folders appear. A full
//!   OS watch table stops adding watches, logged once; it never panics. There
//!   is no polling backend.
//! - **Filter.** Only file events count: never folders, metadata-only
//!   changes or reads. Paths with a [`DEPENDENCY_FOLDERS`] component (which
//!   includes `.git`), inside a user-excluded folder, or matched by a
//!   `.gitignore`/`.tractionignore` (gitignore semantics, any level inside the
//!   Project) are dropped.
//! - **Hold.** Each observed mutation is held for the configured hold
//!   ([`crate::config::LOG_TAIL_HOLD`] in production), keyed by file. Repeats
//!   within the hold collapse into one; the kind is the last one seen, except
//!   that a file first seen `created` stays `created`, and `created` followed
//!   by `removed` is dropped. The kind is checked against the file's
//!   existence when decided. Past `pending_cap` held files in one Project,
//!   its oldest is decided early.
//! - **Decide.** At the end of the hold the mutation is built as manual work
//!   and handed to the Router ([`RouterSender::send_mutation`]), which owns
//!   the attribution decision and the validated write path. Shutdown decides
//!   and flushes everything still held.
//!
//! Live only: there is no start-up scan and no replay; only notifications
//! received while the daemon runs become events. File paths and names stay
//! in memory and never reach an event, a log line or SQLite.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use notify::event::{CreateKind, EventKind, ModifyKind, RemoveKind};
use notify::{Config, EventKindMask, RecommendedWatcher, RecursiveMode, Watcher as _};
use traction_core::{
    AttributionSource, EventType, MetricsOnly, MutationKind, NetworkClass, SCHEMA_VERSION,
    TractionEvent,
};

use crate::config::WatcherSettings;
use crate::event_id::fresh_event_id;
use crate::exclusions::{DEPENDENCY_FOLDERS, Exclusions};
use crate::log;
use crate::paths::{self, is_within, name_matches};
use crate::router::{RegisteredProject, RouterSender};

/// Per-folder ignore files, highest precedence first.
pub const IGNORE_FILES: [&str; 2] = [".tractionignore", ".gitignore"];
/// Notifications queued for the watcher thread before new ones are dropped
/// (an `npm install` storm must not grow memory without bound).
const MAX_QUEUED_NOTIFICATIONS: usize = 65_536;
/// Parsed ignore files kept at once (cleared when full).
const IGNORE_CACHE_CAPACITY: usize = 4096;
/// macOS: files remembered (as path hashes) per Project in the existence
/// baseline; past this, new files are no longer remembered.
#[cfg(target_os = "macos")]
const MAX_BASELINE_FILES: usize = 1_000_000;

/// The watched Projects and the OS watch backend, shared by the attach
/// callers and the watcher thread. Cheap to clone.
#[derive(Clone)]
pub struct Watches(Arc<Mutex<WatchTable>>);

struct WatchTable {
    /// `None` when the OS backend could not start (logged once).
    backend: Option<RecommendedWatcher>,
    roots: Vec<WatchedRoot>,
    exclusions: Exclusions,
    ignores: IgnoreCache,
    limit_logged: bool,
    missing_logged: bool,
    failure_logged: bool,
}

struct WatchedRoot {
    project_id: String,
    /// Canonical Project Root.
    root: PathBuf,
    /// When the watch started (macOS replay filter).
    #[cfg(target_os = "macos")]
    attached_at: SystemTime,
    /// Folders with an inotify watch (Linux), for detaching.
    #[cfg(target_os = "linux")]
    folders: Vec<PathBuf>,
    /// The OS watch limit was hit while adding this Project's folders.
    #[cfg(target_os = "linux")]
    full: bool,
    /// macOS: hashes of the paths of files known to exist (seeded when
    /// attached, updated by every notification). FSEvents flags carry a
    /// file's recent history (a removal can arrive flagged "created,
    /// modified, removed"), so the kind comes from this and the file's
    /// existence instead. Only ever compared, never reported.
    #[cfg(target_os = "macos")]
    known: std::collections::HashSet<u64>,
}

enum Message {
    Notify {
        at: SystemTime,
        observed: Instant,
        event: notify::Event,
    },
    Stop,
}

/// The receiving end of the backend's notifications, handed to [`spawn`].
pub struct Notifications {
    rx: Receiver<Message>,
    tx: Sender<Message>,
    queued: Arc<AtomicUsize>,
}

impl Watches {
    /// Starts the OS backend (no Project watched yet). If it cannot start,
    /// the daemon runs without file watching, logged once.
    pub fn new(exclusions: Exclusions) -> (Self, Notifications) {
        raise_open_file_limit();
        let (tx, rx) = mpsc::channel();
        let queued = Arc::new(AtomicUsize::new(0));
        let backend = start_backend(tx.clone(), Arc::clone(&queued));
        let table = WatchTable {
            backend,
            roots: Vec::new(),
            exclusions,
            ignores: IgnoreCache::default(),
            limit_logged: false,
            missing_logged: false,
            failure_logged: false,
        };
        (
            Self(Arc::new(Mutex::new(table))),
            Notifications { rx, tx, queued },
        )
    }

    /// Starts watching `project`. A root already watched for another
    /// Project is re-pointed to this one (a migrated Project keeps its
    /// folder). Missing, excluded or user-excluded roots are skipped.
    pub fn attach(&self, project: &RegisteredProject) {
        self.lock().attach(project);
    }

    /// The Project `project_id` was merged into `into` (a migration). Its
    /// watch is handed over in place, re-keyed to `into` without being
    /// stopped or restarted, so no in-flight notification is lost; the
    /// `into` Project's own attach then finds it. When `into` is already
    /// watched, the old watch is simply stopped.
    pub fn merge(&self, project_id: &str, into: &str) {
        self.lock().merge(project_id, into);
    }

    fn lock(&self) -> MutexGuard<'_, WatchTable> {
        // A panic while holding the lock leaves plain data behind; keep going.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn start_backend(tx: Sender<Message>, queued: Arc<AtomicUsize>) -> Option<RecommendedWatcher> {
    let dropped_logged = AtomicBool::new(false);
    let handler = move |result: notify::Result<notify::Event>| {
        // Backend errors carry paths; they are never logged.
        let Ok(event) = result else { return };
        if matches!(event.kind, EventKind::Access(_)) {
            return;
        }
        if queued.fetch_add(1, Ordering::AcqRel) >= MAX_QUEUED_NOTIFICATIONS {
            queued.fetch_sub(1, Ordering::AcqRel);
            if !dropped_logged.swap(true, Ordering::AcqRel) {
                log::warn("the file watcher is overloaded; some file changes were not recorded");
            }
            return;
        }
        let message = Message::Notify {
            at: SystemTime::now(),
            observed: Instant::now(),
            event,
        };
        if tx.send(message).is_err() {
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
            log::warn("the OS file watcher could not start; file changes are not recorded");
            None
        }
    }
}

impl WatchTable {
    fn attach(&mut self, project: &RegisteredProject) {
        if let Some(watched) = self
            .roots
            .iter_mut()
            .find(|w| paths::same(&w.root, &project.root))
        {
            watched.project_id.clone_from(&project.project_id);
            return;
        }
        // Already watched under this ID elsewhere: a merged `dir:` root
        // inside this new Root gives way once the Root is watched.
        let replaced = self
            .roots
            .iter()
            .position(|w| w.project_id == project.project_id);
        if let Some(index) = replaced
            && !is_within(&self.roots[index].root, &project.root)
        {
            return;
        }
        if !project.root.is_dir() {
            if !self.missing_logged {
                self.missing_logged = true;
                log::warn("a registered Project folder is missing; it is not watched");
            }
            return;
        }
        if self.exclusions.is_excluded(&project.root)
            || self.exclusions.is_user_excluded(&project.root)
        {
            return;
        }
        let mut watched = WatchedRoot {
            project_id: project.project_id.clone(),
            root: project.root.clone(),
            #[cfg(target_os = "macos")]
            attached_at: SystemTime::now(),
            #[cfg(target_os = "linux")]
            folders: Vec::new(),
            #[cfg(target_os = "linux")]
            full: false,
            #[cfg(target_os = "macos")]
            known: std::collections::HashSet::new(),
        };
        self.watch_root(&mut watched);
        self.roots.push(watched);
        if let Some(index) = replaced {
            self.detach_at(index);
        }
    }

    fn merge(&mut self, project_id: &str, into: &str) {
        let Some(index) = self.roots.iter().position(|w| w.project_id == project_id) else {
            return;
        };
        if self.roots.iter().any(|w| w.project_id == into) {
            self.detach_at(index);
        } else {
            self.roots[index].project_id = into.to_owned();
        }
    }

    fn detach_at(&mut self, index: usize) {
        let watched = self.roots.swap_remove(index);
        let Some(backend) = self.backend.as_mut() else {
            return;
        };
        #[cfg(target_os = "linux")]
        for folder in &watched.folders {
            let _ = backend.unwatch(folder);
        }
        #[cfg(not(target_os = "linux"))]
        let _ = backend.unwatch(&watched.root);
    }

    #[cfg(not(target_os = "linux"))]
    fn watch_root(&mut self, watched: &mut WatchedRoot) {
        let Some(backend) = self.backend.as_mut() else {
            return;
        };
        match backend.watch(&watched.root, RecursiveMode::Recursive) {
            Err(error) => self.watch_failed(&error),
            #[cfg(target_os = "macos")]
            Ok(()) => {
                let root = watched.root.clone();
                self.walk_tree(watched, &root, None);
            }
            #[cfg(not(target_os = "macos"))]
            Ok(()) => {}
        }
    }

    #[cfg(target_os = "linux")]
    fn watch_root(&mut self, watched: &mut WatchedRoot) {
        let root = watched.root.clone();
        self.walk_tree(watched, &root, None);
    }

    /// Walks `top` and every folder below it that is not pruned. Linux
    /// adds one non-recursive inotify watch per folder; macOS records each
    /// file in the existence baseline. Files found are pushed to `found` (a
    /// folder that just appeared: its files are new).
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn walk_tree(
        &mut self,
        watched: &mut WatchedRoot,
        top: &Path,
        mut found: Option<&mut Vec<PathBuf>>,
    ) {
        let mut stack = vec![top.to_path_buf()];
        while let Some(folder) = stack.pop() {
            #[cfg(target_os = "linux")]
            {
                if watched.full {
                    return;
                }
                let Some(backend) = self.backend.as_mut() else {
                    return;
                };
                match backend.watch(&folder, RecursiveMode::NonRecursive) {
                    Ok(()) => watched.folders.push(folder.clone()),
                    Err(error) => {
                        if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) {
                            watched.full = true;
                        }
                        self.watch_failed(&error);
                        continue;
                    }
                }
            }
            let Ok(entries) = fs::read_dir(&folder) else {
                continue;
            };
            for entry in entries.flatten() {
                let Ok(kind) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if kind.is_dir() {
                    if !self.is_filtered(&watched.root, &path, true) {
                        stack.push(path);
                    }
                    continue;
                }
                #[cfg(target_os = "macos")]
                if watched.known.len() < MAX_BASELINE_FILES {
                    watched.known.insert(path_key(&path));
                }
                if let Some(found) = found.as_deref_mut()
                    && !self.is_filtered(&watched.root, &path, false)
                {
                    found.push(path);
                }
            }
        }
    }

    fn watch_failed(&mut self, error: &notify::Error) {
        if matches!(error.kind, notify::ErrorKind::MaxFilesWatch) {
            if !self.limit_logged {
                self.limit_logged = true;
                log::warn(
                    "the OS file watch limit is reached; some Project folders are not watched",
                );
            }
        } else if !self.failure_logged {
            self.failure_logged = true;
            log::warn("a Project folder could not be watched");
        }
    }

    /// The watched Project holding `path` (the deepest root), if any.
    fn root_of(&self, path: &Path) -> Option<usize> {
        self.roots
            .iter()
            .enumerate()
            .filter(|(_, w)| is_within(path, &w.root))
            .max_by_key(|(_, w)| w.root.components().count())
            .map(|(index, _)| index)
    }

    /// Whether `path` (below `root`) is never reported: a dependency or
    /// `.git` component, a user exclusion, or an ignore-file match.
    fn is_filtered(&mut self, root: &Path, path: &Path, is_dir: bool) -> bool {
        let names: Vec<&str> = paths::components_below(path, root)
            .filter_map(|c| c.as_os_str().to_str())
            .collect();
        if names.is_empty()
            || names
                .iter()
                .any(|name| DEPENDENCY_FOLDERS.iter().any(|d| name_matches(name, d)))
        {
            return true;
        }
        if self.exclusions.is_user_excluded(path) {
            return true;
        }
        self.ignores.is_ignored(root, &names, is_dir)
    }
}

/// Parsed `.gitignore`/`.tractionignore` files, by file path.
#[derive(Default)]
struct IgnoreCache {
    files: HashMap<PathBuf, Option<Gitignore>>,
}

impl IgnoreCache {
    /// Gitignore semantics: the deepest folder's files decide first, and
    /// within a folder `.tractionignore` beats `.gitignore`.
    fn is_ignored(&mut self, root: &Path, names: &[&str], is_dir: bool) -> bool {
        let mut folders = Vec::with_capacity(names.len());
        let mut folder = root.to_path_buf();
        folders.push(folder.clone());
        for name in &names[..names.len() - 1] {
            folder.push(name);
            folders.push(folder.clone());
        }
        let path = folder.join(names[names.len() - 1]);
        for folder in folders.iter().rev() {
            for file in IGNORE_FILES {
                let Some(matcher) = self.matcher(&folder.join(file), folder) else {
                    continue;
                };
                let decision = matcher.matched_path_or_any_parents(&path, is_dir);
                if decision.is_ignore() {
                    return true;
                }
                if decision.is_whitelist() {
                    return false;
                }
            }
        }
        false
    }

    fn matcher(&mut self, file: &Path, folder: &Path) -> Option<&Gitignore> {
        if !self.files.contains_key(file) {
            if self.files.len() >= IGNORE_CACHE_CAPACITY {
                self.files.clear();
            }
            let parsed = file.is_file().then(|| {
                let mut builder = GitignoreBuilder::new(folder);
                builder
                    .case_insensitive(cfg!(any(windows, target_os = "macos")))
                    .ok();
                // A partly invalid file still applies its valid lines.
                let _ = builder.add(file);
                builder.build().unwrap_or_else(|_| Gitignore::empty())
            });
            self.files.insert(file.to_path_buf(), parsed);
        }
        self.files.get(file).and_then(Option::as_ref)
    }

    /// Forgets `file` (an ignore file that changed).
    fn forget(&mut self, file: &Path) {
        self.files.remove(file);
    }
}

/// One held mutation.
struct Held {
    project_id: String,
    root: PathBuf,
    first_ms: u64,
    deadline: Instant,
    kind: MutationKind,
    created_first: bool,
    seq: u64,
}

/// The held mutations, by file, with each Project's in hold order.
#[derive(Default)]
struct Holds {
    held: HashMap<PathBuf, Held>,
    order: HashMap<String, VecDeque<(u64, PathBuf)>>,
    counts: HashMap<String, usize>,
    next_seq: u64,
}

/// A decided mutation, ready to build.
struct Decided {
    project_id: String,
    root: PathBuf,
    first_ms: u64,
    kind: MutationKind,
}

impl Holds {
    /// Records one observation. Returns a mutation decided early because
    /// its Project's cap was reached.
    fn observe(
        &mut self,
        path: PathBuf,
        project: (&str, &Path),
        kind: MutationKind,
        at: (u64, Instant),
        settings: &WatcherSettings,
    ) -> Option<Decided> {
        if let Some(held) = self.held.get_mut(&path) {
            match kind {
                MutationKind::Removed if held.created_first => {
                    self.release(&path);
                }
                MutationKind::Created if held.kind == MutationKind::Removed => {
                    held.kind = MutationKind::Modified;
                }
                _ if held.created_first => {}
                _ => held.kind = kind,
            }
            return None;
        }
        let (project_id, root) = project;
        let count = self.counts.get(project_id).copied().unwrap_or(0);
        let early = if count >= settings.pending_cap.max(1) {
            self.oldest_of(project_id)
                .and_then(|oldest| self.decide(&oldest))
        } else {
            None
        };
        let seq = self.next_seq;
        self.next_seq += 1;
        self.held.insert(
            path.clone(),
            Held {
                project_id: project_id.to_owned(),
                root: root.to_path_buf(),
                first_ms: at.0,
                deadline: at.1 + settings.hold,
                kind,
                created_first: kind == MutationKind::Created,
                seq,
            },
        );
        self.order
            .entry(project_id.to_owned())
            .or_default()
            .push_back((seq, path));
        *self.counts.entry(project_id.to_owned()).or_default() += 1;
        early
    }

    fn oldest_of(&mut self, project_id: &str) -> Option<PathBuf> {
        let queue = self.order.get_mut(project_id)?;
        while let Some((seq, path)) = queue.pop_front() {
            if self.held.get(&path).is_some_and(|held| held.seq == seq) {
                return Some(path);
            }
        }
        None
    }

    /// The earliest deadline among held mutations.
    fn next_deadline(&mut self) -> Option<Instant> {
        let held = &self.held;
        self.order
            .values_mut()
            .filter_map(|queue| {
                while let Some((seq, path)) = queue.front() {
                    match held.get(path) {
                        Some(h) if h.seq == *seq => return Some(h.deadline),
                        _ => {
                            queue.pop_front();
                        }
                    }
                }
                None
            })
            .min()
    }

    /// Every held mutation whose deadline is at or before `now` (all of
    /// them when `now` is `None`), in hold order per Project.
    fn due(&mut self, now: Option<Instant>) -> Vec<Decided> {
        let mut due: Vec<(Instant, u64, PathBuf)> = self
            .held
            .iter()
            .filter(|(_, h)| now.is_none_or(|now| h.deadline <= now))
            .map(|(path, h)| (h.deadline, h.seq, path.clone()))
            .collect();
        due.sort();
        due.into_iter()
            .filter_map(|(_, _, path)| self.decide(&path))
            .collect()
    }

    /// Removes `path`'s held mutation and decides its final kind from the
    /// file's existence now. `None` when nothing is reported.
    fn decide(&mut self, path: &Path) -> Option<Decided> {
        let held = self.release(path)?;
        let kind = match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => return None,
            Ok(_) if held.kind == MutationKind::Removed => MutationKind::Modified,
            Ok(_) => held.kind,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if held.created_first {
                    return None;
                }
                MutationKind::Removed
            }
            Err(_) => held.kind,
        };
        Some(Decided {
            project_id: held.project_id,
            root: held.root,
            first_ms: held.first_ms,
            kind,
        })
    }

    fn release(&mut self, path: &Path) -> Option<Held> {
        let held = self.held.remove(path)?;
        if let Some(count) = self.counts.get_mut(&held.project_id) {
            *count = count.saturating_sub(1);
        }
        Some(held)
    }

    /// Drops the (stale) order entries of a Project with nothing held.
    fn tidy(&mut self) {
        let counts = &self.counts;
        self.order
            .retain(|project, _| counts.get(project).is_some_and(|c| *c > 0));
        self.counts.retain(|_, c| *c > 0);
    }
}

/// The watcher thread. [`WatcherThread::stop`] flushes and joins it.
pub struct WatcherThread {
    tx: Sender<Message>,
    handle: JoinHandle<()>,
}

impl WatcherThread {
    /// Decides and flushes every held mutation to the Router, then stops.
    pub fn stop(self) -> thread::Result<()> {
        let _ = self.tx.send(Message::Stop);
        self.handle.join()
    }
}

/// Starts the watcher thread: it filters and holds what the backend
/// reports and sends decided mutations to `router`.
pub fn spawn(
    watches: Watches,
    notifications: Notifications,
    router: RouterSender,
    settings: WatcherSettings,
) -> io::Result<WatcherThread> {
    let tx = notifications.tx.clone();
    let handle = thread::Builder::new()
        .name("traction-watcher".to_owned())
        .spawn(move || {
            let mut worker = Worker {
                watches,
                router,
                settings,
                holds: Holds::default(),
                router_gone_logged: false,
            };
            worker.run(&notifications);
        })?;
    Ok(WatcherThread { tx, handle })
}

struct Worker {
    watches: Watches,
    router: RouterSender,
    settings: WatcherSettings,
    holds: Holds,
    router_gone_logged: bool,
}

impl Worker {
    fn run(&mut self, notifications: &Notifications) {
        loop {
            // Blocks with no timeout while nothing is held: no idle polling.
            let message = match self.holds.next_deadline() {
                None => notifications
                    .rx
                    .recv()
                    .map_err(|_| RecvTimeoutError::Disconnected),
                Some(deadline) => notifications
                    .rx
                    .recv_timeout(deadline.saturating_duration_since(Instant::now())),
            };
            match message {
                Ok(Message::Notify {
                    at,
                    observed,
                    event,
                }) => {
                    notifications.queued.fetch_sub(1, Ordering::AcqRel);
                    self.observe(at, observed, &event);
                }
                Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let due = self.holds.due(Some(Instant::now()));
            self.emit_all(due);
            self.holds.tidy();
        }
        let rest = self.holds.due(None);
        self.emit_all(rest);
    }

    fn observe(&mut self, at: SystemTime, observed: Instant, event: &notify::Event) {
        let at_ms = at
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        let mut found = Vec::new();
        let mut observations = Vec::new();
        {
            let mut table = self.watches.lock();
            for raw in &event.paths {
                let Some((index, path)) = locate(&table, raw) else {
                    continue;
                };
                if path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|name| IGNORE_FILES.iter().any(|f| name_matches(name, f)))
                {
                    table.ignores.forget(&path);
                }
                let root = table.roots[index].root.clone();
                let Some(kind) = classify(&mut table, index, &event.kind, &path, &mut found) else {
                    continue;
                };
                if table.is_filtered(&root, &path, false) {
                    continue;
                }
                let project_id = table.roots[index].project_id.clone();
                observations.push((path, project_id, root, kind));
            }
            // Files inside a folder that just appeared (Linux): new files.
            for path in found {
                let Some(index) = table.root_of(&path) else {
                    continue;
                };
                let watched = &table.roots[index];
                observations.push((
                    path,
                    watched.project_id.clone(),
                    watched.root.clone(),
                    MutationKind::Created,
                ));
            }
        }
        for (path, project_id, root, kind) in observations {
            let early = self.holds.observe(
                path,
                (&project_id, &root),
                kind,
                (at_ms, observed),
                &self.settings,
            );
            if let Some(decided) = early {
                self.emit(decided);
            }
        }
    }

    fn emit_all(&mut self, decided: Vec<Decided>) {
        for one in decided {
            self.emit(one);
        }
    }

    fn emit(&mut self, decided: Decided) {
        let Some(cwd) = decided.root.to_str().map(str::to_owned) else {
            return;
        };
        let mutation = TractionEvent {
            schema_version: SCHEMA_VERSION,
            event_id: fresh_event_id(decided.first_ms),
            timestamp_ms: decided.first_ms,
            harness_id: None,
            session_id: None,
            parent_orchestrator_id: None,
            event_type: EventType::FileMutation,
            end_reason: None,
            cwd: Some(cwd),
            project_id: Some(decided.project_id),
            model_id: None,
            network_class: NetworkClass::Manual,
            attribution_source: AttributionSource::FileWatcher,
            metrics_only: MetricsOnly {
                mutation_kind: Some(decided.kind),
                ..MetricsOnly::default()
            },
        };
        if self.router.send_mutation(mutation).is_err() && !self.router_gone_logged {
            self.router_gone_logged = true;
            log::warn("the Project Router has stopped; file changes are being dropped");
        }
    }
}

/// The watched root holding `raw` and the path as compared against it.
/// A path outside every root (for example, reported through a symlinked
/// prefix such as macOS `/var` for `/private/var`) is retried with its
/// parent canonicalized.
fn locate(table: &WatchTable, raw: &Path) -> Option<(usize, PathBuf)> {
    if let Some(index) = table.root_of(raw) {
        return Some((index, raw.to_path_buf()));
    }
    let parent = paths::canonicalize(raw.parent()?)?;
    let path = parent.join(raw.file_name()?);
    Some((table.root_of(&path)?, path))
}

/// The mutation kind an event means for `path`, or `None` for folders,
/// metadata and other non-file events. On Linux a new folder is watched
/// here (its files are pushed to `found`).
fn classify(
    table: &mut WatchTable,
    index: usize,
    kind: &EventKind,
    path: &Path,
    found: &mut Vec<PathBuf>,
) -> Option<MutationKind> {
    let meta = fs::symlink_metadata(path).ok();
    let is_dir = meta.as_ref().is_some_and(fs::Metadata::is_dir);
    let changes_file = matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Modify(
                ModifyKind::Name(_) | ModifyKind::Data(_) | ModifyKind::Any | ModifyKind::Other
            )
    );
    if changes_file && !is_dir && predates_watch(&table.roots[index], meta.as_ref()) {
        return None;
    }
    #[cfg(target_os = "macos")]
    if (changes_file && !is_dir) || matches!(kind, EventKind::Remove(_)) {
        return Some(state_kind(&mut table.roots[index], path, meta.is_some()));
    }
    match kind {
        EventKind::Create(CreateKind::Folder) => {
            new_folder(table, index, path, Some(found));
            None
        }
        EventKind::Create(_) if is_dir => {
            new_folder(table, index, path, Some(found));
            None
        }
        EventKind::Create(_) => Some(MutationKind::Created),
        EventKind::Modify(ModifyKind::Metadata(_)) => None,
        EventKind::Modify(ModifyKind::Name(_)) if is_dir => {
            new_folder(table, index, path, None);
            None
        }
        EventKind::Modify(ModifyKind::Name(_)) => Some(if meta.is_some() {
            MutationKind::Renamed
        } else {
            MutationKind::Removed
        }),
        EventKind::Modify(_) if is_dir => None,
        EventKind::Modify(_) => Some(MutationKind::Modified),
        EventKind::Remove(RemoveKind::Folder) => None,
        EventKind::Remove(_) => Some(MutationKind::Removed),
        EventKind::Access(_) | EventKind::Any | EventKind::Other => None,
    }
}

/// macOS FSEvents also delivers changes made just before its stream
/// started. A file whose status last changed (write, create or rename all
/// set `ctime`) before its Project was attached is such a replay: live-only,
/// it is never reported. Other backends report only what follows the watch.
#[cfg_attr(not(target_os = "macos"), allow(unused_variables))]
fn predates_watch(watched: &WatchedRoot, meta: Option<&fs::Metadata>) -> bool {
    #[cfg(target_os = "macos")]
    if let Some(meta) = meta {
        use std::os::unix::fs::MetadataExt;
        let changed = u64::try_from(meta.ctime()).ok().map(|secs| {
            UNIX_EPOCH
                + std::time::Duration::new(secs, u32::try_from(meta.ctime_nsec()).unwrap_or(0))
        });
        return changed.is_some_and(|changed| changed < watched.attached_at);
    }
    false
}

/// macOS: the kind from the file's state and the existence baseline. A
/// missing file never known to exist was short-lived (`created`, which the
/// hold drops once it is gone).
#[cfg(target_os = "macos")]
fn state_kind(watched: &mut WatchedRoot, path: &Path, exists: bool) -> MutationKind {
    let key = path_key(path);
    if exists {
        if watched.known.contains(&key) {
            MutationKind::Modified
        } else {
            if watched.known.len() < MAX_BASELINE_FILES {
                watched.known.insert(key);
            }
            MutationKind::Created
        }
    } else if watched.known.remove(&key) {
        MutationKind::Removed
    } else {
        MutationKind::Created
    }
}

/// A fixed hash of `path` (the baseline keeps no paths).
#[cfg(target_os = "macos")]
fn path_key(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish()
}

/// A folder appeared inside a watched Project. Linux watches it (and the
/// folders below it) unless it is pruned; recursive backends need nothing.
#[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
fn new_folder(table: &mut WatchTable, index: usize, path: &Path, found: Option<&mut Vec<PathBuf>>) {
    #[cfg(target_os = "linux")]
    {
        let root = table.roots[index].root.clone();
        if table.roots[index].full || table.is_filtered(&root, path, true) {
            return;
        }
        // Take the root out while walking, so the table stays borrowable.
        let mut watched = table.roots.swap_remove(index);
        table.walk_tree(&mut watched, path, found);
        table.roots.push(watched);
    }
}

/// macOS: FSEvents refuses (or, past the edge, corrupts descriptors) beyond
/// about a twelfth of the soft open-file limit in watched roots, and the
/// default soft limit is often 256. Raise it towards the hard limit, as
/// many daemons do; no privilege is needed.
#[cfg(target_os = "macos")]
fn raise_open_file_limit() {
    const WANTED: libc::rlim_t = 10_240;
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: `getrlimit`/`setrlimit` only read or write the struct passed.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut limit) != 0 || limit.rlim_cur >= WANTED {
            return;
        }
        limit.rlim_cur = WANTED.min(limit.rlim_max);
        let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &raw const limit);
    }
}

#[cfg(not(target_os = "macos"))]
fn raise_open_file_limit() {}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(id: &str, root: &Path) -> RegisteredProject {
        RegisteredProject {
            project_id: id.to_owned(),
            kind: crate::storage::ProjectKind::Dir,
            root: root.to_path_buf(),
            merged_into: None,
        }
    }

    /// A fake home with a Project folder inside it, and the watch table.
    fn table_with_project(tmp: &Path) -> (Watches, PathBuf) {
        let home = paths::canonicalize(tmp).unwrap();
        let root = home.join("notes");
        fs::create_dir(&root).unwrap();
        let exclusions = Exclusions::load(Some(&home), &home.join(".traction"), &home.join("none"));
        let (watches, _notifications) = Watches::new(exclusions);
        (watches, root)
    }

    #[test]
    fn a_merge_on_the_same_root_hands_the_watch_over_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let (watches, root) = table_with_project(tmp.path());
        watches.attach(&project("dir:a", &root));
        #[cfg(target_os = "macos")]
        let attached_at = watches.lock().roots[0].attached_at;

        watches.merge("dir:a", "git:b");
        watches.attach(&project("git:b", &root));

        let table = watches.lock();
        let ids: Vec<&str> = table.roots.iter().map(|w| w.project_id.as_str()).collect();
        assert_eq!(ids, ["git:b"]);
        // Not re-attached: in-flight notifications still predate nothing.
        #[cfg(target_os = "macos")]
        assert_eq!(table.roots[0].attached_at, attached_at);
    }

    #[test]
    fn a_merged_sub_root_gives_way_to_the_new_git_root() {
        let tmp = tempfile::tempdir().unwrap();
        let (watches, repo) = table_with_project(tmp.path());
        let sub = repo.join("docs");
        fs::create_dir(&sub).unwrap();
        watches.attach(&project("dir:a", &sub));

        watches.merge("dir:a", "git:b");
        watches.attach(&project("git:b", &repo));

        let table = watches.lock();
        let roots: Vec<(&str, &Path)> = table
            .roots
            .iter()
            .map(|w| (w.project_id.as_str(), w.root.as_path()))
            .collect();
        assert_eq!(roots, [("git:b", repo.as_path())]);
    }

    #[test]
    fn a_merge_into_an_already_watched_project_drops_the_old_watch() {
        let tmp = tempfile::tempdir().unwrap();
        let (watches, repo) = table_with_project(tmp.path());
        let sub = repo.join("docs");
        fs::create_dir(&sub).unwrap();
        watches.attach(&project("git:b", &repo));
        watches.attach(&project("dir:a", &sub));

        watches.merge("dir:a", "git:b");

        let table = watches.lock();
        let ids: Vec<&str> = table.roots.iter().map(|w| w.project_id.as_str()).collect();
        assert_eq!(ids, ["git:b"]);
    }
}
