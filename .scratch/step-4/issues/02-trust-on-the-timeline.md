# 02: S2 Trust on the timeline: anchor timing flags, multi-device overlap merge, Project metadata, Timeline Marks, timeline API and Hack Club OIDC

**What to build:** This ticket adds the server-side trust signals on top of S1's ingest path: the `late_anchor`, `offline_buffered` and `clock_skew` flags, a read-time merge capping one user's Human Active Time at 60 s per minute across devices, Project metadata with owner- and reviewer-only visibility, roles, account deletion, unsigned Timeline Marks from daemon to server, the timeline API, and the Hack Club OIDC `AuthProvider`. See spec "S2: Trust on the timeline", "Testing seams" (row S2) and Resolved Decisions Q5, Q9, Q11, Q19, Q20 and Q25.

**Blocked by:** 01

**Status:** ready-for-agent

Acceptance:

**Anchor timing (`[anchor_policy]`)**
- [ ] `late_anchor`: a `realtime` window first anchored more than `MAX_ANCHOR_LAG` (20 min) after it ends.
- [ ] `offline_buffered`: such windows are accepted and tagged. They count as Logged Hours only and are never treated as proof of timing.
- [ ] `clock_skew`: `skew = server_received_at − sent_at_ms` is stored for every heartbeat, and the device is flagged while `|skew| > MAX_CLOCK_SKEW` (2 min).

**Multi-device overlap merge (computed at read time; stored buckets are never changed)**
- [ ] A pure function in `traction-core` caps the total `human_active_sec` per user per UTC minute at 60.
  - Each bucket shrinks by the same proportion, using integer math and the largest-remainder rule. Ties are broken by sorting on `(device_id, project_id)`.
  - `struggle_credit_sec` scales the same way, and the minute gets the `multi_device_overlap` flag.
  - Agent Compute Time is never capped.
- [ ] Property test: merged totals are ≤60 s, the result is identical across runs and across input order, and the merged credit never exceeds the merged human time.
- [ ] Live totals (the S1 SSE) and every read API use the merged figures.

**Projects, roles and deletion**
- [ ] Project metadata is stored from the heartbeat: `project_name`, `project_root_path`, `project_home_relative_path`, `os_username` and `linked_repo` (`protocol.md` §2a).
- [ ] Roles `owner`, `reviewer` and `admin` come from `sub` lists under `[roles]`. Paths and `os_username` are visible only to the owner and to reviewers; the public API returns `project_name` and totals only.
- [ ] `DELETE /v1/me` hard-deletes every row the user owns: devices, keys, buckets, rollups, anchors, marks and Projects.

**Timeline Marks (unsigned)**
- [ ] A new core type, `TimelineMark { project_id, minute_start_ms, mutations_manual, mutations_harness, turns_started, tool_failures, harnesses: [harness_id] }`, with `deny_unknown_fields`, added to the `sync` heartbeat.
- [ ] The daemon finalizer pass computes the marks from stored events into a new table that sits beside the buckets and outside the chain, written only through the single writer. The sync client sends them.
- [ ] On the server, a partitioned `timeline_marks` table on `minute_start_ms`, with the same partition policy as S1. Every API response labels the marks `unsigned`.
- [ ] Canary test: no raw text, `cwd`, `model_id` or session ID reaches the daemon marks table or the heartbeat.

**Timeline API**
- [ ] For one Project, it lines up Work Buckets, Activity Rollups, commit oids and Timeline Marks per minute (the data source for 5b's Audit Strip). Access follows the Project visibility rules.

**Hack Club OIDC `AuthProvider`**
- [ ] OIDC authorization code + PKCE, configured from `[auth]` + env (the client secret is env/file only), and wired into S1's Pairing page.

**Tests (spec "Testing seams" S2)**
- [ ] **All 3 OSes:**
  - the core `TimelineMark` and overlap-merge math;
  - the daemon finalizer writing marks from fixture events, plus the canary;
  - the OIDC provider against a test identity provider run inside the test, which issues real signed tokens.
- [ ] **Linux only (Postgres):**
  - two devices with offset clocks: `clock_skew` is flagged, the merged minute is ≤60 s, and `multi_device_overlap` is set;
  - `late_anchor` and `offline_buffered` tagging;
  - the public JSON contains no `project_root_path`, `project_home_relative_path` or `os_username`;
  - `DELETE /v1/me` leaves zero rows;
  - the timeline API lines up all four sources, with the marks labelled `unsigned`.

**Docs**
- [ ] `docs/protocol.md` adds `TimelineMark` and its heartbeat field.
- [ ] New `docs/adr/0005-unsigned-timeline-marks.md`, which notes that signing them could be a later `bundle_schema` bump.
- [ ] `cargo fmt`/`clippy`/`test` green on all 3 OSes, and the Linux Postgres job green.

## Comments
