# 09: Step 1c Exit Gate: 3-OS CI and human sign-off

**What to build:** Evidence that the Step 1c items in the Step 1 Exit Gate pass: all automated tests green on the Windows, macOS and Linux CI matrix, docs consistent, plus the human's one-off local `claude -p` check. See spec "Gate items this sub-step owns" and "Gate evidence plan".

**Blocked by:** 02, 06, 08

**Status:** ready-for-agent

- [x] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass locally, and the fixture `--check` from 01 passes.
- [ ] The 3-OS CI run is green, and its link is recorded under `## Comments`. No CI job calls the Anthropic API. (CI link: added by orchestrator.)
- [x] `docs/protocol.md`, `GLOSSARY.md`, the roadmap and `docs/steps.md` match what shipped (route, cap, pause, dedup, `STALE_TURN_TIMEOUT`).
- [x] The 1c gate items are ticked in the spec. The "resumes JSONL tailing" item in `docs/steps.md` stays unticked until 1d delivers Codex.
- [ ] A sign-off checklist for the human is written here: install, then run `claude -p` with the daemon stopped (Claude unaffected), then running (events appear in SQLite), then `/stop-track` (paused, nothing stored). Then set `Status: ready-for-human` and stop for sign-off.

## Comments

Local checks (fmt, clippy -D warnings, `cargo test --workspace`, `scripts/claude_fixtures.py --check`) pass. Docs already matched shipped behavior except the transcript tailer mapping, now added to `docs/protocol.md` ("Hook coverage") and `docs/steps.md`. CI link: added by orchestrator.

### Human sign-off checklist

Use a throwaway folder and your own `claude` login. Daemon data is in `~/.traction` (`traction.db`, `daemon.token`).

1. Install: `traction-daemon install claude-code`, then open a new terminal (so `TRACTION_TOKEN` is set). Check `~/.claude/settings.json` has Traction hook entries with `timeout: 1` and `onFailure: continue`, no raw token, and no `WorktreeCreate`.
2. Daemon stopped: in the throwaway folder run `claude -p "say hi"`. Claude answers normally, with no error and no noticeable delay.
3. Daemon running: start `traction-daemon`, run `claude -p "say hi"` again. Then `sqlite3 ~/.traction/traction.db` shows new `claude-code` events (SESSION_START, PROMPT_SUBMIT, ...) and no prompt text anywhere in the file (`strings ~/.traction/traction.db | grep "say hi"` finds nothing).
4. Pause: in an interactive `claude` session type `/stop-track`. Claude says tracking is paused and makes no model call. Send another prompt and edit a file; no new events for that session are stored. Type `/resume-track`, then confirm events resume.
5. Restart: stop and start the daemon; confirm no duplicate events for the earlier session.
6. Confirm the 3-OS CI run is green (link above).

### Open decisions for sign-off

1. A paused session's session row and any new Projects it touches are still written to SQLite; only events are skipped.
2. The command-file body text also pauses (spec-mandated), which is wider than AGENTS.md's "exact user commands".
3. The Windows `HKCU\Environment` `TRACTION_TOKEN` is stored in plain text (the registry cannot read a file).
4. Deferred code-review refactors: two `pause` modules, the `Command` name, the `(HarnessId, session_id)` data clump, and repeated switches over `Hook`.

**Status:** ready-for-human (set after CI link is added by the orchestrator).
