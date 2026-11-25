# Claude Code fixtures

Real Claude Code hook payloads and session transcripts, captured from the real `claude -p` CLI
with synthetic prompts on a throwaway Git repo. Tickets 03 (hook endpoint) and 07 (transcript
tailer) build their tests on them. They never come from anyone's real work.

## Usage

```sh
# Capture (local only, never in CI). A temp CLAUDE_CONFIG_DIR has no login, so pass a token:
claude setup-token                       # prints a long-lived OAuth token
CLAUDE_CODE_OAUTH_TOKEN=<token> python3 scripts/claude_fixtures.py
#   --scenario NAME   capture only some scenarios (repeatable)
#   --timeout SECS    hard wall-clock limit per claude call (default 240)
#   --model ALIAS     default: haiku

# Check the committed fixtures (no claude needed; safe in CI)
python3 scripts/claude_fixtures.py --check

# Tests for the script itself
python3 -m unittest discover -s scripts -p 'test_*.py'
```

Each scenario runs with a fresh temp `HOME`, a temp `CLAUDE_CONFIG_DIR` holding only the
capture hooks (HTTP hooks for the whole spec hook set, pointing at an ephemeral loopback
listener that answers `200 {}` and records each payload unchanged) and the Traction command
files `commands/stop-track.md` / `resume-track.md`, and a fresh temp Git repo. Only
`CLAUDE_CODE_OAUTH_TOKEN` / `ANTHROPIC_API_KEY` are passed through from your environment. A
preflight (`claude` on `PATH`, parseable `--version`, one short `claude -p`) runs first and
exits non-zero within seconds with `claude-missing`, `not-logged-in`, `no-network` or
`timeout`, writing nothing. A failed or timed-out scenario is reported and skipped.

## Layout

```text
<scenario>/
  manifest.json      Claude Code version, capture date, OS, scenario, simulated, prompts, session IDs
  hooks/NNN-<Hook>.json   one hook payload per file, in arrival order
  transcript.jsonl   the session transcript(s) from <config>/projects/<slug>/<session>.jsonl
  subagents/*.jsonl  subagent transcripts, when present (not tailed; for reference)
```

Scenarios: `normal-multi-tool`, `tool-failure`, `permission-request`, `subagent`, `cwd-change`,
`interrupted-turn`, `api-error-stop` (simulated: a local stub answers every API call with HTTP
400), `stop-track`.

Scrubbing replaces the temp repo, config dir, home, temp dir, hostname and username with
`<REPO>`, `<CLAUDE_CONFIG_DIR>`, `<HOME>`, `<TMP>`, `<HOST>`, `<USER>` (including Claude's
project-folder slug form, e.g. `-<REPO>`), and redacts emails, account identifiers and tokens.
Session IDs and record UUIDs are kept (dedup and the deterministic `event_id` need them).
Prompts carry `TRACTION-CANARY-<scenario>` canary strings; `--check` fails on any leftover
home/temp path, email, account identifier or token, and on a missing canary.

## Findings (Claude Code 2.1.332, macOS)

Observed from real `claude -p` runs on 2026-11-25. These runs were **not logged in** (the
agent's environment has no token for a temp config dir), so every model call ended in an
authentication error; the authenticated capture is still pending (see ticket 01).

- **`/stop-track` text:** with the command files installed, `UserPromptSubmit` receives the
  **literal** command, `"prompt": "/stop-track"` (and `"/resume-track"`), not the expanded body.
- **Block stops the model call:** answering `{"decision": "block", "reason": "..."}` ends the
  turn before any API call: `num_turns: 0`, `duration_api_ms: 0`, `is_error: false`, exit 0,
  no `StopFailure`; the next hook is `SessionEnd`. This held even while not logged in, which
  shows the block is decided before the request is attempted. The transcript gets no `user`
  record for the blocked prompt, but it does get a `system` record (`subtype: "informational"`,
  `preventContinuation: true`, `promptSubmitEnding.ending.kind: "blocked_by_hook"`) whose
  `content` quotes `Original prompt: /stop-track`. The tailer must not treat it as a prompt.
- **API-error record:** a failed model call writes an `assistant` record with
  `isApiErrorMessage: true`, an `error` code (here `"authentication_failed"`) and
  `message.model: "<synthetic>"`, and the `StopFailure` hook fires with the same `error` and
  a `last_assistant_message`. So an explicit API-error record exists (`STOP_FAILURE` mapping).
- **Turn-end record:** not yet known. In the failing runs the only records after the turn are
  `last-prompt`, `atis-latch` and `cost-state` (session bookkeeping, not a per-turn end
  marker). Confirm against the authenticated capture before mapping `TURN_END`.
- **`SessionStart`:** not delivered in these `-p` runs (first hook was `UserPromptSubmit`).
  Re-check in the authenticated capture.
- Transcripts also carry large `api-request-shape` / `api-request-blob` / `attachment` records
  (system prompt, environment snapshot). The tailer must ignore them.
