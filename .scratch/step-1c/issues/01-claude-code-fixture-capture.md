# 01: Claude Code fixture capture script + clean, version-tagged fixtures

**What to build:** A repeatable capture script, run locally and never in CI, that drives the real `claude -p` CLI with synthetic prompts on a throwaway Git repo. A capturing HTTP listener records every hook payload, and the script then copies each session transcript. The result is clean, version-tagged fixtures committed under the daemon's test data, which tickets 03 and 07 build their tests on. The script must never hang and never stall the ticket when nested `claude -p` calls cannot run (for example inside a headless subagent worktree with no login, no TTY or no network). See spec "Fixtures" and "Pause: verification task".

**Blocked by:** None (can start immediately).

**Status:** resolved

### Capture script
- [x] One script, with a short usage note in the fixture folder's README, captures every scenario: normal multi-tool session, tool failure, permission request, subagent, `cwd` change, interrupted turn, and API-error stop (simulated if it can't be triggered, and clearly marked as simulated).
- [x] Each run uses a fresh temp Git repo, a temp `CLAUDE_CONFIG_DIR` holding only the capture hooks, and the Traction command files from the spec, so neither the developer's own settings nor their transcripts are ever read. Prompts are synthetic and checked into the script.
- [x] The capturing listener binds an ephemeral loopback port, answers `200 {}` to every hook, and writes each payload unchanged to the scenario's fixture folder.
- [x] Every scenario sets non-interactive flags and gets a hard wall-clock timeout that kills `claude` and its children. A failed or timed-out scenario is reported and skipped; the remaining scenarios still run.
- [x] A **preflight** step runs before any scenario: `claude` on `PATH`, `claude --version` parseable, and one short authenticated `claude -p` call, all with a short timeout. On failure it exits non-zero within seconds with one clear reason (`claude-missing`, `not-logged-in`, `no-network`, `timeout`) and writes nothing to the fixtures folder.
- [x] A `--check` mode validates the committed fixtures without calling `claude`, so CI and later agents can run it: every scenario is present, every hook payload parses, transcripts are valid JSONL, and the cleanliness rules below hold.

### Clean, version-tagged fixtures
- [x] Each scenario folder holds a manifest with the Claude Code version, capture date, OS, scenario name, and whether it was simulated.
- [x] A scrub pass replaces machine-specific values with stable placeholders: home dir, temp paths, username, hostname, and any auth or account identifiers. Session IDs and record UUIDs are kept, because dedup and the deterministic `event_id` depend on them. `--check` fails if any of these values are left in.
- [x] Prompts and tool content are synthetic, with known canary strings that later canary tests can look for.

### Findings recorded (in the fixture README and the Comments below)
- [x] Whether transcripts contain explicit turn-end and API-error records (this decides the tailer's `TURN_END` and `STOP_FAILURE` mapping in ticket 07).
- [x] What text `UserPromptSubmit` receives for `/stop-track` when the command files are installed (the literal command or the expanded body), and whether a `{"decision":"block"}` answer stops the model call. Ticket 04 copies this finding into `docs/protocol.md`.

### Not stalling
- [x] If preflight fails in the agent's environment, the agent does **not** retry in a loop, fabricate payloads, or hand-write fixtures. It commits the script, the README and `--check`, appends the preflight reason and the one exact command a human should run under `## Comments`, and sets `Status: ready-for-human`. Once the human run commits the fixtures and `--check` passes, the ticket is resolved.
- [x] No CI job runs the capture. `--check` may run in CI.

## Comments

**2026-11-25 (agent):** Script `scripts/claude_fixtures.py` (stdlib Python; tests in `scripts/test_claude_fixtures.py`, 17 passing), README at `crates/traction-daemon/tests/data/claude-code/README.md`. Scrub, manifest and canary logic are in the script and covered by `--check` tests, but no fixtures are committed yet.

Preflight result in the agent worktree: **`not-logged-in`** (a temp `CLAUDE_CONFIG_DIR` has no login; the developer's own login lives under `~/.claude` and its keychain item, which the capture must not use). Exit 2 in ~0.2 s, nothing written. Human command to finish the ticket:

```sh
CLAUDE_CODE_OAUTH_TOKEN=<token printed by `claude setup-token`> python3 scripts/claude_fixtures.py
```

(The capture runs `--check` itself at the end.) Then commit `crates/traction-daemon/tests/data/claude-code/*/` and set this ticket to resolved.

Findings from real but unauthenticated `claude -p` 2.1.332 runs (hooks still fire; details in the README):
- `UserPromptSubmit` gets the **literal** `/stop-track` / `/resume-track`, not the command file body, with the command files installed.
- A `{"decision":"block"}` answer stops the model call: `num_turns: 0`, `duration_api_ms: 0`, `is_error: false`, no `StopFailure`. The transcript gets a `system`/`informational` record with `promptSubmitEnding.ending.kind: "blocked_by_hook"` quoting the original prompt, and no `user` record.
- API errors: the transcript has an explicit `assistant` record with `isApiErrorMessage: true` and an `error` code; `StopFailure` carries the same `error`. So `STOP_FAILURE` can be mapped from transcripts in ticket 07.
- Turn end: still open. No per-turn end record appeared in the failing runs, so this needs the authenticated capture. `SessionStart` was not delivered in these `-p` runs either; re-check then.


**2026-11-25 (human + agent):** The human ran the authenticated capture locally: all 8 scenarios captured with Claude Code 2.1.332. `--check` flagged 4 leftover `/private/tmp/claude-501/...` paths (Claude Code's own background-task scratch folder, quoted in tool results) in `subagent/hooks/003-PostToolUse.json`, `subagent/hooks/008-UserPromptSubmit.json`, `subagent/transcript.jsonl` and `interrupted-turn/transcript.jsonl`. Fix: `CLAUDE_SCRATCH_RE` in `scrub()` rewrites `(/private)?/tmp/claude-<uid>` to `<CLAUDE_TMP>`, plus a `--rescrub` mode that re-cleans fixtures on disk without calling `claude` (2 new tests, 19 passing). The fixtures were re-scrubbed in place and `--check` passes.

Final findings (also in the fixture README):
- `UserPromptSubmit` gets the literal `/stop-track` / `/resume-track`; a block answer stops the model call (confirmed authenticated).
- `STOP_FAILURE` maps from transcripts: `assistant` records with `isApiErrorMessage: true` (api-error-stop is simulated via a local 400 stub, and marked as such).
- **`TURN_END` is not mapped from transcripts.** The only per-turn record is `system`/`stop_hook_summary`, which exists only because a `Stop` hook is configured, so tailer-only sessions would not have it. A tailed turn ends at the next prompt or by `STALE_TURN_TIMEOUT`.
- `SessionStart` is never delivered in `-p` mode; the tailer's synthesized first `SESSION_START` covers it.
