# 05: S4 `JevEngine`: TypeSafe `v1/systemone` behind the `DecisionEngine` seam, with a hard spend limit and Heuristic fallback

**What to build:** A second `DecisionEngine` that calls TypeSafe System One. It works from the anonymized `TriageInput` only, with rubrics in `[jev]` config, a hard monthly spend limit that falls back to `HeuristicEngine`, WebP → baseline JPEG transcoding for submitted or flagged bundles, and a three-way router that never bans. It owns the Jev half of Gate 4. See spec "S4: `JevEngine`", "Testing seams" (row S4) and Resolved Decision Q22.

**Blocked by:** 03

**Status:** ready-for-agent

Acceptance:

**Before any Jev code**
- [ ] Install the TypeSafe skill (`claude plugin marketplace add typesafe-ai/skills && claude plugin install typesafe@typesafe-ai`) and read its live `v1/systemone` contract.
- [ ] Hold a short grill with the human on mapping `TriageInput` to the request and response fields. Record the agreed mapping in a comment below. **No field is guessed.**

**Engine**
- [ ] `POST https://api.typesafe.ai/v1/systemone` with `Authorization: Bearer <TYPESAFE_API_KEY>` (env/file only), pinned to `jev-1.14.0`, never an unpinned alias. The base URL can be injected for tests.
- [ ] Its input is the anonymized `TriageInput` only, cut into 15-minute windows. No user, path, username or Project name is ever sent, and a test asserts this on the serialized request.
- [ ] Questions are batched and atomic:
  - `noul` starts at the 0.7 / 0.3 thresholds;
  - every `choice` includes `"other"`;
  - `score` reads the **full level probability distribution**, never an average.
- [ ] All questions, criteria and routing thresholds live in `[jev]`, never in source code.
- [ ] Images go only with submitted or flagged bundles:
  - sampled WebP frames are transcoded **in memory on the server** to 8-bit baseline JPEG with the longest side ≤1568 px;
  - they go in the top-level `assets` map under UUID placeholders;
  - the request carries `TypeSafe-Danger-Zone-Images: true`;
  - images are a secondary reviewer hint only.

  Until 3b ships frames, this path is exercised with fixture images only.
- [ ] There is a hard monthly spend limit in `[jev]`. When the limit is reached, or on any Jev error or timeout, triage falls back to `HeuristicEngine` and the decision records `engine_id` = heuristic plus a fallback reason. Spend never goes over the limit.
- [ ] The three-way confidence router sends results to Green (fast-track human), Yellow (fail-closed human review) or Red (anomaly pin), and **never bans**. Deterministic Red from S3a still wins.

**Tests (spec "Testing seams" S4; all 3 OSes; no real network)**
- [ ] Contract tests against a local test HTTP server built from the skill's schema:
  - payload shape and the pinned model;
  - `"other"` in every choice;
  - handling the full score distribution (two distributions with the same mean route differently where the rubric says so);
  - the `noul` thresholds;
  - the Danger-Zone header present only for submitted or flagged bundles.
- [ ] Transcoding real WebP fixtures produces 8-bit baseline (non-progressive) JPEG with the longest side ≤1568 px.
- [ ] Spend-limit fallback and error/timeout fallback both produce a Heuristic decision, recorded as a fallback.
- [ ] The router never returns a ban, and an engine cannot upgrade a deterministic Red.
- [ ] Gate 4 (Jev part) is ticked in the spec with test names.
- [ ] `cargo fmt`/`clippy`/`test` green on all 3 OSes.

## Comments
