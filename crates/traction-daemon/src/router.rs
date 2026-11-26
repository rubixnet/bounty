//! The Project Router: the only place `project_id` is derived.
//!
//! Every accepted Traction Event (and every `/v1/project` registration)
//! passes through one dedicated Router thread, in arrival order, before the
//! single writer. The bus only queues ([`RouterSender`]), so Git calls
//! never delay a bus response.
//!
//! For an event the Router:
//! 1. updates the session's start folder (its first `cwd`) and last `cwd`;
//! 2. takes the event's `cwd`, else the session's last `cwd`;
//! 3. drops the event when that folder is user-excluded (still `202`);
//! 4. resolves the folder ([`Router::resolve`]): the Git root (worktrees
//!    collapsed through the common dir), else the nearest marker folder,
//!    else, when the folder is inside it, the session's start folder;
//! 5. migrates any registered `dir:` Project inside a `git:` Root it just
//!    resolved (alias-only, one [`WriteOp::Migration`]);
//! 6. resolves the ID through `project_alias` to its final ID, so a late
//!    event for a migrated `dir:` Project lands under the `git:` ID;
//! 7. registers a Project seen for the first time (a [`WriteOp::Project`],
//!    then every [`ProjectHook`] is told); and
//! 8. replaces any Adapter-supplied `project_id` and queues the event,
//!    unless its session was paused at the event's timestamp (Session
//!    Pause): a paused event still feeds the attribution windows, but is
//!    not stored.
//!
//! `git:` IDs are pinned per canonical common dir: the first ID stored for
//! a repo is reused even after `origin` changes. A `.git` entry appearing
//! (or changing) at a `dir:` Root drops that Root's cached resolutions at
//! once, so `git init` + first commit migrates on the next event.
//!
//! Registry and session writes go through the single writer, in the same
//! channel as the events they precede. The Router keeps its own in-memory
//! copy of the registry and sessions, loaded from SQLite at startup and
//! updated as it queues writes, and never reads SQLite afterwards.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tokio::sync::oneshot;
use traction_core::{
    AttributionSource, EventType, HarnessId, NetworkClass, TractionEvent, dir_project_id,
    git_project_id,
};

use crate::attribution::Attribution;
use crate::claude_code::pause::PauseAction;
use crate::config::WatcherSettings;
#[cfg(doc)]
use crate::config::{LOG_TAIL_HOLD, LOG_TAIL_TRAILING_WINDOW, STALE_TURN_TIMEOUT};
use crate::coverage::HookCoverage;
use crate::exclusions::{Exclusions, has_project_marker};
use crate::git::Git;
use crate::log;
use crate::paths::{self, is_within, same};
use crate::pause::Pauses;
use crate::secret::DeviceSecret;
use crate::storage::{
    EventRow, MigrationRow, ProjectKind, ProjectRow, RunRow, SessionRow, StoredRegistry,
    TailedBatch, TailerStartRow, TranscriptOffset, WriteOp, final_alias,
};
use crate::writer::WriterSender;

/// How long a folder's resolution is reused before Git is asked again.
pub const RESOLUTION_TTL: Duration = Duration::from_secs(30);
/// Cached folder resolutions kept at most (expired ones are pruned first).
const CACHE_CAPACITY: usize = 4096;

/// Where a folder routes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub project_id: String,
    pub kind: ProjectKind,
    /// Canonical Project Root.
    pub root: PathBuf,
    /// Canonical common Git dir, for `git:` Projects (ID pinning).
    pub git_common_dir: Option<PathBuf>,
}

/// A Project in the registry, as announced to [`ProjectHook`]s.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredProject {
    pub project_id: String,
    pub kind: ProjectKind,
    /// Canonical Project Root.
    pub root: PathBuf,
    /// Set when this `dir:` Project was just merged into the `git:` Project
    /// with this ID: it is no longer registered and its watcher moves to
    /// the `git:` Project (announced separately, right after, when new).
    pub merged_into: Option<String>,
}

/// Called on the Router thread, after the write is queued, for every newly
/// registered Project and for every `dir:` Project merged by a migration
/// (`merged_into` set). The file watcher attaches here.
pub type ProjectHook = Box<dyn FnMut(&RegisteredProject) + Send>;

