//! The per-Project file-name index behind `repo_symbol_hits`.
//!
//! The index holds 64-bit hashes of the lower-cased file names, file stems
//! and folder names the file watcher sees in a Project (a bounded names-only
//! walk when the Project is attached, then every mutation it reports). It
//! reads no file contents, honors the watcher's exclusions and ignore files,
//! lives only in memory and is capped per Project and in the number of
//! Projects (all caps are `TRACTION_RULES_1` constants).
//!
//! A prompt is reduced to [`Candidates`]: the hashed words it contains,
//! capped. They are matched against the index and dropped in the same call
//! chain that drops the prompt text; neither the words nor the names ever
//! reach an event, SQLite or a log line.

use std::collections::HashSet;
use std::path::{Component, Path};
use std::sync::{Arc, Mutex, MutexGuard};

use traction_core::TRACTION_RULES_1;

use crate::paths;

/// Shortest word (in characters) that is matched at all.
const MIN_WORD_CHARS: usize = 3;
/// Shortest file stem or folder name that is indexed or matched alone: shorter
/// ones (`db`, `ui`) are ordinary words.
const MIN_STEM_CHARS: usize = 4;
/// Stems and folder names that are ordinary words in prose, never indexed or
/// matched on their own (the full file name, `main.rs`, still is).
const COMMON_WORDS: [&str; 24] = [
    "index", "main", "mod", "lib", "test", "tests", "spec", "readme", "utils", "util", "types",
    "config", "app", "src", "source", "build", "setup", "model", "models", "this", "that", "file",
    "data", "code",
];

/// The hashed words of one prompt.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Candidates(Vec<[u64; 2]>);

