# Step 2: OS Presence, HID Anti-Spoof, Three Clocks and Productive-Struggle Scoring

Status: ready-for-agent (approved by the human 2026-11-26; see "Step 1 sign-off" below)

Source: `docs/steps.md` (Step 2), `docs/roadmap.md` (sub-steps 2a–2d), `docs/goal.md` sections 1, 2 and 4 (Attacks 1 and 4), `GLOSSARY.md` ("Activity & Time"), `docs/protocol.md` sections 2 and 3, and the AGENTS.md invariants "Zero Raw Content Persistence", "Zero LLM Trust Boundary", "`FILE_MUTATION` Attribution", "Untrusted-Repo Git Hardening", "OS Permissions & Low-Spec Hygiene" and "Offline & Local Models".

**One spec for all of Step 2.** `docs/roadmap.md` plans four sub-steps (2a–2d), each with its own spec. At the human's request, Step 2 is specified once here and will be cut into **4 vertical slices** (below) instead. Each slice must still fit one fresh session (~150k tokens). If a slice can't, split *that* slice, not the spec.

Gate items this Step owns (from the Step 2 Exit Gate in `docs/steps.md`):

- [x] Automated simulation test: prompting an agent and staying idle for 10 minutes logs `10m Agent Compute / 0m Human Active`. *(Slice 2; `tests/rollup.rs::gate_simulation_ten_idle_minutes_is_all_agent_and_mostly_unverified`)*
- [x] Automated simulation test: an active error → edit → test-fail → edit → test-pass cycle scores high `verified_struggle`, whether the code change is a beginner's size or an expert's. *(Slice 3; `tests/struggle.rs::a_fail_edit_fail_edit_pass_loop_scores_high_and_size_is_ignored`)*
- [x] A synthetic HID / robotic-cadence fixture sets `injected_hid_detected = true` and routes those minutes to Unverified Autonomous Time. *(Slices 1 and 2; `tests/rollup.rs::injected_ratio_flags_the_minute_and_it_earns_nothing`, `::robotic_cadence_flags_the_minute`)*

Added at the human's request (owned by Slice 4):

- [x] Property test: summed across every Project and clock rule, credited time never exceeds elapsed wall-clock time, including under floods of events sharing one timestamp. *(`tests/credit_properties.rs::credited_time_never_exceeds_elapsed_wall_clock`, `::a_flood_at_one_timestamp_cannot_mint_time`)*
- [x] A deterministic persona red-team simulator runs in CI: the honest debugger earns real credit; the 59 s key-tapper, the trivial-prompt spammer and the Steer-style rhythm-faking bot earn negligible credit. *(`tests/personas.rs`)*

All of the above pass on Windows, macOS and Linux in CI.

### Step 1 sign-off (recorded deviation from `AGENTS.md` rule 1)

On 2026-11-26 the human signed off the Step 1 Exit Gate with Step 1c (Claude Code plus its review hardening, green on 3-OS CI) as the reference Harness. They deferred the remaining Step 1 sub-steps until after Step 2, so the scoring engine ships first:

- **1d** (OpenCode plugin + Codex rollout tailer) is deferred. The Step 2 engine is Harness-agnostic, and its tests use Claude Code-shaped events. OpenCode `permission.ask` and Codex turn events feed the same rules once 1d lands.
- **1e** (tray, auto-start, idle RSS/CPU measurement) is deferred. Until then, Slice 1 must show its own cost: a test bounds the sensor's wakeups per minute (no busy polling). The hard RSS < 15 MB / CPU < 1 % gate stays with 1e, and 1e must measure with the presence sensor running.

The Step 1 Exit Gate items those sub-steps own (OpenCode and Codex fixtures, OpenCode not blocked, tray Pause, RSS/CPU) remain open and must pass before Step 3.

## Terminology (read first)

This spec uses the canonical `GLOSSARY.md` terms. The request's names map onto them as follows:

