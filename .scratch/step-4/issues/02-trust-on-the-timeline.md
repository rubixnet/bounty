# 02: S2 Trust on the timeline: anchor timing flags, multi-device overlap merge, Project metadata, Timeline Marks, timeline API and Hack Club OIDC

**What to build:** This ticket adds the server-side trust signals on top of S1's ingest path: the `late_anchor`, `offline_buffered` and `clock_skew` flags, a read-time merge capping one user's Human Active Time at 60 s per minute across devices, Project metadata with owner- and reviewer-only visibility, roles, account deletion, unsigned Timeline Marks from daemon to server, the timeline API, and the Hack Club OIDC `AuthProvider`. See spec "S2: Trust on the timeline", "Testing seams" (row S2) and Resolved Decisions Q5, Q9, Q11, Q19, Q20 and Q25.

**Blocked by:** 01

**Status:** resolved

Acceptance:

**Anchor timing (`[anchor_policy]`)**
- [x] `late_anchor`: a `realtime` window first anchored more than `MAX_ANCHOR_LAG` (20 min) after it ends.
- [x] `offline_buffered`: such windows are accepted and tagged. They count as Logged Hours only and are never treated as proof of timing.
- [x] `clock_skew`: `skew = server_received_at − sent_at_ms` is stored for every heartbeat, and the device is flagged while `|skew| > MAX_CLOCK_SKEW` (2 min).

**Multi-device overlap merge (computed at read time; stored buckets are never changed)**
- [x] A pure function in `traction-core` caps the total `human_active_sec` per user per UTC minute at 60.
  - Each bucket shrinks by the same proportion, using integer math and the largest-remainder rule. Ties are broken by sorting on `(device_id, project_id)`.
  - `struggle_credit_sec` scales the same way, and the minute gets the `multi_device_overlap` flag.
  - Agent Compute Time is never capped.
- [x] Property test: merged totals are ≤60 s, the result is identical across runs and across input order, and the merged credit never exceeds the merged human time.
- [x] Live totals (the S1 SSE) and every read API use the merged figures.

**Projects, roles and deletion**
- [x] Project metadata is stored from the heartbeat: `project_name`, `project_root_path`, `project_home_relative_path`, `os_username` and `linked_repo` (`protocol.md` §2a).
- [x] Roles `owner`, `reviewer` and `admin` come from `sub` lists under `[roles]`. Paths and `os_username` are visible only to the owner and to reviewers; the public API returns `project_name` and totals only.
- [x] `DELETE /v1/me` hard-deletes every row the user owns: devices, keys, buckets, rollups, anchors, marks and Projects.

**Timeline Marks (unsigned)**
- [x] A new core type, `TimelineMark { project_id, minute_start_ms, mutations_manual, mutations_harness, turns_started, tool_failures, harnesses: [harness_id] }`, with `deny_unknown_fields`, added to the `sync` heartbeat.
- [x] The daemon finalizer pass computes the marks from stored events into a new table that sits beside the buckets and outside the chain, written only through the single writer. The sync client sends them.
- [x] On the server, a partitioned `timeline_marks` table on `minute_start_ms`, with the same partition policy as S1. Every API response labels the marks `unsigned`.
- [x] Canary test: no raw text, `cwd`, `model_id` or session ID reaches the daemon marks table or the heartbeat.