/// Why `/v1/project` refused a path (answered as `422`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegisterError {
    NotFound,
    NotAFolder,
    /// `$HOME`, a filesystem root, an OS/cache folder or a dependency folder.
    Excluded,
    /// Inside a folder listed in the global ignore file.
    UserExcluded,
}

impl RegisterError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => "path_not_found",
            Self::NotAFolder => "not_a_folder",
            Self::Excluded => "excluded_path",
            Self::UserExcluded => "user_excluded_path",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct SessionState {
    start_folder: Option<String>,
    last_cwd: Option<String>,
}

/// The `.git` entry last seen at a `dir:` Root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GitEntry {
    Absent,
    /// Present, with its mtime when the OS reports one.
    Present(Option<SystemTime>),
}

impl GitEntry {
    fn at(root: &Path) -> Self {
        match std::fs::symlink_metadata(root.join(".git")) {
            Ok(meta) => Self::Present(meta.modified().ok()),
            Err(_) => Self::Absent,
        }
    }
}

struct CacheEntry {
    at: Instant,
    resolution: Option<Resolution>,
}

pub struct Router {
    git: Git,
    exclusions: Exclusions,
    secret: DeviceSecret,
    writer: WriterSender,
    ttl: Duration,
    cache: HashMap<PathBuf, CacheEntry>,
    projects: HashMap<String, RegisteredProject>,
    /// `project_alias`: old ID -> current ID.
    aliases: HashMap<String, String>,
    /// Pinned `git:` IDs by canonical common dir ([`pin_key`]).
    pinned: HashMap<String, String>,
    /// The `.git` entry last seen at each `dir:` Root.
    git_entries: HashMap<PathBuf, GitEntry>,
    sessions: HashMap<(HarnessId, String), SessionState>,
    /// Sessions with a stored (or queued) `SESSION_START`.
    started: HashSet<(HarnessId, String)>,
    /// Session Pause spans, loaded at startup.
    pauses: Pauses,
    /// Which spans of each session its hooks covered (this run and earlier).
    coverage: HookCoverage,
    hooks: Vec<ProjectHook>,
    /// In-memory attribution windows (never persisted).
    windows: Attribution,
    writer_gone_logged: bool,
}

impl Router {
    /// A Router seeded with the registry and sessions stored in SQLite.
    pub fn new(
        git: Git,
        exclusions: Exclusions,
        secret: DeviceSecret,
        writer: WriterSender,
        stored: StoredRegistry,
    ) -> Self {
        let StoredRegistry {
            projects,
            aliases,
            sessions,
            started_sessions,
            pauses,
            runs,
            hook_runs,
        } = stored;
        let pinned = projects
            .iter()
            .filter_map(|row| {
                let dir = row.git_common_dir.as_deref()?;
                Some((pin_key(Path::new(dir)), row.project_id.clone()))
            })
            .collect();
        let projects = projects
            .into_iter()
            .map(|row| {
                let project = RegisteredProject {
                    root: PathBuf::from(&row.root_path),
                    project_id: row.project_id,
                    kind: row.kind,
                    merged_into: None,
                };
                (project.project_id.clone(), project)
            })
            .collect();
        let sessions = sessions
            .into_iter()
            .map(|row| {
                let state = SessionState {
                    start_folder: row.start_folder,
                    last_cwd: row.last_cwd,
                };
                ((row.harness_id, row.session_id), state)
            })
            .collect();
        let coverage = HookCoverage::new(&runs, hook_runs);
        let mut router = Self {
            git,
            exclusions,
            secret,
            writer,
            ttl: RESOLUTION_TTL,
            cache: HashMap::new(),
            projects,
            aliases: aliases.into_iter().collect(),
            pinned,
            git_entries: HashMap::new(),
            sessions,
            started: started_sessions.into_iter().collect(),
            pauses: Pauses::new(pauses),
            coverage,
            hooks: Vec::new(),
            windows: Attribution::new(&WatcherSettings::default()),
            writer_gone_logged: false,
        };
        // This start is a run; the writer keeps its `last_alive_ms` current.
        let run = RunRow {
            run_id: router.coverage.current_run(),
            started_ms: now_ms(),
        };
        router.write(WriteOp::RunStart(run));
        router
    }

    /// Replaces the attribution windows' timing (test overrides of
    /// [`LOG_TAIL_TRAILING_WINDOW`], [`LOG_TAIL_HOLD`] and
    /// [`STALE_TURN_TIMEOUT`]). Call before any event is routed.
    pub fn set_attribution_timing(&mut self, settings: &WatcherSettings) {
        self.windows = Attribution::new(settings);
    }