| Request wording | Canonical term | Notes |
|---|---|---|
| Manual Work (clock) | **Human Active Time** | "Manual Work" is already a glossary term: a `FILE_MUTATION` outside every Attribution Window. Here it is one input to Human Active Time, not a clock. |
| Verified AI Struggle | **Agent Compute Time** with verified presence, plus the struggle-weighted credit (`struggle_credit_sec`) | |
| Excluded / Unattended | **Unverified Autonomous Time** | Per the glossary this is a *subset* of Agent Compute Time, not a separate partition. That is how the gate test "10m Agent Compute / 0m Human Active" holds while most of those 10 minutes are also Unverified. |
| 1–5 min audit buckets | **Activity Rollup** (new term, 120 s) | It sits alongside the 60 s **Work Bucket**, which is unchanged. |

New glossary terms that Slice 4 adds: **Presence Minute**, **Activity Rollup**, **Telemetry Tier**, **Ruleset**, **Outcome Anchor**, **Credited Time**.

## Problem Statement

After Step 1, Traction knows when each Harness turn started and ended and which file edits belong to whom. It still can't say whether a human was there. A turn that ran for 20 minutes while the user watched videos looks the same as one where they read every diff and tested in the browser. That is exactly the Stardance failure: 123,400 hours submitted, 27,600 approved.

Measuring presence has its own sharp edges:

- **Privacy.** Presence detection runs right next to the keyboard. If any keystroke content, window title or URL is stored, a privacy-conscious user won't install Traction (criterion 6).
- **Spoofing.** Jigglers, `xdotool`/`SendInput` macros, a key tap every 59 s, and LLM-driven prompt loops can all make "input happened" true. Because the code is AGPLv3 (Attack 4), every threshold is public, so no single signal can be trusted on its own.
- **Inflation.** Floods of events, parallel sessions and multiple Projects must never add up to more credited time than wall-clock time.
- **Platform gaps.** Wayland hides the focused window and global input. macOS gates global input taps behind Input Monitoring. Low-spec Windows laptops can't afford a heavy input hook.

From the user's point of view: "Traction counts the time I'm really thinking, testing and steering, and none of the time I'm away while the agent grinds. It never sees what I type or which site I'm on. Faking it with a jiggler or a bot gets nothing."

## Solution

There are four vertical slices, each shipping end-to-end behaviour with real tests on all 3 OSes.

### Slice 1: Presence Sensor (roadmap 2a + 2b)

A `presence` module in `traction-daemon` with one OS backend per platform, chosen at compile time. It samples OS presence signals in memory and emits one **Presence Minute** per wall-clock minute. A Presence Minute holds only counts, ratios and closed enums:

- `key_events`, `pointer_events`: counts only. The key code, character and pointer position are never read into any retained structure.
- `injected_events`: events flagged as synthetic by the OS (see the backends below).
- `active_subslots`: how many of the minute's six 10-second sub-slots saw any real (non-injected) input, 0–6.
- `cadence_entropy_milli`: Shannon entropy of log-binned inter-event intervals, computed in memory over real events and scaled ×1000, or `null` below `MIN_CADENCE_SAMPLES`. The individual intervals are dropped once the minute closes.
- `category_sec`: seconds of foreground time per **App Category**, a closed enum: `ide`, `terminal`, `browser`, `ai_desktop`, `other`.
- `category_switches`: the number of times the foreground category changed.
- `idle_sec`: seconds past `IDLE_THRESHOLD` according to the OS idle timer.
- `telemetry_tier`: `full` | `reduced` | `idle_only` | `none` (see the backends below).

Category mapping is a closed, compiled-in table (bundle IDs on macOS, executable names on Windows, `WM_CLASS` on X11) that resolves straight to an App Category. The raw identifier is matched in memory and never stored or logged. Anything not in the table is `other`.

Per-OS backends (none of them needs admin rights, and none triggers a Screen Recording prompt):

