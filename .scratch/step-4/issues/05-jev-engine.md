# 05: S4 `JevEngine`: TypeSafe `v1/systemone` behind the `DecisionEngine` seam, with a hard spend limit and Heuristic fallback

**What to build:** A second `DecisionEngine` that calls TypeSafe System One. It works from the anonymized `TriageInput` only, with rubrics in `[jev]` config, a hard monthly spend limit that falls back to `HeuristicEngine`, WebP → baseline JPEG transcoding for submitted or flagged bundles, and a three-way router that never bans. It owns the Jev half of Gate 4. See spec "S4: `JevEngine`", "Testing seams" (row S4) and Resolved Decision Q22.

**Blocked by:** 03

**Status:** resolved

Acceptance:

**Before any Jev code**
- [x] Install the TypeSafe skill (`claude plugin marketplace add typesafe-ai/skills && claude plugin install typesafe@typesafe-ai`) and read its live `v1/systemone` contract.
- [x] Hold a short grill with the human on mapping `TriageInput` to the request and response fields. Record the agreed mapping in a comment below. **No field is guessed.**

**Engine**
- [x] `POST https://api.typesafe.ai/v1/systemone` with `Authorization: Bearer <TYPESAFE_API_KEY>` (env/file only), with `base_url` and `model` in `[jev]`, defaulting to `https://api.typesafe.ai` and the pinned `jev-1.14.0`, never a moving alias. Any `v1/systemone`-compatible self-hosted server works by changing them.
- [x] Its input is the anonymized `TriageInput` only, cut into 15-minute windows. No user, path, username or Project name is ever sent, and a test asserts this on the serialized request.
- [x] Questions are batched and atomic:
  - `noul` starts at the 0.7 / 0.3 thresholds;
  - no catch-all `"other"` option; each `choice` has a companion `noul` ("does any option fit?"), and "nothing fits" → Yellow;
  - questions are qualitative pattern judgments per window; arithmetic and numeric thresholds stay in `HeuristicEngine`;
  - `score` reads the **full level probability distribution**, never an average.
- [x] All questions, criteria and routing thresholds live in `[jev]`, never in source code.
- [x] Images go only with submitted or flagged bundles:
  - sampled WebP frames are transcoded **in memory on the server** to 8-bit baseline JPEG with the longest side ≤1568 px;
  - they go in the top-level `assets` map under UUID placeholders;
  - the request carries `TypeSafe-Danger-Zone-Images: true`;
  - images are a secondary reviewer hint only;
  - a frame with either side < 280 px is skipped (never upscaled) with reason code `frame_too_small_skipped`; if a bundle has frames and the skipped share reaches the `[jev]` threshold (default 1.0), triage is at least Yellow.

  Until 3b ships frames, this path is exercised with fixture images only.
- [x] There is a hard monthly spend limit in `[jev]`. When the limit is reached, or on any Jev error or timeout, triage falls back to `HeuristicEngine` and the decision records `engine_id` = heuristic plus a fallback reason. Spend never goes over the limit.
- [x] The three-way confidence router sends results to Green (fast-track human), Yellow (fail-closed human review) or Red (anomaly pin), and **never bans**. Deterministic Red from S3a still wins. `HeuristicEngine` always runs first; Jev is skipped when deterministic checks or Heuristic give Red; otherwise the result is `max_severity(heuristic, jev)`, so Jev escalates but never downgrades.

**Tests (spec "Testing seams" S4; all 3 OSes; no real network)**
- [x] Contract tests against a local test HTTP server built from the skill's schema:
  - payload shape and the pinned model;
  - no `"other"` option, and a companion `noul` per choice whose "nothing fits" answer gives Yellow;
  - max-severity combination, and no Jev request at all when deterministic/Heuristic is already Red;
  - frames under 280 px skipped with `frame_too_small_skipped`, and the all-skipped Yellow guard;
  - handling the full score distribution (two distributions with the same mean route differently where the rubric says so);
  - the `noul` thresholds;
  - the Danger-Zone header present only for submitted or flagged bundles.
- [x] Transcoding real WebP fixtures produces 8-bit baseline (non-progressive) JPEG with the longest side ≤1568 px.
- [x] Spend-limit fallback and error/timeout fallback both produce a Heuristic decision, recorded as a fallback.
- [x] The router never returns a ban, and an engine cannot upgrade a deterministic Red.
- [x] Gate 4 (Jev part) is ticked in the spec with test names.
- [ ] `cargo fmt`/`clippy`/`test` green on all 3 OSes.

## Comments

**2026-11-27 (grill with the human, before implementation):** TypeSafe skill `typesafe@typesafe-ai` 0.6.2 installed; contract read from `https://api.typesafe.ai/openapi.json` and the live docs (models, images, primitives, API).

