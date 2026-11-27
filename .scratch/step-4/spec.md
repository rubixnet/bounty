# Step 4: Centralized Backend, Hash Anchoring and Swappable `DecisionEngine`

Status: ready-for-agent (approved by the human 2026-11-26: ADR numbers 0004–0006, every policy default and `422` for `future_window` and `window_too_old` confirmed)

Source: `docs/steps.md` (Step 4 Build 1–2 and Exit Gate items 1–4), `docs/roadmap.md` (sub-steps 4a–4d), `docs/protocol.md` sections 2, 2a, 2b, 2c, 3, 4 and 5, `docs/goal.md` (server, `DecisionEngine`, cost), `docs/criteria.md` (Requirements 5, 6, 9, 10, 11), `GLOSSARY.md`, `docs/adr/0002`, and the AGENTS.md invariants "Single Source of Shared Types" (rule 5), "Zero Mocking of Core Mechanics" (rule 4), "Zero Raw Content Persistence", "Jev Integration (Step 4)" and "Offline & Local Models".

Every decision below was settled in the Step 4 grill (2026-11-26). The decisions are listed by number (Q2–Q32) in "Resolved Decisions" at the end. Anything the grill did not settle is listed under "Left to tickets" and is **not** decided here.

**One spec, 5 vertical slices.** `docs/roadmap.md` plans four sub-steps (4a–4d). At the human's request this spec covers all of Step 4 in five tracer-bullet slices, S1, S2, S3a, S3b and S4, each sized for one fresh session (~150k tokens). If a slice does not fit, split *that* slice, not the spec. Slice 1 rewrites `docs/roadmap.md` Step 4 to match.

**Cost targets are unchanged.** The hard ceiling is ≤$500/mo all-in (hosting, storage and Jev together) at 15,488 MAU / 109,759 hrs/mo (Requirement 11). The production reference target is ≤$300/mo, on a ~$60–85/mo dedicated server class. No cost wording changes in `steps.md`, `goal.md` or `AGENTS.md`.

## Gate items this spec owns

- [ ] **Gate 1:** a benchmark simulates 15,488 MAU (110k hrs/mo peak burst) against Postgres + Axum with <50 ms p95 ingest latency and <512 MB server RAM. *(S3b)*
- [x] **Gate 2:** an itemized all-in monthly cost model totals ≤$300 at 15,488 MAU and stays ≤$500 under the load test's peak burst. *(S3b; `docs/cost-model.md`: $167.71 and $449.97, with dated sources. Jev's price is third-party only and the bytes per bucket come from the smoke run: refresh both from the nightly report.)*
- [x] **Gate 3:** recomputing a submitted Audit Bundle against anchored `server_received_at` tips catches backdating, `frame_digest` tampering and clock-skew overlap attacks. *(S2 builds the flags; S3a proves it. Real frame tampering moves to 3b/3c, see the deviation below.)* Proven by `crates/traction-server/tests/postgres_submission.rs`: `a_backdated_tip_is_late_anchor_red_when_common_and_yellow_when_rare`, `a_cut_tail_is_red_because_an_anchor_is_missing_from_the_bundle`, `rows_that_differ_from_the_live_sync_are_red`, `a_rewritten_chain_with_the_same_key_fails_against_the_stored_anchors`, `a_changed_frame_digest_is_red`, `two_device_clock_skew_is_yellow_with_clock_skew_and_multi_device_overlap`, and `e2e_daemon.rs::traction_submit_sends_a_real_daemons_bundle_and_the_server_triages_it` (CI-green on Linux still pending).
- [ ] **Gate 4:** `HeuristicEngine` passes its full suite with zero network calls *(S3a; done: `heuristic::tests::*`, notably `the_engine_runs_with_no_network_and_no_runtime`, `telemetry_tier_never_changes_the_colour`, `each_yellow_signal_alone_makes_yellow`)*; `JevEngine` passes contract tests for the `v1/systemone` payload shape, WebP → baseline JPEG transcoding, the companion-`noul` "nothing fits" handling, the max-severity combination with Heuristic and score-distribution handling *(S4)*.
- [ ] All of the above pass in CI under the split recorded below: every job on all 3 OSes, except the Postgres- and MinIO-backed jobs, which run on Linux.

### Step 4 scope deviations (recorded in `docs/steps.md` by S1)

