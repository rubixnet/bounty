# 07: Transcript tailer: tailer-only Claude sessions are recorded and resume exactly after restart

**What to build:** The daemon tails Claude Code session transcripts (`<claude config dir>/projects/<slug>/<session-id>.jsonl`) by file identity and byte offset, using OS notifications and never idle polling. It maps records to content-free Traction Events (`attribution_source: harness_log`) with deterministic IDs. A Claude session whose hooks never authenticate is still fully recorded, and after a restart the tailer resumes from saved offsets with zero lost and zero duplicate events. See spec "Transcript tailer".

**Blocked by:** 01 (real transcript shapes and the turn-end findings), 03 (shared content-free metric module)

**Status:** resolved

- [x] The tailer watches `<claude config dir>/projects/` recursively with notifications and skips subagent transcript files. Its config points at a temp Claude folder under the test's fake home.
- [x] A new migration adds `transcript_offset (path, file_identity, offset, updated_ms)` and a one-row first-start marker. File identity is inode + device on Unix, and volume serial + file index on Windows, where files are opened with share-read, share-write and share-delete.
- [x] Only complete lines are read. An identity change or a file shorter than its offset is reread from 0. A malformed line is skipped and logged by reason code only. A file that existed before the first-ever tailer start begins at its end; a file created later begins at 0.
- [x] Record mapping as in the spec (`SESSION_START` on first sight, `PROMPT_SUBMIT`, `TOOL_PRE`, `TOOL_POST`/`TOOL_FAIL`, and `TURN_END`/`STOP_FAILURE` only if 01's findings show such records). All other record types are ignored. `timestamp_ms` comes from the record. `event_id` is a UUID-formatted SHA-256 of `("claude-code", session_id, record uuid, block index, event type)`.
- [x] The writer gets a "tailer batch" message, so offsets commit in the same transaction as their events.
- [x] Seam A tests cover: a tailer-only session fully recorded, partial line, replaced file, truncated file, malformed line, pre-existing file starting at its end, restart resume (stop, append, restart: zero lost, zero duplicate), and a transcript canary test (no canary string in SQLite).
- [x] The comment on the mapping records which turn-end and API-error records were mapped, based on 01's fixtures.

## Comments

**2026-11-25 (agent):** Built in `crates/traction-daemon/src/claude_code/transcript.rs` (pure record mapping), `claude_code/tailer.rs` (watch, read, resume), `MIGRATION_5` in `storage.rs` (after 04's `MIGRATION_4`), and `WriteOp::Tailed` / `WriteOp::TailerStart`. Tests are in `tests/claude_code_transcripts.rs` (9 Seam A tests).

Mapping decisions, from 01's fixtures (Claude Code 2.1.332):
- **`STOP_FAILURE`** maps from `assistant` records with `isApiErrorMessage: true` (api-error-stop).
- **`TURN_END` is not mapped.** The only candidate, `system`/`stop_hook_summary`, exists only when a Stop hook is configured, so a tailer-only session never has it. A tailed turn ends at the next prompt or by `STALE_TURN_TIMEOUT`.
- The `system`/`informational` `blocked_by_hook` record that a `/stop-track` block writes is ignored, like every other `system` record. The stop-track fixture maps to no events.
- `PROMPT_SUBMIT` comes from `user` records whose content is prompt text, minus `isMeta` records and slash-command wrappers (`<command-name>`, `<command-message>`, `<local-command-*`). The `promptSource: system` task-notification prompt in the subagent fixture is mapped, because its `UserPromptSubmit` hook fires too.
- `TOOL_POST` / `TOOL_FAIL` get their `tool_name` from the matching `tool_use` ID (kept in memory per file and bounded; `null` after a restart that splits a call).
- Sidechain records and `<session>/subagents/*.jsonl` files are never read.
- `event_id` is the SHA-256 of the length-prefixed parts, formatted as a UUIDv8 (`event_id::derived_event_id`). The synthesized `SESSION_START` uses the first mapped record's uuid with block index 0.

Other decisions:
- The `SESSION_START` "never stored" check lives in the Router (`started` set, loaded at startup through a partial index on `SESSION_START` rows). The tailer announces each session once per run and the Router drops repeats. Hook `SESSION_START`s mark the set too.
- On the first-ever start, `TailerStart` stores the marker together with an end-of-file offset for every existing transcript, in one transaction. So any later file with no offset row starts at 0, even if the first run never appended to it.
- If `projects/` (or the config folder itself) does not exist yet, the tailer watches its nearest existing ancestor, not recursively, until it appears.
- `DaemonConfig.claude_config_dir` defaults to 05's `claude_config_dir()`. Tests and the binary tests point it at a temp folder.
- Pause spans from 04 already drop tailed records by their own timestamp in `Router::routed_row`.

**Seam for 08:** every tailed event passes through `Router::keeps_tailed` (inside `Router::handle_tailed`) before routing. Add the hook-coverage rule there: drop a tailed record whose timestamp falls in `[first_hook_ms, last_alive_ms]` of any run for its session. `handle_tailed` always commits the offset, including for dropped events. For a hook-reported `transcript_path`, `tailer.rs` keys files by path string (`Worker::read(path)`), so the remembered path only needs to reach that thread, for example as a new `Message` variant. Watching `projects/` already covers normal sessions.

Not verifiable on this macOS host: the `cfg(windows)` file identity (`GetFileInformationByHandle`) and the share-mode open. Their signatures were checked against windows-sys 0.61.2. Cross-compiling failed only because bundled SQLite needs a MinGW C compiler, so CI's Windows job is the real check.
