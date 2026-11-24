# Step 1a: Foundation (workspace, core schema, daemon storage and Local Ingestion Bus)

Status: ready-for-agent

Source: `docs/roadmap.md` (Step 1a), `docs/steps.md` (Step 1, Build items 1 and 2), `docs/protocol.md` sections 1 and 2.

Gate items this sub-step owns (from the Step 1 Exit Gate):

- [ ] The Local Ingestion Bus rejects missing or wrong bearer tokens, foreign `Host` headers and foreign `Origin` headers.
- [ ] Zero raw prompt text is persisted in SQLite. 1a owns the structural guarantee; the fixture-based check over real Harness payloads lands in 1c and 1d.

## Problem Statement

Traction has docs and no code. Every later sub-step (Project Router, the three Adapters, controls, the Three Clocks, the hash-chain) needs the same foundation:

- a Rust workspace that builds and tests on Windows, macOS and Linux;
- one shared definition of a Traction Event;
- a daemon that can safely receive Traction Events from local Adapters and persist them without corrupting state.

Without that foundation, each Adapter would invent its own payload, storage and transport. Raw prompt text could also leak into storage, and any local web page or process could inject fake activity into the user's Logged Hours.

From the user's point of view, a Hack Club teen installs Traction and expects three things. It must never slow down or break their Harness. It must never store what they typed. Nobody else on the machine, and no website open in their browser, should be able to forge or read their activity.

## Solution

Stand up the repository foundation and the first working slice of the daemon:

- A pinned Rust toolchain, an AGPLv3 licence and a Cargo workspace containing `traction-core` and `traction-daemon`. `traction-server` is not created until Step 4.
- A GitHub Actions matrix (Windows, macOS, Linux) running fmt, clippy and tests on every push, in a private GitHub repo that is created only after the human confirms.
- `traction-core` defines the Traction Event wire type and canonical JSON serialization. These are the single source of truth for the daemon now and the server later.
- `traction-daemon` runs as a single instance per user. It persists events to SQLite in WAL mode through one dedicated writer. It exposes the Local Ingestion Bus over a Unix Domain Socket (macOS/Linux) and loopback HTTP on `127.0.0.1:41937`, and authenticates every request with a per-user bearer token. It also rejects foreign `Host` and `Origin` headers.

When 1a is done, an Adapter (or `curl`) holding the token can POST a valid Traction Event and have it land in SQLite. Every other caller is refused, and no raw content can reach storage.

## User Stories

