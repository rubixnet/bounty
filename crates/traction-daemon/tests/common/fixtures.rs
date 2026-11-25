//! Fixture builder for routing tests: real Git repos (built with the real,
//! unhardened `git`, since the test builds even the hostile ones itself),
//! worktrees, marker folders and hostile repos whose code must never run.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Runs the real `git` with a fixed identity so fixtures can commit on any
/// CI runner. Panics on failure.
pub fn git(dir: &Path, args: &[&str]) {
    let status = git_command(dir, args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run fixture git");
    assert!(status.success(), "fixture git {args:?} failed");
}

/// The trimmed stdout of a fixture `git` call.
pub fn git_output(dir: &Path, args: &[&str]) -> String {
    let output = git_command(dir, args)
        .stderr(Stdio::null())
        .output()
        .expect("run fixture git");
    assert!(output.status.success(), "fixture git {args:?} failed");
    String::from_utf8(output.stdout)
        .expect("UTF-8 git output")
        .trim()
        .to_owned()
}

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.args([
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "init.defaultBranch=main",
    ])
    .arg("-C")
    .arg(dir)
    .args(args)
    .stdin(Stdio::null());
    cmd
}

/// A real repo at `path` with a committed `src/` file (so `.git/index`
/// exists and worktrees get `src/` too) and, when given, an `origin`
/// remote. The commit names the folder, so every repo has its own root.
pub fn repo(path: &Path, origin: Option<&str>) -> PathBuf {
    std::fs::create_dir_all(path.join("src")).expect("create repo folder");
    std::fs::write(path.join("src").join("main.txt"), "fixture\n").expect("write file");
    git(path, &["init", "-q"]);
    git(path, &["add", "src"]);
    let message = format!("first commit of {}", path.display());
    git(path, &["commit", "-q", "-m", &message]);
    if let Some(origin) = origin {
        git(path, &["remote", "add", "origin", origin]);
    }
    path.to_path_buf()
}

/// The lexicographically smallest root commit of `repo`'s `HEAD`.
pub fn root_commit(repo: &Path) -> String {
    git_output(repo, &["rev-list", "--max-parents=0", "HEAD"])
        .lines()
        .min()
        .expect("a root commit")
        .to_owned()
}

/// The `git:` ID the daemon must derive for `repo`.
pub fn git_id(repo: &Path, origin: Option<&str>) -> String {
    traction_core::git_project_id(&root_commit(repo), origin)
}

/// Adds a linked worktree of `repo` at `path` on a new branch.
pub fn worktree(repo: &Path, path: &Path, branch: &str) -> PathBuf {
    let target = path.to_str().expect("UTF-8 path");
    git(repo, &["worktree", "add", "-q", "-b", branch, target]);
    path.to_path_buf()
}

/// A folder at `path` holding the given (empty) marker files.
pub fn folder(path: &Path, files: &[&str]) -> PathBuf {
    std::fs::create_dir_all(path).expect("create folder");
    for file in files {
        std::fs::write(path.join(file), "{}\n").expect("write marker");
    }
    path.to_path_buf()
}

/// A path as Git's `sh` (also Git for Windows' bundled `sh`) wants it.
fn sh_path(path: &Path) -> String {
    path.to_str().expect("UTF-8 path").replace('\\', "/")
}

/// Hooks a hostile repo plants; none may ever run.
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
    std::fs::create_dir_all(dir).expect("create hooks folder");
    for hook in HOSTILE_HOOKS {
        let script = dir.join(hook);
        let sentinel = sh_path(&sentinels.join(format!("{tag}-{hook}")));
        std::fs::write(&script, format!("#!/bin/sh\necho pwned > '{sentinel}'\n"))
            .expect("write hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))
                .expect("chmod hook");
        }
        #[cfg(windows)]
        for ext in ["bat", "cmd"] {
            let sentinel = sentinels.join(format!("{tag}-{hook}-{ext}"));
            let body = format!("@echo pwned > \"{}\"\r\n", sentinel.display());
            std::fs::write(dir.join(format!("{hook}.{ext}")), body).expect("write hook");
        }
    }
}

/// The `core.fsmonitor` value that writes `sentinel` when Git runs it
/// (through its `sh`, also on Git for Windows).
fn fsmonitor_command(sentinel: &Path) -> String {
    format!("echo pwned > '{}'", sh_path(sentinel))
}

/// A hostile repo at `path`: its config names sentinel-writing programs
/// for `core.fsmonitor`, `core.hooksPath` and `core.pager`, and its
/// `.git/hooks/*` do the same. Sentinels land in `sentinels` (created).
pub fn hostile_repo(path: &Path, sentinels: &Path) -> PathBuf {
    std::fs::create_dir_all(sentinels).expect("create sentinels folder");
    let repo = repo(path, Some("https://example.invalid/evil/repo.git"));
    let hooks = path.with_file_name(format!(
        "{}-hooks",
        path.file_name().unwrap().to_str().unwrap()
    ));
    plant_hooks(&hooks, sentinels, "hooks-path");
    plant_hooks(&repo.join(".git").join("hooks"), sentinels, "dot-git-hooks");
    git(
        &repo,
        &[
            "config",
            "core.fsmonitor",
            &fsmonitor_command(&sentinels.join("fsmonitor")),
        ],
    );
    git(&repo, &["config", "core.hooksPath", &sh_path(&hooks)]);
    git(
        &repo,
        &[
            "config",
            "core.pager",
            &format!("echo pwned > '{}'", sh_path(&sentinels.join("pager"))),
        ],
    );
    repo
}

/// A bare repo at `path` with a hostile `core.fsmonitor`.
pub fn hostile_bare_repo(path: &Path, sentinels: &Path) -> PathBuf {
    std::fs::create_dir_all(path).expect("create bare repo folder");
    std::fs::create_dir_all(sentinels).expect("create sentinels folder");
    git(path, &["init", "-q", "--bare"]);
    git(
        path,
        &[
            "config",
            "core.fsmonitor",
            &fsmonitor_command(&sentinels.join("bare-fsmonitor")),
        ],
    );
    path.to_path_buf()
}

/// The files in `sentinels`; any entry means repo code ran.
pub fn sentinels_fired(sentinels: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(sentinels)
        .expect("read sentinels")
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

/// Every `dev/null` path (file or folder) at or below `root`: what a
/// literal `core.hooksPath=/dev/null` turns into under Git for Windows.
pub fn dev_null_paths(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if dir.join("dev").join("null").exists() {
            found.push(dir.join("dev").join("null"));
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                stack.push(entry.path());
            }
        }
    }
    found
}
