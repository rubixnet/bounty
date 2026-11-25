//! The hardened Git wrapper: the only place the daemon runs `git`.
//!
//! Every repo is hostile: its `.git/config` can name programs Git would run.
//! So callers get a closed set of typed read-only queries ([`GitQuery`]),
//! never free-form arguments, and every invocation carries a fixed hardening
//! prefix ([`HARDENED_PREFIX`]), a scrubbed `GIT_*` environment, closed
//! stdin, discarded stderr, a stdout cap and a hard kill-and-reap timeout.
//! `safe.bareRepository=explicit` needs Git >= 2.38 (older Git ignores it).

use crate::log;
use crate::paths;

use std::ffi::OsString;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A closed set of read-only Git queries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitQuery {
    CommonDir,
    TopLevel,
    RootCommits,
    OriginUrl,
}

/// `core.hooksPath` pointed at the OS null device, chosen at compile time so
/// Git for Windows never resolves a literal `dev/null` path.
#[cfg(not(windows))]
const HOOKS_PATH_CONFIG: &str = "core.hooksPath=/dev/null";
#[cfg(windows)]
const HOOKS_PATH_CONFIG: &str = "core.hooksPath=NUL";

/// The fixed hardening prefix every invocation carries, before `-C <folder>`.
const HARDENED_PREFIX: [&str; 8] = [
    "--no-pager",
    "--no-optional-locks",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "safe.bareRepository=explicit",
    "-c",
    HOOKS_PATH_CONFIG,
];

impl GitQuery {
    /// The read-only plumbing subcommand for this query (the allowlist).
    fn subcommand(self) -> &'static [&'static str] {
        match self {
            Self::CommonDir => &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            Self::TopLevel => &["rev-parse", "--show-toplevel"],
            Self::RootCommits => &["rev-list", "--max-parents=0", "HEAD"],
            Self::OriginUrl => &["config", "--get", "remote.origin.url"],
        }
    }
}

/// Why a query has no answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GitError {
    /// No usable `git` binary; the Router falls back to markers and the
    /// session folder. Logged once per [`Git`].
    Unavailable,
    /// The repo's top-level is exactly `$HOME` (a dotfile repo): refused.
    HomeRooted,
    /// Not inside a non-bare Git work tree, or Git refused or failed.
    NotARepo,
    /// Git outlived [`GIT_TIMEOUT`] and was killed and reaped. Callers treat
    /// this as "not a Git repo" for that resolution.
    TimedOut,
}

/// The daemon's Git client. Construct one and share it; it is `Sync`.
#[derive(Debug)]
pub struct Git {
    program: PathBuf,
    /// The user's home folder, canonicalized when possible.
    home: Option<PathBuf>,
    timeout: Duration,
    unavailable_logged: AtomicBool,
}

impl Git {
    /// A client running `git` from `PATH`, refusing repos rooted at `home`.
    #[cfg(test)]
    pub fn new(home: Option<&Path>) -> Self {
        Self::with_program(Path::new("git"), home)
    }

    /// A client running `program` (the daemon's configured `git`).
    pub fn with_program(program: &Path, home: Option<&Path>) -> Self {
        Self {
            program: program.to_owned(),
            home: home.map(paths::canonical_or_lexical),
            timeout: GIT_TIMEOUT,
            unavailable_logged: AtomicBool::new(false),
        }
    }

    /// The work tree's top-level folder containing `folder`, as Git prints
    /// it (forward slashes on Windows). Production never needs it on its
    /// own (the Router collapses worktrees via [`Git::common_dir`]); every
    /// query still runs the same `$HOME` check through
    /// [`Git::checked_top_level`].
    #[cfg(test)]
    pub fn top_level(&self, folder: &Path) -> Result<PathBuf, GitError> {
        self.checked_top_level(folder)
    }

    /// The absolute common Git dir: the same for every worktree of a repo.
    pub fn common_dir(&self, folder: &Path) -> Result<PathBuf, GitError> {
        self.checked_top_level(folder)?;
        self.run(GitQuery::CommonDir, folder).map(PathBuf::from)
    }

