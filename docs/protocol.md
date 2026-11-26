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

### Tables (all append-only: no-update/no-delete triggers)

- `commit_observation (project_id, commit_oid, observed_ms)`, key `(project_id, commit_oid, observed_ms)`.
- `work_bucket`, key `(project_id, minute_start_ms, ruleset_id)`, one column per Work Bucket field.
- `activity_rollup`, key `(window_start_ms, ruleset_id)`.
- `struggle_baseline (day, friction_hist, specificity_hist)`: the local comfort baseline (see "Struggle signals and credit"). The one table exempt from append-only: a day's row is upserted as days are finalized and days past `baseline_window_days` are pruned. It holds 10-bin histogram counts only, never events or timestamps.
- `rollup_cursor (ruleset_id, finalized_through_ms)`: the finalizer's bookmark, the only mutable table of the three-clock output, written in the same transaction as the rows it covers.

The finalizer computes each minute once it is older than `bucket_settle_ms` (aligned down to a whole rollup window), writes buckets with `INSERT OR IGNORE`, and resumes after a restart from `rollup_cursor`. Later events never rewrite a finalized minute (attribution is live-only). Its first pass waits one settle period after daemon start, so a restarting tailer can add the down-time events first.

### Struggle signals and credit

Scored per Work Bucket by `traction-core` (pure, integer-only, no I/O). Every constant is a Ruleset field.

**Outcome Anchors** (a timestamp per Project): Manual Work (a `FILE_MUTATION` no Harness session owns); a Harness `FILE_MUTATION` inside an open turn that a human `PROMPT_SUBMIT` started, with a strength equal to that prompt's `specificity` (a terse `"continue"` anchors nothing; every other anchor has strength 1000); a human commit (`commits_human`, anchored at its minute); the `TOOL_POST` that closes a friction loop; an approved permission that earned real-input credit.

**Credit.** `struggle_credit_sec = human_active_sec x weight`, `weight = anchor x comfort x tier`, where `tier` is `degraded_tier_credit_milli` on a `reduced`, `idle_only` or `none` minute and 1 otherwise (weaker evidence for the user, never against), so it never exceeds `human_active_sec`, which is kept unchanged for audit. A present minute's anchor weight is the strongest anchor inside it or within `anchor_window_ms` of it (each at its own strength, 0 to 1); with none in the window, 0. A minute whose foreground is mostly `browser`/`other` and holds no anchor of its own decays linearly from 1 to 0 over `drift_decay_ms` of distance to the nearest anchor (a 5-minute-old anchor leaves weight 0.5 and a 7-minute-old one 0.3; such a minute counts anchors out to `drift_decay_ms`, while every other minute counts them only inside `anchor_window_ms`).

**Axes** (0 to 1000):

- `steering`: of the Project's present minutes within 2 minutes of the bucket, the share that switch between at least two of `ide`/`terminal`/`browser`/`ai_desktop` (`category_switches >= 1`) within `steering_window_ms` of an anchor.
- `specificity`: for the turn each `PROMPT_SUBMIT` starts, `min(1000, repo_symbol_hits x specificity_symbol_milli + (has_stacktrace ? specificity_stacktrace_milli : 0))`, multiplied by `terse_big_turn_keep_milli` when the prompt is shorter than `terse_prompt_chars` and the turn's summed `|diff_lines_delta|` is at least `large_turn_diff_lines`. A minute takes the maximum over the turns covering it. `"continue"`/`"fix all"` therefore score 0.
- `friction`: 1000 for each minute a verification loop spans, in event order within a session: `TOOL_FAIL`, then a `FILE_MUTATION` (Harness in that session, or Manual Work in the Project), then a `TOOL_POST` of the same `tool_name` within `stale_turn_timeout_ms`. Code size is never read. A Manual Work minute within `churn_revert_window_ms` after a Harness `FILE_MUTATION` scores `friction_revert_milli` (the watcher sees no contents, so rework right after agent churn is the observable shape of a revert).
- `comfort_delta`: `1000 - familiarity`, where familiarity is the share of the baseline's present minutes that had no friction (bin 0) and specificity below `comfort_low_specificity_milli` (the smaller of the two shares). A minute with friction or real specificity, or any minute with fewer than `baseline_min_days` days of history, is 1000 and its comfort weight is 1. Otherwise the comfort weight is `1 - (1 - comfort_floor) x familiarity`: it falls smoothly toward `comfort_floor_milli` and never reaches 0.