| OS | Foreground category | Idle | Injection signal | Tier |
|---|---|---|---|---|
| Windows | `GetForegroundWindow` → owning process executable name | `GetLastInputInfo` | **No `WH_KEYBOARD_LL` / `WH_MOUSE_LL`, ever:** an unsigned binary with a global low-level hook trips Windows Defender's keylogger heuristics. Instead a spike checks Raw Input: a message-only window registered with `RIDEV_INPUTSINK` for keyboard and mouse, where a `WM_INPUT` whose `RAWINPUTHEADER.hDevice` is `NULL` counts as injected (`SendInput`). It reads only the header and never `VKey`, `MakeCode` or any key data. If the spike shows that `SendInput` isn't cleanly flagged, or that Raw Input raises an AV/hook risk, Windows drops to `reduced` (idle + category, no injection signal). | `full` if the Raw Input spike passes, else `reduced` |
| macOS | `NSWorkspace.frontmostApplication` bundle ID | `CGEventSourceSecondsSinceLastEventType` | **Never an Input Monitoring or Accessibility prompt by default.** A spike checks whether a prompt-free signal exists (e.g. `CGEventSourceCounterForEventType` deltas, `HIDSystemState` against `CombinedSessionState`). If it does, it is the injection signal. If not, macOS defaults to `reduced`: idle + app category + outcome-anchored scoring. A listen-only `CGEventTap` stays an opt-in for later; it is not built in Step 2. | `full` only if the prompt-free spike passes, else `reduced` |
| Linux X11 | `_NET_ACTIVE_WINDOW` → `WM_CLASS` | XScreenSaver extension idle | XInput2 raw events: an event whose source device is the XTEST slave (`Virtual core XTEST …`) is injected. No `/dev/input` access, no `input` group. | `full` |
| Linux Wayland | none (`category_sec` all `other`, `category_switches = 0`) | `ext-idle-notify-v1`, else `org.freedesktop.ScreenSaver`, else none | none | `idle_only`, or `none` when no idle source exists |

Wayland and any backend failure degrade cleanly: the tier drops, the fields the backend can't fill are `null` rather than `0`, and the daemon never panics. A `reduced`/`idle_only` minute never counts as evidence *against* the user. It just weighs less as evidence *for* them (Slice 3).

Presence Minutes are written through the single writer into a new append-only `presence_minute` table (with the same no-update/no-delete triggers as `traction_event`). They are device-wide, not per-Project. Slice 2 assigns them.

### Slice 2: Three-Clock Rollup Engine (roadmap 2c)

A pure, deterministic `rollup` module in `traction-core`, so the server can recompute exactly the same buckets in Step 4. It takes the **Ruleset**, the Traction Events, the Presence Minutes and the Commit Observations for a time range, and returns 60 s **Work Buckets** and 120 s **Activity Rollups**. It does no I/O, uses no clock and has no randomness.

**Ruleset.** Every constant below lives in one `Ruleset` struct identified by an explicit `ruleset_id` (first value: `"traction-rules/1"`). Every Work Bucket and Activity Rollup records the `ruleset_id` that produced it. Changing any constant means a new ID. The daemon has no runtime overrides except test-only ones, matching the 1b constants policy.

**Per Work Bucket (Project × minute):**

- `agent_compute_sec`: the union (not the sum) of all open turn intervals of every session in that Project during the minute, from `PROMPT_SUBMIT` to `TURN_END`, `STOP_FAILURE` or `SESSION_END`. `STOP_FAILURE` and `SESSION_END` close every open interval of their session immediately. A turn with no event for `STALE_TURN_TIMEOUT` closes at its last event (the existing 1c rule). Parallel subagents never add time.
- `idle_autonomous_sec` (Unverified Autonomous Time): the part of `agent_compute_sec` that lies more than `UNVERIFIED_AFTER` = **90 s** past the last **verified human presence**. It is always ≤ `agent_compute_sec`.
- `human_active_sec`: seconds of verified human presence attributed to this Project.
- `permission_wait_ms`: the human-decision latency from each `PERMISSION_REQUEST` to the session's next `TOOL_POST`, `TOOL_FAIL`, `PROMPT_SUBMIT` or turn close. It is clamped to `[0, PERMISSION_LATENCY_CAP]` = 120 s. It counts toward `human_active_sec` only when that interval holds real input. Approvals that arrive with no real input, or consistently in under `PERMISSION_LATENCY_FLOOR` = 300 ms, set the `auto_approver_suspected` flag and earn nothing.
- `commits_human`, `commits_agent`: from Commit Observations (below), using the **60 s agent-quiet rule**. A commit is human when its Project had no open turn and no Harness event in the 60 s before it. Otherwise it is agent-attributed.
- `injected_hid_detected`: true when the minute's injected ratio (`injected_events / (key_events + pointer_events)`) is ≥ `INJECTED_RATIO_FLAG` or `robotic_cadence` is set. A flagged minute provides **no** verified presence, so any agent time in it becomes Unverified Autonomous Time once past the 90 s rule, and its `human_active_sec` is 0.
- `ruleset_id`, `telemetry_tier`.