    /// Adds a hook told about every Project registered from now on.
    pub fn add_project_hook(&mut self, hook: ProjectHook) {
        self.hooks.push(hook);
    }

    /// Every registered (non-merged) Project, including those loaded at
    /// startup.
    pub fn projects(&self) -> impl Iterator<Item = &RegisteredProject> {
        self.projects.values()
    }

    /// Forgets every cached resolution of a folder at or below `root`, or
    /// resolving to `root`, so the next event asks Git again.
    pub fn invalidate(&mut self, root: &Path) {
        self.cache.retain(|folder, entry| {
            let resolves_there = entry
                .resolution
                .as_ref()
                .is_some_and(|r| same(&r.root, root));
            !is_within(folder, root) && !resolves_there
        });
    }

    /// Routes one event. Returns it with the Router's `project_id`, or
    /// `None` when it falls under a user exclusion and must be dropped.
    pub fn route(&mut self, mut event: TractionEvent) -> Option<TractionEvent> {
        let (cwd, start_folder) = self.track_session(&event);
        let canonical_cwd = cwd
            .as_deref()
            .and_then(|c| paths::canonicalize(Path::new(c)));
        let excluded_by_user = match (&canonical_cwd, &cwd) {
            (Some(canon), _) => self.exclusions.is_user_excluded(canon),
            (None, Some(raw)) => self
                .exclusions
                .is_user_excluded(&paths::canonical_or_lexical(Path::new(raw))),
            (None, None) => false,
        };
        if excluded_by_user {
            return None;
        }
        let folder = canonical_cwd.as_deref();
        let start_folder = start_folder.as_deref();
        let mut resolution = self.resolve_event(folder, start_folder);
        if let Some(root) = self.repo_appeared(resolution.as_ref()) {
            self.invalidate(&root);
            resolution = self.resolve_event(folder, start_folder);
        }
        event.project_id = resolution.map(|r| self.settle(&r));
        Some(event)
    }

    fn resolve_event(
        &mut self,
        folder: Option<&Path>,
        start_folder: Option<&str>,
    ) -> Option<Resolution> {
        folder
            .and_then(|folder| self.resolve(folder))
            .or_else(|| self.resolve_start_folder(folder, start_folder))
    }

    /// The Root of a `dir:` resolution whose `.git` entry appeared or
    /// changed since last seen (a `git init` or a first commit), so its
    /// cached resolutions are stale.
    fn repo_appeared(&mut self, resolution: Option<&Resolution>) -> Option<PathBuf> {
        let resolution = resolution.filter(|r| r.kind == ProjectKind::Dir)?;
        let entry = GitEntry::at(&resolution.root);
        let changed = match self.git_entries.insert(resolution.root.clone(), entry) {
            Some(previous) => previous != entry,
            None => entry != GitEntry::Absent,
        };
        changed.then(|| resolution.root.clone())
    }

    /// Migrates, aliases and registers as needed; returns the final ID.
    fn settle(&mut self, resolution: &Resolution) -> String {
        if resolution.kind == ProjectKind::Git {
            self.migrate_into(resolution);
        }
        let id = self.final_id(&resolution.project_id);
        if id == resolution.project_id {
            self.register(resolution);
        }
        id
    }

    /// `id` resolved through `project_alias` (itself when not aliased, or
    /// when its chain is broken).
    fn final_id(&self, id: &str) -> String {
        final_alias(|current| self.aliases.get(current).cloned(), id)
            .unwrap_or_else(|| id.to_owned())
    }

    /// Alias-only migration of every registered `dir:` Project whose Root
    /// lies inside the `git:` Root just resolved (a `git init` + commit on
    /// it, or a `dir:` fallback taken while Git was unavailable).
    fn migrate_into(&mut self, resolution: &Resolution) {
        let mut merged: Vec<RegisteredProject> = self
            .projects
            .values()
            .filter(|p| p.kind == ProjectKind::Dir && is_within(&p.root, &resolution.root))
            .cloned()
            .collect();
        if merged.is_empty() {
            return;
        }
        merged.sort_by(|a, b| a.project_id.cmp(&b.project_id));
        let into = self.final_id(&resolution.project_id);
        let row = self.project_row(resolution);
        for mut dir in merged {
            if into == dir.project_id {
                log::warn("refused a Project migration that would close an alias cycle");
                continue;
            }
            self.write(WriteOp::Migration(MigrationRow {
                alias_id: dir.project_id.clone(),
                into: row.clone(),
                migrated_at_ms: now_ms(),
            }));
            self.projects.remove(&dir.project_id);
            self.aliases.insert(dir.project_id.clone(), into.clone());
            dir.merged_into = Some(into.clone());
            for hook in &mut self.hooks {
                hook(&dir);
            }
        }
    }

