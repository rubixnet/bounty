# 05: S4 `JevEngine`: TypeSafe `v1/systemone` behind the `DecisionEngine` seam, with a hard spend limit and Heuristic fallback

**What to build:** A second `DecisionEngine` that calls TypeSafe System One. It works from the anonymized `TriageInput` only, with rubrics in `[jev]` config, a hard monthly spend limit that falls back to `HeuristicEngine`, WebP → baseline JPEG transcoding for submitted or flagged bundles, and a three-way router that never bans. It owns the Jev half of Gate 4. See spec "S4: `JevEngine`", "Testing seams" (row S4) and Resolved Decision Q22.

**Blocked by:** 03

**Status:** ready-for-agent

Acceptance:

**Before any Jev code**
- [x] Install the TypeSafe skill (`claude plugin marketplace add typesafe-ai/skills && claude plugin install typesafe@typesafe-ai`) and read its live `v1/systemone` contract.
- [x] Hold a short grill with the human on mapping `TriageInput` to the request and response fields. Record the agreed mapping in a comment below. **No field is guessed.**

**Engine**
- [ ] `POST https://api.typesafe.ai/v1/systemone` with `Authorization: Bearer <TYPESAFE_API_KEY>` (env/file only), with `base_url` and `model` in `[jev]`, defaulting to `https://api.typesafe.ai` and the pinned `jev-1.14.0`, never a moving alias. Any `v1/systemone`-compatible self-hosted server works by changing them.
- [ ] Its input is the anonymized `TriageInput` only, cut into 15-minute windows. No user, path, username or Project name is ever sent, and a test asserts this on the serialized request.
- [ ] Questions are batched and atomic:
  - `noul` starts at the 0.7 / 0.3 thresholds;
  - no catch-all `"other"` option; each `choice` has a companion `noul` ("does any option fit?"), and "nothing fits" → Yellow;
  - questions are qualitative pattern judgments per window; arithmetic and numeric thresholds stay in `HeuristicEngine`;
  - `score` reads the **full level probability distribution**, never an average.
- [ ] All questions, criteria and routing thresholds live in `[jev]`, never in source code.
- [ ] Images go only with submitted or flagged bundles:
  - sampled WebP frames are transcoded **in memory on the server** to 8-bit baseline JPEG with the longest side ≤1568 px;
  - they go in the top-level `assets` map under UUID placeholders;
  - the request carries `TypeSafe-Danger-Zone-Images: true`;
  - images are a secondary reviewer hint only;
  - a frame with either side < 280 px is skipped (never upscaled) with reason code `frame_too_small_skipped`; if a bundle has frames and the skipped share reaches the `[jev]` threshold (default 1.0), triage is at least Yellow.

  Until 3b ships frames, this path is exercised with fixture images only.
- [ ] There is a hard monthly spend limit in `[jev]`. When the limit is reached, or on any Jev error or timeout, triage falls back to `HeuristicEngine` and the decision records `engine_id` = heuristic plus a fallback reason. Spend never goes over the limit.
- [ ] The three-way confidence router sends results to Green (fast-track human), Yellow (fail-closed human review) or Red (anomaly pin), and **never bans**. Deterministic Red from S3a still wins. `HeuristicEngine` always runs first; Jev is skipped when deterministic checks or Heuristic give Red; otherwise the result is `max_severity(heuristic, jev)`, so Jev escalates but never downgrades.

**Tests (spec "Testing seams" S4; all 3 OSes; no real network)**
- [ ] Contract tests against a local test HTTP server built from the skill's schema:
  - payload shape and the pinned model;
  - no `"other"` option, and a companion `noul` per choice whose "nothing fits" answer gives Yellow;
  - max-severity combination, and no Jev request at all when deterministic/Heuristic is already Red;
  - frames under 280 px skipped with `frame_too_small_skipped`, and the all-skipped Yellow guard;
  - handling the full score distribution (two distributions with the same mean route differently where the rubric says so);
  - the `noul` thresholds;
  - the Danger-Zone header present only for submitted or flagged bundles.
- [ ] Transcoding real WebP fixtures produces 8-bit baseline (non-progressive) JPEG with the longest side ≤1568 px.
- [ ] Spend-limit fallback and error/timeout fallback both produce a Heuristic decision, recorded as a fallback.
- [ ] The router never returns a ban, and an engine cannot upgrade a deterministic Red.
- [ ] Gate 4 (Jev part) is ticked in the spec with test names.
- [ ] `cargo fmt`/`clippy`/`test` green on all 3 OSes.

## Comments

**2026-11-27 (grill with the human, before implementation):** TypeSafe skill `typesafe@typesafe-ai` 0.6.2 installed; contract read from `https://api.typesafe.ai/openapi.json` and the live docs (models, images, primitives, API).

- **Request mapping:** `model` ← `[jev].model` (default `jev-1.14.0`); `state` ← one JSON object per 15-minute window built from `TriageInput` with named fields (no IDs, paths, usernames or Project names); `questions` ← a named map of `noul`/`choice`/`score` questions from `[jev]`; `assets` ← UUID-keyed `{type: "image", format: "jpg", data_base64}` only for submitted or flagged bundles, with `TypeSafe-Danger-Zone-Images: true`. Response: `answers[name]` typed by `type`; `noul` is a probability, `choice` reads `probabilities`, `score` reads `probabilities` per level (never only `score`), and `model`/`usage.input_tokens` are logged and drive spend ($0.042 per million input tokens, output free, per the models page on 2026-11-27).
- **Decisions:**
  1. No catch-all `"other"` (AGENTS.md amended); a companion `noul` per `choice`, "nothing fits" → Yellow.
  2. Heuristic always runs first; skip Jev when already Red; final = `max_severity(heuristic, jev)`.
  3. Qualitative per-window pattern questions only; colour mapping in `[jev]`; `base_url`/`model` configurable for self-hosted servers.
  4. Frames < 280 px on a side are skipped (`frame_too_small_skipped`); skipped share ≥ `[jev]` threshold (default 1.0) on a bundle with frames → at least Yellow. Frames over 1568 px are downscaled by the transcoder.