**Verified human presence** for a minute requires all of the following: real (non-injected) input in at least `MIN_ACTIVE_SUBSLOTS` = 3 of the 6 sub-slots, `injected_hid_detected = false`, and a foreground category in {`ide`, `terminal`, `browser`, `ai_desktop`} for at least part of the minute. (On `idle_only`/`none` tiers the category clause is skipped and the minute is marked tier-limited.) A single tap every 59 s fills one sub-slot and never qualifies.

**Assigning presence to a Project (one Project per device-minute).** A present minute goes to exactly one Project: the one with the most recent Outcome Anchor or Harness event within `PRESENCE_PROJECT_LOOKBACK` = 5 min. Ties go to the most recent event. If no Project qualifies, the minute goes to no Project. That makes Σ over Projects of `human_active_sec` ≤ 60 for every device-minute. Likewise, device-wide `agent_compute_sec` per minute is the union across Projects, split evenly between the Projects active in it, so neither clock can exceed wall-clock time.

**Commit Observations.** `logs/HEAD` also changes on `checkout`, `switch`, `pull`, `reset`, `merge --ff` and `rebase`. A branch switch or a pull must never count as a commit. For each tracked Git Project and worktree the daemon uses the **per-worktree** git dir (`rev-parse --git-dir`, not the common dir, because `HEAD`, `logs/HEAD` and `COMMIT_EDITMSG` are per worktree). It watches `logs/HEAD` for mtime changes, never reading the file (the reflog holds commit messages). On a change it records a **new local commit** only when all three checks pass:

1. **`COMMIT_EDITMSG` mtime moved forward** since the last observation, by `stat` only, never opened. `git commit`, `cherry-pick` and `revert` write it; `checkout`, `switch`, `pull`, `reset` and fast-forward merges don't.
2. **The new HEAD's first parent is the previous HEAD** (`rev-parse HEAD` and `rev-parse HEAD^` through the hardened wrapper). For a repo's first commit, HEAD has no parent and there was no previous HEAD. This drops amends and rebases, which rewrite history instead of adding to it.
3. **The committer time is within `COMMIT_FRESHNESS` = 60 s of `observed_ms`**, read with `rev-list -1 --no-commit-header --format=%ct HEAD` (the format prints only the timestamp, never the message or author). This drops a pulled commit that happened to touch `COMMIT_EDITMSG` timing.

The daemon records `(project_id, commit_oid, observed_ms)` in a new append-only `commit_observation` table. Message, author and diff are never read. All three commands are already on the wrapper's allowlist (`rev-parse`, `rev-list`). The wrapper also gains a check that `--format` is exactly `%ct`. Any failed check or Git timeout means "no commit". This is a daemon-local table, not a Traction Event, so `schema_version: 1` and the closed `event_type` enum stay unchanged.

**Activity Rollup (120 s, aligned to the Anchor cadence).** One per device and window: per-category share, switch rate, mean cadence entropy, injected ratio, tier, and the `ruleset_id`. Together with `FILE_MUTATION`s, commits and turns, these are what the reviewer's Session Audit Strip (4b/5b) displays.

**Finalizing.** A bucket is computed once the minute is older than `BUCKET_SETTLE` = 120 s (longer than `LOG_TAIL_HOLD` plus the router delay), then written once to an append-only `work_bucket` table keyed `(project_id, minute_start_ms, ruleset_id)`. Events that arrive later never rewrite a finalized bucket, matching the live-only attribution rule. Minutes when the daemon was down, filled in by the tailer, have no Presence Minute and are written with `telemetry_tier: none`, so they earn zero Human Active Time.

### Slice 3: Struggle Signals and Credit (roadmap 2d)