1. **Postgres and MinIO tests run on Linux only.** `traction-server` deploys only on Linux, and installing Postgres on Windows and macOS runners is slow and flaky. These still run on **all 3 OSes**: `cargo fmt`, `cargo clippy --workspace --all-targets`, every `traction-core` test, every `traction-daemon` test (including the sync client against a local test HTTP server), and every `traction-server` test that needs no database. The real Postgres 17 and MinIO service-container tests run on `ubuntu-latest`. (ADR, see Documentation.)
2. **Real frame tampering moves to the 3b/3c gate.** Frames do not exist until 3b. Step 4 proves `frame_digest` integrity (changing a link's `frame_digest` fails verification). Server verification goes through `verify_bundle`, so real frames are covered once they exist. 3b/3c gain a gate item that runs frame tampering through this same server path.

## Terminology (read first)

| Wording | Canonical term | Notes |
|---|---|---|
| Heartbeat, sync | **Anchor** (existing) | One `POST /v1/anchors` per **device** every 2 minutes, carrying the Chain Tip plus the rows sealed since the last acknowledged seq. `server_received_at` is assigned by the server. |
| Per-minute mutation/turn counts | **Timeline Mark** (new) | An unsigned, content-free per-Project, per-minute count record, outside the chain. Always labelled `unsigned`. |
| `traction login` handshake | **Pairing** (new) | Enrolling a device: approving it in a browser, issuing its device token and pinning its Device Signing Key. |
| Green / Yellow / Red | **Triage** (new) | A `DecisionEngine`'s routing of a submitted Audit Bundle. Green = fast-track human confirmation; Yellow = fail-closed human review; Red = anomaly pin. Never a ban, never automatic approval. |
| Live stats | **Logged Hours** (existing) | Live totals are built from buckets that are synced but not yet verified. They are never proof; only a verified Audit Bundle is. |

S1 adds **Timeline Mark**, **Pairing** and **Triage** to `GLOSSARY.md`, and spells out the line between Logged Hours and verified stats.

## Problem Statement

After Step 3a, each device holds a signed, hash-chained ledger that verifies offline, but the chain only shows "unchanged since sealing". Without an outside reference, cutting off the tail is invisible, the key is trusted on first use, and timestamps are the device's own clock. There are no accounts, no profiles, no live dashboard (Requirement 5), and no way for a reviewer to receive or check Submitted Hours (Requirement 10).

Step 4 adds the server that:

- timestamps Chain Tips as they arrive, so the times can't be backdated;
- pins each device's key when it pairs;
- turns synced buckets into live Logged Hours;
- merges a user's devices so one wall-clock hour never yields more than one human hour;
- verifies a submitted Audit Bundle against everything it has seen;
- routes the result through a swappable `DecisionEngine`.

It stays within the cost cap and keeps strict zero-raw-content privacy. `traction-core`'s `verify_chain`, `verify_chains`, `verify_bundle` and rollup types are reused **unchanged**.

## Architecture invariants (apply to every slice)

- **Core unchanged, additions allowed.** No existing `traction-core` function or type changes. New things are added to core: the `sync` wire-type module, the `TimelineMark` type, and any pure `validate_*` functions the ingest checks need. (Q3, Q12, Q25, Q29)
- **Verify at submission, not per heartbeat.** The server never runs chain verification on ingest. Each Anchor stores `(device, key_id, seq, entry_hash, window_start_ms, server_received_at)`. Full `verify_chains` runs only when an Audit Bundle is submitted. (Q3, ADR)
- **Anchors never get overwritten.** `(device, key_id, seq)` is idempotent only when `entry_hash` matches. A different `entry_hash` for an already-anchored `seq` gets `409 Conflict`, and the stored Anchor is never changed. (Q3)
- **One wire contract.** The request and response structs for heartbeats, pairing and bundles live in `traction-core::sync` with `deny_unknown_fields`. The daemon, the real server and the macOS/Windows test server all use them. (Q29)
- **Zero raw content.** The server receives only Work Buckets, Activity Rollups, Timeline Marks (counts plus the `harness_id` set; no `cwd`, `model_id` or session IDs), Project metadata (per `protocol.md` §2a), Anchors, provenance, and Audit Bundles. GitHub commit data is stored as `oid`, `authored_at`, `committed_at` and an author-match flag only: no messages, no diffs. (Q16, Q20, Q25)
- **Never auto-ban.** `Triage` has no ban variant. (Q14)
- **Deterministic failures beat engines.** A chain verification failure, an Anchor that matches no link, or a row/live-bucket mismatch is Red before any engine runs, and no engine can upgrade it. (Q14)
- **Secrets live only in env vars or files.** `TYPESAFE_API_KEY`, the R2 keys, the GitHub App private key and the OIDC client secret come from environment variables or files that the config references. The server refuses to start if a secret is written inline (a test covers this). CI provenance **public** keys may be written inline. (Q30)

## Solution

### S1: Anchor tracer (daemon → server → Postgres → SSE)

**Server scaffold.** A new `crates/traction-server` workspace member, built on Axum with runtime-checked SQLx queries (`sqlx::query_as`, no `query!` macros and no `.sqlx` cache). Migrations are embedded with `sqlx::migrate!`. Configuration comes from one `traction-server.toml` with the sections `[anchor_policy]`, `[heuristic]`, `[jev]`, `[auth]`, `[roles]`, `[blob]` and `[github]`; each slice fills in its own sections. (Q7, Q13, Q30)

**Schema.** The tables are `users`, `devices`, `device_keys` (historical keys are kept), `pairings`, `projects`, `project_aliases`, `work_buckets`, `activity_rollups`, `anchors`, `timeline_marks`, `bundles`, `triage_decisions`, `linked_repos` and `repo_commits`. Each slice migrates the tables it uses.
- **Partitioning.** Native `PARTITION BY RANGE` on monthly partitions:
  - `work_buckets` on `minute_start_ms`
  - `activity_rollups` on `window_start_ms`
  - `anchors` on `server_received_at`
  - `timeline_marks` on `minute_start_ms`
- **Partition creation.** The server creates partitions for the previous month, the current month and the next two at startup and again daily, with no `pg_partman`. It also creates them on demand for any window within `MAX_BACKLOG_AGE`. There is no `DEFAULT` partition; a row outside every partition gets a structured error.
- **Column storage.** Buckets and rollups are stored as typed columns that round-trip *exactly* to `WorkBucket` and `ActivityRollup`, so the submission cross-check is plain struct equality, with no JSONB. (Q13, Q25, Q26)

**Accounts and Pairing.** One `user` has many `device`s. A user is keyed by the `AuthProvider`'s stable `sub`. S1 ships the dev `AuthProvider`; S2 adds Hack Club OIDC. The pairing flow (Q5):
1. `traction login` gets a pairing code from the server and opens `<server>/auth/login?pair=…` in the browser.
2. The server runs the `AuthProvider`. It then renders one plain HTML page (not Next.js) that shows the pairing code and an explicit, CSRF-protected **Approve device** button. A GET redirect never approves anything on its own.
3. The CLI polls and receives a device token.
4. The device uploads its Ed25519 public key during pairing, and the server pins it in `device_keys`.

When a Successor Chain re-pairs, its new key is added and **every historical `key_id` stays trusted**, so older chains still verify.

**Device token.** 32 random bytes, stored on the server only as a SHA-256 hash. On the device it lives at `<data-dir>/server.token` (default `~/.traction/server.token`, respecting `--data-dir`), protected with the same `owner_only` code as `signing.key` (`0600` on Unix, an owner-only DACL on Windows). `DELETE /v1/devices/{id}` revokes the token and stops its pinned keys from being accepted for new Anchors; the keys stay in `trusted_keys` so history still verifies. Every device has its own token-bucket rate limit, and a `429` response carries `Retry-After`. (Q5, Q27)

**Daemon sync client.**
- It runs as one daemon task. It reads sealed rows through a `mode=ro` connection, and a `sync_cursor` row is written only through the single writer.
- The server URL and the token path can be injected through config or flags, so tests stay isolated.
- When the server is unreachable it retries with jittered backoff and honours `Retry-After`. On reconnect it sends the backlog, split into ordered requests under a per-request batch-size cap. Once the backlog is older than one documented constant, those requests are marked `sync_mode: offline_buffered`.
- The server answers with the items it accepted and rejected. The daemon moves the cursor past terminal rejections (`window_too_old`), keeps a local count of them, and retries every other rejection.
- The full `network_class` and offline UX stays in 3c. (Q4, Q8, Q26)

**`POST /v1/anchors` (per device, every 2 minutes).** Each request carries:
- the Chain Tip `(key_id, seq, entry_hash)` and its `window_start_ms`
- `sync_mode` and `sent_at_ms`
- every Work Bucket and Activity Rollup sealed since the last acknowledged seq, with their Project IDs
- any Project metadata that changed

Timeline Marks join the request in S2. The server assigns a monotonic `server_received_at`. Each item is validated (Q12) by deserializing it into the unchanged core types with `deny_unknown_fields`, and rejected with value-free reason codes on any of:
- an unknown `ruleset_id` (only `TRACTION_RULES_1` is known)
- `human_active_sec > 60`
- `struggle_credit_sec > human_active_sec`
- `idle_autonomous_sec > agent_compute_sec`
- a misaligned minute or window
- a window after the tip's `window_start_ms`
- invalid `project_id` syntax
- a body over the size cap

A window that starts after `server_received_at + MAX_FUTURE_SKEW` gets `422 future_window`, and one older than `MAX_BACKLOG_AGE` gets `422 window_too_old` (Q11, Q26). The constants are in "Policy constants" below. S1 also rewrites `protocol.md` §3 to describe this per-device heartbeat.

**Live fanout.** Live updates go out over SSE, fed by an in-process `tokio::sync::broadcast` channel per user, behind a `LiveFanout` trait (a single node; no `LISTEN/NOTIFY` and no WebSockets). If a subscriber falls behind and gets `RecvError::Lagged`, it is sent a resync/refresh event, so a client never silently misses updates. (Q10)

### S2: Trust on the timeline

**Anchor timing flags (`[anchor_policy]`).** (Q11)
- **`late_anchor`:** a `realtime` window first anchored more than `MAX_ANCHOR_LAG` after it ends.
- **`offline_buffered`:** windows sent in `offline_buffered` mode are accepted and tagged. They count as Logged Hours, they push a submission to Yellow, and they are never treated as proof of timing.
- **`clock_skew`:** the server stores `skew = server_received_at − sent_at_ms` for every heartbeat and flags the device while `|skew| > MAX_CLOCK_SKEW`.

**Merging multi-device overlap (computed at read time; stored buckets stay untouched).** (Q9)
- **The cap.** When one user's devices together report more than 60 s of `human_active_sec` in one UTC minute, the total is capped at 60.
- **How buckets shrink.** Each bucket shrinks by the same proportion, using integer math and the largest-remainder rule. Ties are broken deterministically by sorting on `(device_id, project_id)`.
- **What follows.** `struggle_credit_sec` scales the same way, and the minute is flagged `multi_device_overlap`. Agent Compute Time is never capped.

**Project metadata and visibility.** (Q19)
- **Fields.** `project_name`, `project_root_path`, `project_home_relative_path`, `os_username` and `linked_repo`, as in `protocol.md` §2a.
- **Who sees what.** Paths and the username are visible only to the owner and to reviewers. The public API returns `project_name` and totals only.
- **Roles.** `owner`, `reviewer` and `admin`, granted by listing provider `sub`s under `[roles]`.
- **Account deletion.** `DELETE /v1/me` hard-deletes the user's rows and their R2 objects. Until S3a adds `BlobStore`, there are none to delete.

**Timeline Marks.** (Q20, Q25)
- **The type.** A new core type, `TimelineMark { project_id, minute_start_ms, mutations_manual, mutations_harness, turns_started, tool_failures, harnesses: [harness_id] }`.
- **On the daemon.** The finalizer pass computes the marks from stored events into a new table that sits beside the buckets but outside the chain.
- **Sync and storage.** The marks are synced in the heartbeat and stored in a partitioned `timeline_marks` table, labelled `unsigned` in every API response.

**Timeline API.** For one Project, it lines up Work Buckets, Activity Rollups, commit oids and Timeline Marks per minute. This is the data source for the 5b Audit Strip. (Q20)

**Hack Club OIDC `AuthProvider`.** OIDC authorization code + PKCE, configured from env and `[auth]`, and wired into the S1 Pairing flow. (Q5)

### S3a: Submission path

**`BlobStore` trait.** (Q15)
- **`R2BlobStore`:** sans-IO SigV4 presigning (for example `rusty-s3`).
- **`LocalBlobStore`:** URLs signed with an HMAC and served by the server itself, so a self-hoster needs no R2.

**Upload flow.** (Q15)
1. `traction submit --project <id>` exports the bundle and calls `POST /v1/bundles`, which returns `{bundle_id, put_url}`. The URL is presigned with the exact `Content-Length`, expires after 15 minutes, and is capped at 64 MB.
2. The CLI uploads the bundle with a `PUT` straight to the blob store.
3. The CLI calls `POST /v1/bundles/{id}/complete`.

**Verifying on `complete`.** (Q3)
1. **The chain.** `verify_bundle` / `verify_chains` run unchanged, with `trusted_keys` set to the device's pinned keys, historical ones included.
2. **Every Anchor matches a link.** Every stored Anchor for that device must appear among the bundle's links with the same `entry_hash`. A missing or different one means truncation or rewriting.
3. **Backdating.** A link whose window was first anchored late carries `late_anchor`.
4. **Rows match the live sync.** The bundle's rows must equal the buckets synced live; this is a struct-equality check.
5. **Project IDs.** Verification keeps the original `project_id`s, because those are what the hashes cover.

**`DecisionEngine`.** (Q14)
```rust
trait DecisionEngine { async fn triage(&self, input: &TriageInput) -> Result<Triage, EngineError>; }
```
- **`TriageInput` is anonymized.** It holds opaque per-submission IDs, 15-minute aggregates of the submitted Project's buckets and rollups, the bundle verification report, and the anchor flags. It holds no user, path, username or Project name.
- **`Triage`.** `Green | Yellow | Red`, reason codes, `engine_id` and the rubric version.
- **When it runs.** Triage runs only on bundle submission in Step 4.
- **What gets stored.** The stored decision records which engine actually decided.

**`HeuristicEngine` (`[heuristic]`, deterministic, zero network).** The rules run in this order. (Q21, Q32)
- **Red:** the deterministic failures, plus either of:
  - `injected_hid_detected` minutes make up at least `red_injected_share` (default 10%) of the submitted Human Active Time;
  - `late_anchor` windows make up at least `red_late_share` (default 5%) of the submitted realtime windows.
- **Yellow:** any of:
  - any `offline_buffered` windows
  - `clock_skew`
  - a key discontinuity
  - a `self_built` binary (from S3b provenance; until then the rule has nothing to read)
  - a `multi_device_overlap` share at or above its threshold
  - long runs of Human Active Time where the Timeline Marks show no mutations, no commits and no turns
  - Credited Time at or below `yellow_credit_ratio` of Human Active Time
- **Green:** none of the above. Green is a fast-track human confirmation, never an automatic approval.
- **Telemetry Tier never changes the colour.** It already lowers Credited Time through `degraded_tier_credit_milli`. The engine reports it only as the informational reason code `tier_limited_share`. macOS and Windows ship `reduced` and Wayland ships `none`, so a tier rule would keep those users out of Green, which contradicts the glossary.
- **The Red injection rule.** It can only trigger today on Linux X11, the one backend with an injection signal (`protocol.md` §2b), and the engine's docs say so.

### S3b: Ecosystem and scale

**GitHub App linking.** (Q16)
- **The App.** It gets read-only `metadata` and `contents` permissions on the repos the user selects when installing it.
- **Ownership check.** A user-to-server token must show push access before a link is created.
- **Commit fetch.** Installation tokens fetch commits, which are stored as `oid`, `authored_at`, `committed_at` and whether the author's login matches the user's linked GitHub login.
- **The rule on linking.** Linking is optional for tracking (ADR 0002). Submission Eligibility, a public Linked Repo, stays in 5a.

**Project merge.** It is alias-only, through `project_aliases(from_id → to_id)`. Alias chains are followed, cycles are refused, and merging only works within one user's own Projects. Buckets are never rewritten, and every query resolves aliases. Unmerging deletes the alias. (Q17)

**Build provenance (soft).** (Q18)
- **The statement.** Release CI signs `{version, git_commit, target_triple}` with a CI Ed25519 key, and that statement is compiled into the binary.
- **When it's sent.** The daemon sends it at pairing and whenever its version changes.
- **The tag.** The server checks it against the CI public keys in config and tags the device `ci_signed` or `self_built`. `self_built` sends a submission to a standard spot-check, never to Red.
- **Documented as soft.** The statement can be copied into a fork, so it is a tag, not proof.

**Load test.** A `load` binary in `crates/traction-server` drives the real server and Postgres 17. (Q23)
- **What it simulates.** 15,488 enrolled devices, peak concurrency at 10× the ~153-device average, realistic heartbeat sizes, and chunked offline backlogs.
- **Its data.** A database preseeded with a month of data (~6.6M buckets).
- **What it measures.** Ingest p95 and server RSS.
- **In CI.** Every PR runs a reduced-scale smoke version on Linux. A nightly or manually triggered (`workflow_dispatch`) Linux job runs it at full scale and publishes the report that Gate 1 is ticked against.

**`docs/cost-model.md`.** It itemizes the monthly cost with dated sources. (Q24) The lines:
- the dedicated server
- Postgres storage growth, from the load test's measured bytes per bucket projected over 12 months
- R2 storage and operations
- Jev at its spend limit
- bandwidth
- backups (`pg_dump` to R2)
- domain and email

It must show ≤$300 at 15,488 MAU and ≤$500 at the load test's peak burst.

**`docs/deploy.md`.** One Linux `traction-server` binary with migrations embedded, a `systemd` unit, and a sample Caddy config for TLS. There is no Docker requirement, and actual provisioning stays a human step. (Q31)

### S4: `JevEngine`

**Prerequisite.** Install the TypeSafe skill (`typesafe@typesafe-ai`), then hold a short grill on mapping the fields to the live `v1/systemone` schema. This spec fixes only the invariants below; the exact field mapping is decided in that grill, and fields are never guessed. (Q22)

**Invariants.** (Q22)
- `POST <base_url>/v1/systemone`. `base_url` and `model` live in `[jev]`, defaulting to `https://api.typesafe.ai` and the pinned `jev-1.14.0` (never a moving alias), so a self-hosted `v1/systemone`-compatible server works too.
- Its input is the anonymized `TriageInput` only, cut into 15-minute windows.
- The questions are batched and atomic:
  - `noul` starts at the 0.7 / 0.3 thresholds;
  - no `choice` has a catch-all `"other"` option; each `choice` is paired with a companion `noul` asking whether any option fits, and "nothing fits" routes to Yellow (per the installed TypeSafe skill (`typesafe@typesafe-ai` 0.6.2) and its live `v1/systemone` contract);
  - `score` is read as the full level probability distribution, never as an average;
  - Jev asks qualitative pattern judgments per window (for example scripted cadence vs a person debugging or building). All arithmetic and numeric thresholds stay in `HeuristicEngine`; the mapping from answers to colours lives in `[jev]`.
- Images are sent only for submitted or flagged bundles. Sampled WebP frames are transcoded in memory on the server to 8-bit baseline JPEG (longest side ≤1568 px), placed in the top-level `assets` map under UUID placeholders, and sent with `TypeSafe-Danger-Zone-Images: true` as a secondary reviewer hint only. A frame with either side under 280 px (TypeSafe's minimum) is skipped, never upscaled, with reason code `frame_too_small_skipped`. If a bundle has frames and the skipped share reaches a `[jev]` threshold (default: all of them), triage is at least Yellow, so tiny frames cannot dodge visual review.
- A hard monthly spend limit lives in `[jev]`. When the limit is reached, or Jev returns an error, triage falls back to `HeuristicEngine` and the decision records the fallback.
- All rubrics, criteria and routing thresholds live in `[jev]`, never in source code.
- **Combination order.** Deterministic checks (S3a) run first, then `HeuristicEngine`, which always runs. If either gives Red, Jev is not called. Otherwise the final colour is the more severe of the Heuristic and Jev colours (Green < Yellow < Red): Jev can escalate but never downgrade.
- The three-way router sends results to Green, Yellow or Red, and never bans.

## Testing seams, by slice

Every seam below uses real mechanics: a real Postgres 17, real Ed25519 and SHA-256, real core verifiers, and the real daemon storage and writer. Only *external third-party services* (Hack Club's IdP, GitHub, TypeSafe, R2) are replaced, by local test servers or test vectors. Nothing is stubbed with `todo!()` or a hardcoded `true`.

| Slice | All 3 OSes | Linux only (deviation 1) |
|---|---|---|
| **S1** | Core: round trips of the `sync` wire types and rejection of unknown fields. Daemon: the sync client against a local test HTTP server built from `traction-core::sync` (cursor resume across a restart, ordered chunking of a backlog, backoff with `Retry-After`, skipping past terminal rejections, `offline_buffered` marking, `server.token` permissions through `owner_only`). Server: the Q12 validation functions, token hashing, and the CSRF check on the approve page, all with no database. | The full end-to-end test: real daemon, real `traction-server` and Postgres 17. Covers pairing, then heartbeat, then buckets stored, then the SSE total going up. Also: idempotent re-send, `409` on a different `entry_hash`, `future_window`, `window_too_old`, partition creation (previous, current, next 2, and on demand), the exact typed-column round-trip property test, `429` rate limiting, resync on `Lagged`, and the test that the server refuses to start with an inline secret. |
| **S2** | Core: `TimelineMark` and the overlap-merge math, pure (proportional scaling with the largest remainder and the `(device_id, project_id)` tie-break, merged totals ≤60 s and identical across runs). Daemon: the finalizer writes Timeline Marks from fixture events, and a canary test proves no raw text, `cwd` or `model_id` reaches the marks table. Server: the OIDC provider (code + PKCE) against a test identity provider run inside the test, which issues real signed tokens. | Two devices with offset clocks: `clock_skew` is flagged and the merged minute is ≤60 s, with `multi_device_overlap` set. `late_anchor` and `offline_buffered` tagging. The public-JSON test (no paths, `os_username` or `project_home_relative_path`). `DELETE /v1/me` removes every row. The timeline API lines up all four sources, with the marks labelled `unsigned`. |
| **S3a** | `HeuristicEngine` over fixture `TriageInput`s, one per Red/Yellow/Green reason code, run with no network access available, including `tier_limited_share` never changing the colour. SigV4 presigning against AWS's published test vectors. `LocalBlobStore` with HMAC URLs: a round trip, expiry, and the `Content-Length` cap. | A red-team suite against the real server and Postgres: a backdated tip, a cut tail (an Anchor missing from the bundle), rewritten rows (bundle vs live-synced buckets), a changed `frame_digest`, and two-device clock skew. Each produces its expected Red or Yellow reason code. Also: the `R2BlobStore` upload round trip against a MinIO service container, and a deterministic failure staying Red whatever the engine says. |
| **S3b** | Provenance: signature checks against the configured CI public key, and the `ci_signed`/`self_built` tagging. The GitHub client against a local test HTTP server replaying recorded real GitHub API responses (pagination, a 403 rate-limit response, the push-access check); the client stores no messages or diffs. The alias resolution logic, including refusing a cycle. | The Project merge API across two devices' Projects. A reduced-scale load smoke run on every PR. The full-scale nightly or `workflow_dispatch` load run (Gate 1 report). |
| **S4** | Contract tests against a local test HTTP server built from the TypeSafe skill's schema: payload shape, the pinned default model and configurable `base_url`/`model`, no `"other"` option and a companion `noul` per choice ("nothing fits" → Yellow), max-severity combination with Heuristic and no Jev call when already Red, skipping frames under 280 px (`frame_too_small_skipped`) and the all-skipped Yellow guard, handling of the full score distribution, the `noul` thresholds, transcoding real WebP fixtures to 8-bit baseline JPEG with the longest side ≤1568 px, the Danger-Zone header sent only for submitted or flagged bundles, falling back to Heuristic when the spend limit is reached, an error also falling back and being recorded, and the router never returning a ban. | (none) |

## Policy constants (server config, not the core Ruleset)

| Constant | Section | Default | Settled in |
|---|---|---|---|
| `MAX_FUTURE_SKEW` | `[anchor_policy]` | 2 min | Q11 |
| `MAX_ANCHOR_LAG` | `[anchor_policy]` | 20 min (≈12 min settle + 2 min cadence + slack) | Q11 |
| `MAX_CLOCK_SKEW` | `[anchor_policy]` | 2 min | Q11 |
| `MAX_BACKLOG_AGE` | `[anchor_policy]` | 45 days | Q26 |
| bundle upload cap / presign expiry | `[blob]` | 64 MB / 15 min | Q15 |
| `red_injected_share` | `[heuristic]` | 10% | Q21 |
| `red_late_share` | `[heuristic]` | 5% | Q21 |
| `noul` thresholds | `[jev]` | 0.7 / 0.3 | Q22 |

## Documentation changes

| Doc | Change | Slice |
|---|---|---|
| `docs/roadmap.md` | Step 4 rewritten as S1, S2, S3a, S3b, S4 | S1 |
| `docs/steps.md` | Both Step 4 scope deviations recorded; 3b/3c gain the frame-tampering item. No cost wording changes | S1 |
| `docs/protocol.md` | §3 becomes the per-device heartbeat, plus the sync contract (S1); `TimelineMark` added (S2) | S1, S2 |
| `GLOSSARY.md` | Timeline Mark, Pairing, Triage, and the Logged Hours vs verified stats line | S1 |
| `docs/adr/0004-verify-at-submission.md` | Verify at submission, not incrementally; why core stays unchanged | S1 |
| `docs/adr/0005-unsigned-timeline-marks.md` | Unsigned, content-free Timeline Marks; signing them could be a later `bundle_schema` bump | S2 |
| `docs/adr/0006-linux-only-db-ci.md` | Postgres and MinIO tests on Linux only | S1 |
| `docs/cost-model.md`, `docs/deploy.md` | New | S3b |

## User Stories

1. As a teenager, I run `traction login`, approve the device in my browser, and my live hours start showing up on the server within a few minutes, without my thinking about it.
2. As a user with a laptop and a desktop, I can't accidentally (or deliberately) earn two human hours in one wall-clock hour.
3. As a privacy-conscious user, I know the server never receives my prompts, file contents, file names, window titles, `cwd`s or commit messages, and that my folder paths and OS username are visible only to me and to reviewers.
4. As a user, I can delete my account and everything the server holds about me.
5. As a reviewer, a submitted Audit Bundle arrives already checked against every Anchor the server timestamped, with a Green, Yellow or Red triage and reason codes.
6. As a self-hoster, I can run Traction with `HeuristicEngine` and `LocalBlobStore`, with no R2, no Jev and no network calls.
7. As Fraud Squad, I can tune the anchor policy, heuristic thresholds and Jev rubrics in one config file, with no code change.
8. As an operator, the whole system stays at or under $300/mo at Hackatime scale, and never goes over $500.

## Out of Scope

- `apps/web/` and `packages/ui/`. The only HTML is the server-rendered pairing page.
- Submission Eligibility enforcement, the reviewer UI, verdicts, `ProjectDbExporter` and the Audit Strip UI (Step 5).
- Snapshot frames, Redaction, the zstd `.traction-bundle` archive, the full `network_class` and offline UX (3b/3c).
- Verifying the chain on every heartbeat, and running several server nodes.
- WakaTime compatibility and any extra Harnesses (Step 6).

## Left to tickets (not settled in the grill; to be decided in a ticket or raised with the human)

- The exact paths of the pairing-poll, Project-metadata, timeline, GitHub-link and merge endpoints (only `/auth/login?pair=`, `POST /v1/anchors`, `POST /v1/bundles`, `POST /v1/bundles/{id}/complete`, `DELETE /v1/me` and `DELETE /v1/devices/{id}` are fixed).
- The per-request batch-size cap, the heartbeat body-size cap, the rate-limit bucket size, and the backlog-age constant above which the daemon marks requests `offline_buffered`.
- The thresholds for `multi_device_overlap` share, `yellow_credit_ratio` and "long runs" of activity with no marks (all go in `[heuristic]`).
- The exact field mapping for `v1/systemone` (the S4 grill, after the skill is installed).

## Resolved Decisions (human, 2026-11-26)

- **Q1 (withdrawn):** the cost targets stay as documented: ≤$500 hard ceiling, ≤$300 production target, ~$60–85 dedicated server class.
- **Q2:** one spec, slices S1, S2, S3a, S3b and S4; `docs/roadmap.md` is updated to match.
- **Q3:** (a) verify at submission. `(device, key_id, seq)` is idempotent only when `entry_hash` matches; otherwise `409`.
- **Q4:** one heartbeat per device, with a per-request batch cap so backlogs split into ordered chunks.
- **Q5:** Pairing; `server.token` lives in the data folder with `owner_only`; an explicit CSRF-protected Approve; historical `key_id`s stay trusted after re-pairing.
- **Q6:** Postgres tests on Linux only (a deviation); everything else on all 3 OSes.
- **Q7:** runtime `sqlx::query_as`.
- **Q8:** the daemon sync client is in scope, with the URL and token path injectable.
- **Q9:** the cap and largest-remainder rule, with the `(device_id, project_id)` tie-break.
- **Q10:** SSE over `broadcast`, with a resync event on `Lagged`.
- **Q11:** `future_window`, `late_anchor`, `offline_buffered` and `clock_skew`, configured in `[anchor_policy]`.
- **Q12:** ingest validation through the core types plus new pure `validate_*` functions in core.
- **Q13:** the schema, monthly partitions (previous, current, next 2), typed columns and `sqlx::migrate!`.
- **Q14:** the `DecisionEngine` trait; deterministic failures win; triage runs only on submission.
- **Q15:** `BlobStore` (R2 + Local) and the presign, PUT, `complete` flow.
- **Q16:** a GitHub App; commits stored as oid and timestamps only.
- **Q17:** alias-only Project merge.
- **Q18:** soft CI-signed provenance.
- **Q19:** roles from config, owner/reviewer-only fields, `DELETE /v1/me`.
- **Q20:** (b) unsigned Timeline Marks.
- **Q21 + Q32:** `HeuristicEngine` rules; Telemetry Tier never changes the colour.
- **Q22:** the Jev invariants only; the field mapping is decided after the skill is installed.
- **Q23:** the `load` binary, a PR smoke run and a nightly full run.
- **Q24:** `docs/cost-model.md`.
- **Q25:** `TimelineMark` in core.
- **Q26:** on-demand partitions within 45 days; terminal `window_too_old` beyond that.
- **Q27:** hashed device tokens, revocation, rate limiting.
- **Q28:** the frame-tampering deviation.
- **Q29:** `traction-core::sync` wire types.
- **Q30:** one TOML config; secrets only in env vars or files; public keys may be inline.
- **Q31:** a binary, a `systemd` unit, Caddy and `docs/deploy.md`.