1. As a contributor, I want a pinned Rust toolchain in the repo, so that every machine and CI runner builds with the same compiler and lints.
2. As a contributor, I want a Cargo workspace with `traction-core` and `traction-daemon`, so that shared types live in one crate and the daemon depends on it.
3. As a contributor, I want an AGPLv3 `LICENSE` at the repo root, so that the project's licence is unambiguous from the first commit.
4. As a maintainer, I want CI to run `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test` on Windows, macOS and Linux on every push, so that no Exit Gate item is ever signed off on a single OS.
5. As a maintainer, I want the agent to stop and ask before creating the GitHub repo, so that nothing is published or created on my account without consent.
6. As a maintainer, I want the GitHub repo to be private at creation, so that work stays unpublished until I choose to open-source it.
7. As an Adapter author, I want one `TractionEvent` type with every field from `docs/protocol.md` section 2, so that all three Harness Adapters emit the same normalized shape.
8. As an Adapter author, I want `event_type` to be a closed enum (`SESSION_START`, `PROMPT_SUBMIT`, `PERMISSION_REQUEST`, `TOOL_PRE`, `TOOL_POST`, `TOOL_FAIL`, `SUBAGENT_START`, `SUBAGENT_STOP`, `CWD_CHANGE`, `TURN_END`, `STOP_FAILURE`, `SESSION_END`, `FILE_MUTATION`), so that typos are rejected at the bus instead of silently stored.
9. As an Adapter author, I want `harness_id` to be a closed enum (`claude-code`, `opencode`, `codex-cli`, `codex-desktop`), so that a v1 scope creep Harness can't sneak in.
10. As an Adapter author, I want `network_class` to be a closed enum (`cloud_api`, `local_model`, `manual`), so that offline anomaly detection in later Steps has clean input.
11. As an Adapter author, I want `end_reason` (`completed` or `aborted`) to be accepted only on `TURN_END` and rejected on every other event type, so that turn outcomes are unambiguous.
12. As an Adapter author, I want `metrics_only` to hold only typed numeric, boolean and short enum-like fields, so that there is no field where raw text could fit.
13. As a user, I want any Traction Event containing an unknown field to be rejected, so that a buggy or malicious Adapter can't smuggle my prompt text in under a new key.
14. As a user, I want `tool_name` and `model_id` to be length-limited and limited to a safe character set, so that they can't be used to carry prompt or output fragments.
15. As a developer of the later hash-chain, I want a canonical JSON function in `traction-core` (keys sorted lexicographically, no extra whitespace, stable number formatting), so that the daemon and server produce byte-identical hashes.
16. As a developer of the server, I want `TractionEvent` round-trip through canonical JSON to be lossless, so that the server can re-derive exactly what the daemon stored.
17. As an Adapter author, I want `event_id` to be a UUID (v7 recommended), so that events sort roughly by time and duplicates can be detected by ID.
18. As a user, I want the daemon to refuse to start a second instance for my user, so that two processes never write to the same database.
19. As a user, I want a second daemon start attempt to exit cleanly with a clear message instead of crashing, so that auto-start retries don't spam errors.
20. As a user, I want the single-instance lock to be released automatically when the daemon dies, even after a crash, so that a restart never needs manual cleanup.
21. As a user, I want my events stored in SQLite in WAL mode, so that a crash or power loss doesn't corrupt my Logged Hours.
22. As a user, I want all SQLite writes funnelled through a single dedicated writer, so that concurrent Adapters never cause lock contention or `SQLITE_BUSY` errors.
23. As a user on low-spec hardware, I want writes batched and committed about every 5 seconds, so that the daemon doesn't hammer my disk.
24. As a user, I want pending events flushed to disk on `SIGINT` or `SIGTERM` (and Ctrl+C or console close on Windows), so that stopping the daemon loses nothing that was already accepted.
25. As a user, I want an event with an `event_id` that is already stored to be ignored rather than stored twice, so that Adapter retries and fallback tailers never double-count.
26. As an Adapter author, I want the bus to accept a valid event quickly and acknowledge it before the batch commit, so that my 15 ms or 1 s timeout is never at risk.
27. As a user, I want the bearer token auto-generated on first run as 32 random bytes in hex, so that I never have to configure anything.
28. As a user, I want the token file and the socket to be readable only by me (mode `0600` on Unix), so that other local users can't read or forge my activity.
29. As a user on Windows, I want the token stored under my user profile where other standard users can't read it, so that I get the same protection as on Unix.
30. As a user, I want the token reused across daemon restarts, so that installed Adapter configuration keeps working.
31. As a user, I want any request without a bearer token, or with a wrong one, rejected, so that random local processes can't inject events.
32. As a user, I want token comparison to be constant-time, so that the token can't be recovered through timing.
33. As a user, I want requests whose `Host` header is not `127.0.0.1:41937` or `localhost:41937` rejected, so that DNS-rebinding attacks from websites fail.
34. As a user, I want requests carrying an `Origin` header other than the daemon's own origin rejected, so that a website open in my browser can't POST fake events (CSRF).
35. As an Adapter author, I want requests with no `Origin` header (non-browser clients) allowed once the token and `Host` checks pass, so that hooks and plugins work.
36. As a user, I want the HTTP listener bound only to `127.0.0.1`, never `0.0.0.0` or `::`, so that nothing on my network can reach it.
37. As a macOS or Linux user, I want the bus also available on a Unix Domain Socket at `$XDG_RUNTIME_DIR/traction.sock` (fallback `~/.traction/daemon.sock`, mode `0600`), so that local Adapters can use a channel that never touches TCP.
38. As an Adapter author, I want the UDS to apply the same token and payload validation as HTTP, so that both transports have one security model.
39. As a user, I want request bodies capped at a small size, so that a misbehaving client can't exhaust the daemon's memory.
40. As an Adapter author, I want distinct, documented status codes for each kind of rejection (bad host or origin, bad token, malformed or invalid event, success), so that I can debug my Adapter without reading daemon logs.
41. As a user, I want rejection responses and logs never to echo the request body, so that raw content doesn't leak through error paths.
42. As a user, I want the daemon to start correctly when port 41937 is already taken, failing with a clear error rather than binding elsewhere, so that Adapters never talk to an impostor on a different port.
43. As a test author, I want the daemon's data directory, socket path and port to be configurable, so that integration tests run in parallel on temp directories without touching my real `~/.traction`.
44. As a reviewer of the code, I want no `todo!()`, stubbed lock or hardcoded `true` in validation or persistence, so that the anti-mocking rule holds from day one.
45. As a user, I want the stored row to contain only the validated, normalized event fields, so that what's on disk is exactly what the Traction Event contract allows.