These are deterministic `traction-core` functions that score each Work Bucket on the four `struggle_axes` from `docs/protocol.md` §3, each in `[0, 1000]`, and produce `struggle_credit_sec = human_active_sec × weight`, where `weight ∈ [0, 1]`.

- **Steering:** the share of the Project's present minutes with interleaved category switching around real work (`ide` ↔ `terminal` ↔ `browser` ↔ `ai_desktop` within ±2 min of an Outcome Anchor).
- **Specificity:** for each prompt, computed in memory in the Adapter before raw text is dropped (the existing `content_metrics` path): `repo_symbol_hits` against a bounded per-Project index of file names and path stems (capped entries, built from the watcher's view, never file contents), `has_stacktrace`, and the prompt-to-diff ratio (`prompt_char_len` against the Harness's `diff_lines_delta` in the following turn). Short, unspecific prompts (`"continue"`, `"fix all"`) that trigger large agent turns score near 0.
- **Friction:** detected verification loops in event order, `TOOL_FAIL` → `FILE_MUTATION` (Manual Work or Harness) → `TOOL_POST` of the same tool name, plus Manual Work that reverts recent churn. Code size is ignored: a 1-line fix and a 300-line fix that close the same loop score the same.
- **Comfort delta:** compares the minute against the user's own rolling 30-day baseline, stored as a compact local aggregate (a histogram of axis values, never events). With fewer than `BASELINE_MIN_DAYS` = 3 days of history the weight is never reduced, so a beginner's 0→1 earns full credit. Minutes with no friction and low specificity that match the baseline decay smoothly toward a floor of `COMFORT_FLOOR` = 0.5. They never reach zero.

**Outcome anchoring (the core anti-spoof rule).** Human Active Time in a minute is *confirmed* only when an **Outcome Anchor** (Manual Work, a Harness `FILE_MUTATION` after a prompt the human submitted, a human commit, a `TOOL_POST`/`TOOL_FAIL` that closes a loop, or an approved permission) lands within `ANCHOR_WINDOW` = ±5 min. Present but unanchored minutes in `browser`/`other` decay linearly to 0 over `DRIFT_DECAY` = 10 min. Present minutes with no anchor in any category get weight 0 in `struggle_credit_sec`, but they keep their `human_active_sec` for audit. Rhythm-faking input with nothing to show for it therefore earns nothing.

### Slice 4: Anti-Spoof Verification Suite and Step Gate

- **Credit-cap property test** (`proptest`, a dev-dependency of `traction-core`): arbitrary event streams (including thousands of events sharing one `timestamp_ms`, overlapping sessions, parallel subagents, many Projects, unbalanced/duplicate turn closes and out-of-order arrival) and arbitrary Presence Minutes. It asserts, for every device-minute and every range: Σ `human_active_sec` ≤ elapsed, device `agent_compute_sec` ≤ elapsed, `idle_autonomous_sec` ≤ `agent_compute_sec`, `struggle_credit_sec` ≤ `human_active_sec`, and that the result is identical for any arrival order of the same input set (determinism).
- **Persona red-team simulator:** a seeded generator in `traction-core` test support that produces 60-minute event + Presence Minute streams for each persona and runs them through the real `rollup` + scoring code. Pass criteria (Ruleset `traction-rules/1`):

  | Persona | Behaviour | Must earn (`struggle_credit_sec` / wall-clock) |
  |---|---|---|
  | Honest debugger | Prompts naming files and stack traces, reads diffs, edits by hand, runs tests that fail and then pass, switches `ide`/`terminal`/`browser`, human-like timing | ≥ 60 % |
  | 59 s key-tapper | Agent turns left running; one real key every 59 s; no other input | ≤ 2 % |
  | Trivial-prompt spammer | `"continue"`/`"fix all"` every ~2 min, no own edits, no test inspection, input only to submit the prompt | ≤ 5 % |
  | Steer-style rhythm faker | Log-normal, human-looking real (non-injected) key and pointer cadence across all sub-slots, category switching, **no** Outcome Anchors of its own. A variant uses injected input. | ≤ 2 %; the injected variant 0 % with `injected_hid_detected = true` |

  The thresholds sit in one place next to the Ruleset, so a stricter future Ruleset can't silently loosen them.