    /// The lexicographically smallest root commit of `HEAD`, or `None` when
    /// the repo has no commits yet.
    pub fn root_commit(&self, folder: &Path) -> Result<Option<String>, GitError> {
        self.checked_top_level(folder)?;
        match self.run(GitQuery::RootCommits, folder) {
            Ok(out) => Ok(out
                .lines()
                .map(str::trim)
                .filter(|line| is_object_id(line))
                .min()
                .map(str::to_owned)),
            Err(GitError::NotARepo) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `remote.origin.url` as configured (not normalized), if any.
    pub fn origin_url(&self, folder: &Path) -> Result<Option<String>, GitError> {
        self.checked_top_level(folder)?;
        match self.run(GitQuery::OriginUrl, folder) {
            Ok(url) if url.is_empty() => Ok(None),
            Ok(url) => Ok(Some(url)),
            Err(GitError::NotARepo) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Refuses a `$HOME`-rooted repo: lexically on the discovered `.git`
    /// parent first (before Git runs at all), then confirmed from
    /// [`GitQuery::TopLevel`].
    fn checked_top_level(&self, folder: &Path) -> Result<PathBuf, GitError> {
        let folder_canon = paths::canonicalize(folder).ok_or(GitError::NotARepo)?;
        let is_home = |dir: &Path| {
            self.home
                .as_deref()
                .is_some_and(|home| paths::same(dir, home))
        };
        let dot_git_parent = folder_canon
            .ancestors()
            .find(|dir| std::fs::symlink_metadata(dir.join(".git")).is_ok());
        if dot_git_parent.is_some_and(is_home) {
            return Err(GitError::HomeRooted);
        }
        let top = PathBuf::from(self.run(GitQuery::TopLevel, folder)?);
        if is_home(&paths::canonical_or_lexical(&top)) {
            return Err(GitError::HomeRooted);
        }
        Ok(top)
    }

    /// Runs one query and returns its stdout minus the trailing newline.
    fn run(&self, query: GitQuery, folder: &Path) -> Result<String, GitError> {
        let mut cmd = command(
            &self.program,
            query,
            folder,
            std::env::vars_os().map(|(key, _)| key),
        );
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let Ok(child) = cmd.spawn() else {
            if !self.unavailable_logged.swap(true, Ordering::Relaxed) {
                log::warn("git is unavailable; Project routing falls back to markers");
            }
            return Err(GitError::Unavailable);
        };
        match wait_bounded(child, self.timeout) {
            Ok(stdout) => {
                let text = String::from_utf8(stdout).map_err(|_| GitError::NotARepo)?;
                Ok(text.trim_end_matches(['\n', '\r']).to_owned())
            }
            Err(RunError::TimedOut) => {
                log::warn("git query timed out and was killed; treating as not a Git repo");
                Err(GitError::TimedOut)
            }
            Err(RunError::TooLarge) => {
                log::warn("git query output exceeded the cap; treating as not a Git repo");
                Err(GitError::NotARepo)
            }
            Err(RunError::Failed) => Err(GitError::NotARepo),
        }
    }
}

/// A full SHA-1 or SHA-256 object id.
fn is_object_id(s: &str) -> bool {
    matches!(s.len(), 40 | 64) && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// The production hard timeout for one `git` invocation.
pub const GIT_TIMEOUT: Duration = Duration::from_secs(5);

/// The most stdout one query may produce; anything longer is refused.
pub const MAX_STDOUT_BYTES: u64 = 16 * 1024;

/// How often [`wait_bounded`] checks whether the child has exited.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[derive(Debug)]
pub(crate) enum RunError {
    /// The child outlived the timeout; it was killed and reaped.
    TimedOut,
    /// The child exited non-zero, or waiting on it failed.
    Failed,
    /// The child wrote more than [`MAX_STDOUT_BYTES`].
    TooLarge,
}

/// Waits for `child` (stdout piped) for at most `timeout`, then kills and
/// reaps it. Stdout is read on a helper thread, capped at
/// [`MAX_STDOUT_BYTES`] (the pipe is dropped past the cap). That thread is
/// never joined: its result is awaited only until the same deadline, so a
/// grandchild holding the pipe open can never hang the caller.
pub(crate) fn wait_bounded(mut child: Child, timeout: Duration) -> Result<Vec<u8>, RunError> {
    let deadline = Instant::now() + timeout;
    let (tx, rx) = mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let read = stdout.take(MAX_STDOUT_BYTES + 1).read_to_end(&mut buf);
            let _ = tx.send(read.map(|_| buf));
        });
    } else {
        let _ = tx.send(Ok(Vec::new()));
    }
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::TimedOut);
            }
            Ok(None) => std::thread::sleep(POLL_INTERVAL),
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(RunError::Failed);
            }
        }
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    let stdout = match rx.recv_timeout(remaining) {
        Ok(Ok(buf)) => buf,
        Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => return Err(RunError::Failed),
        Err(mpsc::RecvTimeoutError::Timeout) => return Err(RunError::TimedOut),
    };
    if !status.success() {
        return Err(RunError::Failed);
    }
    if stdout.len() as u64 > MAX_STDOUT_BYTES {
        return Err(RunError::TooLarge);
    }
    Ok(stdout)
}

