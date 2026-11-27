# 03: S3a Submission path: `BlobStore`, bundle upload, server verification against Anchors, `DecisionEngine` and `HeuristicEngine`

**What to build:** This ticket covers the whole submission path for an Audit Bundle:
- `traction submit` exports the bundle, gets a presigned URL, uploads it to R2 (or the local blob store), and completes the submission;
- the server verifies the bundle with the **unchanged** `verify_bundle`/`verify_chains` against the pinned keys, every stored Anchor and the buckets synced live;
- the server then triages it through the `DecisionEngine` seam, where `HeuristicEngine` is the zero-network default.

It owns Gate 3 and the Heuristic half of Gate 4. See spec "S3a: Submission path", "Testing seams" (row S3a), "Policy constants" and Resolved Decisions Q3, Q14, Q15, Q21, Q28 and Q32.

**Blocked by:** 01, 02

**Status:** resolved

Acceptance:

**`BlobStore`**
- [x] A `BlobStore` trait with two implementations:
  - `R2BlobStore`: sans-IO SigV4 presigning, for example `rusty-s3`.
  - `LocalBlobStore`: URLs signed with an HMAC and served by the server itself, so self-hosters need no R2.
- [x] Both presign with the exact `Content-Length`, a 15-minute expiry and a 64 MB cap (`[blob]`). The R2 keys and the HMAC key come from env or files only.
- [x] `DELETE /v1/me` (S2) also deletes the user's blobs.

**Upload flow**
- [x] Daemon CLI `traction submit --project <id>`:
  1. exports the bundle with the existing 3a export;
  2. calls `POST /v1/bundles`, which returns `{bundle_id, put_url}`;
  3. `PUT`s the bundle to that URL;
  4. calls `POST /v1/bundles/{id}/complete`.
- [x] New `bundles` and `triage_decisions` tables.

**Verification on `complete` (deterministic; the engine never runs first)**
- [x] `verify_bundle`/`verify_chains` run unchanged, with `trusted_keys` set to the device's pinned keys, historical ones included. Original `project_id`s are kept.
- [x] Every stored Anchor for the device must appear among the bundle's links with a matching `entry_hash`. A missing or different one (a cut tail, a rewrite) is a deterministic failure.
- [x] The bundle's rows must equal, as structs, the buckets synced live. A mismatch is a deterministic failure.
- [x] Windows that were first anchored late carry `late_anchor` into the triage input.
- [x] Any deterministic failure is stored as **Red** before any engine runs, and no engine can upgrade it.

**`DecisionEngine` seam**
- [x] `trait DecisionEngine { async fn triage(&self, input: &TriageInput) -> Result<Triage, EngineError>; }`
- [x] `TriageInput` is anonymized. It holds opaque per-submission IDs, 15-minute aggregates of the submitted Project's buckets and rollups, the verification report, and the anchor flags (`late_anchor`, `offline_buffered`, `clock_skew`, `multi_device_overlap`). It holds no user, path, username or Project name, and a test asserts this on the serialized input.
- [x] `Triage` is `Green | Yellow | Red`, plus reason codes, `engine_id` and the rubric version. It has **no ban variant**.
- [x] Triage runs only on bundle submission. The stored decision records which engine actually decided.

**`HeuristicEngine` (`[heuristic]`, deterministic, zero network)**
- [x] **Red:** the deterministic failures; `injected_hid_detected` minutes at least `red_injected_share` (10%) of the submitted Human Active Time; `late_anchor` windows at least `red_late_share` (5%) of the submitted realtime windows.
- [x] **Yellow:** any of:
  - any `offline_buffered` windows
  - `clock_skew`
  - a key discontinuity
  - `self_built` (the rule must work while provenance is absent, until 04)
  - a `multi_device_overlap` share at or above its threshold
  - long runs of Human Active Time where the Timeline Marks show no mutations, no commits and no turns
  - Credited Time at or below `yellow_credit_ratio`
- [x] **Green:** none of the above. It is a fast-track human confirmation, never an automatic approval.
- [x] Telemetry Tier **never changes the colour**. The engine emits `tier_limited_share` as an informational reason code only.
- [x] The engine's docs note that the Red injection rule can only trigger today on Linux X11.
- [x] The three thresholds the spec leaves open (`multi_device_overlap` share, `yellow_credit_ratio`, and the length of a "long run" with no marks) are set in this ticket, documented in `[heuristic]`, and recorded in a comment below.

**Tests (spec "Testing seams" S3a)**
- [x] **All 3 OSes:** *(green in CI run 46003549496 on `01b596f`.)*
  - `HeuristicEngine` over fixture `TriageInput`s, one per Red, Yellow and Green reason code, including `tier_limited_share` never changing the colour, run in a test that proves no network access;
  - SigV4 presigning against AWS's published test vectors;
  - the `LocalBlobStore` round trip, expiry and `Content-Length` cap.