- **Real OS injection fixtures** (zero mocking of the host OS): Linux CI runs under Xvfb with `xdotool` and asserts the XTEST source is flagged. On Windows, if the Raw Input spike passed, CI injects with `SendInput` and asserts `hDevice == NULL` is flagged; if it didn't, CI asserts the backend reports `reduced` and that `GetLastInputInfo` idle and foreground category still work. On headless macOS CI, a **loud, explicitly reported skip** is acceptable for injection (the test prints `SKIPPED (macOS injection): <reason>` and the CI log step greps for it so the skip shows up in the run summary). It is backed by unit tests of the macOS backend's pure logic (counter deltas → ratio, tier selection) with recorded counter inputs. The idle timer and frontmost-app category still run for real on the macOS host.
- **Docs:** `docs/protocol.md` §3 gets the bucket/rollup fields, the Ruleset and the new tables. `docs/steps.md` Step 2 gets the App Category list reconciled (below). `GLOSSARY.md` gets the new terms.

## User Stories

1. As a user, I want Traction to count my time reading diffs, testing and editing while the agent runs, so that steering an agent earns the same credit as typing code myself.
2. As a user, I want time when I'm away while the agent works to show as Agent Compute but not Human Active, so that my profile is honest and reviewers trust it.
3. As a privacy-conscious user, I want Traction to store only counts and five app categories, never what I type, my window titles or my URLs, so that I'm comfortable installing it.
4. As a macOS user, I want presence tracking to work without any Input Monitoring, Accessibility or Screen Recording prompt, so that install stays silent.
5. As a Wayland user, I want Traction to keep working with less detail instead of crashing or reporting zeros as if I were idle, so that I'm not penalised for my desktop.
6. As a user on a 10-year-old Windows laptop, I want presence tracking to cost nothing noticeable and never be flagged by Windows Defender, so that my typing never lags and my antivirus stays quiet.
7. As a beginner, I want my first-ever bug fix to earn full credit even though the diff is tiny, so that the score rewards struggle, not code size.
8. As a user, I want a crashed or failed agent turn to stop counting the moment it fails, so that a stuck timer never leaks hours.
9. As a user who commits by hand, I want my commits counted as mine, while the agent's own `git commit` calls are not, so that outcomes are attributed correctly.
10. As a reviewer, I want every bucket to carry the Ruleset ID that produced it, so that I can recompute it and know which rules applied.
11. As a reviewer, I want 2-minute Activity Rollups of category share, switch rate, cadence entropy and injection ratio, so that I can tell a debugging loop from browsing or a bot at a glance.
12. As Fraud Squad, I want jigglers, macros, 59 s tappers, prompt spammers and rhythm-faking bots to earn negligible credit, proven in CI, so that the open-source thresholds don't become a cheat sheet.
13. As Fraud Squad, I want a proof that no event pattern can credit more time than wall-clock time, so that event floods can't inflate hours.
14. As a user working on several Projects in one afternoon, I want each minute of my presence credited to exactly one Project, so that time is never double-counted.

## Implementation Decisions

- **App Category list:** the closed list is `ide`, `terminal`, `browser`, `ai_desktop`, `other`. It replaces the `steps.md` list (`browser_localhost_devtools`, `docs`, `other_non_project`). Telling `localhost`/docs tabs apart needs URLs, which the presence sensor never reads; that distinction belongs to the 6d browser-extension Adapter (`dev_docs`, `localhost_preview`, …). Slice 4 edits `docs/steps.md` to match.
- **Two time grains:** the 60 s Work Bucket (glossary, gate, chain input) is the decision unit. The 120 s Activity Rollup is the audit unit, and it matches the 2-minute Anchor cadence so Step 3 can hash both into `H_n` without regrouping.
- **The engine is pure and lives in `traction-core`,** so the daemon and the server (Step 4 recompute) share one implementation (`AGENTS.md` rule 5). The daemon only collects inputs, finalizes and stores.
- **Append-only storage:** `presence_minute`, `commit_observation` and `work_bucket` all get the `traction_event` no-update/no-delete triggers. Nothing is recomputed in place. A new Ruleset writes new rows under its own ID.
- **Schema stays at `schema_version: 1`:** no new `event_type`. Commits and presence are daemon-local tables, and the bucket fields extend the not-yet-implemented `WorkBucket` (protocol §3).
- **Client scores are triage, not proof** (Attack 4): `injected_hid_detected`, `struggle_axes` and credit are hints for the Step 4 `DecisionEngine` and for reviewers. The Audit Bundle stays the proof.
- **Never touch `~/.wakatime.cfg`:** no Step 2 code reads or writes it. Slice 4 adds a test that greps the daemon sources for `.wakatime` and fails on any hit.
- **Budget:** the presence sensor must keep the 1e idle RSS < 15 MB and CPU < 1 %. Sampling is event-driven (Raw Input messages, OS notifications) plus one wake per 10 s sub-slot. No busy polling. Until 1e, a test bounds wakeups per minute.

