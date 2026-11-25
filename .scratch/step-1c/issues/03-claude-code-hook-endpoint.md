# 03: Hook endpoint: Claude Code hooks become content-free Traction Events

**What to build:** A new authenticated bus route, `POST /v1/hooks/claude-code`, accepts Claude Code's native hook payloads and maps each one to at most one Traction Event (`harness_id: claude-code`, `attribution_source: harness_hook`, `network_class: cloud_api`). It keeps only allowlisted metadata and drops raw text in memory. It answers before any storage work, so Claude never waits on the daemon, and a `PreToolUse`→`PostToolUse` hook window attributes Claude's edits exactly. See spec "Hook endpoint" and the mapping table.

**Blocked by:** 01

**Status:** ready-for-agent

- [ ] The route has the same `Host`, `Origin`, bearer-token and Unix-socket rules and error codes as `/v1/event`. Its body cap is 8 MiB (`413` above it). The body is parsed into an allowlisted struct (unknown fields ignored, large values not kept), then the buffer is dropped.
- [ ] Every hook in the spec's mapping table maps as specified, tested with the captured payloads from 01. `DirectoryAdded` registers the folder as a Project. `CwdChanged` moves the session's later events to the new Project. Hooks not in the table store nothing.
- [ ] `prompt_char_len` and `has_stacktrace` are computed in memory by one content-free metric module, which 07 reuses for transcripts. Pure unit tests cover stack-trace detection cases.
- [ ] `event_id` is a fresh UUIDv7, `timestamp_ms` is the receive time, `tool_name` is capped, and the Step 2 fields stay `null`.
- [ ] A body that is not JSON, or that lacks `session_id` or `hook_event_name`, gets `200 {}` and nothing is stored. Every answer is `200 {}` (no blocking) and is sent once the event is queued to the Router. A full Router queue answers `200 {}`, drops the event and counts the drop in the log.
- [ ] Canary test: hook payloads full of canary strings in the prompt, tool input, tool output and file content leave none of them in SQLite.
- [ ] Attribution test: a real watcher mutation inside `PreToolUse`→`PostToolUse` goes to `claude-code`; one between tool calls is Manual Work; one landing just after `PostToolUse` (within `WINDOW_CLOSE_GRACE`) goes to Claude.
- [ ] The hook URL path and the hook set are each defined once, where the installer (05) can share them. `WorktreeCreate` is never in the set.
- [ ] `docs/protocol.md` documents the route, its cap and its responses.