## Implementation Decisions

**Toolchain and workspace**

- Install Rust with `rustup`. Pin a specific stable version (not the `stable` channel name) in `rust-toolchain.toml`, with the `rustfmt` and `clippy` components.
- The workspace root `Cargo.toml` lists `crates/traction-core` and `crates/traction-daemon` only. `traction-server`, `packages/`, and `apps/` are not created in 1a.
- The `LICENSE` file is the full AGPLv3 text. Each crate declares `license = "AGPL-3.0-only"`.
- Shared dependency versions are declared once at the workspace level.

**CI and repo**

- A GitHub Actions workflow runs on every push and pull request with a matrix of `ubuntu-latest`, `macos-latest` and `windows-latest`. Each job runs fmt check, clippy with `-D warnings` across all targets, and `cargo test --workspace`. RSS/CPU benchmarking is deferred to 1e.
- Creating the GitHub repo and pushing is a human-confirmed action and gets its own `ready-for-human`-style checkpoint ticket. The agent prepares the workflow file but asks before running `gh repo create --private` or `git push`. Every gate item is verified on CI only after the repo exists.

**`traction-core` (shared types, used by the daemon now and the server in Step 4)**

- `TractionEvent` mirrors `docs/protocol.md` section 2 exactly: `schema_version` (currently `1`), `event_id`, `timestamp_ms`, `harness_id`, `session_id`, `parent_orchestrator_id` (optional), `event_type`, `end_reason` (optional), `cwd`, `project_id`, `model_id` (optional), `network_class`, `metrics_only`.
- `harness_id`, `event_type`, `network_class` and `end_reason` are closed enums whose wire spelling is exactly the protocol strings.
- `metrics_only` is a typed struct with these fields, each optional because not every event type carries every metric:
  - `prompt_char_len` (unsigned integer)
  - `repo_symbol_hits` (unsigned integer)
  - `has_stacktrace` (boolean)
  - `tool_name` (bounded identifier)
  - `exit_code` (signed integer)
  - `diff_lines_delta` (signed integer)
- Unknown fields are rejected at every level (deny-unknown-fields semantics), both at the top level and inside `metrics_only`. This is the structural "no raw text" guarantee: no free-text slot exists in the type.
- Bounded string rules, enforced at deserialization or by a `validate` step that the bus always calls:
  - `tool_name` and `model_id`: short maximum length (e.g. 64), safe identifier character set (letters, digits, `.`, `_`, `-`, `:`, `/`), no whitespace runs.
  - `session_id`: bounded length and safe character set.
  - `cwd`: an absolute path with a bounded length. It is a path, not content, and ADR 0001 explicitly allows full paths.
  - `project_id`: must match `git:<64 hex>` or `dir:<64 hex>`. In 1a the bus validates the format only. Resolving `project_id` from `cwd` belongs to the Project Router in 1b, which may relax this so the daemon fills `project_id` itself.
  - `event_id`: a valid UUID.
  - `end_reason`: present on `TURN_END` (required there) and absent on every other `event_type`.
- Canonical JSON is a public function in `traction-core`:
  - object keys sorted lexicographically by UTF-8 bytes, recursively;
  - no insignificant whitespace;
  - integers written without exponent or fraction;
  - floats are not allowed in canonical payloads;
  - optional fields that are `None` serialize as explicit `null`, matching `protocol.md` (`"parent_orchestrator_id": null`).
  
  It is the only serializer later used for hashing (Step 3) and must be deterministic across OSes.
- `traction-core` has no OS, I/O or async dependencies.

**`traction-daemon`**