- [x] **Linux only (Postgres + S3 container; versitygw since `01b596f`, MinIO before):** *(green in CI run 46003549496 on `01b596f`.)* a red-team suite against the real server:
  - a backdated tip gives `late_anchor` → Red/Yellow per the thresholds;
  - a cut tail (an Anchor missing from the bundle) gives Red;
  - rewritten rows (bundle vs live sync) give Red;
  - a changed `frame_digest` gives Red;
  - two-device clock skew gives Yellow with `clock_skew` + `multi_device_overlap`;
  - a deterministic failure stays Red whatever the engine returns;
  - the `R2BlobStore` upload round trip against MinIO.
- [x] **Gate 3** and **Gate 4 (Heuristic part)** are ticked in the spec, with test names. *(CI run 46003549496 on `01b596f`.)*
- [x] `cargo fmt`/`clippy`/`test` green on all 3 OSes, and the Linux Postgres + S3 job green. *(CI run 46003549496 on `01b596f`.)*

## Comments

**2026-11-27 (implementation):** branch `step-4-03-submission-path`.

- **Verification:** `fmt`, `clippy -D warnings` and `cargo test --workspace` are green on macOS against local Postgres 17 and a local MinIO (`TRACTION_REQUIRE_DB=1`, `TRACTION_REQUIRE_MINIO=1`): 677 tests, 0 failed. The 3-OS matrix and the Linux Postgres + MinIO job (MinIO started by a `docker run` step, since a service container cannot take a command) have not run in CI yet, so the last box stays open.
- **`[heuristic]` thresholds set here** (fractions in the TOML, all tunable without a code change):
  - `multi_device_overlap_share = 0.10`: Project minutes that the 60 s merge cap trimmed because another device of the user also had Human Active Time, over the Project's active minutes.
  - `yellow_credit_ratio = 0.15`: Credited Time at or below 15% of Human Active Time. Honest personas earn about 0.6 (full tier) and 0.4 (degraded tier) of wall-clock, every bot at most 0.05, so 0.15 sits well under honest work and above the bots.
  - `long_run_minutes = 90`: six consecutive 15-minute slots, each with at least 300 s of Human Active Time (the documented constant `BUSY_SLOT_MIN_HUMAN_SEC`) and no mutation, commit oid or turn in the marks. Marks are unsigned, so this is Yellow only.
  - `red_injected_share = 0.10` and `red_late_share = 0.05` are the spec defaults.
- **Additions beyond the ticket text (all conservative, Yellow only):** any `late_anchor` window below the Red share and any injected minute below the Red share are Yellow (so a human sees a small amount of a Red signal); `unanchored_tail` (chain links newer than the newest stored Anchor) is Yellow; an engine error is Yellow `engine_error`.
- **Decisions:**
  - **Injection share** is measured in Human Active Time of buckets flagged `injected_hid_detected`. **Late share** is late 2-minute windows over realtime windows (a window is late if any of its buckets is).
  - **No Anchors at all** for the device is a deterministic Red (`no_anchors`): timing cannot be checked.
  - **Row comparison** covers only minutes below the newest Anchor window and newer than `MAX_BACKLOG_AGE` (older rows were terminally rejected at sync; the chain and Anchors protect them). Rollups are compared device-wide the same way.
  - **Deterministic failure codes:** the core `BundleError::code()` strings, plus `malformed_bundle`, `project_mismatch`, `no_anchors`, `anchor_missing`, `anchor_mismatch`, `rows_mismatch`. The engine does not run for any of them (`engine_id = deterministic`).
  - **An engine error** is stored as Yellow `engine_error`. The fallback-to-Heuristic Router is S4.
  - **The submitter is told only `submitted`.** The colour is not returned, because it would let a cheater tune against the thresholds. Reviewers read `triage_decisions`.
  - **The `PUT` goes to the store with no bearer token.** `complete` is idempotent. A device may hold up to 10 unexpired pending bundles.
  - **SigV4 and HMAC are in-crate** (about 100 lines over `sha2`), not `rusty-s3`. AWS's published GET example matches byte for byte and RFC 4231 vectors cover the HMAC. MinIO accepts the signed PUT with `content-length` signed and refuses a body of another length.
  - **`traction submit` is `traction-daemon submit --project <id>`** (there is no separate `traction` binary yet).
  - **No `LocalBlobStore` HMAC key configured** means a random per-process key (URLs die on restart; 15 minute lifetime).
- **Deferred:**
  - `self_built` is always `false` until 04 adds provenance (`AnchorFlags.self_built`).
  - `project_commits` is empty until 04's GitHub client fills it, so the commit side of the long-run rule has nothing to read yet.
  - Blob retention and pruning of abandoned `pending` bundles and their objects.
  - Rate-limiting `POST /v1/bundles` beyond the pending cap.
  - The Windows `server.token` DACL test and the whole CI matrix are still unrun.
