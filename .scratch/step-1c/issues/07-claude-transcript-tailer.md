# 07: Transcript tailer: tailer-only Claude sessions are recorded and resume exactly after restart

**What to build:** The daemon tails Claude Code session transcripts (`<claude config dir>/projects/<slug>/<session-id>.jsonl`) by file identity and byte offset, using OS notifications and never idle polling. It maps records to content-free Traction Events (`attribution_source: harness_log`) with deterministic IDs. A Claude session whose hooks never authenticate is still fully recorded, and after a restart the tailer resumes from saved offsets with zero lost and zero duplicate events. See spec "Transcript tailer".

**Blocked by:** 01 (real transcript shapes and the turn-end findings), 03 (shared content-free metric module)

**Status:** ready-for-agent

- [ ] The tailer watches `<claude config dir>/projects/` recursively with notifications and skips subagent transcript files. Its config points at a temp Claude folder under the test's fake home.
- [ ] A new migration adds `transcript_offset (path, file_identity, offset, updated_ms)` and a one-row first-start marker. File identity is inode + device on Unix, and volume serial + file index on Windows, where files are opened with share-read, share-write and share-delete.
- [ ] Only complete lines are read. An identity change or a file shorter than its offset is reread from 0. A malformed line is skipped and logged by reason code only. A file that existed before the first-ever tailer start begins at its end; a file created later begins at 0.
- [ ] Record mapping as in the spec (`SESSION_START` on first sight, `PROMPT_SUBMIT`, `TOOL_PRE`, `TOOL_POST`/`TOOL_FAIL`, and `TURN_END`/`STOP_FAILURE` only if 01's findings show such records). All other record types are ignored. `timestamp_ms` comes from the record. `event_id` is a UUID-formatted SHA-256 of `("claude-code", session_id, record uuid, block index, event type)`.
- [ ] The writer gets a "tailer batch" message, so offsets commit in the same transaction as their events.
- [ ] Seam A tests cover: a tailer-only session fully recorded, partial line, replaced file, truncated file, malformed line, pre-existing file starting at its end, restart resume (stop, append, restart: zero lost, zero duplicate), and a transcript canary test (no canary string in SQLite).
- [ ] The comment on the mapping records which turn-end and API-error records were mapped, based on 01's fixtures.
