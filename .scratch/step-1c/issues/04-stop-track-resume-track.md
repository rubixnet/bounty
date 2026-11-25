# 04: `/stop-track` and `/resume-track` session pause

**What to build:** Typing exactly `/stop-track` in a Claude Code session stops tracking that session immediately: Claude shows "Traction tracking paused for this session.", the command never reaches the model, and nothing is stored. `/resume-track` reverses it. Pause is per session and survives a daemon restart. While a session is paused, Claude's file edits are dropped, never recorded as the user's Manual Work. See spec "Pause (`/stop-track`, `/resume-track`)".

**Blocked by:** 03

**Status:** ready-for-agent

- [ ] Match rule: the `UserPromptSubmit` prompt, trimmed, equals exactly `/stop-track` or `/resume-track`, or equals exactly the matching command file's body text. The body text is defined once and shared with the installer (06). No other hook and no other prompt can pause; "what does /stop-track do?" is a normal prompt.
- [ ] Response: `{"decision":"block","reason":"Traction tracking paused for this session."}` (or "…resumed…"), and no event is stored for the command prompt. Repeating a command is idempotent and gives the same answer.
- [ ] A new migration adds `session_pause (harness_id, session_id, from_ms, to_ms)`, written through the single writer. The Router loads open spans at startup.
- [ ] While a session is paused, its events are not stored but still feed the Attribution Windows. A `FILE_MUTATION` claimed by a paused session is dropped. A mutation outside every window is still Manual Work.
- [ ] Seam A tests: exact match only, block answer, nothing stored while paused, paused-session mutation dropped, other sessions unaffected, pause surviving a restart, idempotent repeats, and both the literal command and the body text producing the block answer.
- [ ] `docs/protocol.md` documents the pause semantics, the command-text match, and ticket 01's verification finding (which text the hook receives, and that the block stops the model call). `GLOSSARY.md` gets a "Session Pause" entry.
