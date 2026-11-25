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
- Hook windows (`harness_hook` / `harness_plugin`) run from a `TOOL_PRE` sent while the session's turn is open (a `PROMPT_SUBMIT` with no later `TURN_END`, `STOP_FAILURE` or `SESSION_END`) to the matching `TOOL_POST` / `TOOL_FAIL`, the session's next `TOOL_PRE`, or the turn's end.
- Log-tailed sessions (`harness_log` / `harness_db_poll`) that have ever reported a turn boundary (`PROMPT_SUBMIT` or `TURN_END`) get turn windows: from a `PROMPT_SUBMIT` to its `TURN_END` / `STOP_FAILURE` / `SESSION_END` (or still open). Those that never have get the trailing fallback window (`LOG_TAIL_TRAILING_WINDOW`) instead.
- Windows compare event `timestamp_ms` with the mutation's timestamp (its first observation), never arrival time, so a record delivered late but within `LOG_TAIL_HOLD` still claims the edit. They are scoped to the session's current Project (a session that moves to another Project ends its open windows on the old one), held only in daemon memory and never persisted: a restart starts with none open.
- Precedence: hook window, then log-tailed turn window, then trailing fallback window; among windows of one kind the most recently opened wins. The winner's `harness_id`, `session_id` and `network_class` are copied onto the mutation (`attribution_source` stays `file_watcher`).
- All four constants are public in `traction-daemon` (`LOG_TAIL_HOLD`, `LOG_TAIL_TRAILING_WINDOW`, `WINDOW_CLOSE_GRACE`, `STALE_TURN_TIMEOUT`); the daemon config allows test-only overrides of all but `WINDOW_CLOSE_GRACE`.

### Hook coverage (Claude Code hooks and the transcript tailer)

A Claude Code session can reach the daemon two ways: HTTP hooks (`harness_hook`) and the tailed transcript (`harness_log`). Together they must give exactly one set of events, and the transcript must fill in what the hooks missed.

- The daemon stores a `daemon_run` row per start (`run_id`, `started_ms`, `last_alive_ms`); the single writer bumps `last_alive_ms` in every commit. Per session and run it stores `session_hook_run` (`harness_id`, `session_id`, `run_id`, `first_hook_ms`): the `timestamp_ms` of the session's first accepted hook event in that run (hook timestamps are the daemon's receive times).
- A tailed record is **dropped** when its `timestamp_ms` falls in `[first_hook_ms, last_alive_ms]` of any run for its session (the current run is open-ended). It is **emitted** otherwise: sessions whose hooks never authenticated, records before a run's first hook, and records from spans when the daemon was down. A tailed `SESSION_START` is also dropped when one is already stored for the session. Dropped records still advance the transcript's saved byte offset.
- `HOOK_COVERAGE_SKEW` = **1 s** (public in `traction-daemon`) widens both ends of a span, because transcript timestamps are Claude Code's write times, not the daemon's clock. A record within 1 s outside a span is treated as covered; one further out is stored.
- **Hook-coverage assumption:** once a session's hooks authenticate in a run, they are assumed to keep delivering for the rest of that run. A hook dropped mid-run (full Router queue, a later `401`) is lost, not filled in from the transcript. This trades a possible gap for zero duplicates.
- Hooks may report a `transcript_path`. It is only a hint to the tailer: it is used only when it canonicalizes to a `.jsonl` file exactly one folder below `<claude config dir>/projects/` (no `..` or symlink escape, no subagent transcripts). Any other value is ignored and never opened.
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

## 3. Server Sync Heartbeat (Every 2 Minutes — No PII)

Sent to `POST /v1/anchors` per `project_id`, containing aggregated 60-second `WorkBucket` records plus the rolling chain tip:

- `human_active_sec`, `agent_compute_sec`, `idle_autonomous_sec`
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