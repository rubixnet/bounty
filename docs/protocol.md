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
  "harness_id": "claude-code | opencode | codex-cli | codex-desktop",
  "session_id": "string",
  "parent_orchestrator_id": null,
  "event_type": "SESSION_START | PROMPT_SUBMIT | PERMISSION_REQUEST | TOOL_PRE | TOOL_POST | TOOL_FAIL | SUBAGENT_START | SUBAGENT_STOP | CWD_CHANGE | TURN_END | STOP_FAILURE | SESSION_END | FILE_MUTATION",
  "end_reason": "completed | aborted | null (TURN_END only)",
  "cwd": "/abs/path/to/working/dir",
  "project_id": "git:<sha256(root-commit || normalized-origin)> | dir:<sha256(device-secret || canonical-project-root-path)>",
  "model_id": "optional-model-string",
  "network_class": "cloud_api | local_model | manual",
  "metrics_only": {
    "prompt_char_len": 142,
    "repo_symbol_hits": 3,
    "has_stacktrace": true,
    "tool_name": "Bash",
    "exit_code": 1,
    "diff_lines_delta": 24
  }
}
```

## 2a. Project Identity

- A **Project** is a folder, Git or not (see `docs/adr/0001`). Project root resolution: Git repo root (worktrees collapsed via `--git-common-dir`) → nearest ancestor with a project marker → the Harness session's starting folder. System, home-root, cache, and dependency folders never become Projects.
- `git:` IDs are used once a repo has a first commit; a `dir:` Project that gains one is migrated to its `git:` ID with history preserved.
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