**Timeline API**
- [x] For one Project, it lines up Work Buckets, Activity Rollups, commit oids and Timeline Marks per minute (the data source for 5b's Audit Strip). Access follows the Project visibility rules.

**Hack Club OIDC `AuthProvider`**
- [x] OIDC authorization code + PKCE, configured from `[auth]` + env (the client secret is env/file only), and wired into S1's Pairing page.

**Tests (spec "Testing seams" S2)**
- [x] **All 3 OSes:** *(green in CI run 46003549496 on `01b596f`.)*
  - the core `TimelineMark` and overlap-merge math;
  - the daemon finalizer writing marks from fixture events, plus the canary;
  - the OIDC provider against a test identity provider run inside the test, which issues real signed tokens.
- [x] **Linux only (Postgres):** *(green in CI run 46003549496 on `01b596f`.)*
  - two devices with offset clocks: `clock_skew` is flagged, the merged minute is ≤60 s, and `multi_device_overlap` is set;
  - `late_anchor` and `offline_buffered` tagging;
  - the public JSON contains no `project_root_path`, `project_home_relative_path` or `os_username`;
  - `DELETE /v1/me` leaves zero rows;
  - the timeline API lines up all four sources, with the marks labelled `unsigned`.

**Docs**
- [x] `docs/protocol.md` adds `TimelineMark` and its heartbeat field.
- [x] New `docs/adr/0005-unsigned-timeline-marks.md`, which notes that signing them could be a later `bundle_schema` bump.
- [x] `cargo fmt`/`clippy`/`test` green on all 3 OSes, and the Linux Postgres job green. *(CI run 46003549496 on `01b596f`.)*

## Comments

**2026-11-27 (implementation):** branch `step-4-02-trust-on-the-timeline`.

- **Verification:** `fmt`, `clippy -D warnings` and `cargo test --workspace` are green on macOS against a local Postgres 17 (`TRACTION_TEST_DATABASE_URL` + `TRACTION_REQUIRE_DB=1`): 627 tests, 0 failed. The 3-OS matrix and the Linux Postgres job have not run in CI yet, so the last box stays open until CI is green at the Exit Gate. `tests/oidc.rs` and the core and daemon tests need no database and run on all 3 OSes; `tests/postgres_trust.rs` is Postgres-only.
- **Decisions:**
  - **`owner` role:** implicit. A user owns their own Projects and sees their private fields. Only `reviewers` and `admins` are config `sub` lists. Admins see everything a reviewer does. Reviewers and admins read other users' data with a device token of a user whose `sub` is listed; browser sessions arrive with the website (5a).
  - **Credit in the merge:** `struggle_credit_sec` is scaled by the same proportion with a floor (`credit * 60 / total`), capped at the share's merged human time, with no remainder redistribution. Only Human Active Time gets the largest-remainder rule (credit is a share of human time, not a second time budget).
  - **Timing tags:** `late_anchor` and `offline_buffered` are columns on `work_buckets` (set when first anchored, never changed) and on the Anchor (`late_anchor`, `skew_ms`). A bucket is late when `server_received_at` is more than `MAX_ANCHOR_LAG` after its minute ended. `offline_buffered` requests are never `late_anchor`. `clock_skew` is a device flag from its newest heartbeat (`GET /v1/me`).
  - **Timeline Marks never block sync:** the daemon ignores rejected mark items (no retry, no held cursor), since marks are unsigned context (ADR 0005).
  - **Commit oids:** the timeline reads a new `project_commits` table, which S3b's GitHub client must fill. The test inserts rows directly.
  - **OIDC:** RS256 only, endpoints default to the Doorkeeper paths under the issuer, no discovery request, https required except on loopback. Sessions and pending logins are in memory (single node). The session cookie is `SameSite=Lax` because it is set on a redirect from the IdP; the Approve POST keeps its own `Strict` CSRF cookie.
  - **`[heuristic]` `multi_device_overlap` share threshold:** not needed in this ticket, so **not decided**; it stays with S3a.
- **Deferred:**
  - Verifying the default Hack Club endpoint paths against the live IdP (deploy-time check).
  - Pruning expired pairings and rate-limiting the unauthenticated pairing start (carried over from 01).
  - R2 object deletion in `DELETE /v1/me` (S3a adds `BlobStore`).
  - A `Secure`-less http cookie is used only when `public_url` is not https.
  - The test RSA keys are committed under `crates/traction-server/tests/data/`; allowlist them if a secret scanner objects.
