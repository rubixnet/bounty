# Traction Wire Protocol & Cryptographic Spec (v1)

## 1. Local Ingestion Bus (Daemon)

- **Unix (macOS/Linux):** `$XDG_RUNTIME_DIR/traction.sock` (fallback `~/.traction/daemon.sock`, mode `0600`)
- **HTTP Loopback (All OS):** `http://127.0.0.1:41937/v1/event`
- **Auth:** `Authorization: Bearer <token>` read from `~/.traction/daemon.token` (auto-generated 32-byte random hex, mode `0600`).
- **Request Hardening:** Reject any request whose `Host` header is not `127.0.0.1:41937` or `localhost:41937`, and any request carrying an `Origin` header other than the daemon's own Review UI origin (blocks DNS rebinding and browser CSRF). The `Host` check applies to loopback HTTP only: Unix-socket requests skip it (no TCP origin; the socket is `0600`) but still pass the `Origin` and token checks (see `docs/adr/0003-uds-skips-host-check.md`).
- **Source Policy:** after validation the bus refuses, with `400`, any `FILE_MUTATION` (`external_file_mutation`; only the daemon's own watcher produces them), `attribution_source: file_watcher` (`internal_attribution_source`) and the reserved `browser_extension`, `editor_extension` and `manual_pick` (`reserved_attribution_source`).
- **Claude Code hooks:** `POST /v1/hooks/claude-code` accepts Claude Code's native HTTP hook payloads, with the same `Host`, `Origin`, bearer-token and Unix-socket rules and error codes as `/v1/event` (`403 forbidden_host` / `forbidden_origin`, `401`, `404`, `405`). Its body cap is **8 MiB** (hook payloads embed whole tool outputs); above it the answer is `413 body_too_large`, which Claude Code treats as a non-blocking hook error. The body is parsed into an allowlist (`session_id`, `hook_event_name`, `cwd`, `prompt`, `tool_name`, `model`, `new_cwd`, `directory`); every other field (tool input and output, assistant messages, error text) is skipped and never kept, the prompt is reduced in memory to `prompt_char_len` (Unicode scalar values) and `has_stacktrace`, and the buffer is dropped before anything is queued. Each hook becomes at most one Traction Event (`harness_id: claude-code`, `attribution_source: harness_hook`, `network_class: cloud_api`, a fresh UUIDv7 `event_id`, `timestamp_ms` = the daemon's receive time, `repo_symbol_hits` / `exit_code` / `diff_lines_delta` always `null`):

  | Hook | Traction Event | Kept metadata |
  |---|---|---|
  | `SessionStart` | `SESSION_START` | `cwd`, `model_id` (`model`, when it is a safe identifier of at most 64 bytes) |
  | `UserPromptSubmit` | `PROMPT_SUBMIT` | `prompt_char_len`, `has_stacktrace` |
  | `PermissionRequest` | `PERMISSION_REQUEST` | `tool_name` |
  | `PreToolUse` / `PostToolUse` / `PostToolUseFailure` | `TOOL_PRE` / `TOOL_POST` / `TOOL_FAIL` | `tool_name` |
  | `SubagentStart` / `SubagentStop` | `SUBAGENT_START` / `SUBAGENT_STOP` | — |
  | `CwdChanged` | `CWD_CHANGE` | `cwd` = `new_cwd` (later events route to its Project) |
  | `DirectoryAdded` | none: `directory` is registered as a Project, as `/v1/project` does | — |
  | `Stop` | `TURN_END` (`end_reason: completed`) | — |
  | `StopFailure` | `STOP_FAILURE` | — |
  | `SessionEnd` | `SESSION_END` | — |
  | anything else (never `WorktreeCreate`) | none | — |

  `tool_name` is cut to 64 bytes and dropped (the event kept) unless it is a safe identifier. Every answer that passes the request checks is `200 {}`, sent once the event is queued to the Project Router and never after a SQLite commit: a body that is not JSON, lacks `session_id` or `hook_event_name`, or fails Traction Event validation, and a hook outside the table, all get `200 {}` with nothing stored. A full Router queue also answers `200 {}`; the event is dropped and counted in the daemon log. A hook is never told to block, except by the Session Pause commands below.

  **Session Pause (`/stop-track`, `/resume-track`).** A `UserPromptSubmit` whose `prompt`, trimmed of surrounding whitespace, equals exactly `/stop-track` or `/resume-track`, or equals exactly the body text of the matching Traction command file (`Traction: pause tracking for this session.` / `Traction: resume tracking for this session.`, defined once in `crates/traction-daemon/src/claude_code/pause.rs` and written by the installer), is a pause command. No other hook and no other prompt can pause: `what does /stop-track do?` is a normal prompt. A pause command gets `200 {"decision":"block","reason":"Traction tracking paused for this session."}` (or `"…resumed…"`), and no Traction Event is stored for it. The answer is sent once the command is queued to the Router and does not depend on the session's state, so a repeat is idempotent and gets the same answer; if the Router queue is full the prompt is still blocked, with the reason `Traction is busy and did not change tracking. Try again.`, and nothing changes. Pause is per `(harness_id, session_id)`, stored as spans in `session_pause (harness_id, session_id, from_ms, to_ms)` (`to_ms` null while open, at most one open span per session) through the single writer, and loaded by the Router at startup, so it survives a restart. An event whose timestamp falls in a span of its session is not stored, but still feeds the Attribution Windows; a `FILE_MUTATION` claimed by a paused session is dropped. A mutation outside every window is Manual Work as usual.

  Verified with Claude Code 2.1.332 and the command files installed (Step 1c ticket 01, `crates/traction-daemon/tests/data/claude-code/README.md`): `UserPromptSubmit` receives the **literal** `/stop-track` / `/resume-track`, not the expanded command body (the body match is a fallback for other versions), and the block answer stops the model call: `num_turns: 0`, `duration_api_ms: 0`, no `user` transcript record, only a `system` record with `promptSubmitEnding.ending.kind: "blocked_by_hook"`.
- **Client Failure Policy:** All harness adapters MUST enforce a short timeout (15 ms for in-process plugins; `"timeout": 1` — seconds — for Claude Code HTTP hooks with `"onFailure": "continue"`) and never block the user's coding tool if the daemon is offline.

## 2. Normalized `TractionEvent` Payload

Raw prompt text, file contents, secrets, and raw tool stdout/stderr are **FORBIDDEN** in this payload.

```json
{
  "schema_version": 1,
  "event_id": "019a7c2e-8b4d-7000-8000-000000000001",
  "timestamp_ms": 1795432800000,
  "harness_id": "claude-code | opencode | codex-cli | codex-desktop | null (manual FILE_MUTATION only)",
  "session_id": "string | null (manual FILE_MUTATION only)",
  "parent_orchestrator_id": null,
  "event_type": "SESSION_START | PROMPT_SUBMIT | PERMISSION_REQUEST | TOOL_PRE | TOOL_POST | TOOL_FAIL | SUBAGENT_START | SUBAGENT_STOP | CWD_CHANGE | TURN_END | STOP_FAILURE | SESSION_END | FILE_MUTATION",
  "end_reason": "completed | aborted | null (TURN_END only)",
  "cwd": "/abs/path/to/working/dir | null",
  "project_id": "git:<64 hex> | dir:<64 hex> | null (see 2a)",
  "model_id": "optional-model-string",
  "network_class": "cloud_api | local_model | manual",
  "attribution_source": "harness_hook | harness_plugin | harness_log | harness_db_poll | file_watcher | browser_extension | editor_extension | manual_pick",
  "metrics_only": {
    "prompt_char_len": 142,
    "repo_symbol_hits": 3,
    "has_stacktrace": true,
    "tool_name": "Bash",
    "exit_code": 1,
    "diff_lines_delta": 24,
    "mutation_kind": "created | modified | removed | renamed | null (FILE_MUTATION only)"
  }
}
```

