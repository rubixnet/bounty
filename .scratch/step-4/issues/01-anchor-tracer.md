# 01: S1 Anchor tracer: pairing, daemon sync client, `POST /v1/anchors`, partitioned Postgres and SSE live totals

**What to build:** The thinnest end-to-end path from a real daemon to a live total. It includes a new `traction-server` crate (Axum, runtime SQLx, embedded migrations, monthly partitions), the dev `AuthProvider`, Pairing with pinned Device Signing Keys, hashed device tokens, the daemon sync client, the per-device heartbeat with validation, and SSE fanout. It also records the Step 4 deviations and the docs/ADRs this slice owns. See spec "S1: Anchor tracer", "Architecture invariants", "Testing seams" (row S1), "Policy constants" and Resolved Decisions Q2–Q10, Q12, Q13, Q26, Q27, Q29 and Q30.

**Blocked by:** none

**Status:** ready-for-agent

Acceptance:

**Core (`traction-core`; existing functions and types unchanged)**
- [ ] A new `sync` module holds the wire types for pairing, the heartbeat request/response (accepted and rejected items with value-free reason codes) and device-token exchange. All use `#[serde(deny_unknown_fields)]` and no floats. The heartbeat carries `(key_id, seq, entry_hash)`, the tip's `window_start_ms`, `sync_mode` (`realtime | offline_buffered`), `sent_at_ms`, the Work Buckets and Activity Rollups since the last acknowledged seq, and changed Project metadata. Tests cover the round trip and rejection of unknown fields at every level.
- [ ] New pure `validate_*` functions cover every Q12 ingest check: `ruleset_id` known (`TRACTION_RULES_1`), `human_active_sec ≤ 60`, `struggle_credit_sec ≤ human_active_sec`, `idle_autonomous_sec ≤ agent_compute_sec`, minute/window alignment, no window after the tip's `window_start_ms`, and `project_id` syntax. Each returns a stable `code()`.

**Server (`crates/traction-server`, new workspace member)**
- [ ] Axum + SQLx using runtime `sqlx::query_as` only. A source-grep test fails on any `query!`/`query_as!` macro, and the build needs no `.sqlx` cache or live database. Migrations use `sqlx::migrate!`.
- [ ] The config is `traction-server.toml` with the sections `[anchor_policy]`, `[heuristic]`, `[jev]`, `[auth]`, `[roles]`, `[blob]` and `[github]` (later slices fill theirs). Secrets come only from env or referenced files. The server **refuses to start** with a secret written inline, which a test covers. Public keys may be inline.
- [ ] S1 tables: `users`, `devices`, `device_keys` (historical keys kept), `pairings`, `projects`, `work_buckets`, `activity_rollups` and `anchors`.
- [ ] Partitioning:
  - `work_buckets` (`minute_start_ms`), `activity_rollups` (`window_start_ms`) and `anchors` (`server_received_at`) use native monthly `PARTITION BY RANGE`.
  - Partitions for the previous, current and next 2 months are created at startup and daily, without `pg_partman`, plus on demand for any window within `MAX_BACKLOG_AGE` (45 days).
  - There is no `DEFAULT` partition.
- [ ] Buckets and rollups are stored as typed columns. A property test proves an exact round trip, `WorkBucket`/`ActivityRollup` → row → struct equality.
- [ ] Pairing:
  - The dev `AuthProvider` exists, and users are keyed by the provider's stable `sub`.
  - `<server>/auth/login?pair=…` renders one plain HTML page that shows the pairing code and a CSRF-protected **Approve device** button. A GET never approves.
  - Approval binds the device's uploaded Ed25519 public key into `device_keys`. Re-pairing a Successor Chain adds the new key and keeps every historical `key_id` trusted.
- [ ] Device token: 32 random bytes, stored server-side only as SHA-256. `DELETE /v1/devices/{id}` revokes the token and stops the device's keys being accepted for new Anchors, while they stay trusted for history.
- [ ] There is a per-device token-bucket rate limit, and `429` carries `Retry-After`.
- [ ] `POST /v1/anchors`, one per device:
  - It assigns a monotonic `server_received_at`.
  - `(device, key_id, seq)` is idempotent when `entry_hash` matches, and gets **`409 Conflict`** when it differs. The stored Anchor never changes.
  - Each item is validated with the core `validate_*` functions and returned as accepted or rejected.
  - `422 future_window` applies after `server_received_at + MAX_FUTURE_SKEW` (2 min), `422 window_too_old` beyond `MAX_BACKLOG_AGE`, and a size cap applies to the body.
- [ ] SSE live totals per user (Logged Hours) come from an in-process `tokio::sync::broadcast` behind a `LiveFanout` trait. `RecvError::Lagged` pushes a resync/refresh event, never a silent drop.

**Daemon (`traction-daemon`)**
- [ ] `traction login` runs Pairing and stores the token at `<data-dir>/server.token` (default `~/.traction/server.token`, respecting `--data-dir`) through the existing `owner_only` code (`0600` on Unix, an owner-only DACL on Windows).
- [ ] The sync client is one task:
  - It reads sealed rows through `mode=ro` + `PRAGMA query_only = ON`. The `sync_cursor` row is written only through the single writer.
  - The server URL and the token path can be injected through config or flags.
  - It retries with jittered backoff and honours `Retry-After`. The backlog splits into ordered requests under a documented per-request batch cap.
  - Requests are marked `offline_buffered` once the backlog is older than one documented constant.
  - It moves the cursor past terminal rejections (`window_too_old`) and keeps a local count of them; it retries everything else.

**Tests (spec "Testing seams" S1)**
- [ ] **All 3 OSes:**
  - the core `sync` round-trip and unknown-field tests;
  - the daemon sync client against a local test HTTP server built from `traction-core::sync`: cursor resume across a restart, ordered backlog chunking, backoff with `Retry-After`, skipping past terminal rejections, `offline_buffered` marking, and `server.token` permissions;
  - server tests that need no database: validation, token hashing, the CSRF check, and refusing an inline secret.
- [ ] **Linux only (Postgres 17 service container):**
  - the full end-to-end run: real daemon, real `traction-server` and Postgres, covering pairing, then heartbeat, then stored rows, then the SSE total going up;
  - idempotent re-send and `409`;
  - `future_window` and `window_too_old`;
  - partition creation (previous, current, next 2, and on demand);
  - the typed-column round-trip property test;
  - `429` and resync on `Lagged`.
- [ ] The CI workflow gains a Linux job with a Postgres 17 service, and every non-database job stays on the 3-OS matrix.

**Docs**
- [ ] `docs/steps.md` records both Step 4 deviations (Postgres/MinIO tests on Linux only; frame tampering moves to the 3b/3c gate, and 3b/3c gain that item). No cost wording changes.
- [ ] `docs/roadmap.md` Step 4 is rewritten as S1, S2, S3a, S3b and S4.
- [ ] `docs/protocol.md` §3 becomes the per-device heartbeat and the sync contract.
- [ ] `GLOSSARY.md` adds Pairing, Timeline Mark and Triage, and sets out Logged Hours vs verified stats.
- [ ] New ADRs: `docs/adr/0004-verify-at-submission.md` and `docs/adr/0006-linux-only-db-ci.md`.
- [ ] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` green on all 3 OSes, and the Linux Postgres job green.

## Comments