    /// Registers the Project Root `path` resolves to (`/v1/project`).
    pub fn register_path(&mut self, path: &Path) -> Result<String, RegisterError> {
        if self
            .exclusions
            .is_user_excluded(&paths::canonical_or_lexical(path))
        {
            return Err(RegisterError::UserExcluded);
        }
        let folder = paths::canonicalize(path).ok_or(RegisterError::NotFound)?;
        if !folder.is_dir() {
            return Err(RegisterError::NotAFolder);
        }
        if self.exclusions.is_user_excluded(&folder) {
            return Err(RegisterError::UserExcluded);
        }
        if self.exclusions.is_excluded(&folder) {
            return Err(RegisterError::Excluded);
        }
        let mut resolution = self
            .resolve(&folder)
            .unwrap_or_else(|| self.dir_resolution(folder.clone()));
        if let Some(root) = self.repo_appeared(Some(&resolution)) {
            self.invalidate(&root);
            resolution = self
                .resolve(&folder)
                .unwrap_or_else(|| self.dir_resolution(folder));
        }
        Ok(self.settle(&resolution))
    }

    /// Resolves a canonical folder through the Git root and nearest-marker
    /// steps, reusing a cached answer younger than [`RESOLUTION_TTL`].
    pub fn resolve(&mut self, folder: &Path) -> Option<Resolution> {
        if let Some(entry) = self.cache.get(folder)
            && entry.at.elapsed() < self.ttl
        {
            return entry.resolution.clone();
        }
        let resolution = self.resolve_uncached(folder);
        if self.cache.len() >= CACHE_CAPACITY {
            let ttl = self.ttl;
            self.cache.retain(|_, entry| entry.at.elapsed() < ttl);
            if self.cache.len() >= CACHE_CAPACITY {
                self.cache.clear();
            }
        }
        self.cache.insert(
            folder.to_path_buf(),
            CacheEntry {
                at: Instant::now(),
                resolution: resolution.clone(),
            },
        );
        resolution
    }

    fn resolve_uncached(&self, folder: &Path) -> Option<Resolution> {
        self.git_step(folder).or_else(|| self.marker_step(folder))
    }