- **Request mapping:** `model` ← `[jev].model` (default `jev-1.14.0`); `state` ← one JSON object per 15-minute window built from `TriageInput` with named fields (no IDs, paths, usernames or Project names); `questions` ← a named map of `noul`/`choice`/`score` questions from `[jev]`; `assets` ← UUID-keyed `{type: "image", format: "jpg", data_base64}` only for submitted or flagged bundles, with `TypeSafe-Danger-Zone-Images: true`. Response: `answers[name]` typed by `type`; `noul` is a probability, `choice` reads `probabilities`, `score` reads `probabilities` per level (never only `score`), and `model`/`usage.input_tokens` are logged and drive spend ($0.042 per million input tokens, output free, per the models page on 2026-11-27).
- **Decisions:**
  1. No catch-all `"other"` (AGENTS.md amended); a companion `noul` per `choice`, "nothing fits" → Yellow.
  2. Heuristic always runs first; skip Jev when already Red; final = `max_severity(heuristic, jev)`.
  3. Qualitative per-window pattern questions only; colour mapping in `[jev]`; `base_url`/`model` configurable for self-hosted servers.
  4. Frames < 280 px on a side are skipped (`frame_too_small_skipped`); skipped share ≥ `[jev]` threshold (default 1.0) on a bundle with frames → at least Yellow. Frames over 1568 px are downscaled by the transcoder.

**2026-11-27 (implementation):** branch `step-4-05-jev-engine`.

- **Verification:** `fmt`, `clippy -D warnings` and `cargo test --workspace` are green on macOS, also against a local Postgres 17 (`TRACTION_REQUIRE_DB=1`; MinIO-gated tests skip locally): 748 tests, 0 failed. `tests/jev.rs` (28) and `tests/transcode.rs` (6) need no database, so they run on all 3 OSes. CI has not run yet, so the last box stays open. No real TypeSafe request was made and `TYPESAFE_API_KEY` was never read: the tests pass a test key and an injected `env` lookup.
- **Contract source:** TypeSafe's OpenAPI document v0.2.0 (`https://api.typesafe.ai/openapi.json`) plus the models and images pages, fetched 2026-11-27. The earlier session's downloaded copy was gone and was fetched again; these are public docs, not API calls. Request and response types (`src/jev/wire.rs`) and the local test server (`tests/common/typesafe.rs`) were built from that document. The test server validates the request shape, the bearer key, the Danger-Zone header, each asset's format, its 280–1568 px sides and that its placeholder is used.
- **Where things live:** every question, criterion, colour mapping and threshold is in `crates/traction-server/jev.default.toml`, which is embedded as the default `[jev]`. Keys in `traction-server.toml` override it key by key, and arrays are replaced whole. `Jev::validate` refuses a catch-all option (`other`, `none`, `none_of_the_above`, `unknown`, `else`), an option with no colour, a score rule on a level that does not exist, and `noul_no >= noul_yes`. `[jev] enabled = false` by default, so the server runs HeuristicEngine with no network. When it is enabled and no key is available, the server refuses to start.
- **State:** each request's `state.windows` holds `w01`, `w02`, … for at most `windows_per_request` (24) windows. Only the `max_windows` (96) busiest windows by Human Active Time are asked about. Each window has these named fields: `minutes_from_first_window`, `minutes_with_activity`, `person_input_seconds`, `ai_agent_working_seconds`, `manual_file_edits`, `ai_file_edits`, `ai_turns_started`, `ai_tool_failures`, `commits_by_the_person`, `commits_by_an_ai_agent`, and `app_switches_per_minute` (only when rollups exist). Telemetry Tier, injected-input seconds, credited time and the submission ref are left out. Tier never changes the colour, and the rest are Heuristic's arithmetic.
- **Questions:** each is named `<name>__wNN` and refers to its window by the path `` `windows.wNN` ``.
  - `scripted_input` (noul): "Does the activity in {window} look like a script or bot producing input, rather than a person working?" yes → yellow (`jev_scripted_input`); unsettled and no → green.
  - `work_pattern` (choice) has the options `building`, `debugging`, `directing_ai` (green) and `presence_without_output` (yellow, `jev_work_pattern_presence_without_output`). Its companion is `work_pattern_fits` (noul): "Does {window} show one of these: …?" A no gives yellow `jev_work_pattern_nothing_fits` and an unsettled answer gives yellow `jev_work_pattern_fit_unsettled`; the option's colour is used only when the companion says yes. The top option is taken from `probabilities`.
  - `visible_effort` (score, 3 levels: no effort / a little / steady). Rule: mass on level 0 ≥ 0.5 → yellow `jev_visible_effort_no_visible_effort`. The full distribution is read and the expected `score` never is, so [0.5, 0, 0.5] is yellow while [0, 1, 0] is green.
  - `frame_shows_software_work` (noul per frame, `` `frames.fN` ``) is a hint only. A no adds `jev_frame_hint_no_software_work` and an unsettled answer adds `jev_frame_hint_unclear`. It never changes the colour.
