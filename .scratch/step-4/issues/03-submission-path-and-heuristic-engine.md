# 03: S3a Submission path: `BlobStore`, bundle upload, server verification against Anchors, `DecisionEngine` and `HeuristicEngine`

**What to build:** This ticket covers the whole submission path for an Audit Bundle:
- `traction submit` exports the bundle, gets a presigned URL, uploads it to R2 (or the local blob store), and completes the submission;
- the server verifies the bundle with the **unchanged** `verify_bundle`/`verify_chains` against the pinned keys, every stored Anchor and the buckets synced live;
- the server then triages it through the `DecisionEngine` seam, where `HeuristicEngine` is the zero-network default.

It owns Gate 3 and the Heuristic half of Gate 4. See spec "S3a: Submission path", "Testing seams" (row S3a), "Policy constants" and Resolved Decisions Q3, Q14, Q15, Q21, Q28 and Q32.

**Blocked by:** 01, 02

**Status:** ready-for-agent

Acceptance:

**`BlobStore`**
- [ ] A `BlobStore` trait with two implementations:
  - `R2BlobStore`: sans-IO SigV4 presigning, for example `rusty-s3`.
  - `LocalBlobStore`: URLs signed with an HMAC and served by the server itself, so self-hosters need no R2.
- [ ] Both presign with the exact `Content-Length`, a 15-minute expiry and a 64 MB cap (`[blob]`). The R2 keys and the HMAC key come from env or files only.
- [ ] `DELETE /v1/me` (S2) also deletes the user's blobs.

**Upload flow**
- [ ] Daemon CLI `traction submit --project <id>`:
  1. exports the bundle with the existing 3a export;
  2. calls `POST /v1/bundles`, which returns `{bundle_id, put_url}`;
  3. `PUT`s the bundle to that URL;
  4. calls `POST /v1/bundles/{id}/complete`.
- [ ] New `bundles` and `triage_decisions` tables.

**Verification on `complete` (deterministic; the engine never runs first)**
- [ ] `verify_bundle`/`verify_chains` run unchanged, with `trusted_keys` set to the device's pinned keys, historical ones included. Original `project_id`s are kept.
- [ ] Every stored Anchor for the device must appear among the bundle's links with a matching `entry_hash`. A missing or different one (a cut tail, a rewrite) is a deterministic failure.
- [ ] The bundle's rows must equal, as structs, the buckets synced live. A mismatch is a deterministic failure.
- [ ] Windows that were first anchored late carry `late_anchor` into the triage input.
- [ ] Any deterministic failure is stored as **Red** before any engine runs, and no engine can upgrade it.

**`DecisionEngine` seam**
- [ ] `trait DecisionEngine { async fn triage(&self, input: &TriageInput) -> Result<Triage, EngineError>; }`
- [ ] `TriageInput` is anonymized. It holds opaque per-submission IDs, 15-minute aggregates of the submitted Project's buckets and rollups, the verification report, and the anchor flags (`late_anchor`, `offline_buffered`, `clock_skew`, `multi_device_overlap`). It holds no user, path, username or Project name, and a test asserts this on the serialized input.
- [ ] `Triage` is `Green | Yellow | Red`, plus reason codes, `engine_id` and the rubric version. It has **no ban variant**.
- [ ] Triage runs only on bundle submission. The stored decision records which engine actually decided.

**`HeuristicEngine` (`[heuristic]`, deterministic, zero network)**
- [ ] **Red:** the deterministic failures; `injected_hid_detected` minutes at least `red_injected_share` (10%) of the submitted Human Active Time; `late_anchor` windows at least `red_late_share` (5%) of the submitted realtime windows.
- [ ] **Yellow:** any of:
  - any `offline_buffered` windows
  - `clock_skew`
  - a key discontinuity
  - `self_built` (the rule must work while provenance is absent, until 04)
  - a `multi_device_overlap` share at or above its threshold
  - long runs of Human Active Time where the Timeline Marks show no mutations, no commits and no turns
  - Credited Time at or below `yellow_credit_ratio`
- [ ] **Green:** none of the above. It is a fast-track human confirmation, never an automatic approval.
- [ ] Telemetry Tier **never changes the colour**. The engine emits `tier_limited_share` as an informational reason code only.
- [ ] The engine's docs note that the Red injection rule can only trigger today on Linux X11.
- [ ] The three thresholds the spec leaves open (`multi_device_overlap` share, `yellow_credit_ratio`, and the length of a "long run" with no marks) are set in this ticket, documented in `[heuristic]`, and recorded in a comment below.

**Tests (spec "Testing seams" S3a)**
- [ ] **All 3 OSes:**
  - `HeuristicEngine` over fixture `TriageInput`s, one per Red, Yellow and Green reason code, including `tier_limited_share` never changing the colour, run in a test that proves no network access;
  - SigV4 presigning against AWS's published test vectors;
  - the `LocalBlobStore` round trip, expiry and `Content-Length` cap.
- [ ] **Linux only (Postgres + MinIO service container):** a red-team suite against the real server:
  - a backdated tip gives `late_anchor` → Red/Yellow per the thresholds;
  - a cut tail (an Anchor missing from the bundle) gives Red;
  - rewritten rows (bundle vs live sync) give Red;
  - a changed `frame_digest` gives Red;
  - two-device clock skew gives Yellow with `clock_skew` + `multi_device_overlap`;
  - a deterministic failure stays Red whatever the engine returns;
  - the `R2BlobStore` upload round trip against MinIO.
- [ ] **Gate 3** and **Gate 4 (Heuristic part)** are ticked in the spec, with test names.
- [ ] `cargo fmt`/`clippy`/`test` green on all 3 OSes, and the Linux Postgres + MinIO job green.

## Comments
