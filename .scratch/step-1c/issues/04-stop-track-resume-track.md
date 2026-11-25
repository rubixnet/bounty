# 04: `/stop-track` and `/resume-track` session pause

**What to build:** Typing exactly `/stop-track` in a Claude Code session stops tracking that session immediately: Claude shows "Traction tracking paused for this session.", the command never reaches the model, and nothing is stored. `/resume-track` reverses it. Pause is per session and survives a daemon restart. While a session is paused, Claude's file edits are dropped, never recorded as the user's Manual Work. See spec "Pause (`/stop-track`, `/resume-track`)".

**Blocked by:** 03

**Status:** resolved

- [x] Match rule: the `UserPromptSubmit` prompt, trimmed, equals exactly `/stop-track` or `/resume-track`, or equals exactly the matching command file's body text. The body text is defined once and shared with the installer (06). No other hook and no other prompt can pause; "what does /stop-track do?" is a normal prompt.
- [x] Response: `{"decision":"block","reason":"Traction tracking paused for this session."}` (or "…resumed…"), and no event is stored for the command prompt. Repeating a command is idempotent and gives the same answer.
- [x] A new migration adds `session_pause (harness_id, session_id, from_ms, to_ms)`, written through the single writer. The Router loads open spans at startup.
- [x] While a session is paused, its events are not stored but still feed the Attribution Windows. A `FILE_MUTATION` claimed by a paused session is dropped. A mutation outside every window is still Manual Work.
- [x] Seam A tests: exact match only, block answer, nothing stored while paused, paused-session mutation dropped, other sessions unaffected, pause surviving a restart, idempotent repeats, and both the literal command and the body text producing the block answer.
- [x] `docs/protocol.md` documents the pause semantics, the command-text match, and ticket 01's verification finding (which text the hook receives, and that the block stops the model call). `GLOSSARY.md` gets a "Session Pause" entry.

## Comments

- Shared command text: `crates/traction-daemon/src/claude_code/pause.rs` (`STOP_TRACK`, `RESUME_TRACK`, `Command::ALL`; fields `literal`, `file_name`, `description`, `body`, `reason`), re-exported from the crate as `PauseCommand`, `PauseAction`, `STOP_TRACK`, `RESUME_TRACK`. The installer (06) writes `file_name` with `description` in the frontmatter and exactly `body` after it. It keeps its Traction marker outside the body (for example as a frontmatter comment) so the hook's body match still holds.
- Pause state: `crates/traction-daemon/src/pause.rs` (`Pauses::is_paused_at(harness, session, ts)`), owned by the Router. Every span, open or closed, is loaded at startup, so 08's tailer can drop records from past spans by timestamp.
- Storage: migration 4 `session_pause` (`WriteOp::PauseStart` / `PauseEnd`, `load_pauses`). `tests/ingestion.rs` asserts `user_version`; bump it when another migration is appended.
- Added beyond the spec: when the Router queue is full, the command is still blocked, but with `BUSY_REASON` ("Traction is busy and did not change tracking. Try again."), so the user is never told about a pause that did not happen.
- Session and Project registry rows are still written for a paused session (routing metadata, not events), so its windows stay scoped to its Project.
- Tests: `crates/traction-daemon/tests/claude_code_pause.rs` (Seam A).