    /// The folders a walk from `folder` may consider, nearest first. It
    /// never reaches `$HOME` or above it (when `folder` is inside `$HOME`).
    fn walk<'a>(&'a self, folder: &'a Path) -> impl Iterator<Item = &'a Path> + 'a {
        let home = self
            .exclusions
            .home()
            .filter(|home| is_within(folder, home));
        folder
            .ancestors()
            .take_while(move |dir| home.is_none_or(|home| !same(dir, home)))
    }

    /// Step 1: the nearest non-excluded ancestor with a `.git` entry, asked
    /// through the hardened wrapper. A `$HOME`-rooted repo never matches
    /// (the walk stops below `$HOME`), and any Git failure falls through.
    fn git_step(&self, folder: &Path) -> Option<Resolution> {
        let candidate = self
            .walk(folder)
            .filter(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok())
            .find(|dir| !self.exclusions.is_excluded(dir))?;
        let common_dir = paths::canonicalize(&self.git.common_dir(candidate).ok()?)?;
        let root = if common_dir
            .file_name()
            .is_some_and(|name| paths::name_matches(&name.to_string_lossy(), ".git"))
        {
            // A normal repo: the main worktree, so linked worktrees collapse.
            common_dir.parent()?.to_path_buf()
        } else {
            // A bare repo reached through a linked worktree.
            common_dir.clone()
        };
        if self.exclusions.is_excluded(&root) {
            return None;
        }
        match self.git.root_commit(candidate).ok()? {
            Some(commit) => {
                // ID pinning: the first ID stored for this common dir wins,
                // even after `origin` changes.
                let project_id = match self.pinned.get(&pin_key(&common_dir)) {
                    Some(pinned) => pinned.clone(),
                    None => {
                        let origin = self.git.origin_url(candidate).ok().flatten();
                        git_project_id(&commit, origin.as_deref())
                    }
                };
                Some(Resolution {
                    project_id,
                    kind: ProjectKind::Git,
                    root,
                    git_common_dir: Some(common_dir),
                })
            }
            // No commit yet: a `dir:` Project on the repo root.
            None => Some(self.dir_resolution(root)),
        }
    }

    /// Step 2: the nearest non-excluded ancestor holding a project marker.
    fn marker_step(&self, folder: &Path) -> Option<Resolution> {
        let root = self
            .walk(folder)
            .filter(|dir| !self.exclusions.is_excluded(dir))
            .find(|dir| has_project_marker(dir))?;
        Some(self.dir_resolution(root.to_path_buf()))
    }

    /// Step 3: the session's start folder, when the event's folder is
    /// inside it (or the event has no usable folder at all). The start
    /// folder itself is resolved first, so a start inside a repo routes to
    /// the repo.
    fn resolve_start_folder(
        &mut self,
        folder: Option<&Path>,
        start_folder: Option<&str>,
    ) -> Option<Resolution> {
        let start = paths::canonicalize(Path::new(start_folder?))?;
        if folder.is_some_and(|folder| !is_within(folder, &start))
            || !start.is_dir()
            || self.exclusions.is_excluded(&start)
        {
            return None;
        }
        self.resolve(&start)
            .or_else(|| Some(self.dir_resolution(start)))
    }

    fn dir_resolution(&self, root: PathBuf) -> Resolution {
        let root_text = root.to_str().unwrap_or_default();
        Resolution {
            project_id: dir_project_id(self.secret.bytes(), root_text),
            kind: ProjectKind::Dir,
            root,
            git_common_dir: None,
        }
    }

    /// Records the session's start folder (its first `cwd`) and last `cwd`,
    /// queueing a session write when either changed. Returns the event's
    /// effective `cwd` and the session's start folder.
    fn track_session(&mut self, event: &TractionEvent) -> (Option<String>, Option<String>) {
        let (Some(harness), Some(session_id)) = (event.harness_id, &event.session_id) else {
            return (event.cwd.clone(), None);
        };
        let key = (harness, session_id.clone());
        let state = self.sessions.entry(key).or_default();
        let mut changed = false;
        if let Some(cwd) = &event.cwd {
            if state.start_folder.is_none() {
                state.start_folder = Some(cwd.clone());
                changed = true;
            }
            if state.last_cwd.as_ref() != Some(cwd) {
                state.last_cwd = Some(cwd.clone());
                changed = true;
            }
        }
        let effective = event.cwd.clone().or_else(|| state.last_cwd.clone());
        let start_folder = state.start_folder.clone();
        if changed {
            let row = SessionRow {
                harness_id: harness,
                session_id: session_id.clone(),
                start_folder: state.start_folder.clone(),
                last_cwd: state.last_cwd.clone(),
            };
            self.write(WriteOp::Session(row));
        }
        (effective, start_folder)
    }

    /// Registers `resolution`'s Project the first time it is seen.
    fn register(&mut self, resolution: &Resolution) {
        if self.projects.contains_key(&resolution.project_id) {
            return;
        }
        let row = self.project_row(resolution);
        if let Some(dir) = &resolution.git_common_dir {
            self.pinned
                .insert(pin_key(dir), resolution.project_id.clone());
        }
        self.write(WriteOp::Project(row));
        let project = RegisteredProject {
            project_id: resolution.project_id.clone(),
            kind: resolution.kind,
            root: resolution.root.clone(),
            merged_into: None,
        };
        for hook in &mut self.hooks {
            hook(&project);
        }
        self.projects.insert(project.project_id.clone(), project);
    }

    fn project_row(&self, resolution: &Resolution) -> ProjectRow {
        let root_path = resolution.root.to_str().unwrap_or_default().to_owned();
        let name = resolution
            .root
            .file_name()
            .map_or_else(|| root_path.clone(), |n| n.to_string_lossy().into_owned());
        ProjectRow {
            project_id: resolution.project_id.clone(),
            kind: resolution.kind,
            home_relative_path: self.home_relative(&resolution.root),
            root_path,
            name,
            first_seen_ms: now_ms(),
            git_common_dir: resolution
                .git_common_dir
                .as_ref()
                .map(|dir| dir.to_str().unwrap_or_default().to_owned()),
        }
    }

    /// `~/a/b` for a root inside `$HOME` (always `/`-separated).
    fn home_relative(&self, root: &Path) -> Option<String> {
        let home = self
            .exclusions
            .home()
            .filter(|home| is_within(root, home))?;
        let parts: Vec<String> = paths::components_below(root, home)
            .map(|c| c.as_os_str().to_string_lossy().into_owned())
            .collect();
        Some(format!("~/{}", parts.join("/")))
    }

    fn write(&mut self, op: WriteOp) {
        if self.writer.send(op).is_err() && !self.writer_gone_logged {
            self.writer_gone_logged = true;
            log::warn("the SQLite writer has stopped; routed writes are being dropped");
        }
    }

    /// Decides who a held `FILE_MUTATION` belongs to, at the end of its hold.
    ///
    /// The watcher builds every mutation as manual work (no Harness, no
    /// session, `network_class: manual`). This is the single attribution
    /// decision point: it runs on the Router thread, after every event the
    /// bus accepted before the hold ended, so the window covering the
    /// mutation's timestamp on its own Project (if any) claims it by
    /// copying the winning session's `harness_id`, `session_id` and
    /// `network_class`. Returns `false` when the claiming session was
    /// paused at the mutation's timestamp: the mutation must be dropped.
    fn attribute(&self, mutation: &mut TractionEvent) -> bool {
        debug_assert!(mutation.harness_id.is_none() && mutation.session_id.is_none());
        mutation.network_class = NetworkClass::Manual;
        let Some(project) = mutation.project_id.as_deref() else {
            return true;
        };
        let claim = self
            .windows
            .claim(mutation.timestamp_ms, |id| self.final_id(id) == project);
        if let Some(claim) = claim {
            // Session Pause: the paused session's edit is dropped, never
            // recorded as Manual Work.
            let ts = i64::try_from(mutation.timestamp_ms).unwrap_or(i64::MAX);
            if self
                .pauses
                .is_paused_at(claim.harness_id, &claim.session_id, ts)
            {
                return false;
            }
            mutation.harness_id = Some(claim.harness_id);
            mutation.session_id = Some(claim.session_id);
            mutation.network_class = claim.network_class;
        }
        true
    }

    /// Attributes a watcher `FILE_MUTATION`, re-checks it against the
    /// Traction Event rules and queues it for the writer. Its `project_id`
    /// and `cwd` (the Project Root) come from the watcher; the bus policy
    /// does not apply to the daemon's own events.
    fn handle_mutation(&mut self, mut mutation: TractionEvent) {
        // Held across a migration: store it under the final (`git:`) ID.
        mutation.project_id = mutation.project_id.map(|id| self.final_id(&id));
        if !self.attribute(&mut mutation) {
            return;
        }
        if mutation.validate().is_err() {
            log::warn("dropped a file watcher mutation that is not a valid Traction Event");
            return;
        }
        match EventRow::try_from(mutation) {
            Ok(row) => self.write(WriteOp::Event(Box::new(row))),
            Err(_) => {
                log::warn("dropped a file watcher mutation that does not fit the events table")
            }
        }
    }

    /// Routes `event` and queues it for the writer.
    fn handle_event(&mut self, event: TractionEvent) {
        self.note_hook(&event);
        if let Some(row) = self.routed_row(event) {
            self.write(WriteOp::Event(Box::new(row)));
        }
    }

    /// Routes `event`, feeds it to the attribution windows and converts it
    /// to a row. `None` when it is dropped.
    fn routed_row(&mut self, event: TractionEvent) -> Option<EventRow> {
        if event.event_type == EventType::SessionStart
            && let (Some(harness), Some(session)) = (event.harness_id, &event.session_id)
        {
            self.started.insert((harness, session.clone()));
        }
        let event = self.route(event)?;
        self.windows
            .observe(&event, u64::try_from(now_ms()).unwrap_or_default());
        if self.is_paused(&event) {
            return None;
        }
        match EventRow::try_from(event) {
            Ok(row) => Some(row),
            // The bus ran `EventRow::check` before accepting it, and the
            // daemon's own Adapters build only rows that fit.
            Err(_) => {
                log::warn("dropped a routed event that does not fit the events table");
                None
            }
        }
    }

    /// Records the first accepted hook event of a session in this run (its
    /// `first_hook_ms`), the start of the span the transcript tailer must
    /// not duplicate.
    fn note_hook(&mut self, event: &TractionEvent) {
        if event.attribution_source != AttributionSource::HarnessHook {
            return;
        }
        let (Some(harness), Some(session)) = (event.harness_id, &event.session_id) else {
            return;
        };
        let at_ms = i64::try_from(event.timestamp_ms).unwrap_or(i64::MAX);
        if let Some(row) = self.coverage.note_hook(harness, session, at_ms) {
            self.write(WriteOp::HookRun(row));
        }
    }

    /// Whether a tailed (`harness_log`) event is stored. Dropped: a
    /// synthesized `SESSION_START` once one is stored for its session, and
    /// a record inside a span the session's hooks covered (see
    /// [`HookCoverage`]).
    ///
    /// A tailed record inside a paused span is dropped later, by
    /// [`Self::routed_row`] (pause spans compare the record's own
    /// timestamp).
    fn keeps_tailed(&self, event: &TractionEvent) -> bool {
        debug_assert_eq!(event.attribution_source, AttributionSource::HarnessLog);
        let (Some(harness), Some(session)) = (event.harness_id, &event.session_id) else {
            return true;
        };
        if event.event_type == EventType::SessionStart
            && self.started.contains(&(harness, session.clone()))
        {
            return false;
        }
        let ts = i64::try_from(event.timestamp_ms).unwrap_or(i64::MAX);
        !self.coverage.covers(harness, session, ts)
    }

    /// Routes what the transcript tailer read from one file, in record
    /// order, and queues the events with the file's new offset as one write, so the offset
    /// commits in the same transaction as its events. Dropped events still
    /// advance the offset.
    ///
    /// A recorded pause command is applied even inside a hook-covered span
    /// (Claude Code records one only when no hook handled it), and its
    /// write is queued before the offset that passes it.
    fn handle_tailed(&mut self, items: Vec<TailedItem>, offset: TranscriptOffset) {
        let mut rows = Vec::with_capacity(items.len());
        for item in items {
            match item {
                TailedItem::Event(event) => {
                    if self.keeps_tailed(&event)
                        && let Some(row) = self.routed_row(*event)
                    {
                        rows.push(row);
                    }
                }
                TailedItem::Pause(pause) => {
                    let at_ms = i64::try_from(pause.at_ms).unwrap_or(i64::MAX);
                    if let Some(op) = self.pauses.apply_recorded(
                        pause.action,
                        pause.harness,
                        &pause.session_id,
                        at_ms,
                    ) {
                        self.write(op);
                    }
                }
            }
        }
        self.write(WriteOp::Tailed(TailedBatch {
            events: rows,
            offset,
        }));
    }
}

