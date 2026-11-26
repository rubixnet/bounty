# 04: Anti-Spoof Verification Suite, threshold tuning and the Step 2 Exit Gate

**What to build:** The proof that the Step 2 scoring can't be inflated or farmed. A `proptest` credit-cap property suite, a seeded persona red-team simulator running the real `rollup` + scoring code in CI, tuning of `traction-rules/1` against it, the docs reconciliation, and the Step 2 sign-off checklist. See spec "Slice 4" and "Resolved Decisions" 6.

**Blocked by:** 02, 03

**Status:** ready-for-agent

Acceptance:

- [ ] `proptest` is a dev-dependency of `traction-core` only. Generators cover thousands of events at one `timestamp_ms`, overlapping sessions, parallel subagents, many Projects, duplicate and missing turn closes, out-of-order arrival and arbitrary Presence Minutes, including injected and tier-limited ones.
- [ ] Properties hold for every device-minute and every range: Σ `human_active_sec` ≤ elapsed; device Σ `agent_compute_sec` ≤ elapsed; `idle_autonomous_sec` ≤ `agent_compute_sec`; `struggle_credit_sec` ≤ `human_active_sec`; any permutation of the same input gives byte-identical canonical JSON output. A fixed case count and seed keep CI deterministic, and shrunk failures are kept as regression cases.
- [ ] The persona simulator (seeded, in `traction-core` test support) produces 60-minute event + Presence Minute streams and checks `struggle_credit_sec / wall-clock` against pass marks kept beside `TRACTION_RULES_1`:
  - Honest debugger (specific prompts, diff reading, hand edits, fail→pass tests, category switching, human timing) ≥ 60 %
  - 59 s key-tapper (agent running, one real key every 59 s) ≤ 2 %
  - Trivial-prompt spammer (`"continue"`/`"fix all"` every ~2 min, no own edits or tests) ≤ 5 %
  - Steer-style rhythm faker (log-normal real key/pointer cadence across all sub-slots, category switching, no Outcome Anchors of its own) ≤ 2 %; the injected-input variant 0 % with `injected_hid_detected = true`
  - Each persona also runs on `reduced` and `idle_only` tiers: the honest debugger keeps ≥ 40 %, and every bot stays at or below its mark.
- [ ] Thresholds are tuned only by changing `TRACTION_RULES_1` (still `traction-rules/1`; nothing has shipped off-device). The final values and the persona results table are recorded under `## Comments`.
- [ ] A test fails if any workspace source file mentions `.wakatime`.
- [ ] The 1a "no raw text" gate test covers `presence_minute`, `commit_observation`, `work_bucket`, `activity_rollup` and the baseline table.
- [ ] Docs: `docs/steps.md` Step 2 uses the closed App Category list (`ide`, `terminal`, `browser`, `ai_desktop`, `other`) and notes the deferred shell hook and the 1d/1e deferral. `GLOSSARY.md` adds Presence Minute, Activity Rollup, Telemetry Tier, Ruleset, Outcome Anchor and Credited Time. The spec's three Step 2 gate items plus the two added ones are ticked with the test names that prove them.
- [ ] Full verification on 3-OS CI: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`. The CI link and the macOS injection skip line are added here, then `Status: ready-for-human` for Step 2 sign-off. Stop.

## Comments
