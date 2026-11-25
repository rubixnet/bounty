//! Hook coverage: which spans of a session its hooks already reported, so
//! the transcript tailer never stores the same event twice
//! (`docs/protocol.md`, "Hook coverage").
//!
//! Each daemon start is a run. For every session the daemon keeps, per run,
//! the timestamp of the first accepted hook event (`first_hook_ms`). A run's
//! span for a session is `[first_hook_ms, end]`, where `end` is the run's
//! `last_alive_ms` (the last commit before it stopped) or, for the current
//! run, open-ended. A tailed record whose timestamp falls in any span is a
//! duplicate. Both ends are widened by [`HOOK_COVERAGE_SKEW`], because hook
//! timestamps are daemon receive times while transcript timestamps are
//! Claude Code's own write times.
//!
//! The rule trusts that once a session's hooks authenticate in a run they
//! keep delivering for the rest of that run; a hook dropped mid-run is lost,
//! not filled in from the transcript.

use std::collections::HashMap;

use traction_core::HarnessId;

use crate::config::HOOK_COVERAGE_SKEW;
use crate::storage::{HookRunRow, StoredRun};

/// [`HOOK_COVERAGE_SKEW`] in milliseconds.
fn skew_ms() -> i64 {
    i64::try_from(HOOK_COVERAGE_SKEW.as_millis()).unwrap_or(i64::MAX)
}

pub struct HookCoverage {
    current_run: i64,
    /// `last_alive_ms` of every earlier run.
    ended: HashMap<i64, i64>,
    /// Per session: `(run_id, first_hook_ms)` for every run with hooks.
    sessions: HashMap<(HarnessId, String), Vec<(i64, i64)>>,
}

impl HookCoverage {
    /// The coverage stored by earlier runs, plus a new current run.
    pub fn new(runs: &[StoredRun], hook_runs: Vec<HookRunRow>) -> Self {
        let current_run = runs.iter().map(|run| run.run_id).max().unwrap_or(0) + 1;
        let mut sessions: HashMap<(HarnessId, String), Vec<(i64, i64)>> = HashMap::new();
        for hook in hook_runs {
            sessions
                .entry((hook.harness_id, hook.session_id))
                .or_default()
                .push((hook.run_id, hook.first_hook_ms));
        }
        Self {
            current_run,
            ended: runs
                .iter()
                .map(|run| (run.run_id, run.last_alive_ms))
                .collect(),
            sessions,
        }
    }

    /// The current run's ID.
    pub const fn current_run(&self) -> i64 {
        self.current_run
    }

    /// Notes an accepted hook event at `at_ms`. Returns the row to store
    /// when it is the session's first in the current run.
    pub fn note_hook(
        &mut self,
        harness_id: HarnessId,
        session_id: &str,
        at_ms: i64,
    ) -> Option<HookRunRow> {
        let runs = self
            .sessions
            .entry((harness_id, session_id.to_owned()))
            .or_default();
        if runs.iter().any(|(run, _)| *run == self.current_run) {
            return None;
        }
        runs.push((self.current_run, at_ms));
        Some(HookRunRow {
            harness_id,
            session_id: session_id.to_owned(),
            run_id: self.current_run,
            first_hook_ms: at_ms,
        })
    }

    /// Whether a tailed record of the session at `ts_ms` falls in a span its
    /// hooks covered.
    pub fn covers(&self, harness_id: HarnessId, session_id: &str, ts_ms: i64) -> bool {
        let Some(runs) = self.sessions.get(&(harness_id, session_id.to_owned())) else {
            return false;
        };
        let skew = skew_ms();
        runs.iter().any(|&(run, first_hook_ms)| {
            if ts_ms < first_hook_ms.saturating_sub(skew) {
                return false;
            }
            match self.ended.get(&run) {
                Some(&last_alive_ms) => ts_ms <= last_alive_ms.saturating_add(skew),
                // The current run: open-ended.
                None => true,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: HarnessId = HarnessId::ClaudeCode;

    fn hook_run(session: &str, run_id: i64, first_hook_ms: i64) -> HookRunRow {
        HookRunRow {
            harness_id: CLAUDE,
            session_id: session.to_owned(),
            run_id,
            first_hook_ms,
        }
    }

    fn earlier_run() -> HookCoverage {
        // Run 1 had hooks for "s" from 10_000 to 20_000; this is run 2.
        HookCoverage::new(
            &[StoredRun {
                run_id: 1,
                last_alive_ms: 20_000,
            }],
            vec![hook_run("s", 1, 10_000)],
        )
    }

    #[test]
    fn a_new_database_starts_run_one() {
        assert_eq!(HookCoverage::new(&[], vec![]).current_run(), 1);
        assert_eq!(earlier_run().current_run(), 2);
    }

    #[test]
    fn an_earlier_run_covers_its_span_widened_by_the_skew_at_both_ends() {
        let coverage = earlier_run();
        let skew = skew_ms();
        let covers = |ts| coverage.covers(CLAUDE, "s", ts);
        assert!(covers(15_000));
        assert!(covers(10_000 - skew), "a skew before the first hook");
        assert!(!covers(10_000 - skew - 1), "before the skewed start");
        assert!(covers(20_000 + skew), "a skew after the last commit");
        assert!(!covers(20_000 + skew + 1), "the daemon was down");
        assert!(!coverage.covers(CLAUDE, "other", 15_000));
        assert!(!coverage.covers(HarnessId::OpenCode, "s", 15_000));
    }

    #[test]
    fn the_current_run_is_open_ended_from_its_first_hook() {
        let mut coverage = earlier_run();
        assert!(!coverage.covers(CLAUDE, "s", 50_000), "no hook yet in run 2");
        let first = coverage.note_hook(CLAUDE, "s", 40_000).expect("first");
        assert_eq!((first.run_id, first.first_hook_ms), (2, 40_000));
        assert_eq!(coverage.note_hook(CLAUDE, "s", 41_000), None, "only once");
        assert!(coverage.covers(CLAUDE, "s", 9_000_000));
        assert!(!coverage.covers(CLAUDE, "s", 40_000 - skew_ms() - 1));
        // The down span between the runs is still a gap.
        assert!(!coverage.covers(CLAUDE, "s", 30_000));
    }
}
