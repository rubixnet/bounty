# 03: Struggle Signals and Credit: four axes, outcome anchoring and `struggle_credit_sec`

**What to build:** Deterministic `traction-core` scoring over each Work Bucket from 02. It computes the four `struggle_axes` (`steering`, `specificity`, `friction`, `comfort_delta`, each 0–1000), a `verified_struggle` score, and `struggle_credit_sec = human_active_sec × weight` with `weight ∈ [0, 1]`. Human Active Time earns credit only when an **Outcome Anchor** lands nearby. In the daemon, it fills `repo_symbol_hits` from a bounded per-Project file-name index and keeps the local 30-day comfort baseline. See spec "Slice 3".

**Blocked by:** 02

**Status:** ready-for-human

Acceptance:

- [x] All new constants (`ANCHOR_WINDOW` ±5 min, `DRIFT_DECAY` 10 min, `BASELINE_MIN_DAYS` 3, `COMFORT_FLOOR` 0.5, index caps, axis weights) are added to `TRACTION_RULES_1`, not defined anywhere else.
- [x] **Outcome Anchors** are Manual Work, a Harness `FILE_MUTATION` after a prompt the human submitted, `commits_human`, a `TOOL_POST`/`TOOL_FAIL` that closes a friction loop, and an approved permission with real input. A present minute with no anchor within `ANCHOR_WINDOW` gets weight 0. Unanchored `browser`/`other` minutes decay linearly to 0 over `DRIFT_DECAY`. `human_active_sec` itself is unchanged (kept for audit); only `struggle_credit_sec` drops.
- [x] **Steering:** the share of present minutes with interleaved switching between `ide`/`terminal`/`browser`/`ai_desktop` within ±2 min of an anchor.
- [x] **Specificity:** the daemon keeps a bounded per-Project index of file names and path stems, built from the watcher's view only (no file contents read; capped entries; `.tractionignore` and the exclusions respected). The existing `content_metrics` path computes `repo_symbol_hits` against it in memory for Claude Code hook and transcript prompts before the text is dropped. The score combines `repo_symbol_hits`, `has_stacktrace` and the prompt-to-diff ratio. `"continue"` / `"fix all"` prompts followed by large agent turns score near 0. A canary test shows prompt text and indexed file names never reach SQLite.
- [x] **Friction:** detects `TOOL_FAIL` → `FILE_MUTATION` → `TOOL_POST` of the same `tool_name` within one session, plus Manual Work that reverts recent churn. Code size is ignored.
- [x] **Comfort delta:** a local rolling 30-day baseline stored as a compact aggregate (a histogram of axis values per day, never events or timestamps) in a daemon table. New days are appended and days past 30 are pruned. This aggregate is the one table exempt from append-only; that exemption is recorded in `docs/protocol.md`. Below `BASELINE_MIN_DAYS` the weight is never reduced. Minutes that match the baseline (no friction, low specificity) decay smoothly toward `COMFORT_FLOOR` and never to 0.
- [x] `verified_struggle` and `struggle_credit_sec` are written on each finalized Work Bucket by 02's finalizer. `struggle_credit_sec ≤ human_active_sec` always.
- [x] **Gate simulation:** an error → edit → test-fail → edit → test-pass loop gives a high `verified_struggle` (above a Ruleset-defined bar), and the 1-line and 300-line versions of the same loop score equally.
- [x] Beginner test: a user with no baseline doing a short fail→fix→pass loop gets weight 1.0. Boilerplate test: 30 days of identical low-friction minutes decay a new identical minute toward `COMFORT_FLOOR`.
- [x] `docs/protocol.md` documents the axes, Outcome Anchors, decay and the baseline table.

## Comments

Implemented test-first (core `tests/struggle.rs`, daemon `tests/repo_symbols.rs`, finalizer and transcript unit tests). Local workspace fmt, clippy and tests are green. Left for the human:

- 3-OS CI has not run (same as ticket 02). The index walk and watcher feed are portable, but only macOS was exercised.
- **Revert churn** is approximated: the watcher sees no file contents or identity, so "Manual Work that reverts recent churn" is Manual Work within `churn_revert_window_ms` after a Harness `FILE_MUTATION` in the same Project (friction `friction_revert_milli`). A true revert needs content, which Step 2 never reads.
- `repo_symbol_hits` is `null` (unknown, not 0) when no index covers the prompt's `cwd`, e.g. the first prompt of a brand-new Project that races its registration. Claude Code transcripts and hooks only; OpenCode/Codex (1d) will feed the same `SymbolIndexes`.
- Outcome Anchor "approved permission with real input" reuses the rollup's own permission-credit rule (real input, not suspected, not fast).
- Ruleset values are first guesses (weights 150/250/400/200, bar 600, etc.); Slice 4's persona simulator tunes them. Still `traction-rules/1` per Resolved Decision 6.
- Migration 9 added the scored columns by `ALTER TABLE` and the `struggle_baseline` table (exempt from append-only, recorded in `docs/protocol.md`).