- **Configuration.** A daemon config value carries the data directory (default `~/.traction`), the UDS path (default `$XDG_RUNTIME_DIR/traction.sock`, fallback `<data dir>/daemon.sock`) and the HTTP bind port (default `41937`). The binary uses the defaults. Tests inject temp paths and a port. The expected `Host` values are derived from the configured port (`127.0.0.1:<port>`, `localhost:<port>`), so production behaviour is exactly `41937`.
- **Library-first.** The daemon exposes a start function that takes the config and returns a handle. The handle can report the bound address and socket path and can trigger graceful shutdown (flush then exit). The binary is a thin wrapper that wires OS signals to that shutdown. This handle is the integration-test seam.
- **Single-instance lock.**
  - Unix: an exclusive advisory `flock` on a lock file in the data directory.
  - Windows: a per-user named mutex (the name includes the user's SID or username, and the data directory is part of the name so test instances don't collide with a real daemon).
  - The lock is held for the process lifetime and released by the OS on death. A second start returns a distinct "already running" error, and the binary exits non-zero with a one-line message.
- **Token.**
  - On first start, generate 32 bytes from the OS CSPRNG, hex-encode them and write them to `<data dir>/daemon.token`.
  - Unix: create the file with mode `0600` atomically, never chmod after writing.
  - Windows: rely on the user-profile ACL and do not widen it.
  - Reuse the file on later starts. If the file is malformed, regenerate it with a warning that never prints the token.
  - Comparison is constant-time.
- **SQLite.**
  - Bundled SQLite. The database lives in the data directory, opened in WAL mode with `synchronous=NORMAL`.
  - Schema is created by a versioned migration step (`user_version`) so later sub-steps can add tables.
  - An events table stores the normalized fields as typed columns, with `event_id` as the primary key and `INSERT OR IGNORE` for idempotency. `metrics_only` is stored either as typed columns or as its canonical JSON. In both cases only fields that passed validation are written.
  - No column ever holds a request body verbatim.
- **Single writer.** One dedicated writer thread owns the only read-write connection. Ingestion handlers send validated events over a bounded channel.
  - The writer commits a batch every 5 seconds, or sooner when the batch reaches a size threshold.
  - On shutdown it drains the channel and commits before the process exits.
  - If the channel is full, the bus returns a "busy" status rather than blocking the caller.
- **Local Ingestion Bus.**
  - `POST /v1/event` with a JSON `TractionEvent` body, served on HTTP bound to `127.0.0.1` only, and on the UDS on macOS and Linux (no UDS on Windows in 1a).
  - Use a small async HTTP stack on a single-threaded runtime to respect the 15 MB RSS budget measured in 1e.
  - Check order for every request:
    1. `Host` allowlist (HTTP only; UDS requests skip the `Host` check because there is no TCP origin).
    2. `Origin`: absent is allowed; equal to the daemon's own origin (`http://127.0.0.1:<port>` or `http://localhost:<port>`) is allowed; anything else is rejected.
    3. Bearer token.
    4. Body size cap (e.g. 16 KiB).
    5. Deserialize and validate.
    6. Enqueue to the writer.
  - Responses:
    - `403` for bad `Host` or `Origin`;
    - `401` for a missing or bad token;
    - `413` for an oversized body;
    - `400` for malformed JSON or validation failure, with a short machine-readable reason code and no echo of input;
    - `503` when the writer channel is full;
    - `202` when accepted.
  - Unknown paths and methods get `404` or `405` after the `Host`, `Origin` and token checks, so unauthenticated probes learn nothing.
- **Logging.** Logs never include request bodies, token values or field contents beyond `event_type`, `harness_id` and `event_id`.
- **Port conflict.** If the port is in use, startup fails with a clear error. The daemon never falls back to another port.

## Testing Decisions

The user agreed on two seams, both black-box.

**Seam 1: `traction-core` public API.** Tests use only the crate's public types and functions.

- Round-trip each `event_type` and `harness_id` through serialize, deserialize and canonical JSON.
- Golden byte strings for canonical JSON, e.g. the `protocol.md` example event's exact canonical bytes. Key order, nested key order, `null` handling and no whitespace are all asserted on bytes.
- Rejection cases:
  - an unknown top-level field (e.g. `"prompt": "SENTINEL"`);
  - an unknown field inside `metrics_only` (e.g. `"stdout": ...`);
  - wrong enum spellings;
  - `end_reason` on a non-`TURN_END` event;
  - `TURN_END` without `end_reason`;
  - over-long or whitespace-laden `tool_name` or `model_id`;
  - malformed `project_id` and `event_id`;
  - relative `cwd`;
  - floats where integers are required.
- Determinism: canonical JSON of semantically equal values built in different field orders gives identical bytes.

**Seam 2: a real in-process daemon.** Each test starts the daemon library on a fresh temp data directory, an ephemeral or test-chosen port and a temp UDS path. Tests interact only through:

