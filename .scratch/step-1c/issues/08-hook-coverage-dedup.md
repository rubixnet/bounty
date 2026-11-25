# 08: Hook-coverage dedup, hook-reported `transcript_path`, paused-span drops

**What to build:** Hooks and the tailer work together without double-counting. A session delivered both as hooks and as a transcript yields exactly one set of events, while spans the hooks missed (the daemon was down, hooks never authenticated, records before a run's first hook) are filled in from the transcript. The tailer prefers a hook-reported `transcript_path`, but only inside the Claude projects folder, and never records anything from a paused span. See spec "Hook coverage (dedup rule)" and "Pause".

**Blocked by:** 04, 07

**Status:** resolved

- [x] New migrations add `daemon_run (run_id, started_ms, last_alive_ms)`, where `last_alive_ms` is bumped on every writer commit, and `session_hook_run (harness_id, session_id, run_id, first_hook_ms)`, set on a session's first accepted hook event in a run.
- [x] A tailed record is dropped when its timestamp falls in `[first_hook_ms, last_alive_ms]` of any run for its session (the current run is open-ended). A tailed `SESSION_START` is dropped when one is already stored for that session.
- [x] A `transcript_path` from any hook is remembered for that session only if it canonicalizes to a file under `<claude config dir>/projects/`. Anything else is ignored and never opened.
- [x] The tailer drops records whose timestamp falls in a paused span of their session.
- [x] Seam A tests cover: hooks + transcript for the same session giving one set of events; a "daemon down" span filled from the transcript (stop the daemon, append, restart); a record about a second from a coverage boundary (clock skew); a forged `transcript_path` ignored; paused-span records not stored.
- [x] `docs/protocol.md` documents the dedup rule and the hook-coverage assumption from the spec's Further Notes.

## Comments

**2026-11-25 (agent):** Built `MIGRATION_6` (`daemon_run`, `session_hook_run`), `WriteOp::RunStart` / `HookRun`, `src/coverage.rs` (`HookCoverage`, unit-tested at both span ends) and the rule in `Router::keeps_tailed`. The writer bumps the newest run's `last_alive_ms` in every `apply_batch`. Seam A tests: `tests/claude_code_coverage.rs` (5); hint validation unit tests in `tailer.rs`.

- Added `HOOK_COVERAGE_SKEW` (1 s, public): both ends of a span are widened by it, since a transcript record of a hook's own event is stamped a little before the hook's receive time. Records further out are kept.
- Paused spans: 04's check in `Router::routed_row` already covers tailed records; 08 only adds a test. Side effect: a tailed `SESSION_START` attached to a record inside a pause is dropped with it.
- `transcript_path`: `hook::map_hook` now returns `Mapped { action, transcript_path }`. The bus forwards the path (deduplicated, absolute `.jsonl` only) to the tailer through `TranscriptHints`; the tailer thread runs `hint_target` (canonical, exactly `projects/<slug>/<file>.jsonl`, no symlink or `..` escape) before opening anything. A valid hint just reads that file from its saved position. The `projects/` watcher normally gets there first, so no end-to-end test can tell the two apart: unit tests cover the validation, the Seam A forged-path test covers "never opened".
- The end-of-span skew is covered by the `coverage` unit test only; an end-to-end version would race the shutdown time.
