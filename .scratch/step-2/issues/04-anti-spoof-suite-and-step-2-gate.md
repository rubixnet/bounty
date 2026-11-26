# 04: Anti-Spoof Verification Suite, threshold tuning and the Step 2 Exit Gate

**What to build:** The proof that the Step 2 scoring can't be inflated or farmed. A `proptest` credit-cap property suite, a seeded persona red-team simulator running the real `rollup` + scoring code in CI, tuning of `traction-rules/1` against it, the docs reconciliation, and the Step 2 sign-off checklist. See spec "Slice 4" and "Resolved Decisions" 6.

**Blocked by:** 02, 03

**Status:** ready-for-human

Acceptance:

- [x] `proptest` is a dev-dependency of `traction-core` only. Generators cover thousands of events at one `timestamp_ms`, overlapping sessions, parallel subagents, many Projects, duplicate and missing turn closes, out-of-order arrival and arbitrary Presence Minutes, including injected and tier-limited ones.
- [x] Properties hold for every device-minute and every range: Σ `human_active_sec` ≤ elapsed; device Σ `agent_compute_sec` ≤ elapsed; `idle_autonomous_sec` ≤ `agent_compute_sec`; `struggle_credit_sec` ≤ `human_active_sec`; any permutation of the same input gives byte-identical canonical JSON output. A fixed case count and seed keep CI deterministic, and shrunk failures are kept as regression cases.
- [x] The persona simulator (seeded, in `traction-core` test support) produces 60-minute event + Presence Minute streams and checks `struggle_credit_sec / wall-clock` against pass marks kept beside `TRACTION_RULES_1`:
  - Honest debugger (specific prompts, diff reading, hand edits, fail→pass tests, category switching, human timing) ≥ 60 %
  - 59 s key-tapper (agent running, one real key every 59 s) ≤ 2 %
  - Trivial-prompt spammer (`"continue"`/`"fix all"` every ~2 min, no own edits or tests) ≤ 5 %
  - Steer-style rhythm faker (log-normal real key/pointer cadence across all sub-slots, category switching, no Outcome Anchors of its own) ≤ 2 %; the injected-input variant 0 % with `injected_hid_detected = true`
  - Each persona also runs on `reduced` and `idle_only` tiers: the honest debugger keeps ≥ 40 %, and every bot stays at or below its mark.
- [x] Thresholds are tuned only by changing `TRACTION_RULES_1` (still `traction-rules/1`; nothing has shipped off-device). The final values and the persona results table are recorded under `## Comments`.
- [x] A test fails if any workspace source file mentions `.wakatime`.
- [x] The 1a "no raw text" gate test covers `presence_minute`, `commit_observation`, `work_bucket`, `activity_rollup` and the baseline table.
- [x] Docs: `docs/steps.md` Step 2 uses the closed App Category list (`ide`, `terminal`, `browser`, `ai_desktop`, `other`) and notes the deferred shell hook and the 1d/1e deferral. `GLOSSARY.md` adds Presence Minute, Activity Rollup, Telemetry Tier, Ruleset, Outcome Anchor and Credited Time. The spec's three Step 2 gate items plus the two added ones are ticked with the test names that prove them.
- [ ] Full verification on 3-OS CI: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`. The CI link and the macOS injection skip line are added here, then `Status: ready-for-human` for Step 2 sign-off. Stop.
  - Local macOS: fmt, clippy `-D warnings` and `cargo test --workspace` green, run three times.
  - TODO (human): 3-OS CI run link: <pending>
  - TODO (human): macOS injection skip line (`SKIPPED (macOS injection): <reason>`) as printed by CI: <pending>

## Comments

- Built test-first. Tests: `crates/traction-core/tests/{credit_properties,personas,slice4_rules,workspace_guards}.rs`, simulator in `tests/support/mod.rs`; daemon gate `finalizer::tests::derived_tables_hold_only_numbers_enums_and_identifiers`. `proptest` is a dev-dependency of `traction-core` only; 96 cases, fixed seed, ChaCha. Teeth checked by sabotage: sorting events by timestamp only makes the permutation property fail.
- **What the persona suite found (real defects in Slice 2/3 scoring, fixed through the Ruleset):** before tuning, the trivial-prompt spammer scored 99 %, the Steer-style faker 99.9 % (every agent edit anchored credit, whatever the prompt), and an honest user on `reduced`/`idle_only` scored 0 % (no input counts meant no presence). Two rules fixed it: (1) an agent `FILE_MUTATION` anchors credit at the strength of its prompt's `specificity` (all other anchors 1000); (2) a tier with no input counts is present when the OS idle timer was busy (`degraded_idle_max_sec`), at `degraded_tier_credit_milli`. Both are Ruleset-driven and still `traction-rules/1`.
- **Constants added or moved** (all in `TRACTION_RULES_1`): `degraded_idle_max_sec` 10, `degraded_tier_credit_milli` 600 (new); `index_min_word_chars` 3, `index_min_stem_chars` 4, `index_seed_visit_factor` 8 (moved from `MIN_WORD_CHARS`, `MIN_STEM_CHARS`, `SEED_VISIT_FACTOR`). No existing value changed. `COMMON_WORDS` stays a word list. Pass marks: `PERSONA_MARKS_1` (honest 600 / 400 degraded; tapper 20, spammer 50, faker 20, injected 0, per mille).
- **Persona results** (`struggle_credit_sec` per mille of a 3600 s hour; seeds 7 / 42 / 2026):

| Persona | full | reduced | idle_only | Mark |
|---|---|---|---|---|
| Honest debugger | 979 / 980 / 981 | 587 / 588 / 588 | 590 / 590 / 590 | >= 600 full, >= 400 degraded |
| 59 s key-tapper | 0 | 0 | 0 | <= 20 |
| Trivial-prompt spammer | 0 | 0 | 0 | <= 50 |
| Steer-style faker | 0 | 0 | 0 | <= 20 |
| Steer-style faker, injected | 0 (`injected_hid_detected`) | 0 | 0 | 0 |

- Gaps for the human: (a) bots score 0, not merely low, because their only anchors are agent edits behind terse prompts. A bot that writes one long prompt naming real repo symbols, or does a token hand edit near each minute, would anchor credit; the pass marks cover the specified personas only. (b) On `reduced`/`idle_only` there is no injection signal, so a jiggler that keeps the OS idle timer busy is present there at 60 % credit; it is still gated by anchors. (c) 3-OS CI has not run; the Windows and Linux presence backends and the Xvfb tests from ticket 01 remain unverified. (d) The persona streams are synthetic (Claude Code-shaped); they are not recordings of real people.