- HTTP requests to the bound address;
- UDS requests (Unix only);
- the token file the daemon wrote;
- a separate read-only SQLite connection (`mode=ro`) opened after a graceful shutdown or flush.

Tests do not reach into the lock, writer or storage modules. Cases:

- **Auth gate (owns the Exit Gate item):**
  - a valid token, `Host` and no `Origin` returns `202`;
  - a missing `Authorization` returns `401`;
  - a wrong token, including one that differs only in the last character, returns `401`;
  - `Host: evil.example:41937`, `Host: 127.0.0.1:9999` and a missing `Host` each return `403`;
  - `Origin: https://evil.example` and `Origin: null` each return `403`;
  - the daemon's own origin returns `202`;
  - the `Host` and `Origin` checks still reject when the token is valid.
- **No raw text in SQLite (owns the Exit Gate item):**
  - send events where raw content is attempted via unknown fields, and assert `400`;
  - send valid events and shut down;
  - scan every table and column, and the raw database and WAL file bytes, for a sentinel string. Zero hits are expected.
- **Persistence:**
  - N accepted events are present after graceful shutdown;
  - the same `event_id` sent twice is stored once;
  - events sent just before shutdown are flushed;
  - the database is in WAL mode.
- **Single instance:** a second daemon started on the same data directory fails with the "already running" error while the first is alive. After the first shuts down, a new start succeeds.
- **Token file:** created on first start; mode `0600` on Unix; the same value is reused on restart.
- **Transport:** the UDS accepts the same valid request and rejects a bad token (Unix only). The socket file mode is `0600`. The HTTP listener is not reachable on a non-loopback address.
- **Limits:** an oversized body returns `413`. Malformed JSON returns `400`, and the response body doesn't contain the input.

**What makes a good test here:** it asserts observable behaviour at the seam (status codes, bytes on the wire, rows readable from SQLite, file modes) and survives a rewrite of the internals. No mocks of SQLite, the lock or the network. Everything runs for real on each OS in CI, per the anti-mocking rule. Tests are hermetic: temp directories only, never the developer's real `~/.traction`, and parallel-safe.

**Prior art:** none in the repo yet. These are the first tests and set the pattern for 1b through 1e. Integration tests go in the daemon crate's `tests/` directory and share a small test helper that starts a daemon and returns a client.

## Out of Scope

- **Project Router**: root resolution, `git:`/`dir:` derivation and migration, folder exclusions, file watcher, `FILE_MUTATION` emission, `.tractionignore` (1b).
- **Every Harness Adapter**: Claude Code hooks and installer, `/stop-track`, JSONL tailers, the OpenCode plugin and poller, the Codex rollout tailer (1c and 1d). Real Harness fixtures and the cross-fixture "zero duplicates" check also come with the Adapters.
- **Tray, hotkey, `traction pause`/`resume` CLI, auto-start, and RSS/CPU measurement** (1e).
- **`traction-server` crate, `WorkBucket`, the Three Clocks, struggle math and the hash-chain** (Steps 2 to 4). Canonical JSON is built now because the hash-chain will depend on it, but no hashing is implemented.
- **The `/submit` Review UI** (Step 3). Only its origin is allow-listed now.
- **Windows named-pipe transport.** Windows uses loopback HTTP only in 1a.
- **Anything under `apps/web/` or `packages/`.**

## Further Notes

- Glossary terms to use in code and tests: Traction Event, Harness, Adapter, Project, Project Root, Logged Hours. Avoid "heartbeat", "client", "repo" (for Project) and "workspace".
- `protocol.md` puts the UDS fallback at `~/.traction/daemon.sock`. With a configurable data directory, the fallback is `<data dir>/daemon.sock`.
- The `Origin` allowlist (the daemon's own origin) exists for the Step 3 Review UI. Adding it now keeps the check logic final.
- ADR 0001 allows full absolute `cwd` paths in Traction Events. They are personal data, so later Steps must keep them off public surfaces. 1a only stores them locally.
- Suggested ticket order:
  1. Toolchain, workspace, licence and CI workflow file.
  2. Human checkpoint: create the private repo and push.
  3. Core `TractionEvent` and validation.
  4. Core canonical JSON.
  5. Daemon config, lock and token.
  6. SQLite and the single writer.
  7. HTTP bus with Host, Origin and token checks.
  8. UDS transport.
  9. The Gate tests confirmed green on all 3 OSes.