Field rules (still `schema_version: 1`; every rejection is a value-free reason code):

- Unknown fields are rejected at every level. Optional fields may be omitted or sent as `null`; canonical JSON always writes them as explicit `null`.
- `cwd` is optional (a browser tab has none). When present it is absolute, at most 4096 bytes, with no control characters (`invalid_cwd`).
- `project_id` is optional on the wire. When present it must be `git:<64 lowercase hex>` or `dir:<64 lowercase hex>` (`invalid_project_id`). The daemon's Project Router is authoritative and replaces any Adapter-supplied value with its own, so Adapters should omit it.
- `attribution_source` is required and closed (unknown spellings are `malformed_event`):
  - `harness_hook` (Claude Code HTTP hooks), `harness_plugin` (OpenCode plugin), `harness_log` (Codex rollout tailing), `harness_db_poll` (OpenCode read-only SQLite fallback), `file_watcher` (the daemon's own watcher; daemon-internal only).
  - Reserved, accepted by `traction-core` but refused by the Local Ingestion Bus until their sub-step turns them on: `browser_extension`, `editor_extension` (Open Ingestion Spec, Step 6d) and `manual_pick` (a later user-driven attribution; semantics defined when enabled).
- `harness_id` and `session_id`: every event type except `FILE_MUTATION` requires both (`missing_harness`, `missing_session`). A `FILE_MUTATION` carries both (attributed to a Harness session) or neither (manual work); one without the other is rejected with the missing one's code. A manual `FILE_MUTATION` must have `network_class: manual` (`manual_mutation_not_manual`).
- `attribution_source: file_watcher` is valid only on `FILE_MUTATION` (`unexpected_file_watcher`), and a `FILE_MUTATION` must have `attribution_source: file_watcher` (`invalid_attribution_source`).
- `metrics_only.mutation_kind` is required on a `FILE_MUTATION` (`missing_mutation_kind`) and forbidden on every other event type (`unexpected_mutation_kind`).
- A `FILE_MUTATION` never carries the mutated file's path or name; its `cwd` is the Project Root.

### `FILE_MUTATION` attribution windows

The daemon's file watcher emits every `FILE_MUTATION` itself (never over HTTP; the bus refuses external ones). An observed mutation is attributed to a Harness session only inside that session's window, otherwise it is manual work (no Harness, no session, `network_class: manual`). Attribution is live-only: mutations while the daemon was down are never backfilled.

- `LOG_TAIL_HOLD` = **5 s**: each observed mutation is held this long (a delivery-lag buffer, not an attribution window) so late log-tailed events can still claim it.
- `LOG_TAIL_TRAILING_WINDOW` = **30 s**: the single "short fallback window" for sessions that never report turn boundaries; covers a mutation within 30 s after that session's latest event.
- `WINDOW_CLOSE_GRACE` = **1 s**: file-system notification latency grace. A mutation is stamped when its notification reaches the watcher, which can trail the edit, so a closed hook window or log-tailed turn window still claims a mutation stamped up to 1 s after its closing event (`TOOL_POST` / `TOOL_FAIL` / next `TOOL_PRE` / turn end). Later than that it is manual work.
- `STALE_TURN_TIMEOUT` = **10 min**: an open hook window or log-tailed turn window covers a mutation stamped at `t` only if its session's latest event timestamp (when the mutation is decided) is at or after `t − 10 min`. Any event of any type or source counts as activity, and a later event revives the still-open window. A crashed, `kill -9`ed or unterminated turn or tool call therefore stops claiming the user's hand edits instead of holding until a daemon restart. The rule is checked lazily when a held mutation is decided (no timer). Closed windows are unaffected (closing event + `WINDOW_CLOSE_GRACE`).
- Hook windows (`harness_hook` / `harness_plugin`) run from a `TOOL_PRE` sent while the session's turn is open (a `PROMPT_SUBMIT` with no later `TURN_END`, `STOP_FAILURE` or `SESSION_END`) to the matching `TOOL_POST` / `TOOL_FAIL`, a `PERMISSION_REQUEST` (edits while a prompt waits or after a refusal are the user's). No hook reports an approval, so when the approved tool's `TOOL_POST` / `TOOL_FAIL` arrives it adds a closed hook window from `LOG_TAIL_HOLD` before it (never before the request) to the result: the approved tool's own writes, still in their hold, go to the Harness, and only a hand edit in that last hold-length before the result can be credited to it, the session's next `TOOL_PRE`, or the turn's end.
- Log-tailed sessions (`harness_log` / `harness_db_poll`) that have ever reported a turn boundary (`PROMPT_SUBMIT` or `TURN_END`) get turn windows: from a `PROMPT_SUBMIT` to its `TURN_END` / `STOP_FAILURE` / `SESSION_END` (or still open). Those that never have get the trailing fallback window (`LOG_TAIL_TRAILING_WINDOW`) instead.
- Windows compare event `timestamp_ms` with the mutation's timestamp (its first observation), never arrival time, so a record delivered late but within `LOG_TAIL_HOLD` still claims the edit. They are scoped to the session's current Project (a session that moves to another Project ends its open windows on the old one), held only in daemon memory and never persisted: a restart starts with none open.
- Precedence: hook window, then log-tailed turn window, then trailing fallback window; among windows of one kind the most recently opened wins. The winner's `harness_id`, `session_id` and `network_class` are copied onto the mutation (`attribution_source` stays `file_watcher`).
- All four constants are public in `traction-daemon` (`LOG_TAIL_HOLD`, `LOG_TAIL_TRAILING_WINDOW`, `WINDOW_CLOSE_GRACE`, `STALE_TURN_TIMEOUT`); the daemon config allows test-only overrides of all but `WINDOW_CLOSE_GRACE`.

### Hook coverage (Claude Code hooks and the transcript tailer)

A Claude Code session can reach the daemon two ways: HTTP hooks (`harness_hook`) and the tailed transcript (`harness_log`). Together they must give exactly one set of events, and the transcript must fill in what the hooks missed.

