# Traction Wire Protocol & Cryptographic Spec (v1)

## 1. Local Ingestion Bus (Daemon)

- **Unix (macOS/Linux):** `$XDG_RUNTIME_DIR/traction.sock` (fallback `~/.traction/daemon.sock`, mode `0600`)
- **HTTP Loopback (All OS):** `http://127.0.0.1:41937/v1/event`
- **Auth:** `Authorization: Bearer <token>` read from `~/.traction/daemon.token` (auto-generated 32-byte random hex, mode `0600`).
- **Request Hardening:** Reject any request whose `Host` header is not `127.0.0.1:41937` or `localhost:41937`, and any request carrying an `Origin` header other than the daemon's own Review UI origin (blocks DNS rebinding and browser CSRF). The `Host` check applies to loopback HTTP only: Unix-socket requests skip it (no TCP origin; the socket is `0600`) but still pass the `Origin` and token checks (see `docs/adr/0003-uds-skips-host-check.md`).
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
- Hook windows (`harness_hook` / `harness_plugin`) run from `TOOL_PRE` to the matching `TOOL_POST` / `TOOL_FAIL` in an open turn.

## 2a. Project Identity

- A **Project** is a folder, Git or not (see `docs/adr/0001`). Project root resolution: Git repo root (worktrees collapsed via `--git-common-dir`) → nearest ancestor with a project marker → the Harness session's starting folder. System, home-root, cache, and dependency folders never become Projects.
- The daemon is the only place `project_id` is derived (the pure derivation lives in `traction-core`; all file system and Git work stays in the daemon). Both IDs are lowercase SHA-256 hex:
  - `git:` = SHA-256 over the root commit hex and the normalized origin URL (empty when there is no `origin`), joined by a fixed separator.
  - `dir:` = SHA-256 over the per-device secret (a 32-byte random file in the daemon data folder, separate from the bus token) and the canonical Project Root path, joined by a fixed separator.
  - Origin normalization: lowercase the host; strip the scheme, userinfo, port and a trailing `.git` or `/`; rewrite scp-style `git@host:owner/repo` to `host/owner/repo`. So `https://github.com/A/B.git`, `git@github.com:A/B` and `ssh://git@github.com:22/A/B/` give the same ID.
  - The exact byte layout (separator) is pinned by golden tests in `traction-core`.
- **ID pinning:** the first time a Git Project Root resolves with a commit, its `git:` ID is stored against the repo's canonical common dir and reused afterwards, even if `origin` changes later. Another device may compute a different ID if its remote was added at a different time; the website merge (Step 4b) reconciles that.
- `git:` IDs are used once a repo has a first commit. **Migration is alias-only:** when a registered `dir:` Project gains its first commit, the daemon records an alias (`dir:` ID → `git:` ID) in one transaction. Stored Traction Events are append-only and are never rewritten, so events hashed under the `dir:` ID keep verifying; new events (including late ones carrying the old `dir:` ID) are stored under the `git:` ID, and every Project query includes aliased IDs so history stays together.
- **Hardened Git:** every daemon `git` call is read-only plumbing (`rev-parse`, `rev-list`, `config --get`) through one wrapper with `--no-optional-locks -c core.fsmonitor=false -c safe.bareRepository=explicit` and a null `core.hooksPath`, killed after a **5 s** hard timeout (treated as "not a Git repo" for that resolution). Repos rooted at `$HOME` are skipped.
- **Registration:** `POST /v1/project` with a JSON body `{"path": "/abs/path"}` (unknown fields rejected, same size cap and same `Host`/`Origin`/token checks as `/v1/event`) registers the Project Root the path resolves to. Excluded, missing or non-folder paths get `422` with a reason code.
- Project metadata synced to the server (owner- and reviewer-visible; public profiles show only `project_name`):
  - `project_name` (folder basename by default, user-renamable)
  - `project_root_path` (absolute path)
  - `project_home_relative_path` (e.g. `~/code/my-game`)
  - `os_username`
  - `linked_repo` (optional `github.com/<owner>/<repo>`, set from the website)

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