impl Router {
    /// Whether `event`'s session was paused at its timestamp.
    fn is_paused(&self, event: &TractionEvent) -> bool {
        let (Some(harness), Some(session_id)) = (event.harness_id, &event.session_id) else {
            return false;
        };
        let ts = i64::try_from(event.timestamp_ms).unwrap_or(i64::MAX);
        self.pauses.is_paused_at(harness, session_id, ts)
    }

    /// `/stop-track` / `/resume-track`: pauses or resumes the session,
    /// queueing the span write when its state changed (a repeat is a
    /// no-op).
    fn handle_pause(&mut self, pause: Pause) {
        let at_ms = i64::try_from(pause.at_ms).unwrap_or(i64::MAX);
        if let Some(op) = self
            .pauses
            .apply(pause.action, pause.harness, &pause.session_id, at_ms)
        {
            self.write(op);
        }
    }
}

/// One thing the transcript tailer read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailedItem {
    Event(Box<TractionEvent>),
    Pause(Pause),
}

/// A Session Pause command for one session, received (or, from a
/// transcript, recorded) at `at_ms`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pause {
    pub action: PauseAction,
    pub harness: HarnessId,
    pub session_id: String,
    pub at_ms: u64,
}

/// The pinning key of a canonical common dir (case-folded on Windows).
fn pin_key(common_dir: &Path) -> String {
    let text = common_dir.to_string_lossy();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.into_owned()
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The answer to a registration, sent back to the waiting bus request.
pub type RegisterReply = Result<String, RegisterError>;

enum RouterMessage {
    Event(Box<TractionEvent>),
    /// A held watcher `FILE_MUTATION` whose hold just ended.
    Mutation(Box<TractionEvent>),
    Pause(Pause),
    Register {
        path: PathBuf,
        reply: oneshot::Sender<RegisterReply>,
    },
    /// What the transcript tailer read from one file, with the offset
    /// just past it.
    Tailed {
        items: Vec<TailedItem>,
        offset: TranscriptOffset,
    },
    /// The transcript tailer's first-ever start.
    TailerStart(TailerStartRow),
}

/// The bus side of the Router's queue. Cheap to clone; never blocks.
#[derive(Clone)]
pub struct RouterSender(SyncSender<RouterMessage>);

/// The Router's queue is full or it is shutting down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RouterBusy;

impl RouterSender {
    /// Queues an accepted event for routing and storage.
    pub fn try_route(&self, event: TractionEvent) -> Result<(), RouterBusy> {
        self.try_send(RouterMessage::Event(Box::new(event)))
    }

    /// Queues a Session Pause command, in order with the session's events.
    pub fn try_pause(&self, pause: Pause) -> Result<(), RouterBusy> {
        self.try_send(RouterMessage::Pause(pause))
    }

    /// Queues a registration; the receiver yields the Router's answer.
    pub fn try_register(
        &self,
        path: PathBuf,
    ) -> Result<oneshot::Receiver<RegisterReply>, RouterBusy> {
        let (reply, answer) = oneshot::channel();
        self.try_send(RouterMessage::Register { path, reply })?;
        Ok(answer)
    }

    /// Queues a watcher `FILE_MUTATION` for attribution and storage,
    /// waiting while the queue is full (only the watcher thread calls it).
    pub fn send_mutation(&self, mutation: TractionEvent) -> Result<(), RouterBusy> {
        self.0
            .send(RouterMessage::Mutation(Box::new(mutation)))
            .map_err(|_| RouterBusy)
    }

    /// Queues events read from one transcript with the offset just past
    /// them, waiting while the queue is full (only the tailer thread calls
    /// it).
    pub fn send_tailed(
        &self,
        items: Vec<TailedItem>,
        offset: TranscriptOffset,
    ) -> Result<(), RouterBusy> {
        self.0
            .send(RouterMessage::Tailed { items, offset })
            .map_err(|_| RouterBusy)
    }

    /// Queues the tailer's first-ever start record (waits while full).
    pub fn send_tailer_start(&self, start: TailerStartRow) -> Result<(), RouterBusy> {
        self.0
            .send(RouterMessage::TailerStart(start))
            .map_err(|_| RouterBusy)
    }

    fn try_send(&self, message: RouterMessage) -> Result<(), RouterBusy> {
        self.0.try_send(message).map_err(|error| match error {
            TrySendError::Full(_) | TrySendError::Disconnected(_) => RouterBusy,
        })
    }
}

/// The Router thread. It exits once every [`RouterSender`] is gone and
/// the queue is drained, dropping its writer sender.
pub struct RouterThread(JoinHandle<()>);

impl RouterThread {
    pub fn join(self) -> thread::Result<()> {
        self.0.join()
    }
}

/// Starts the Router thread with a queue of `capacity` messages.
pub fn spawn(router: Router, capacity: usize) -> io::Result<(RouterSender, RouterThread)> {
    let (sender, receiver) = mpsc::sync_channel(capacity.max(1));
    let handle = thread::Builder::new()
        .name("traction-router".to_owned())
        .spawn(move || run(router, &receiver))?;
    Ok((RouterSender(sender), RouterThread(handle)))
}

fn run(mut router: Router, receiver: &Receiver<RouterMessage>) {
    for message in receiver {
        match message {
            RouterMessage::Event(event) => router.handle_event(*event),
            RouterMessage::Mutation(mutation) => router.handle_mutation(*mutation),
            RouterMessage::Pause(pause) => router.handle_pause(pause),
            RouterMessage::Register { path, reply } => {
                // The caller may have given up waiting; nothing to do then.
                let _ = reply.send(router.register_path(&path));
            }
            RouterMessage::Tailed { items, offset } => router.handle_tailed(items, offset),
            RouterMessage::TailerStart(start) => router.write(WriteOp::TailerStart(start)),
        }
    }
}
