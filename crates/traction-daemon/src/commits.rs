//! Commit Observations: noticing a new local commit without ever reading a
//! commit message.
//!
//! For each tracked Git worktree the observer polls the `mtime` of
//! `<git-dir>/logs/HEAD` (the per-worktree git dir, not the common dir) and
//! never opens it: the reflog holds commit messages. When it moves, a commit
//! is recorded only if all three checks pass:
//!
//! 1. `COMMIT_EDITMSG`'s `mtime` moved forward (`stat` only, never opened).
//!    `git commit`, `cherry-pick` and `revert` write it; `checkout`,
//!    `switch`, `pull`, `reset` and fast-forward merges do not.
//! 2. `HEAD`'s first parent is the previous `HEAD` (or `HEAD` is a root
//!    commit and there was no previous one): amends and rebases rewrite
//!    history instead of adding to it.
//! 3. `HEAD`'s committer time is within the Ruleset's `commit_freshness_ms` of
//!    the observation, which drops an old pulled commit.
//!
//! Every Git call goes through the hardened wrapper (`git.rs`); any failure
//! or timeout means "no commit". Only `(project_id, commit_oid, observed_ms)`
//! is stored.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};

use traction_core::{CommitObservation, Ruleset};

use crate::clock::now_ms;
use crate::git::Git;
use crate::storage::WriteOp;
use crate::writer::WriterSender;

/// Commit observer tuning. Production uses the defaults; tests shorten the
/// poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct CommitSettings {
    pub enabled: bool,
    /// How often each tracked worktree's `logs/HEAD` is `stat`ed.
    pub poll: Duration,
}

impl Default for CommitSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            poll: Duration::from_secs(2),
        }
    }
}

enum Msg {
    Watch { project_id: String, folder: PathBuf },
    Stop,
}

/// Tells the observer about a folder in a Git Project (cheap, cloneable).
#[derive(Clone)]
pub struct CommitWatch(Sender<Msg>);

impl CommitWatch {
    /// A folder (any folder inside a worktree) of Git Project `project_id`.
    pub fn watch(&self, project_id: &str, folder: &Path) {
        let _ = self.0.send(Msg::Watch {
            project_id: project_id.to_owned(),
            folder: folder.to_owned(),
        });
    }
}

pub struct CommitObserver {
    tx: Sender<Msg>,
    handle: JoinHandle<()>,
}

impl CommitObserver {
    pub fn watch_handle(&self) -> CommitWatch {
        CommitWatch(self.tx.clone())
    }

    /// Stops the thread and waits for it (dropping its writer sender).
    pub fn stop(self) -> thread::Result<()> {
        let _ = self.tx.send(Msg::Stop);
        self.handle.join()
    }
}

struct Tracked {
    project_id: String,
    folder: PathBuf,
    git_dir: PathBuf,
    logs_head: Option<SystemTime>,
    edit_msg: Option<SystemTime>,
    head: Option<String>,
}

fn mtime(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

pub fn spawn(
    rules: &'static Ruleset,
    git: Arc<Git>,
    writer: WriterSender,
    settings: CommitSettings,
) -> io::Result<CommitObserver> {
    let (tx, rx) = mpsc::channel();
    let handle = thread::Builder::new()
        .name("traction-commits".to_owned())
        .spawn(move || {
            let mut tracked: HashMap<PathBuf, Tracked> = HashMap::new();
            loop {
                match rx.recv_timeout(settings.poll) {
                    Ok(Msg::Watch { project_id, folder }) => {
                        track(&git, &mut tracked, project_id, folder);
                    }
                    Ok(Msg::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                    Err(RecvTimeoutError::Timeout) => {
                        for t in tracked.values_mut() {
                            if let Some(commit) = poll(rules, &git, t, now_ms())
                                && writer.send(WriteOp::Commit(commit)).is_err()
                            {
                                return;
                            }
                        }
                    }
                }
            }
        })?;
    Ok(CommitObserver { tx, handle })
}

/// Starts tracking `folder`'s worktree, with its current state as the
/// baseline (a commit that already exists is never observed).
fn track(git: &Git, tracked: &mut HashMap<PathBuf, Tracked>, project_id: String, folder: PathBuf) {
    let Ok(git_dir) = git.git_dir(&folder) else {
        return;
    };
    if tracked.contains_key(&git_dir) {
        return;
    }
    let head = git.head(&folder).ok().flatten();
    tracked.insert(
        git_dir.clone(),
        Tracked {
            project_id,
            logs_head: mtime(&git_dir.join("logs").join("HEAD")),
            edit_msg: mtime(&git_dir.join("COMMIT_EDITMSG")),
            folder,
            git_dir,
            head,
        },
    );
}

/// One poll of one worktree. Returns the commit when all three checks pass.
fn poll(
    rules: &Ruleset,
    git: &Git,
    t: &mut Tracked,
    observed_ms: u64,
) -> Option<CommitObservation> {
    let logs_head = mtime(&t.git_dir.join("logs").join("HEAD"));
    if logs_head == t.logs_head {
        return None;
    }
    t.logs_head = logs_head;

    // Check 1: COMMIT_EDITMSG moved forward (stat only).
    let edit_msg = mtime(&t.git_dir.join("COMMIT_EDITMSG"));
    let moved = match (t.edit_msg, edit_msg) {
        (None, Some(_)) => true,
        (Some(before), Some(after)) => after > before,
        _ => false,
    };
    t.edit_msg = edit_msg;

    // A failed or timed-out query keeps the old baseline: no commit.
    let new_head = git.head(&t.folder).ok()?;
    if new_head == t.head {
        return None;
    }
    let previous = std::mem::replace(&mut t.head, new_head.clone());
    let new_head = new_head?;
    if !moved {
        return None;
    }

    // Check 2: the new HEAD's first parent is the previous HEAD, or it is a
    // root commit with no previous HEAD.
    let adds_to_history = match previous {
        Some(previous) => git.head_parent(&t.folder).ok().flatten() == Some(previous),
        None => git.head_is_root(&t.folder).unwrap_or(false),
    };
    if !adds_to_history {
        return None;
    }

    // Check 3: the committer time is fresh.
    let committed_ms = git.head_committer_time(&t.folder).ok()?.checked_mul(1000)?;
    if committed_ms.abs_diff(observed_ms) > rules.commit_freshness_ms {
        return None;
    }
    Some(CommitObservation {
        project_id: t.project_id.clone(),
        commit_oid: new_head,
        observed_ms,
    })
}