fn command(
    program: &Path,
    query: GitQuery,
    folder: &Path,
    inherited_env_keys: impl Iterator<Item = OsString>,
) -> Command {
    let mut cmd = Command::new(program);
    for key in inherited_env_keys {
        // Case-insensitive: Windows environment keys are.
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            cmd.env_remove(key);
        }
    }
    cmd.env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0");
    cmd.args(HARDENED_PREFIX)
        .arg("-C")
        .arg(folder)
        .args(query.subcommand());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::{OsStr, OsString};
    use std::process::Child;

    const ALL_QUERIES: [GitQuery; 4] = [
        GitQuery::CommonDir,
        GitQuery::TopLevel,
        GitQuery::RootCommits,
        GitQuery::OriginUrl,
    ];

    #[cfg(not(windows))]
    const EXPECTED_HOOKS: &str = "core.hooksPath=/dev/null";
    #[cfg(windows)]
    const EXPECTED_HOOKS: &str = "core.hooksPath=NUL";

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|a| a.to_str().unwrap().to_owned())
            .collect()
    }

    #[test]
    fn every_query_runs_the_exact_hardened_argv() {
        let folder = Path::new("some/project");
        let cases: [(GitQuery, &[&str]); 4] = [
            (
                GitQuery::CommonDir,
                &["rev-parse", "--path-format=absolute", "--git-common-dir"],
            ),
            (GitQuery::TopLevel, &["rev-parse", "--show-toplevel"]),
            (
                GitQuery::RootCommits,
                &["rev-list", "--max-parents=0", "HEAD"],
            ),
            (
                GitQuery::OriginUrl,
                &["config", "--get", "remote.origin.url"],
            ),
        ];
        for (query, tail) in cases {
            let cmd = command(Path::new("git"), query, folder, std::iter::empty());
            assert_eq!(cmd.get_program(), OsStr::new("git"));
            let mut expected = vec![
                "--no-pager",
                "--no-optional-locks",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "safe.bareRepository=explicit",
                "-c",
                EXPECTED_HOOKS,
                "-C",
                "some/project",
            ];
            expected.extend_from_slice(tail);
            assert_eq!(args_of(&cmd), expected, "{query:?}");
        }
    }

    #[test]
    fn every_query_clears_inherited_git_env_and_sets_only_the_safe_vars() {
        let inherited = [
            "GIT_DIR",
            "GIT_CONFIG_PARAMETERS",
            "GIT_SSH_COMMAND",
            "PATH",
        ];
        for query in ALL_QUERIES {
            let cmd = command(
                Path::new("git"),
                query,
                Path::new("."),
                inherited.iter().map(OsString::from),
            );
            let mut set: Vec<(String, String)> = Vec::new();
            let mut removed: Vec<String> = Vec::new();
            for (k, v) in cmd.get_envs() {
                let k = k.to_str().unwrap().to_owned();
                match v {
                    Some(v) => set.push((k, v.to_str().unwrap().to_owned())),
                    None => removed.push(k),
                }
            }
            set.sort();
            removed.sort();
            assert_eq!(
                set,
                [
                    ("GIT_OPTIONAL_LOCKS".to_owned(), "0".to_owned()),
                    ("GIT_TERMINAL_PROMPT".to_owned(), "0".to_owned()),
                ],
                "{query:?}"
            );
            assert_eq!(
                removed,
                ["GIT_CONFIG_PARAMETERS", "GIT_DIR", "GIT_SSH_COMMAND"],
                "{query:?}"
            );
        }
    }

    /// A real child that would run for 30 s and never exit on its own in time.
    fn sleeping_child() -> Child {
        #[cfg(not(windows))]
        let mut cmd = {
            let mut c = Command::new("sleep");
            c.arg("30");
            c
        };
        #[cfg(windows)]
        let mut cmd = {
            let mut c = Command::new("ping");
            c.args(["-n", "30", "127.0.0.1"]);
            c
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleeping child")
    }

    #[test]
    fn production_timeout_is_five_seconds() {
        assert_eq!(GIT_TIMEOUT, Duration::from_secs(5));
    }

    #[test]
    fn a_child_hanging_past_the_timeout_is_killed_and_reaped() {
        let child = sleeping_child();
        let pid = child.id();
        let started = Instant::now();
        let outcome = wait_bounded(child, Duration::from_millis(200));
        let elapsed = started.elapsed();
        assert!(matches!(outcome, Err(RunError::TimedOut)), "{outcome:?}");
        assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
        // Reaped: the pid no longer names any process (not even a zombie).
        #[cfg(unix)]
        {
            let alive = Command::new("kill")
                .args(["-0", &pid.to_string()])
                .stderr(Stdio::null())
                .status()
                .expect("run kill -0");
            assert!(!alive.success(), "pid {pid} still exists");
        }
        #[cfg(windows)]
        {
            let out = Command::new("tasklist")
                .args(["/FI", &format!("PID eq {pid}"), "/NH"])
                .output()
                .expect("run tasklist");
            let listing = String::from_utf8_lossy(&out.stdout);
            assert!(
                !listing.contains(&format!(" {pid} ")),
                "pid {pid} still exists"
            );
        }
    }

    /// Fixture-only: runs the real `git` directly (never through the wrapper)
    /// with an identity, so fixtures can commit on any CI runner.
    fn fixture_git(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args([
                "-c",
                "commit.gpgsign=false",
                "-c",
                "init.defaultBranch=main",
            ])
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run fixture git");
        assert!(status.success(), "fixture git {args:?} failed");
    }

    /// A real repo with one commit and an `origin`, below `root`.
    fn plain_repo(root: &Path) -> PathBuf {
        let repo = root.join("project");
        std::fs::create_dir_all(repo.join("src")).unwrap();
        fixture_git(&repo, &["init", "-q"]);
        fixture_git(&repo, &["commit", "-q", "--allow-empty", "-m", "first"]);
        fixture_git(
            &repo,
            &[
                "remote",
                "add",
                "origin",
                "https://example.invalid/acme/project.git",
            ],
        );
        repo
    }

    /// Git prints paths its own way (forward slashes on Windows).
    fn same_path(a: &Path, b: &Path) -> bool {
        paths::same(
            &paths::canonicalize(a).unwrap(),
            &paths::canonicalize(b).unwrap(),
        )
    }

    #[test]
    fn queries_answer_from_a_real_repo_subfolder() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = plain_repo(tmp.path());
        let git = Git::new(None);
        let sub = repo.join("src");

        assert!(same_path(&git.top_level(&sub).unwrap(), &repo));
        assert!(same_path(
            &git.common_dir(&sub).unwrap(),
            &repo.join(".git")
        ));
        let root = git.root_commit(&sub).unwrap().expect("has a commit");
        assert_eq!(root.len(), 40);
        assert!(root.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_eq!(
            git.origin_url(&sub).unwrap().as_deref(),
            Some("https://example.invalid/acme/project.git")
        );
    }

    #[test]
    fn a_folder_outside_any_repo_is_not_a_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let git = Git::new(None);
        assert_eq!(git.top_level(tmp.path()), Err(GitError::NotARepo));
    }

    #[test]
    fn a_repo_with_no_commits_has_no_root_commit_and_no_origin() {
        let tmp = tempfile::tempdir().unwrap();
        fixture_git(tmp.path(), &["init", "-q"]);
        let git = Git::new(None);
        assert_eq!(git.root_commit(tmp.path()), Ok(None));
        assert_eq!(git.origin_url(tmp.path()), Ok(None));
    }

    /// A path as Git's `sh` (also Git for Windows' bundled `sh`) wants it.
    fn sh_path(path: &Path) -> String {
        path.to_str().unwrap().replace('\\', "/")
    }

    /// Hooks a hostile repo plants; none is run by read-only plumbing.
    const HOSTILE_HOOKS: [&str; 6] = [
        "pre-commit",
        "post-checkout",
        "post-index-change",
        "reference-transaction",
        "pre-auto-gc",
        "post-rewrite",
    ];

    /// Writes hooks into `dir` that each create a sentinel in `sentinels`:
    /// `sh` scripts (run by Git on every OS, incl. Git for Windows) and, on
    /// Windows, `.bat`/`.cmd` twins.
    fn plant_hooks(dir: &Path, sentinels: &Path, tag: &str) {
        std::fs::create_dir_all(dir).unwrap();
        for hook in HOSTILE_HOOKS {
            let script = dir.join(hook);
            let sentinel = sh_path(&sentinels.join(format!("{tag}-{hook}")));
            std::fs::write(&script, format!("#!/bin/sh\necho pwned > '{sentinel}'\n")).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            #[cfg(windows)]
            for ext in ["bat", "cmd"] {
                let sentinel = sentinels.join(format!("{tag}-{hook}-{ext}"));
                let body = format!("@echo pwned > \"{}\"\r\n", sentinel.display());
                std::fs::write(dir.join(format!("{hook}.{ext}")), body).unwrap();
            }
        }
    }

    /// A real repo whose config names hostile programs for `core.fsmonitor`,
    /// `core.hooksPath` and `core.pager`, plus hostile `.git/hooks/*`.
    /// Returns `(repo, sentinels)`; any file in `sentinels` means code ran.
    fn hostile_repo(root: &Path) -> (PathBuf, PathBuf) {
        let repo = plain_repo(root);
        let sentinels = root.join("sentinels");
        std::fs::create_dir_all(&sentinels).unwrap();
        let hooks = root.join("hostile-hooks");
        plant_hooks(&hooks, &sentinels, "hooks-path");
        plant_hooks(
            &repo.join(".git").join("hooks"),
            &sentinels,
            "dot-git-hooks",
        );
        let fsmonitor = format!("echo pwned > '{}'", sh_path(&sentinels.join("fsmonitor")));
        let pager = format!("echo pwned > '{}'", sh_path(&sentinels.join("pager")));
        fixture_git(&repo, &["config", "core.fsmonitor", &fsmonitor]);
        fixture_git(&repo, &["config", "core.hooksPath", &sh_path(&hooks)]);
        fixture_git(&repo, &["config", "core.pager", &pager]);
        (repo, sentinels)
    }

    fn sentinel_names(sentinels: &Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(sentinels)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    /// Runs unhardened `git` (fixture only), ignoring its exit status.
    fn unhardened_git(dir: &Path, args: &[&str]) {
        let _ = Command::new("git")
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
            ])
            .args(["-c", "commit.gpgsign=false"])
            .arg("-C")
            .arg(dir)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("run unhardened git");
    }

    #[test]
    fn control_the_hostile_fixture_really_runs_code_under_plain_git() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, sentinels) = hostile_repo(tmp.path());
        unhardened_git(&repo, &["status"]);
        unhardened_git(&repo, &["commit", "-q", "--allow-empty", "-m", "control"]);
        let fired = sentinel_names(&sentinels);
        assert!(fired.iter().any(|n| n == "fsmonitor"), "{fired:?}");
        assert!(
            fired.iter().any(|n| n == "hooks-path-pre-commit"),
            "{fired:?}"
        );
    }

    #[test]
    fn red_team_every_query_on_a_hostile_repo_runs_no_repo_code() {
        let tmp = tempfile::tempdir().unwrap();
        let (repo, sentinels) = hostile_repo(tmp.path());
        let git = Git::new(None);
        for folder in [repo.clone(), repo.join("src")] {
            assert!(same_path(&git.top_level(&folder).unwrap(), &repo));
            assert!(same_path(
                &git.common_dir(&folder).unwrap(),
                &repo.join(".git")
            ));
            assert!(git.root_commit(&folder).unwrap().is_some());
            assert!(git.origin_url(&folder).unwrap().is_some());
        }
        assert_eq!(sentinel_names(&sentinels), Vec::<String>::new());
        // The per-OS null device never turns into a literal `dev/null` path.
        for base in [tmp.path(), repo.as_path(), repo.join(".git").as_path()] {
            assert!(!base.join("dev").exists(), "{} has dev/", base.display());
        }
    }

    #[test]
    fn a_bare_repo_with_hostile_config_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("bare.git");
        std::fs::create_dir_all(&bare).unwrap();
        fixture_git(&bare, &["init", "-q", "--bare"]);
        let sentinels = tmp.path().join("sentinels");
        std::fs::create_dir_all(&sentinels).unwrap();
        let fsmonitor = format!("echo pwned > '{}'", sh_path(&sentinels.join("fsmonitor")));
        fixture_git(&bare, &["config", "core.fsmonitor", &fsmonitor]);
        fixture_git(
            &bare,
            &[
                "config",
                "remote.origin.url",
                "https://example.invalid/x.git",
            ],
        );

        // Each raw hardened query refuses the bare repo
        // (`safe.bareRepository=explicit`, Git >= 2.38) ...
        for query in ALL_QUERIES {
            let mut cmd = command(
                Path::new("git"),
                query,
                &bare,
                std::env::vars_os().map(|(k, _)| k),
            );
            cmd.stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null());
            let outcome = wait_bounded(cmd.spawn().unwrap(), GIT_TIMEOUT);
            assert!(
                matches!(outcome, Err(RunError::Failed)),
                "{query:?}: {outcome:?}"
            );
        }
        // ... and the public API treats it as not-a-repo.
        let git = Git::new(None);
        assert_eq!(git.top_level(&bare), Err(GitError::NotARepo));
        assert_eq!(git.common_dir(&bare), Err(GitError::NotARepo));
        assert_eq!(git.root_commit(&bare), Err(GitError::NotARepo));
        assert_eq!(git.origin_url(&bare), Err(GitError::NotARepo));
        assert_eq!(sentinel_names(&sentinels), Vec::<String>::new());
    }

    #[test]
    fn a_repo_rooted_at_home_is_refused_before_git_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        std::fs::create_dir_all(home.join("notes")).unwrap();
        fixture_git(&home, &["init", "-q"]);
        let project = plain_repo(&home);

        let git = Git::new(Some(&home));
        for folder in [home.clone(), home.join("notes")] {
            assert_eq!(git.top_level(&folder), Err(GitError::HomeRooted));
            assert_eq!(git.common_dir(&folder), Err(GitError::HomeRooted));
            assert_eq!(git.root_commit(&folder), Err(GitError::HomeRooted));
            assert_eq!(git.origin_url(&folder), Err(GitError::HomeRooted));
        }
        // A real repo below `$HOME` is fine.
        assert!(same_path(&git.top_level(&project).unwrap(), &project));

        // Refused lexically, before any `git` runs: even with no usable
        // binary the answer is HomeRooted, not Unavailable.
        let no_git = Git::with_program(Path::new("traction-test-no-such-git"), Some(&home));
        assert_eq!(
            no_git.top_level(&home.join("notes")),
            Err(GitError::HomeRooted)
        );
    }

    #[test]
    fn a_missing_git_binary_is_unavailable_without_panicking() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = plain_repo(tmp.path());
        let git = Git::with_program(Path::new("traction-test-no-such-git"), None);
        for _ in 0..2 {
            assert_eq!(git.top_level(&repo), Err(GitError::Unavailable));
            assert_eq!(git.common_dir(&repo), Err(GitError::Unavailable));
            assert_eq!(git.root_commit(&repo), Err(GitError::Unavailable));
            assert_eq!(git.origin_url(&repo), Err(GitError::Unavailable));
        }
    }
}