- The daemon stores a `daemon_run` row per start (`run_id`, `started_ms`, `last_alive_ms`); the single writer bumps `last_alive_ms` in every commit. Per session and run it stores `session_hook_run` (`harness_id`, `session_id`, `run_id`, `first_hook_ms`): the `timestamp_ms` of the session's first accepted hook event in that run (hook timestamps are the daemon's receive times).
- A tailed record is **dropped** when its `timestamp_ms` falls in `[first_hook_ms, last_alive_ms]` of any run for its session (the current run is open-ended). It is **emitted** otherwise: sessions whose hooks never authenticated, records before a run's first hook, and records from spans when the daemon was down. A tailed `SESSION_START` is also dropped when one is already stored for the session. Dropped records still advance the transcript's saved byte offset.
- `HOOK_COVERAGE_SKEW` = **1 s** (public in `traction-daemon`) widens only the start of a span, because transcript timestamps are Claude Code's write times, not the daemon's clock. A record up to 1 s before a run's first hook is treated as covered; one further out is stored. The end (`last_alive_ms`) is never widened: any record stamped after a run's last commit was written while the daemon was down and is stored.
- **Hook-coverage assumption:** once a session's hooks authenticate in a run, they are assumed to keep delivering for the rest of that run. A hook dropped mid-run (full Router queue, a later `401`) is lost, not filled in from the transcript. This trades a possible gap for zero duplicates.
- Hooks may report a `transcript_path`. It is only a hint to the tailer: it is used only when it canonicalizes to a `.jsonl` file exactly one folder below `<claude config dir>/projects/` (no `..` or symlink escape, no subagent transcripts). Any other value is ignored and never opened.
- **Transcript mapping** (`harness_log`, Claude Code 2.1.332 fixtures): a `user` record with prompt text → `PROMPT_SUBMIT` (never one marked `isMeta`, `isCompactSummary`, `isVisibleInTranscriptOnly` or `isVirtual`, nor a task notification by `origin.kind` or `<task-notification>` prefix, which the `UserPromptSubmit` hook ignores too; without an `origin`, text starting `This session is being continued`, `<local-command-stdout>`, `<local-command-stderr>`, `<bash-stdout>`, `<bash-stderr>` or `Caveat:` is not a prompt); a record that is exactly the `<command-name>` wrapper of `/stop-track` or `/resume-track` (no arguments) applies that Session Pause command at the record's `timestamp_ms` and stores no event; an `assistant` `tool_use` block → `TOOL_PRE`; a `user` `tool_result` block → `TOOL_POST` (`TOOL_FAIL` when `is_error`; nothing when the record has `toolDenialKind`); an `assistant` record with `isApiErrorMessage: true` → `STOP_FAILURE`; the first event of a session is preceded by a synthesized `SESSION_START`. Nothing maps to `TURN_END` (the per-turn `system`/`stop_hook_summary` record exists only when a `Stop` hook is configured), so a tailed turn ends at the next prompt or by `STALE_TURN_TIMEOUT`. Every other record and every sidechain record maps to nothing. `event_id` is derived from the record, so a reread line gets the same ID.
- A tailed record inside a Session Pause span of its session is never stored (spans compare the record's own `timestamp_ms`), like a live event.

## 2a. Project Identity

- A **Project** is a folder, Git or not (see `docs/adr/0001`). Project root resolution: Git repo root (worktrees collapsed via `--git-common-dir`) → nearest ancestor with a project marker → the Harness session's starting folder. System, home-root, cache, and dependency folders never become Projects.
- The daemon is the only place `project_id` is derived (the pure derivation lives in `traction-core`; all file system and Git work stays in the daemon). The exact formulas are under "Project ID derivation" below. The per-device secret is a 32-byte random file in the daemon data folder (`~/.traction/device.secret`, raw bytes, mode `0600`), separate from the bus token; a malformed one stops startup rather than being replaced, since a new secret would change every `dir:` ID.
- **ID pinning:** the first time a Git Project Root resolves with a commit, its `git:` ID is stored against the repo's canonical common dir and reused afterwards, even if `origin` changes later. Another device may compute a different ID if its remote was added at a different time; the website merge (Step 4b) reconciles that.
- `git:` IDs are used once a repo has a first commit. **Migration is alias-only:** when a registered `dir:` Project gains its first commit, the daemon records an alias (`dir:` ID → `git:` ID) in one transaction. Stored Traction Events are append-only and are never rewritten, so events hashed under the `dir:` ID keep verifying; new events (including late ones carrying the old `dir:` ID) are stored under the `git:` ID, and every Project query includes aliased IDs (through the daemon's `project_event` SQLite view, whose `project_id` is the final ID after following alias chains; cycles are refused) so history stays together.
- **Hardened Git:** every daemon `git` call is read-only plumbing (`rev-parse`, `rev-list`, `config --get`) through one wrapper with `--no-optional-locks -c core.fsmonitor=false -c safe.bareRepository=explicit` and a null `core.hooksPath`, killed after a **5 s** hard timeout (treated as "not a Git repo" for that resolution). Repos rooted at `$HOME` are skipped.
- **Registration:** `POST /v1/project` with a JSON body `{"path": "/abs/path"}` (unknown fields rejected, same size cap and same `Host`/`Origin`/token checks as `/v1/event`) registers the Project Root the path resolves to and answers `200 {"project_id": "..."}`. Excluded, missing or non-folder paths get `422` with a reason code: `excluded_path` (`$HOME`, a filesystem root, an OS/cache folder or a dependency folder such as `node_modules`), `user_excluded_path` (inside a folder listed in the daemon's global ignore file, `~/.traction/ignored-folders`), `path_not_found` or `not_a_folder`. A body that is not `{"path": <string>}` gets `400 malformed_registration`; a relative path gets `400 invalid_path`.
- Project metadata synced to the server (owner- and reviewer-visible; public profiles show only `project_name`):
  - `project_name` (folder basename by default, user-renamable)
  - `project_root_path` (absolute path)
  - `project_home_relative_path` (e.g. `~/code/my-game`)
  - `os_username`
  - `linked_repo` (optional `github.com/<owner>/<repo>`, set from the website)

### Project ID derivation

Derived only by the daemon's Router, using the pure functions in `traction-core` (`git_project_id`, `dir_project_id`, `normalize_origin`). `||` is byte concatenation, `0x00` is the fixed separator, and the digest is lowercase hex.

- `git:` = `"git:" || hex(SHA-256(lower(root_commit_hex) || 0x00 || normalize_origin(origin)))`. A missing or blank `origin` hashes as the empty string.
- `dir:` = `"dir:" || hex(SHA-256(device_secret || 0x00 || canonical_root_utf8))`. `device_secret` is the 32-byte per-device secret, so a `dir:` ID cannot be confirmed from a path alone.
- **Origin normalization:** trim whitespace; strip the scheme, userinfo and port; lowercase the host (path case is kept); rewrite scp-style `user@host:owner/repo` to `host/owner/repo`; strip any trailing `/` and `.git`. Local paths (`file://`, Windows drive paths) keep only their path. `https://github.com/A/B.git`, `git@github.com:A/B` and `ssh://git@github.com:22/A/B/` all normalize to `github.com/A/B`.

## 2b. Presence Minute (Presence Sensor)

One device-wide record per wall-clock minute, written by the daemon through the single SQLite writer into the append-only `presence_minute` table (no-update/no-delete triggers, like `traction_event`; primary key `minute_start_ms`). It never crosses the Local Ingestion Bus. It holds counts, ratios and closed enums only: no key code, character, pointer position, window title, URL or raw application identifier has a slot.

```json
{
  "minute_start_ms": 1795432800000,
  "key_events": 40,
  "pointer_events": 120,
  "injected_events": 0,
  "active_subslots": 5,
  "cadence_entropy_milli": 2750,
  "category_sec": {"ide": 30, "terminal": 20, "browser": 10, "ai_desktop": 0, "other": 0},
  "category_switches": 3,
  "idle_sec": 0,
  "telemetry_tier": "full"
}
```

- Unknown fields are rejected. **A field the minute's tier cannot fill is `null`, never `0`.**
- `telemetry_tier`: `full` (idle, category, real-vs-injected input counts), `reduced` (idle and category; `key_events`, `pointer_events`, `injected_events` and `cadence_entropy_milli` are `null`), `idle_only` (idle only; `category_sec` and `category_switches` are `null`), `none` (everything `null`). A `reduced`/`idle_only` minute is weaker evidence *for* the user and never evidence *against* them.
- Sampling is per 10 s sub-slot (six per minute): the OS idle timer and the foreground category are read once per sub-slot, so `category_sec` has 10 s resolution and `category_switches` counts changes between sub-slot reads. Input events arrive by OS notification (X11 only) and the sensor sleeps otherwise: with no input it wakes once per sub-slot.
- `key_events` / `pointer_events` count every input event of that kind, **injected ones included**; `injected_events` is the injected subset of them (so real input is `key_events + pointer_events - injected_events`, and the injected ratio is `injected_events / (key_events + pointer_events)`, at most 1).
- Sub-slots close on a fixed grid from the minute start. A stall (suspend, a stuck thread) closes the sub-slots it covered: one the sensor reached within a sub-slot of its end is sampled, a later one stays unobserved and adds nothing. A completed minute with any observation is written with only what was observed; a minute with none has no row (the rollup treats a missing row as tier `none`) and a warning is logged.
- `active_subslots` (0 to 6): `full` counts sub-slots with real (non-injected) input. `reduced` and `idle_only` derive it from the idle timer (the timer reset inside the sub-slot), so injected input cannot be told apart there.
- `idle_sec`: seconds in sub-slots whose idle timer was at or above `IDLE_THRESHOLD_SEC` (30). `SUBSLOT_SEC` (10) lives in `traction-core`; `IDLE_THRESHOLD_SEC` (30) and `MIN_CADENCE_SAMPLES` (8) are `idle_threshold_sec` and `min_cadence_samples` in the `traction-rules/1` Ruleset (section 3).
- `cadence_entropy_milli`: Shannon entropy (bits x 1000) of log2-binned intervals between consecutive real events inside the minute, `null` below `min_cadence_samples` intervals. Only a 16-counter histogram is kept; individual intervals are never stored.
- A minute the sensor joined more than a sub-slot late, or slept through (the clock jumped more than a sub-slot), is dropped. Nothing is written for time the daemon was down. A restart inside one minute keeps the first row. Tray and session pause do not stop presence.
- Category mapping is one compiled-in table per OS (macOS bundle IDs, Windows executable names, X11 `WM_CLASS`) resolving to `ide | terminal | browser | ai_desktop | other`, in memory. Unknown is `other`. The raw identifier is never stored or logged.

### Backends and spike outcomes

| OS | Foreground | Idle | Injection signal | Tier now |
|---|---|---|---|---|
| macOS | `NSWorkspace.frontmostApplication` bundle ID | `CGEventSourceSecondsSinceLastEventType` (combined session state) | none | `reduced` |
| Windows | `GetForegroundWindow` -> process executable (`QueryFullProcessImageNameW`) | `GetLastInputInfo` + `GetTickCount` | none yet | `reduced` |
| Linux X11 | `_NET_ACTIVE_WINDOW` -> `WM_CLASS` | XScreenSaver `QueryInfo` | XInput2 raw events; `sourceid` of an XTEST slave = injected. Only the event type and `sourceid` are read. | `full` (lowered on error) |
| Linux Wayland / no display | none | none yet | none | `none` |

- **macOS spike (run on macOS 26, no Accessibility/Input Monitoring granted): no prompt-free injection signal.** `CGEventPost` from an untrusted process (to the session and HID taps) is dropped by the OS: neither `CGEventSourceCounterForEventType(HIDSystemState, ...)` nor the `CombinedSessionState` counter moved after 40 posted events, so a `HIDSystemState` vs `CombinedSessionState` delta could not be shown to separate injected from hardware input. Showing it for a *trusted* poster would need Accessibility, which the product must not ask for. Decision: macOS is `reduced`; the counter-delta helper stays unit-tested (with a recorded wrap and race shapes) for a future opt-in. Idle and the frontmost bundle ID are readable with no prompt.
- **Windows spike: not run.** The development host was macOS, and the Raw Input `SendInput` check (`RAWINPUTHEADER.hDevice == NULL`) and the Defender/AV check on an unsigned build need a Windows machine. Decision: Windows ships `reduced` (the specified baseline) and no input hook of any kind exists in the crate (a test scans the sources). Open: run the spike on a Windows host before building the `full` tier.
- **Linux Wayland idle (`ext-idle-notify-v1`, `org.freedesktop.ScreenSaver`) is not built yet**, so Wayland reports `none`. Open follow-up; the tier model already carries `idle_only`.
- Wayland wins over an XWayland `DISPLAY`: XWayland only sees X clients.

## 2c. Work Buckets, Activity Rollups and Commit Observations (Three-Clock rollup)

A pure, deterministic function in `traction-core` (`rollup`, `work_buckets`, `activity_rollups`): Ruleset + Traction Events + Presence Minutes + Commit Observations in, 60 s Work Buckets and 120 s Activity Rollups out. No I/O, no clock, no floats; inputs are sorted by `(timestamp_ms, event_id)` first, so any input order gives byte-identical canonical JSON. The daemon collects inputs and finalizes; the Step 4 server recomputes with the same code.

### Ruleset

One `Ruleset` struct, one `const TRACTION_RULES_1` (`ruleset_id = "traction-rules/1"`). Nothing else in the workspace defines these numbers; changing any means a new ID. Every Work Bucket and Activity Rollup records its `ruleset_id`.

| Field | Value | Meaning |
|---|---|---|
| `unverified_after_sec` | 90 | Agent time this far past the last verified presence is Unverified Autonomous Time |
| `min_active_subslots` | 3 | Real-input sub-slots (of 6) for verified presence |
| `injected_ratio_flag_milli` | 500 | Injected share of input events (per mille) that flags a minute |
| `robotic_entropy_max_milli` | 500 | Cadence entropy below this is robotic (the sensor already reports `null` below `min_cadence_samples`) |
| `permission_latency_cap_ms` / `_floor_ms` | 120000 / 300 | Clamp and machine-speed floor for approvals |
| `auto_approver_min_fast` | 2 | Floor-fast approvals in one minute bucket that set `auto_approver_suspected` |
| `presence_project_lookback_ms` | 300000 | Window for assigning a present minute to a Project |
| `agent_quiet_ms` | 60000 | Quiet period before a commit counts as human |
| `commit_freshness_ms` | 60000 | Max committer-time distance from the observation |
| `bucket_settle_ms` | 120000 | A minute is finalized once this much older than now |
| `stale_turn_timeout_ms` | 600000 | A silent open turn closes at its last event |
| `activity_rollup_ms` | 120000 | Activity Rollup window (the Anchor cadence) |
| `idle_threshold_sec` / `min_cadence_samples` | 30 / 8 | Presence Sensor constants (section 2b) |
| `anchor_window_ms` / `drift_decay_ms` | 300000 / 600000 | Outcome Anchor window; `browser`/`other` decay span |
| `baseline_min_days` / `baseline_window_days` | 3 / 30 | History needed before comfort may reduce a weight; baseline length |
| `comfort_floor_milli` / `comfort_low_specificity_milli` | 500 / 300 | Lowest comfort weight; specificity below this is "low" |
| `steering_window_ms` | 120000 | Steering looks this close to an anchor |
| `weight_steering_milli` / `_specificity_` / `_friction_` / `_comfort_` | 150 / 250 / 400 / 200 | `verified_struggle` axis weights (sum 1000) |
| `verified_struggle_bar_milli` | 600 | A `verified_struggle` at or above this is "high" (Gate simulation) |
| `friction_revert_milli` / `churn_revert_window_ms` | 500 / 120000 | Friction of Manual Work right after Harness churn |
| `specificity_symbol_milli` / `specificity_stacktrace_milli` | 250 / 400 | Specificity per distinct repo symbol; added by a pasted trace |
| `terse_prompt_chars` / `large_turn_diff_lines` / `terse_big_turn_keep_milli` | 40 / 30 / 250 | A terse prompt launching a large turn keeps this share of its specificity |
| `index_max_entries` / `index_max_projects` / `index_max_name_bytes` / `index_max_prompt_candidates` | 4096 / 16 / 64 / 512 | File-name index caps (below) |
| `index_max_extension_chars` | 8 | Longest file extension stripped to get a stem |
| `index_min_word_chars` / `index_min_stem_chars` / `index_seed_visit_factor` | 3 / 4 / 8 | Shortest matched word; shortest stem or folder name matched alone; attach-time walk visits at most `index_max_entries x factor` entries |
| `degraded_idle_max_sec` / `degraded_tier_credit_milli` | 10 / 600 | A tier with no input counts is present when its OS idle timer ran past `idle_threshold_sec` for at most this many seconds of the minute; credit kept on `reduced`/`idle_only`/`none` minutes |

The persona pass marks (`PERSONA_MARKS_1`, per mille of a 60-minute wall clock) sit beside the Ruleset in `ruleset.rs`: honest debugger at least 600 (400 on `reduced`/`idle_only`); 59 s key-tapper at most 20; trivial-prompt spammer at most 50; Steer-style faker at most 20; its injected variant 0.

### Work Bucket (Project x minute)

`project_id`, `minute_start_ms`, `ruleset_id`, `agent_compute_sec`, `idle_autonomous_sec`, `human_active_sec`, `permission_wait_ms`, `commits_human`, `commits_agent`, `injected_hid_detected`, `auto_approver_suspected`, `telemetry_tier`, `tier_limited`, `struggle_axes` (`steering`, `specificity`, `friction`, `comfort_delta`, each 0 to 1000), `verified_struggle` (0 to 1000), `struggle_credit_sec`.

- **`agent_compute_sec`**: union of open turn intervals per Project per minute. A turn opens at `PROMPT_SUBMIT` and closes at `TURN_END`, `STOP_FAILURE` or `SESSION_END` (the last two close every open interval of their session at their timestamp), or at its last event once `stale_turn_timeout_ms` passes. Subagents and parallel sessions never add time. A second belongs to an interval when its start lies inside it, so a minute-aligned 10 minute turn is exactly 600 s. Device-wide, a second with `k` active Projects is split `1/k` each (rounded down), so the sum over Projects never exceeds 60 s per minute.
- **Verified human presence** (a minute): `active_subslots >= min_active_subslots` (or, on a tier that reports no input counts at all, `idle_sec <= degraded_idle_max_sec`), not `injected_hid_detected`, and some foreground time in `ide`/`terminal`/`browser`/`ai_desktop`. The category clause is skipped when the tier is `idle_only`/`none` or `category_sec` is `null`, and the bucket sets `tier_limited`. A verified minute assigned to a Project is 60 `human_active_sec`.
- **`idle_autonomous_sec`**: agent seconds that are not within `unverified_after_sec` after a verified minute's end (a verified minute keeps its own 60 s plus 90 s fresh) or after an unflagged `PROMPT_SUBMIT`. Always `<= agent_compute_sec`.
- **`injected_hid_detected`**: injected ratio `injected_events / (key_events + pointer_events)` (the counts include injected events) at or above `injected_ratio_flag_milli`, or `cadence_entropy_milli` below `robotic_entropy_max_milli`. A flagged minute gives no verified presence, no anchor and 0 `human_active_sec`.
- **Presence to Project**: each present minute goes to exactly one Project, the one with the most recent Traction Event or Commit Observation in `[minute end - lookback, minute end)`; the later timestamp wins, equal timestamps go to the smaller `project_id`; otherwise none. So the sum of `human_active_sec` over Projects is at most 60 per minute.
- **`permission_wait_ms`**: `PERMISSION_REQUEST` to the session's next `TOOL_POST` / `TOOL_FAIL` / `PROMPT_SUBMIT` / turn close, clamped to `[0, cap]`. It adds to `human_active_sec` (never past the minute's 60 s) only when the span holds real input (`key + pointer - injected > 0`; on `reduced`/`idle_only`, `active_subslots > 0`) and the minute is not flagged. An approval with known zero real input, or `auto_approver_min_fast` approvals under the floor in one bucket, sets `auto_approver_suspected` and earns nothing. Unknown input (no Presence Minute, tier `none`) earns nothing and flags nothing.
- **`commits_human` / `commits_agent`**: a Commit Observation is human when its Project had no open turn and no Harness event (`harness_id` set) in the prior `agent_quiet_ms`; otherwise agent.
- A minute with no Presence Minute (daemon down, filled in by a tailer) is `telemetry_tier: none`, with 0 Human Active Time.

### Activity Rollup (device x 120 s window, aligned)

`window_start_ms`, `ruleset_id`, `minutes_observed`, `category_share_milli` (per category, per mille of category seconds), `switch_rate_milli` (switches per observed minute x 1000), `mean_cadence_entropy_milli`, `injected_ratio_milli`, `telemetry_tier` (the weakest tier among observed minutes). Only whole aligned windows holding at least one Presence Minute are produced.

### Commit Observation (daemon-local, not a Traction Event)

`(project_id, commit_oid, observed_ms)`. For each tracked Git worktree the daemon watches the `mtime` of `<per-worktree git dir>/logs/HEAD` (never opening it: the reflog holds messages) and records a commit only when all three hold: `COMMIT_EDITMSG` `mtime` moved forward (stat only); `HEAD`'s first parent equals the previous `HEAD` (a root commit with no previous `HEAD` also passes); and `HEAD`'s committer time (`rev-list -1 --no-commit-header --format=%ct HEAD`, the only `--format` the hardened wrapper allows) is within `commit_freshness_ms` of the observation. Any failure or timeout means no commit. A linked worktree is tracked under its own git dir. Message, author and diff are never read.

### Tables (append-only, except `struggle_baseline` and `rollup_cursor`)

- `commit_observation (project_id, commit_oid, observed_ms)`, key `(project_id, commit_oid, observed_ms)`.
- `work_bucket`, key `(project_id, minute_start_ms, ruleset_id)`, one column per Work Bucket field.
- `activity_rollup`, key `(window_start_ms, ruleset_id)`.
- `struggle_baseline (day, friction_hist, specificity_hist)`: the local comfort baseline (see "Struggle signals and credit"). The one table exempt from append-only: a day's row is upserted as days are finalized and days past `baseline_window_days` are pruned. It holds 10-bin histogram counts only, never events or timestamps.
- `rollup_cursor (ruleset_id, finalized_through_ms)`: the finalizer's bookmark, the only mutable table of the three-clock output, written in the same transaction as the rows it covers.
- `chain_key (key_id, public_key, created_at_ms)` and `chain_link (key_id, seq, window_start_ms, ruleset_id, prev_hash, payload_digest, entry_hash, sealed_at_ms, clock_regressed, payload_json, signature)`: the signed chain of section 4, **append-only** (`BEFORE UPDATE`/`BEFORE DELETE` triggers abort). Each non-empty window is sealed in the same transaction as its rows and the cursor, so a finalized row is never unsealed. No mutable "tip" table exists: the Chain Tip is the current key's highest `seq`. The only tables that are not append-only remain `struggle_baseline` and `rollup_cursor`.

**Device Signing Key.** `<data dir>/signing.key` holds the 32 raw bytes of an Ed25519 seed (created from the OS CSPRNG, never the macOS Keychain or the Linux Secret Service, which can prompt or fail on a headless machine); `<data dir>/signing.pub` holds the public key as lowercase hex plus a newline, for the user to give a reviewer. `signing.key` is owner-only: mode `0600` on Unix (a looser mode is tightened and warned about), a protected DACL with one full-control ACE for the current user on Windows. It is distinct from `device.secret` (the `dir:` Project ID key) and the bus token, never logged or sent, and a malformed file stops startup instead of being replaced. A missing file while a chain exists starts a successor chain (section 4).

The finalizer computes each minute once `bucket_settle_ms` has passed over the end of everything the minute can depend on (aligned down to a whole rollup window). That forward context is derived from the Ruleset (`context_fwd_ms`: Anchors and steering neighbours out to `drift_decay_ms`, a friction loop closing up to `stale_turn_timeout_ms` later, a permission reply within the latency cap), so a minute is final about 12 minutes after it ends. Each batch runs the pure engine over the batch plus `context_back_ms` before it and `context_fwd_ms` after it, with all inputs of that span loaded, and keeps only its own minutes: finalizing in many small batches is byte-identical to one whole-range engine call (while the baseline holds fewer than `baseline_min_days` days; the baseline fold between batches is the one intended difference), writes buckets with `INSERT OR IGNORE`, and resumes after a restart from `rollup_cursor`. Later events never rewrite a finalized minute (attribution is live-only). Its first pass waits one settle period after daemon start, so a restarting tailer can add the down-time events first.

### Struggle signals and credit

Scored per Work Bucket by `traction-core` (pure, integer-only, no I/O). Every constant is a Ruleset field.

**Outcome Anchors** (a timestamp per Project): Manual Work (a `FILE_MUTATION` no Harness session owns); a Harness `FILE_MUTATION` inside an open turn that a human `PROMPT_SUBMIT` started (the prompt's minute must not be `injected_hid_detected`; a prompt in a flagged minute launches no anchor), with a strength equal to that prompt's `specificity` (a terse `"continue"` anchors nothing; every other anchor has strength 1000); a human commit (`commits_human`, anchored at its minute); the `TOOL_POST` that closes a friction loop; an approved permission that earned real-input credit.

**Credit.** `struggle_credit_sec = human_active_sec x weight`, `weight = anchor x comfort x tier`, where `tier` is `degraded_tier_credit_milli` on a `reduced`, `idle_only` or `none` minute and 1 otherwise (weaker evidence for the user, never against), so it never exceeds `human_active_sec`, which is kept unchanged for audit. A present minute's anchor weight is the strongest anchor inside it or within `anchor_window_ms` of it (each at its own strength, 0 to 1); with none in the window, 0. A minute whose foreground is mostly `browser`/`other` and holds no anchor of its own decays linearly from 1 to 0 over `drift_decay_ms` of distance to the nearest anchor (a 5-minute-old anchor leaves weight 0.5 and a 7-minute-old one 0.3; such a minute counts anchors out to `drift_decay_ms`, while every other minute counts them only inside `anchor_window_ms`).

**Open question (human decision).** An unflagged `PROMPT_SUBMIT` counts as a presence instant for the `unverified_after_sec` rule (it keeps the following 90 s of agent time verified) even when the Presence Minute it lands in is not itself verified. The submission is a deliberate act the daemon saw, but a hook-driven bot could replay it; whether to require a verified minute is undecided and the behaviour is left as is.

**Axes** (0 to 1000):

- `steering`: of the Project's present minutes within 2 minutes of the bucket, the share that switch between at least two of `ide`/`terminal`/`browser`/`ai_desktop` (`category_switches >= 1`) within `steering_window_ms` of an anchor.
- `specificity`: for the turn each `PROMPT_SUBMIT` starts, `min(1000, repo_symbol_hits x specificity_symbol_milli + (has_stacktrace ? specificity_stacktrace_milli : 0))`, multiplied by `terse_big_turn_keep_milli` when the prompt is shorter than `terse_prompt_chars` and the turn's summed `|diff_lines_delta|` is at least `large_turn_diff_lines`. A minute takes the maximum over the turns covering it. `"continue"`/`"fix all"` therefore score 0.
- `friction`: 1000 for each minute a verification loop spans, in event order within a session: `TOOL_FAIL`, then a `FILE_MUTATION` (Harness in that session, or Manual Work in the Project), then a `TOOL_POST` of the same `tool_name` within `stale_turn_timeout_ms`. Code size is never read. A Manual Work minute within `churn_revert_window_ms` after a Harness `FILE_MUTATION` scores `friction_revert_milli` (the watcher sees no contents, so rework right after agent churn is the observable shape of a revert).
- `comfort_delta`: `1000 - familiarity`, where familiarity is the share of the baseline's present minutes that had no friction (bin 0) and specificity below `comfort_low_specificity_milli` (the smaller of the two shares). A minute with friction or real specificity, or any minute with fewer than `baseline_min_days` days of history, is 1000 and its comfort weight is 1. Otherwise the comfort weight is `1 - (1 - comfort_floor) x familiarity`: it falls smoothly toward `comfort_floor_milli` and never reaches 0.

`verified_struggle` is the weighted mean of the four axes (`weight_*_milli`).

**Baseline.** One `BaselineDay` per UTC day: two 10-bin (100 per mille wide) histograms of the present minutes' friction and specificity. The finalizer scores a batch against the stored baseline, then folds the batch's present minutes in and prunes to the newest `baseline_window_days` days, in the transaction that writes the batch.

**`repo_symbol_hits`.** The daemon keeps, per watched Project, an in-memory set of 64-bit hashes of lower-cased file names, file stems and folder names (never contents): a bounded breadth-first names-only walk when the Project is attached (honoring the watcher's exclusions, `.gitignore` and `.tractionignore`), then every mutation the watcher reports. Common words (`main`, `index`, `test`, ...) and stems under 4 characters never match alone. A Claude Code prompt (hook or transcript) is reduced in memory to at most `index_max_prompt_candidates` hashed words, counted against the index of the Project holding its `cwd`, and dropped with the text; the count is stored. `null` means no index covers the `cwd` (for example the Project's first prompt, which can arrive before registration), not zero.

## 3. Server Sync: the Per-Device Heartbeat (Every 2 Minutes, No PII)

The daemon's sync client (one task) sends **one heartbeat per device**, not per Project, to `POST /v1/anchors`. The wire types live in `traction-core::sync` (`deny_unknown_fields`, no floats) and are shared by the daemon, the server and the cross-platform test servers. The server never runs chain verification on a heartbeat (verify at submission, `docs/adr/0004`).

### Pairing (`traction login`)

1. `POST /v1/pairings` `{public_key, previous_key_id, device_label}` -> `{pair_code, poll_secret, login_url, expires_in_sec}`. The public key is the Device Signing Key's; `previous_key_id` names the key being replaced when a Successor Chain re-pairs.
2. The CLI opens `login_url` (`<server>/auth/login?pair=<code>`). The server authenticates the user through the `AuthProvider` (users are keyed by the provider's stable `sub`) and shows one plain HTML page with the pairing code and an **Approve device** button. A `GET` never approves; `POST /auth/approve` needs a double-submit CSRF token (`SameSite=Strict` cookie plus form field) and refuses a foreign `Origin`.
3. `POST /v1/pairings/poll` `{pair_code, poll_secret}` -> `{status: pending|approved|expired, device_token, device_id}`. The token (32 random bytes, 64 hex) is delivered exactly once; the server keeps only its SHA-256. On approval the server pins the public key in `device_keys`; a successor key is added to the same device and **every historical `key_id` stays trusted**.
4. The device stores the token at `<data-dir>/server.token` (`0600`, an owner-only DACL on Windows), the server URL at `server.url`.

`DELETE /v1/devices/{id}` revokes a device's token and stops its keys being accepted for new Anchors; the keys stay trusted for history. Every authenticated call is rate limited per device (token bucket); a refusal is `429` with `Retry-After`.

### The heartbeat request

```json
{
  "tip": {"key_id": "ed25519:...", "seq": 41, "entry_hash": "<64 hex>", "window_start_ms": 1800000000000},
  "sync_mode": "realtime",
  "sent_at_ms": 1800000900000,
  "work_buckets": [ ...WorkBucket... ],
  "activity_rollups": [ ...ActivityRollup... ],
  "projects": [ {"project_id": "...", "project_name": "...", "project_root_path": "...",
                 "project_home_relative_path": "...", "os_username": "..."} ],
  "timeline_marks": [ ...TimelineMark... ]
}
```

- `tip` is the Chain Tip of this request (the newest Chain Link it carries) and its window. One request carries one key: a backlog never mixes two chains.
- `sync_mode` is `realtime` or `offline_buffered`. The daemon marks a request `offline_buffered` once its oldest window ended more than **20 minutes** ago (`OFFLINE_BUFFERED_AFTER`, the server's default `MAX_ANCHOR_LAG`).
- The rows are the Work Buckets and Activity Rollups (section 2c, unchanged) sealed since the last acknowledged window, in ordered chunks of at most **30 Chain Links** (`MAX_LINKS_PER_REQUEST`); `projects` is the metadata (section 2a) of the Projects those buckets and marks name. `linked_repo` is set from the website, never sent by the daemon. `timeline_marks` are the unsigned Timeline Marks of the same minutes (below); the field may be absent (older daemons).
- The server body cap is 2 MiB (`[anchor_policy] max_body_bytes`).

### The server's checks and answers

The server assigns a monotonic per-device `server_received_at` and stores the Anchor `(device, key_id, seq, entry_hash, window_start_ms, sync_mode, sent_at_ms, server_received_at)`.

- `(device, key_id, seq)` is **idempotent** when `entry_hash` matches (`anchor: "duplicate"`), and **`409 anchor_conflict`** when it differs. A stored Anchor never changes.
- The tip's key must be pinned for the calling device (`403 key_not_pinned`); a revoked device or bad token is `401`.
- `422 future_window`: the tip window starts after `server_received_at + MAX_FUTURE_SKEW` (2 min). `422 window_too_old`: it is older than `MAX_BACKLOG_AGE` (45 days). Both are policy constants in `[anchor_policy]`, not part of the Ruleset.
- Each row is validated by pure `traction-core::sync` functions and answered `accepted` or `rejected` with a value-free reason code: `unknown_ruleset` (only `traction-rules/1`), `human_active_over_minute` (`human_active_sec` > 60), `credit_over_human_active`, `idle_over_agent_compute`, `agent_compute_over_minute`, `misaligned_minute`, `misaligned_window`, `window_after_tip` (a window after the tip's), `invalid_project_id`, `window_too_old` (an item beyond `MAX_BACKLOG_AGE`) and `value_out_of_range`.
- The daemon moves its `sync_cursor` past **terminal** rejections (`window_too_old`, per item or as a whole-request `422`), keeping a local count, and retries everything else with jittered backoff, honouring `Retry-After`.

### Timeline Mark (unsigned, outside the chain; `docs/adr/0005`)

```json
{"project_id": "dir:<64 hex>", "minute_start_ms": 1800000000000,
 "mutations_manual": 2, "mutations_harness": 1, "turns_started": 1, "tool_failures": 0,
 "harnesses": ["claude-code", "codex-cli"]}
```

- **Counts per Project and UTC minute**, computed by the daemon finalizer from stored events (the same events the bucket batch reads) and written once with that batch into the daemon's `timeline_mark` table, through the single writer. `mutations_manual` is `FILE_MUTATION` with no `harness_id`, `mutations_harness` is `FILE_MUTATION` attributed to a Harness, `turns_started` is `PROMPT_SUBMIT`, `tool_failures` is `TOOL_FAIL`. `harnesses` is the sorted set of `harness_id`s that produced any event in the minute. A minute with no event has no mark.
- **Content-free.** The type is `deny_unknown_fields` and holds counts and `harness_id`s only: never `cwd`, `model_id`, a session ID, a path or any text.
- **Unsigned.** A mark is not in a Chain Link and is never proof. The server stores it in a `timeline_marks` table partitioned on `minute_start_ms` (same partition policy as the other tables), and every API response labels marks `"integrity": "unsigned"`. Signing them could be a later `bundle_schema` bump.
- **Validation.** The server checks `project_id` (`invalid_project_id`), minute alignment (`misaligned_minute`), the tip window (`window_after_tip`), age (`window_too_old`) and the bounds (`invalid_timeline_mark`: a count above 100,000 or a repeated `harness_id`). A refused mark is answered like any rejected item (`item.kind: "timeline_mark"`), and the daemon never holds its cursor back or retries because of one.

### Timing flags, overlap merge and read APIs (server, S2)

- **`late_anchor`:** a `realtime` bucket first anchored more than `MAX_ANCHOR_LAG` (20 min) after its minute ended. **`offline_buffered`:** buckets of a request sent in `offline_buffered` mode are tagged and count as Logged Hours only, never as proof of timing. The tags are set when a row is first anchored and never change.
- **`clock_skew`:** the server stores `skew = server_received_at - sent_at_ms` on every Anchor and flags the device (`GET /v1/me`) while `|skew| > MAX_CLOCK_SKEW` (2 min) for its newest heartbeat.
- **Multi-device overlap merge (read time only):** one user's Human Active Time is capped at 60 s per UTC minute across devices and Projects. Every bucket in an over-cap minute shrinks by the same proportion (`60 / total`) with integer math and the largest-remainder rule, ties broken by `(device_id, project_id)`; `struggle_credit_sec` scales by the same proportion (floored, never above the merged human time); the minute is flagged `multi_device_overlap`. Agent Compute Time is never capped and stored rows are never changed. Live totals (`GET /v1/live`), the Project list and the timeline all report the merged figures.
- **Visibility:** `GET /v1/users/{user_id}/projects` returns `project_name` and totals to anyone; the owner, reviewers and admins (`[roles]` lists of provider `sub`s) also get `project_id`, `project_root_path`, `project_home_relative_path`, `os_username` and `linked_repo`. `GET /v1/users/{user_id}/projects/{project_id}/timeline?from_ms&to_ms` (at most 7 days) lines up Work Buckets, Activity Rollups, commit oids and Timeline Marks per minute for the owner, reviewers and admins. `DELETE /v1/me` hard-deletes every row the user owns.

Stored buckets and rollups are typed columns that round-trip exactly to `WorkBucket` and `ActivityRollup` (no JSONB), so the later submission cross-check is struct equality. Live Logged Hours totals (`GET /v1/live`, Server-Sent Events, event `totals`; `resync` after a missed update) are sums of these synced, unverified buckets and are never proof.

## 4. Signed Hash-Chain & Redaction Tombstones

Step 3a (`traction-core::chain`, sealed by the daemon's single writer, verified by the same code in the `verify` command and, from Step 4, the server). Frame hashing and redaction tombstones are Step 3b/3c; their slot in the format is already reserved (see `frame_digest`).

**Domain separation.** Every hash and signature input starts with a fixed ASCII tag that ends in `0x00`. No tag is a prefix of another, so no byte string is valid in two domains. All JSON is canonical (sorted keys, no whitespace, no floats), all digests are SHA-256 in lowercase hex, signatures are 64 bytes of lowercase hex.

| Tag | Input after the tag |
|---|---|
| `traction/v1/key-id\0` | the 32-byte Ed25519 public key |
| `traction/v1/rows\0` | canonical JSON of one Project's rows in one window |
| `traction/v1/device-rows\0` | canonical JSON of the window's Activity Rollup, or `null` |
| `traction/v1/payload\0` | canonical JSON of the Link Payload |
| `traction/v1/link\0` | canonical JSON of the Link Header |
| `traction/v1/sig/link\0` | the 32-byte `entry_hash` (what Ed25519 signs) |
| `traction/v1/manifest\0` | canonical JSON of the bundle manifest body (and of the `chains` array, for `chains_digest`) |
| `traction/v1/sig/manifest\0` | the 32-byte manifest digest |
| `traction/v1/frames\0` | reserved for Step 3b (`frame_digest`) |

**Chain Link.** One per sealed 120 s Activity Rollup window that holds any row (a window with nothing in it gets no link).

```text
key_id         = "ed25519:" || hex(SHA256("traction/v1/key-id\0" || public_key)[..16])
rows_digest    = SHA256("traction/v1/rows\0" || canonical_json(ProjectWindowRows))
payload        = { window_start_ms, ruleset_id, device_rows_digest,
                   projects: [{ project_id, rows_digest }, ...],   # sorted by project_id, unique
                   frame_digest,                                   # 32 zero bytes until Step 3b
                   supersedes }                                    # null, or the previous chain's tip (see below)
header         = { schema_version: 1, seq, prev_hash, payload_digest, key_id, ruleset_id,
                   window_start_ms, sealed_at_ms, clock_regressed }
payload_digest = SHA256("traction/v1/payload\0" || canonical_json(payload))
entry_hash     = SHA256("traction/v1/link\0" || canonical_json(header))
signature      = Ed25519.sign("traction/v1/sig/link\0" || entry_hash)
```

- `ProjectWindowRows = { project_id, work_buckets (sorted by minute), commit_oids (sorted, de-duplicated, lowercase 40/64 hex) }` for one Project in one window. Commit oids replace the old `git_diff_hash`: a Step 4 reviewer can cross-check them against the Linked Repo, and reading them needs no diff.
- `device_rows_digest` covers the window's Activity Rollup (counts and ratios only), device-wide.
- Genesis sentinel: `prev_hash` is 64 `'0'` characters exactly when `seq == 0`, and never otherwise. Later links carry the previous link's `entry_hash`.
- `frame_digest` replaces the old `frame_hash_n`. It is 32 zero bytes in Step 3a and is already signed into every payload, so Step 3b needs no format change.
- `sealed_at_ms` is the wall clock at sealing, stored as is. If it is earlier than the previous link's, the link sets `clock_regressed: true`: advisory, signed, counted in the report, never a failure on its own. `window_start_ms` must still strictly increase.

**Verification** (`verify_strict` only: it rejects small-order public keys and non-canonical `S`; a loaded public key must also not be weak). The checks run per link in this order and the first failure in chain order is returned, so the result is deterministic for any tamper. Each error has a stable `code`.

| # | Check | Error (`code`) |
|---|---|---|
| 1 | `schema_version == 1` | `unsupported_schema` |
| 2 | position: link 0 is `seq 0` with the genesis sentinel; link `i > 0` has `seq == i` | `truncated_genesis`, `duplicate_seq`, `seq_gap` |
| 3 | `key_id` is the chain's (and pinned) key | `unknown_key` |
| 4 | `payload_digest` recomputes; window and ruleset match the header; every digest the held rows cover recomputes (a Project in scope with rows but no entry, or an entry but no rows, also fails) | `payload_hash_mismatch` (`part`: payload, device_rows or the Project) |
| 5 | `entry_hash` recomputes from the header | `entry_hash_mismatch` |
| 6 | the signature verifies strictly | `invalid_signature` |
| 7 | `prev_hash` is the previous `entry_hash` | `broken_prev_hash` |
| 8 | the window advances and is aligned to the window length | `window_order` |

After the walk: a row in a window between the first genesis and the last tip that no link covers is `unanchored_row`; an expected tip that differs from the last link is `tip_mismatch`. Rows after the last tip are reported as `pending`, rows before the first genesis as `legacy` (they predate Step 3; they are not proof and are left out of bundles).

**Key discontinuity and successor chains.** A device has one chain per Device Signing Key it ever held. A missing `signing.key` while a chain exists starts a **successor chain**: a new key, a new genesis at `seq 0`, and `payload.supersedes = { key_id, seq, entry_hash }` naming the previous chain's tip (only the new key can sign it, since the old key is gone). Chains are ordered by genesis window; a chain that starts before the previous one ends is `chain_overlap`, two chains with one key are `duplicate_chain`, and a `supersedes` that is not the previous tip is `supersedes_mismatch` (the old chain's tail was cut or changed). A successor shown without its predecessor never passes: rows before its genesis are `unanchored_row`, and with those removed too its `supersedes` is `supersedes_mismatch`. A successor genesis with `supersedes: null` is allowed (a key lost together with the database cannot name a tip) and is reported as an unlinked discontinuity. Several chains are **never merged silently**: the report lists every chain and every discontinuity, and there is no combined tip.

**`verify` output and exit codes.** `traction-daemon verify <bundle> | --data-dir <path> [--trusted-key <hex>]... [--expect-tip <key_id>:<seq>:<entry_hash>]` prints:

- `OK chains=<n> links=<n> tip=<key_id>:<seq>:<hash> pinned=<bool> clock_regressions=<n> pending=<n> legacy=<n>`
- one `WARN key_discontinuity from=<key_id> to=<key_id> at_window=<ms> linked=<bool>` per discontinuity
- a `NOTE` line when no `--trusted-key` was given (the keys are self-asserted by the input)
- or `FAIL <code> key=<key_id> seq=<n>` (`-` where a field does not apply; `part=` and `window=` follow for `payload_hash_mismatch` and `unanchored_row`).

Exit code **0**: verified, exactly one chain. **2**: verified, but more than one chain (a key discontinuity must never read as a clean pass). **1**: verification failed or the input was unreadable.

**Honest limits.**

- The local chain is tamper-evident *since sealing*: rows finalized before Step 3a, and rows a window after the tip, are not covered.
- Truncating the **tail** of a chain (or of a database) is invisible without a tip learned from outside (`--expect-tip`, or an Anchor the server holds from Step 4). A bundle's manifest tip stops edits only because the manifest is signed.
- Without `--trusted-key` the chain's own public key is trusted on first use, so a whole chain rebuilt under a fresh key verifies (with a different `key_id` and `pinned=false`). Pin the key from the user's `signing.pub`.
- `clock_regressed` and `sealed_at_ms` are the device's own clock; they flag, they do not prove.

**User Redaction (Step 3c).** Dropping frame `i` will delete its bytes and salt from the Audit Bundle while retaining `{ "frame_id": "i", "redacted": true, "frame_hash": "<hex>" }`, so `frame_digest` still recomputes. Not implemented in Step 3a.

## 5. Audit Bundle and `.traction-bundle` Archive (Step 3a export / Step 3c archive / Step 4)

### Frame-less Audit Bundle (Step 3a)

`traction-daemon export-bundle --project <project_id> --out <file> [--data-dir <path>]` writes one canonical JSON file (suggested extension `.traction-bundle.json`). It opens SQLite `mode=ro` with `PRAGMA query_only = ON`, so it is safe while the daemon runs and never blocks its writer. An unknown Project ID is an error (exit 1).

```text
{
  "manifest":     { <manifest body>, "signature": "<hex>" },
  "chains":       [ { "public_key": "<hex>", "links": [ every Chain Link of that key, genesis to tip ] }, ... ],
  "project_rows": { "work_buckets": [ ... ], "commit_oids": { "<window_start_ms>": [ ... ] } },
  "device_rows":  { "activity_rollups": [ ... ] },
  "frames":       []
}
```

- `chains` holds every chain the device holds, in order, device-wide (see "Disclosure" below). `project_rows` holds the exported Project's rows only; `device_rows` holds the Activity Rollups (counts only). Rows before the first genesis window are not exported.
- Manifest body: `bundle_schema: 1`, `project_id`, `linked_repo: null` (Step 5 fills it), the current `key_id` and `public_key`, `chain_count`, `key_ids`, `first_window_ms`, `tip { key_id, seq, entry_hash }` (the last chain's), `link_count`, `bucket_count`, `ruleset_ids`, `exported_at_ms`, `chains_digest` (`SHA256("traction/v1/manifest\0" || canonical_json(chains))`). It is signed with the current Device Signing Key: `Ed25519.sign("traction/v1/sig/manifest\0" || SHA256("traction/v1/manifest\0" || canonical_json(body)))`.
- Verification order: manifest signature (`manifest_signature_invalid`), `chain_count`/`key_ids` against `chains` (`manifest_chains_mismatch`), then the chain checks of section 4 over this Project's rows (`scope = Project`) with the manifest tip as the expected tip, then the manifest's counts, `first_window_ms`, tip key and `chains_digest` (`manifest_chains_mismatch`). With `--trusted-key`, the manifest's signer must be pinned too. A malformed or non-bundle file is `malformed_bundle`.
- Step 3c wraps this same JSON as `manifest.json` + `buckets.jsonl` (+ frames) inside the zstd archive below; the digests do not change.

### Disclosure: other Projects in a device-wide bundle (`git:` ID trade-off)

One chain per device means each Link Payload carries a `{ project_id, rows_digest }` entry for **every** Project that was active in that window (needed to recompute `payload_digest`). A bundle therefore reveals, per sealed window, how many other Projects were active and their opaque `project_id`s, never their rows. `dir:` IDs are keyed by `device.secret` and cannot be linked to a path. A `git:` ID is `SHA-256(root_commit, normalized origin)`, so anyone who already knows a repository's root commit and origin can confirm that the user worked on it in that window. Do not export while a sensitive `git:` Project is tracked. A blinded per-bundle encoding is a possible later change (a new `bundle_schema`), not part of Step 3a.

### `.traction-bundle` archive (Step 3c)

Zstd-compressed tar archive containing:

- `manifest.json` — the manifest above plus device provenance tag, total frames, redacted frame count.
- `buckets.jsonl` — canonical Work Buckets matching the sealed digests.
- `frames/<frame_id>.webp` + `frames/<frame_id>.salt` — only for unredacted frames.
