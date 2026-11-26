//! Source-level guards over the whole workspace. Banned strings are
//! assembled from pieces so this file does not trip its own scan.

use std::path::{Path, PathBuf};

const SOURCE_EXTENSIONS: [&str; 8] = ["rs", "toml", "ts", "js", "json", "yml", "yaml", "sh"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("workspace root")
}

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let path = entry.expect("entry").path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if path.is_dir() {
            if !matches!(
                name,
                "target" | "node_modules" | ".git" | ".claude" | ".scratch"
            ) {
                sources(&path, out);
            }
        } else if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| SOURCE_EXTENSIONS.contains(&e))
            && name != "Cargo.lock"
        {
            out.push(path);
        }
    }
}

fn workspace_sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    for top in ["crates", "packages", "apps", ".github"] {
        let dir = workspace_root().join(top);
        if dir.is_dir() {
            sources(&dir, &mut files);
        }
    }
    assert!(files.len() > 20, "found the workspace sources");
    files
        .into_iter()
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|t| (p, t)))
        .collect()
}

/// No WakaTime compatibility before Step 6 (`AGENTS.md` rule 3): nothing in
/// the workspace may mention its config file or its folder.
#[test]
fn no_workspace_source_mentions_the_wakatime_config_or_folder() {
    let banned = [[".waka", "time"].concat()];
    for (path, text) in workspace_sources() {
        for name in &banned {
            assert!(
                !text.to_ascii_lowercase().contains(name.as_str()),
                "{} mentions {name}",
                path.display()
            );
        }
    }
}

/// Every specificity and credit tuning number lives in `TRACTION_RULES_1`:
/// the daemon may not re-introduce its own.
#[test]
fn no_specificity_tuning_constant_lives_outside_the_ruleset() {
    let banned = [
        ["const MIN_WORD", "_CHARS"].concat(),
        ["const MIN_STEM", "_CHARS"].concat(),
        ["const SEED_VISIT", "_FACTOR"].concat(),
    ];
    for (path, text) in workspace_sources() {
        for name in &banned {
            assert!(
                !text.contains(name.as_str()),
                "{} defines {name}",
                path.display()
            );
        }
    }
}

/// How many `[[package]]` entries a `Cargo.lock` has for exactly `name`.
fn locked_versions(lock: &str, name: &str) -> usize {
    let wanted = format!("name = \"{name}\"");
    lock.lines().filter(|line| *line == wanted).count()
}

/// Git checks `Cargo.lock` out with CRLF line endings on Windows.
#[test]
fn locked_versions_reads_crlf_and_lf_lockfiles_alike() {
    let lf = "[[package]]\nname = \"sha2\"\nversion = \"0.10.9\"\n\n\
              [[package]]\nname = \"sha2-asm\"\nversion = \"0.6.4\"\n";
    for lock in [lf.to_owned(), lf.replace('\n', "\r\n")] {
        assert_eq!(locked_versions(&lock, "sha2"), 1, "{lock:?}");
        assert_eq!(locked_versions(&lock, "digest"), 0, "{lock:?}");
    }
}

/// The hash-chain's crypto crates must resolve to one version each, so the
/// daemon and the server never link two `sha2` / `curve25519-dalek` trees.
#[test]
fn cargo_lock_has_one_version_of_each_crypto_crate() {
    let lock = std::fs::read_to_string(workspace_root().join("Cargo.lock")).expect("Cargo.lock");
    for name in ["sha2", "digest", "rand_core", "curve25519-dalek"] {
        let count = locked_versions(&lock, name);
        assert_eq!(count, 1, "Cargo.lock has {count} versions of {name}");
    }
}