/// FNV-1a over the lower-cased characters.
fn hash(word: impl Iterator<Item = char>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for c in word.flat_map(char::to_lowercase) {
        for byte in u32::from(c).to_le_bytes() {
            h ^= u64::from(byte);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | '.')
}

/// `name` without its last `.extension` (a short, alphanumeric one).
fn stem(name: &str) -> Option<&str> {
    let (stem, ext) = name.rsplit_once('.')?;
    let ok = !stem.is_empty()
        && (1..=8).contains(&ext.len())
        && ext.chars().all(|c| c.is_ascii_alphanumeric());
    ok.then_some(stem)
}

fn is_common(word: &str) -> bool {
    COMMON_WORDS.iter().any(|w| w.eq_ignore_ascii_case(word))
}

fn stem_ok(stem: &str) -> bool {
    stem.chars().count() >= MIN_STEM_CHARS && !is_common(stem)
}

/// The hashes `name` (a file or folder name, or a prompt word) answers to:
/// the whole name when it has an extension, then its stem; 0 for "none".
fn forms(name: &str) -> [u64; 2] {
    let name = name.trim_matches(|c| matches!(c, '.' | '-'));
    if name.chars().count() < MIN_WORD_CHARS {
        return [0, 0];
    }
    match stem(name) {
        Some(stem) => [
            hash(name.chars()),
            if stem_ok(stem) { hash(stem.chars()) } else { 0 },
        ],
        None if stem_ok(name) => [hash(name.chars()), 0],
        None => [0, 0],
    }
}

/// Reduces `text` to its hashed words, at most `TRACTION_RULES_1.index_max_prompt_candidates`.
/// A word and the same word with an extension count as one. Linear in the
/// text; keeps no part of it.
pub fn candidates(text: &str) -> Candidates {
    let cap = TRACTION_RULES_1.index_max_prompt_candidates as usize;
    let mut keys: HashSet<u64> = HashSet::new();
    let mut words: Vec<[u64; 2]> = Vec::new();
    for word in text.split(|c: char| !is_word_char(c)) {
        if words.len() >= cap {
            break;
        }
        let f = forms(word);
        // One word per stem, so `a` and `a.rs` are one mention.
        let key = if f[1] != 0 { f[1] } else { f[0] };
        if key != 0 && keys.insert(key) {
            words.push(f);
        }
    }
    words.sort_unstable();
    Candidates(words)
}

struct ProjectIndex {
    root: std::path::PathBuf,
    names: HashSet<u64>,
    last_used: u64,
}

#[derive(Default)]
struct State {
    projects: Vec<ProjectIndex>,
    clock: u64,
}

/// The indexes of every watched Project. Cheap to clone.
#[derive(Clone, Default)]
pub struct SymbolIndexes(Arc<Mutex<State>>);

impl SymbolIndexes {
    fn lock(&self) -> MutexGuard<'_, State> {
        // Plain data: keep going after a panic elsewhere.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Adds the names of `path` (below `root`) to `root`'s index. Returns
    /// `false` once the Project's index is full.
    pub fn note(&self, root: &Path, path: &Path) -> bool {
        let max_name = TRACTION_RULES_1.index_max_name_bytes as usize;
        let cap = TRACTION_RULES_1.index_max_entries as usize;
        let mut state = self.lock();
        state.clock += 1;
        let clock = state.clock;
        let at = match state
            .projects
            .iter()
            .position(|p| paths::same(&p.root, root))
        {
            Some(at) => at,
            None => {
                let limit = TRACTION_RULES_1.index_max_projects as usize;
                if state.projects.len() >= limit.max(1)
                    && let Some(oldest) = state
                        .projects
                        .iter()
                        .enumerate()
                        .min_by_key(|(_, p)| p.last_used)
                        .map(|(i, _)| i)
                {
                    state.projects.swap_remove(oldest);
                }
                state.projects.push(ProjectIndex {
                    root: root.to_path_buf(),
                    names: HashSet::new(),
                    last_used: clock,
                });
                state.projects.len() - 1
            }
        };
        let index = &mut state.projects[at];
        index.last_used = clock;
        for component in paths::components_below(path, root) {
            let Component::Normal(name) = component else {
                continue;
            };
            let Some(name) = name.to_str() else { continue };
            if name.len() > max_name {
                continue;
            }
            for h in forms(name).into_iter().filter(|h| *h != 0) {
                if index.names.len() < cap {
                    index.names.insert(h);
                }
            }
        }
        index.names.len() < cap
    }

    /// How many distinct words of `words` name something in the Project
    /// holding `cwd`. `None` when no index covers `cwd` (unknown, which is
    /// not the same as no hits).
    pub fn hits(&self, cwd: &Path, words: &Candidates) -> Option<u64> {
        let cwd = paths::canonical_or_lexical(cwd);
        let mut state = self.lock();
        state.clock += 1;
        let clock = state.clock;
        let index = state
            .projects
            .iter_mut()
            .filter(|p| paths::is_within(&cwd, &p.root))
            .max_by_key(|p| p.root.components().count())?;
        index.last_used = clock;
        let hits = words
            .0
            .iter()
            .filter(|w| w.iter().any(|h| *h != 0 && index.names.contains(h)))
            .count();
        Some(u64::try_from(hits).unwrap_or(u64::MAX))
    }

    /// Entries in the index of the Project rooted at `root` (tests).
    #[cfg(test)]
    pub fn len_of(&self, root: &Path) -> usize {
        self.lock()
            .projects
            .iter()
            .find(|p| paths::same(&p.root, root))
            .map_or(0, |p| p.names.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn root() -> PathBuf {
        std::env::temp_dir().join("symbol-index-unit-root")
    }

    fn index_of(files: &[&str]) -> SymbolIndexes {
        let indexes = SymbolIndexes::default();
        for file in files {
            indexes.note(&root(), &root().join(file));
        }
        indexes
    }

    fn hits(indexes: &SymbolIndexes, text: &str) -> Option<u64> {
        indexes.hits(&root(), &candidates(text))
    }

    #[test]
    fn prompt_words_match_file_names_stems_and_folders() {
        let indexes = index_of(&["src/claude_code/transcript.rs", "docs/Protocol.md"]);
        assert_eq!(hits(&indexes, "see transcript.rs and Protocol.md"), Some(2));
        // Stems and folders match on their own, case-insensitively.
        assert_eq!(hits(&indexes, "the claude_code transcript"), Some(2));
        // Each distinct word counts once.
        assert_eq!(
            hits(&indexes, "transcript transcript.rs transcript"),
            Some(1)
        );
        assert_eq!(hits(&indexes, "continue"), Some(0));
        assert_eq!(hits(&indexes, "fix all"), Some(0));
    }

    #[test]
    fn ordinary_words_are_never_symbols() {
        // `main`, `index`, `test` and `src` are prose, so only the whole
        // file name can hit.
        let indexes = index_of(&["src/main.rs", "src/index.ts", "tests/test.py"]);
        assert_eq!(
            hits(&indexes, "the main test for the index in src"),
            Some(0)
        );
        assert_eq!(hits(&indexes, "look at main.rs"), Some(1));
    }

    #[test]
    fn a_path_outside_every_index_is_unknown_not_zero() {
        let indexes = index_of(&["a/widgetron.rs"]);
        let elsewhere = std::env::temp_dir().join("symbol-index-unit-other");
        assert_eq!(indexes.hits(&elsewhere, &candidates("widgetron.rs")), None);
    }

    #[test]
    fn the_index_and_the_candidates_are_capped() {
        let indexes = SymbolIndexes::default();
        let cap = TRACTION_RULES_1.index_max_entries as usize;
        let mut full = false;
        for i in 0..cap * 2 {
            full |= !indexes.note(&root(), &root().join(format!("module_{i}.rs")));
        }
        assert!(full, "note reports a full index");
        assert_eq!(indexes.len_of(&root()), cap);
        let long: String = (0..5000).map(|i| format!("word_{i}.rs ")).collect();
        assert!(candidates(&long).0.len() <= TRACTION_RULES_1.index_max_prompt_candidates as usize);
    }

    #[test]
    fn the_least_recently_used_project_goes_when_too_many_are_indexed() {
        let indexes = SymbolIndexes::default();
        let max = TRACTION_RULES_1.index_max_projects;
        for i in 0..=max {
            let r = std::env::temp_dir().join(format!("symbol-index-proj-{i}"));
            indexes.note(&r, &r.join("alpha_file.rs"));
        }
        let first = std::env::temp_dir().join("symbol-index-proj-0");
        assert_eq!(indexes.hits(&first, &candidates("alpha_file.rs")), None);
        let last = std::env::temp_dir().join(format!("symbol-index-proj-{max}"));
        assert_eq!(indexes.hits(&last, &candidates("alpha_file.rs")), Some(1));
    }

    #[test]
    fn over_long_names_are_not_indexed() {
        let long = format!("{}.rs", "x".repeat(200));
        let indexes = index_of(&[&long]);
        assert_eq!(indexes.len_of(&root()), 0);
    }
}