`verified_struggle` is the weighted mean of the four axes (`weight_*_milli`).

**Baseline.** One `BaselineDay` per UTC day: two 10-bin (100 per mille wide) histograms of the present minutes' friction and specificity. The finalizer scores a batch against the stored baseline, then folds the batch's present minutes in and prunes to the newest `baseline_window_days` days, in the transaction that writes the batch.

**`repo_symbol_hits`.** The daemon keeps, per watched Project, an in-memory set of 64-bit hashes of lower-cased file names, file stems and folder names (never contents): a bounded breadth-first names-only walk when the Project is attached (honoring the watcher's exclusions, `.gitignore` and `.tractionignore`), then every mutation the watcher reports. Common words (`main`, `index`, `test`, ...) and stems under 4 characters never match alone. A Claude Code prompt (hook or transcript) is reduced in memory to at most `index_max_prompt_candidates` hashed words, counted against the index of the Project holding its `cwd`, and dropped with the text; the count is stored. `null` means no index covers the `cwd` (for example the Project's first prompt, which can arrive before registration), not zero.

## 3. Server Sync Heartbeat (Every 2 Minutes — No PII)

Sent to `POST /v1/anchors` per `project_id`, containing aggregated 60-second `WorkBucket` records plus the rolling chain tip:

- `human_active_sec`, `agent_compute_sec`, `idle_autonomous_sec` (the Work Bucket, Activity Rollup, Ruleset and their tables are defined in section 2c)
- `injected_hid_detected`, `window_category_counts`, `struggle_axes` (`steering`, `specificity`, `friction`, `comfort_delta`)
- `sync_mode`: `"realtime" | "offline_buffered"`
- `chain_tip_hex`: 32-byte SHA-256 hex digest

Server assigns authoritative monotonic `server_received_at`.

## 4. Salted Hash-Chain & Redaction Tombstones

**Per Snapshot Frame `i`:** Generate 16 random bytes `salt_i`, store encrypted locally, compute:

```text
frame_hash_i = SHA256(webp_bytes_i || salt_i)
```

**Per 2-Minute Anchor `n`:**

```text
H_n = SHA256(
  H_(n-1) ||
  canonical_json(buckets_n) ||
  frame_hash_n ||
  git_diff_hash_n
)
```

- `H_0` = 32 zero bytes at project start.
- `frame_hash_n` = SHA-256 of the concatenated `frame_hash_i` captured in that anchor window, or 32 zero bytes when none.
- `git_diff_hash_n` = 32 zero bytes for non-Git Projects (shadow diff uses a file size/mtime churn digest instead).
- JSON keys are sorted lexicographically with zero extra whitespace.

**User Redaction:** Dropping frame `i` deletes `webp_bytes_i` and `salt_i` from the Audit Bundle while retaining:

```json
{
  "frame_id": "i",
  "redacted": true,
  "frame_hash": "<hex>"
}
```

The server recomputes the exact chain without pixel data and computes the percentage redacted.

## 5. `.traction-bundle` Archive Format (Step 3 / Step 4)

Zstd-compressed tar archive containing:

- `manifest.json` — schema version, `project_id`, `linked_repo` (submission requires a public GitHub repo), device provenance tag, total frames, redacted frame count, start/end anchor hashes.
- `buckets.jsonl` — canonical 60-second buckets matching anchored hashes.
- `frames/<frame_id>.webp` + `frames/<frame_id>.salt` — only for unredacted frames.