- **Router:** deterministic Red comes first (`decide`). HeuristicEngine always runs; if it says Red, no request is sent. Otherwise the result is `max(heuristic, jev, frame guard)` with `engine_id = jev` and `rubric_version = jev-rubric/1@<answering model>+heuristic/1`. On fallback the decision keeps Heuristic's colour and reasons plus `jev_fallback_spend_limit`, `jev_fallback_error` (a non-200, unparseable, missing or mistyped answer, or a probability outside 0..1) or `jev_fallback_timeout` (`timeout_ms`, default 10 s), with `engine_id = heuristic`. JevEngine never returns `Err`.
- **Frames:** these go only when `TriageInput.review` is `submitted` or `flagged` (new field; `frames` is `#[serde(skip)]`, so frames are never stored with the decision). Every frame's header is probed; frames under 280 px, or under it after downscaling (for example 3000×300), and undecodable or animated frames are skipped (`frame_too_small_skipped` / `frame_undecodable`). Up to `max_frames` (4) usable frames are sampled evenly, transcoded and sent in a separate request: UUID v4 placeholders in `state.frames`, a top-level `assets` map, and `TypeSafe-Danger-Zone-Images: true` on that request only. If the skipped share reaches `frame_skip_yellow_share` (1.0), the decision is at least yellow (`frame_skip_share`), even on a fallback.
- **Spend:** counted in integer pico-dollars. The price is `price_usd_per_million_input_tokens = 0.042`, so 42,000 pico-dollars per token, and the limit is `monthly_spend_limit_usd = 40` (as in `docs/cost-model.md`). The whole submission's upper-bound estimate is reserved before anything is sent: one token per UTF-8 byte of the JSON body excluding base64, plus `image_token_ceiling = 4096` per image, since TypeSafe publishes no image-token estimate yet. If `spent + reserved + estimate > limit`, nothing is sent. Each reply books `usage.input_tokens` and persists `{month, spent_pico_usd}` to `spend_ledger_file` (write, then rename); a new UTC month resets it. A corrupt ledger refuses start. **Assumption:** the estimate is a true upper bound (≤1 token per byte for text). If TypeSafe ever bills more than estimated, a month could overshoot by that difference on its last request.
- **Crates:** `image-webp` 0.2.4 (decode) and `jpeg-encoder` 0.7.1 (encode, baseline; progressive is never set) are pure Rust, so they build on all 3 OSes. The resize is an in-crate area average. Alpha is flattened onto white. `jpeg-encoder`'s licence is `(MIT OR Apache-2.0) AND IJG` (permissive). Fixtures are real `cwebp` output in `tests/data/frames/` (≈16 KB total): 2400×1500 lossy → 1568×980, 1280×800 unchanged, 600×400 lossless with alpha, 200×150 skipped, 3000×300 skipped after downscaling. The test checks for exactly one SOF0 marker, 8-bit precision and 3 components by parsing the markers itself.
- **Deferred:**
  - Real frames arrive with Step 3b. Until then `TriageInput.frames` is empty in production, and only `Submitted` is ever produced. `Flagged` is ready for whatever flags a bundle later.
  - Tuning the wording against the live API needs the human's permission, a budget and a key (per the skill), so it has not been done. The questions are a first draft.
  - Triage runs inside the `complete` handler; at most 4 window requests plus 1 frame request run sequentially per submission. Moving triage to a background job is left for later if latency matters.
  - Multi-node spend sharing: the ledger is a local file, which is fine for the documented single-node deploy.

**2026-11-27 (follow-up: spend safety margin):** the spend estimate assumes `image_token_ceiling` tokens per image, which TypeSafe does not publish, so a reply could bill above it and the month's last request could overshoot. `[jev] spend_safety_margin` (default 0.05, must be in 0..1) now makes reservations fit under limit × (1 − margin); the margin absorbs any overrun up to $2 at the $40 limit, and a single request is bounded by the 64k-token context (~$0.003). Tests: `tests/jev.rs` `a_submission_inside_the_safety_margin_is_not_sent` and `a_bill_above_the_estimate_never_pushes_the_month_over_the_limit` (which fails with the margin set to 0), plus `config::tests::the_jev_spend_safety_margin_defaults_to_five_percent_and_is_bounded`. The live TypeSafe probe is still deferred.