## Testing Decisions

- Unit tests per backend's pure parts (category table, sub-slot counting, entropy binning, tier fallback ladder).
- Real host-OS integration tests per backend in 3-OS CI (Slice 1 and Slice 4 fixtures). No `todo!()` and no hardcoded `true` for OS checks.
- Slice 2 gate simulation: `PROMPT_SUBMIT`, then 10 minutes of idle Presence Minutes, then `TURN_END` → `agent_compute_sec = 600`, `human_active_sec = 0`, `idle_autonomous_sec = 510`.
- Slice 2: a `STOP_FAILURE`/`SESSION_END` mid-turn closes the interval at that timestamp; a turn left open after a crash closes at `STALE_TURN_TIMEOUT`.
- Slice 2: commit 30 s after a `TOOL_POST` → agent; commit 90 s after `TURN_END` → human.
- Slice 2, real Git repos on all 3 OSes: `git commit` → one observation; `git checkout`/`switch` to another branch, `git pull` (fast-forward from a second clone), `git reset --hard HEAD~1`, `git commit --amend` → none. A commit in a linked worktree is observed for that worktree only. A canary commit message never appears in SQLite or the daemon log.
- Slice 3 gate simulation: the error → edit → test-fail → edit → test-pass loop at a 1-line and at a 300-line diff gives an equal, high `verified_struggle`.
- Slice 4: the property test and the persona table above.
- No raw content: extend the 1a "no raw text in SQLite" gate test to the three new tables.

## Out of Scope

- Snapshots, hash-chain, Anchors (Step 3). Server recompute and `DecisionEngine` (Step 4).
- URL/tab classification and the browser-extension Adapter (6d).
- The optional shell hook for command category and exit code (`steps.md` Step 2 Build 3) is **deferred**. Friction uses Harness `TOOL_FAIL`/`TOOL_POST` and Manual Work only, which keeps the slices to four.
- Any UI for the scores, including the tray (1e) and the reviewer strip (5b).
- WakaTime compatibility, T3 Code, Cursor and any other Harness.

## Resolved Decisions (human, 2026-11-26)

1. **Departures approved:** glossary clock names, the 5 closed App Categories, fixed 120 s Activity Rollups, deferred shell hook, 4 vertical slices.
2. **Commit detection:** a branch switch, pull or reset must never count as a human commit. The three checks are under "Commit Observations".
3. **macOS:** never an Input Monitoring or Accessibility prompt by default. Spike first; if there is no prompt-free synthetic-input signal, default to `telemetry_tier: reduced`.
4. **macOS CI:** a loud, explicitly reported skip of the injection test is acceptable, backed by unit tests of the backend logic.
5. **Windows:** no `WH_KEYBOARD_LL` / `WH_MOUSE_LL`. The baseline is `GetLastInputInfo` + the foreground executable name. Spike Raw Input (`hDevice == NULL`, no key data read); fall back to `reduced` if it fails.
6. **Thresholds:** every constant, including the persona pass marks, lives in one place under `traction-rules/1` in `traction-core` and is tuned against the persona simulator in Slice 4. Tuning that changes a constant after Slice 2 lands is still `traction-rules/1` until any bucket is shipped off-device (Step 4). After that, every change needs a new ID.

Spike outcomes (macOS prompt-free signal, Windows Raw Input) are recorded under `## Comments` in ticket 01 and copied into `docs/protocol.md`.
