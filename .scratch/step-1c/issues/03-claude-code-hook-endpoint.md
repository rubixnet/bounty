# 03: Hook endpoint: Claude Code hooks become content-free Traction Events

**What to build:** A new authenticated bus route, `POST /v1/hooks/claude-code`, accepts Claude Code's native hook payloads and maps each one to at most one Traction Event (`harness_id: claude-code`, `attribution_source: harness_hook`, `network_class: cloud_api`). It keeps only allowlisted metadata and drops raw text in memory. It answers before any storage work, so Claude never waits on the daemon, and a `PreToolUse`→`PostToolUse` hook window attributes Claude's edits exactly. See spec "Hook endpoint" and the mapping table.

**Blocked by:** 01

**Status:** resolved

- [x] The route has the same `Host`, `Origin`, bearer-token and Unix-socket rules and error codes as `/v1/event`. Its body cap is 8 MiB (`413` above it). The body is parsed into an allowlisted struct (unknown fields ignored, large values not kept), then the buffer is dropped.
- [x] Every hook in the spec's mapping table maps as specified, tested with the captured payloads from 01. `DirectoryAdded` registers the folder as a Project. `CwdChanged` moves the session's later events to the new Project. Hooks not in the table store nothing.
- [x] `prompt_char_len` and `has_stacktrace` are computed in memory by one content-free metric module, which 07 reuses for transcripts. Pure unit tests cover stack-trace detection cases.
- [x] `event_id` is a fresh UUIDv7, `timestamp_ms` is the receive time, `tool_name` is capped, and the Step 2 fields stay `null`.
- [x] A body that is not JSON, or that lacks `session_id` or `hook_event_name`, gets `200 {}` and nothing is stored. Every answer is `200 {}` (no blocking) and is sent once the event is queued to the Router. A full Router queue answers `200 {}`, drops the event and counts the drop in the log.
- [x] Canary test: hook payloads full of canary strings in the prompt, tool input, tool output and file content leave none of them in SQLite.
- [x] Attribution test: a real watcher mutation inside `PreToolUse`→`PostToolUse` goes to `claude-code`; one between tool calls is Manual Work; one landing just after `PostToolUse` (within `WINDOW_CLOSE_GRACE`) goes to Claude.
- [x] The hook URL path and the hook set are each defined once, where the installer (05) can share them. `WorktreeCreate` is never in the set.
- [x] `docs/protocol.md` documents the route, its cap and its responses.

## Comments

- Shared definitions for later tickets:
  - Hook URL path, body cap and hook set: `crates/traction-daemon/src/claude_code/hook.rs` (`HOOK_PATH`, `MAX_HOOK_BODY_BYTES`, `Hook::ALL`, `Hook::name`, `Hook::is_tool_hook` for the installer's all-tools matcher groups; `WorktreeCreate` is not a `Hook` variant). Re-exported from the crate as `CLAUDE_CODE_HOOK_PATH` / `MAX_HOOK_BODY_BYTES`. 05 builds its entries from these. 04 adds its `/stop-track` exact match where the `UserPromptSubmit` prompt is measured (the `measured` deserializer of `Payload` in `hook.rs`, before the text is dropped) and a block variant to `HookAction`; the bus answers hooks in `Bus::queue_hook` / `Reply::Hook` (`bus.rs`).
  - Content-free metrics: `crates/traction-daemon/src/content_metrics.rs` (`measure` -> `TextMetrics { prompt_char_len, has_stacktrace }`). 07 reuses it for transcript prompts.
  - Fresh UUIDv7 event IDs: `crates/traction-daemon/src/event_id.rs` (`fresh_event_id`, moved out of the watcher).
- `DirectoryAdded` carries the folder in `directory` (read from the Claude Code 2.1.332 hook schema; `-p` never fires it, so the test uses a payload shaped like the captured ones). `SessionStart` carries `model`, kept as `model_id` only when it is a safe identifier of at most 64 bytes.
- Tests: `crates/traction-daemon/tests/claude_code_hooks.rs` (Seam A), plus stack-trace unit tests in `content_metrics.rs`.
