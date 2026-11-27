# Traction — Staged Agent Execution Guide

> **Strict Agent Operating Rules:**
> 1. **One Step (or Sub-Step) Per Session:** Execute **strictly one step at a time**, split into sub-steps (e.g. 1a, 1b) each with its own `.scratch/<step>/spec.md` and tickets so no session exceeds ~150k tokens. Do NOT start Step N+1 until every Exit Gate checkbox in Step N passes automated tests on Windows, macOS, and Linux CI and the human signs off.
> 2. **No Early Frontend:** Do NOT create or touch `apps/web/` (landing page, Hack Club login UI, public profile UI, comparison page) until **Step 6**. Premature UI work hides broken tracking mechanics.
> 3. **No Scope Creep in v1:** Do NOT implement WakaTime compatibility, T3 Code, Cursor, or extra harnesses in Step 1. Nail **Claude Code, OpenCode, and Codex CLI/Desktop** first.
> 4. **Anti-Mocking Rule:** Never stub OS HID/window APIs, harness parsers, or hash verification with `todo!()` or hardcoded `true` values on the host OS. Ship real implementations + deterministic integration tests with realistic harness payloads and red-team fixtures on every step.
> 5. **Zero LLM Trust Boundary:** Never create a `.md` instruction file or MCP tool asking an LLM to self-report activity.

## Target Workspace Layout

```text
./  (repo root = workspace root)
├── Cargo.toml                    # Rust workspace root
├── LICENSE                       # AGPLv3
├── crates/
│   ├── traction-core/            # Shared schemas, canonical JSON, salted hash-chain, struggle math
│   ├── traction-daemon/          # Native daemon, tray, OS HID/window, 3 harness adapters, SQLite WAL, localhost review UI
│   └── traction-server/          # Axum API, Postgres SQLx, R2 presigned URLs, DecisionEngine (Heuristic + Jev)
├── packages/
│   ├── opencode-plugin/          # Zero-dep TS plugin (@traction/opencode-plugin)
│   └── ui/src/design-seam/       # ONLY folder allowed to import @hackclub/* packages (Step 6)
├── apps/
│   └── web/                      # Next.js 16+ (React 19) — DO NOT TOUCH UNTIL STEP 6
└── docs/
    ├── criteria.md
    ├── goal.md
    ├── steps.md
    ├── design-tokens.md
    ├── protocol.md
    └── adr/
```

---

## Step 1: Passive Core Daemon, OS Auto-Start & 3-Harness Event Bus
**Objective:** Build the Rust workspace (`traction-core` + `traction-daemon`) that runs silently (<15 MB RAM, <1% CPU idle) and reliably ingests normalized lifecycle events from **Claude Code**, **OpenCode**, and **Codex CLI/Desktop** with zero manual timers.

### Build
1. **Workspace & Core Schemas (`traction-core`):**
   - Initialize AGPLv3 repo, Rust workspace (pinned via `rust-toolchain.toml`), `docs/protocol.md`, and a GitHub Actions matrix (`windows-latest`, `macos-latest`, `ubuntu-latest`) running fmt, clippy, tests, and the RSS/CPU benchmark on every push.
   - Define the normalized `TractionEvent` struct (raw prompt text, raw tool output, and file contents must **never** exist in this struct; compute prompt specificity metrics in memory and discard raw text immediately):
     ```text
     TractionEvent {
       event_id, timestamp_ms, harness_id, session_id, parent_orchestrator_id?,
       event_type: SESSION_START | PROMPT_SUBMIT | PERMISSION_REQUEST | TOOL_PRE | TOOL_POST | TOOL_FAIL | SUBAGENT_START | SUBAGENT_STOP | CWD_CHANGE | TURN_END | STOP_FAILURE | SESSION_END | FILE_MUTATION,
       end_reason?: completed | aborted,
       cwd, project_id, model_id?, network_class,
       metrics_only: { prompt_char_len, repo_symbol_hits, has_stacktrace, tool_name, exit_code?, diff_lines_delta }
     }
     ```
2. **Daemon Core & Crash-Safe Storage (`traction-daemon`):**
   - Single-instance OS lock (`flock` on Unix, named mutex on Windows) so two daemon processes never corrupt state.
   - SQLite in WAL mode behind a dedicated single-writer channel/thread (batched 5s commits + graceful flush on `SIGINT`/`SIGTERM`).
   - Local Ingestion Bus: Unix Domain Socket (macOS/Linux) + loopback HTTP `127.0.0.1:41937` protected by a local per-user bearer token stored with `0600` permissions.
3. **Project Router (every folder, Git or not — see `docs/adr/0001`):**
   - Resolve every active `cwd` to its **Project root**: the Git repo root via `.git` / `--git-common-dir` (collapsing worktrees into one Project) without ever acquiring `.git/index.lock`; otherwise the nearest ancestor containing a project marker (`package.json`, `Cargo.toml`, `pyproject.toml`, `go.mod`, `*.sln`, `.traction-project`, …); otherwise the folder the Harness session started in.
   - Never create Projects for system, cache, or dependency folders (OS dirs, `~` itself, `node_modules`, `target`, `.venv`, `.git`).
   - When a non-Git Project later gets `git init` + a first commit, migrate its Project ID to the repo ID, keeping all history.
   - **Passive manual-coding coverage:** Watch the files of known Projects (recursive OS file notifications, honoring `.gitignore` / `.tractionignore`) and emit metadata-only `FILE_MUTATION` events, so hand-coding with no AI session open is still tracked.
4. **Wire the 3 Initial Harnesses:**
   - **Claude Code:**
     - Installer merges non-blocking HTTP hooks into `~/.claude/settings.json` with `"onFailure": "continue"` and `"timeout": 1` (seconds; token via `allowedEnvVars` header interpolation) for `SessionStart`, `UserPromptSubmit`, `PermissionRequest`, `PreToolUse`, `PostToolUse`, `PostToolUseFailure`, `SubagentStart`, `SubagentStop`, `CwdChanged`, `DirectoryAdded`, `Stop`, `StopFailure`, and `SessionEnd`.
     - `PreToolUse` → `TOOL_PRE` and `PostToolUse`/`PostToolUseFailure` → `TOOL_POST`/`TOOL_FAIL` give every tool call an exact hook window for `FILE_MUTATION` attribution.
     - **DO NOT hook `WorktreeCreate`** (replaces native worktree creation).
     - `UserPromptSubmit` handler matches **only** exact `/stop-track` and `/resume-track` user commands, flips local session pause state, and returns `{ "decision": "block", "reason": "Traction tracking paused for this session." }` without blocking automated turns.
     - Fallback passive inode + byte-offset JSONL tailer on `~/.claude/projects/` (prefer the hook's `transcript_path`) handling partial lines, rotation, async write lag, and daemon restarts. Mapping: prompt → `PROMPT_SUBMIT`, `tool_use`/`tool_result` → `TOOL_PRE`/`TOOL_POST`/`TOOL_FAIL`, `isApiErrorMessage` → `STOP_FAILURE`; no `TURN_END` (ends at the next prompt or `STALE_TURN_TIMEOUT`). Hook-covered spans are deduplicated (`docs/protocol.md`, "Hook coverage").
   - **OpenCode (`packages/opencode-plugin` + fallback):**
     - Zero-dependency `@traction/opencode-plugin` registered via `"plugin"` (OpenCode 1.x) or `"plugins"` (newer majors) in `opencode.json`, or `.opencode/plugins/`. Hooks `chat.message`, `tool.execute.before`, `tool.execute.after`, `permission.ask`, `command.execute.before`, `experimental.chat.messages.transform`, `experimental.text.complete`, and the generic `event` hook (TUI commands such as `/stop-track` arrive here). Strict 15ms fire-and-forget timeout. CI fixtures for OpenCode 1.19.x and the latest major.
     - Safe read-only SQLite fallback poller on `~/.local/share/opencode/opencode.db` (`mode=ro`, `PRAGMA query_only = ON`, cursor `WHERE time_updated > ?` on `session`/`message`/`part`) active only while an `opencode` process is running.
   - **Codex CLI & Codex Desktop:**
     - Primary passive Rust inode + byte-offset tailer on `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (handles Windows `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE`). Map `event_msg` `task_started` → `PROMPT_SUBMIT`, `task_complete` → `TURN_END{completed}`, `turn_aborted` → `TURN_END{aborted}`, `function_call`/`custom_tool_call` (+ `_output`) → `TOOL_PRE`/`TOOL_POST`; tolerate `compacted`, `world_state`, `token_usage_record`; distinguish Desktop via `session_meta.originator`. Verify the error record type against the codex-rs protocol source before mapping `STOP_FAILURE`.
     - Stale-turn timeout (shared with every Harness, built in Step 1c): any open Attribution Window, a log-tailed turn window or a hook window (an open tool call inside an open turn), stops claiming mutations once its session has sent no event for `STALE_TURN_TIMEOUT` (10 min), so a crashed, `kill -9`ed or unterminated turn or tool call stops claiming manual edits instead of holding until daemon restart.
     - No `notify` wrapper in v1 (raw prompt text in payload; slot may be owned by another tool).
     - **HARD RULE:** Never open, read, or lock `~/.codex/state_5.sqlite` (or any `~/.codex/*.sqlite`).
5. **Instant Privacy Controls & OS Auto-Start:**
   - Native system tray / menu bar (`tray-icon`) with 1-click `Pause 30m / 2h / Resume`, global hotkey, `traction pause` / `traction resume` CLI, and `.tractionignore` directory exclusions.
   - User-space auto-start installer (zero admin/UAC): Windows `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`, macOS `~/Library/LaunchAgents/dev.traction.daemon.plist` (**do NOT request macOS Screen Recording permissions in Step 1**), Linux `systemd --user`.

### Exit Gate (Must Pass Before Step 2)
- [ ] Integration tests verify normalized event ingestion across realistic Claude Code, OpenCode, and Codex rollout fixtures (generated by running the real Harnesses with synthetic prompts on a throwaway repo — never the developer's own transcripts) with zero duplicate events and zero raw prompt text persisted in SQLite.
- [ ] Switching across 3 Git repos (including a Git worktree) and 2 non-Git Project folders routes all events to the correct Project IDs without touching `.git/index.lock`; system/dependency folders never become Projects; `git init` on a tracked folder migrates its history.
- [ ] Hand-editing files in a known Project with no Harness running produces `FILE_MUTATION` events.
- [ ] Stopping the daemon does **not** block or error out Claude Code (`"onFailure": "continue"`) or OpenCode (15ms timeout).
- [ ] Running `/stop-track` or tray Pause immediately halts recording; restarting the daemon resumes JSONL tailing from saved byte offsets with zero lost events.
- [ ] Measured idle RSS of the headless daemon process is <15 MB and CPU is <1% on all 3 OSes in CI (spike first: if an in-process tray pushes RSS over budget, move the tray to a separate helper process whose RSS is reported separately and honestly).
- [ ] Local ingestion bus rejects missing/wrong bearer tokens, foreign `Host` headers, and foreign `Origin` headers.

---

## Step 2: OS Presence, Three Clocks & Local Productive-Struggle Heuristics
**Objective:** Separate **Human Active Time** from **Agent Compute Time** and compute deterministic, local-only productive-struggle scores on 60-second buckets.

### Build
1. **Cross-Platform OS Presence & HID Module (`traction-daemon`):**
   - Active foreground App Category from a closed list (`ide`, `terminal`, `browser`, `ai_desktop`, `other`; resolved in memory from a compiled-in table, never storing a window title, URL or raw application identifier) + OS idle timer (`GetLastInputInfo` on Windows, `CGEventSourceSecondsSinceLastEventType` on macOS, X11 + graceful Wayland degradation on Linux without crashing).
   - Hardware input injection detection (`LLKHF_INJECTED` / `LLMHF_INJECTED` on Windows, synthetic `CGEventSource` checks on macOS) and inter-event cadence entropy (detecting robotic mouse jigglers / macros).
2. **Three-Clock 60-Second Bucket Aggregator (`traction-core` + `traction-daemon`):**
   - Compute `human_active_sec`, `agent_compute_sec`, and `idle_autonomous_sec` per 60-second bucket.
   - Enforce the core rule: agent running while human actively tests `localhost`/reviews diffs/edits files = `human_active_sec` ticks; agent running while human is AFK/tabbed out = `agent_compute_sec` only (and `idle_autonomous_sec` after >90s unverified drift).
   - Ensure `STOP_FAILURE` and `SESSION_END` immediately terminate open agent compute intervals so stuck timers never leak hours.
3. **Deterministic Local Struggle Signals (`traction-core` — Zero LLM, Zero PII):**
   - **Prompt Specificity:** Repo symbol/filename index overlap, stack-trace detection, and prompt-to-diff ratio (computed in memory before raw text is discarded).
   - **Verification & Friction Loops:** Correlate `TOOL_FAIL` (`PostToolUseFailure`), optional lightweight shell hook (command category + exit code only—never flags or env vars), and shadow file diff churn/reverts. *(The shell hook is deferred: Step 2 ships without it, and a revert is approximated by Manual Work right after Harness churn because the watcher never reads contents.)*
   - **Personal Comfort-Zone Delta:** Maintain a local rolling 30-day per-user baseline so beginners going 0→1 earn full credit while repetitive boilerplate grinding decays smoothly.

> **Deferrals recorded 2026-11-26:** Step 1 was signed off on Step 1c (Claude Code) alone. Sub-steps 1d (OpenCode plugin and Codex rollout tailer) and 1e (tray, auto-start, idle RSS/CPU) follow Step 2; their Step 1 Exit Gate items stay open and must pass before Step 3. Step 2 is Harness-agnostic and tested with Claude Code-shaped events.

### Exit Gate (Must Pass Before Step 3)
- [x] Automated simulation test: prompting an agent and staying idle for 10 minutes logs `10m Agent Compute / 0m Human Active`. (`traction-core` `tests/rollup.rs::gate_simulation_ten_idle_minutes_is_all_agent_and_mostly_unverified`; daemon `finalizer::tests::the_gate_simulation_is_finalized_once_the_minutes_settle`.)
- [x] Automated simulation test: active error → edit → test-fail → edit → test-pass cycle scores high `verified_struggle` regardless of beginner vs. advanced code size. (`tests/struggle.rs::a_fail_edit_fail_edit_pass_loop_scores_high_and_size_is_ignored`.)
- [x] Synthetic HID / robotic cadence fixture sets `injected_hid_detected = true` and routes minutes to `idle_autonomous_sec`. (`tests/rollup.rs::injected_ratio_flags_the_minute_and_it_earns_nothing`, `::robotic_cadence_flags_the_minute`.)
- [x] Property test: summed across every Project and clock rule, credited time never exceeds elapsed wall-clock time, including floods of events at one timestamp. (`tests/credit_properties.rs::credited_time_never_exceeds_elapsed_wall_clock`, `::any_arrival_order_gives_byte_identical_output`, `::a_flood_at_one_timestamp_cannot_mint_time`.)
- [x] Deterministic persona red-team simulator: the honest debugger earns real credit; the 59 s key-tapper, trivial-prompt spammer and Steer-style rhythm faker earn negligible credit, on `full`, `reduced` and `idle_only` tiers. (`tests/personas.rs`, marks in `PERSONA_MARKS_1`.)
- [ ] All of the above green on Windows, macOS and Linux in CI (the first 3-OS run is the human's sign-off step; see `.scratch/step-2/issues/04-anti-spoof-suite-and-step-2-gate.md`).

---

## Step 3: Tamper-Evident Local Vault, Sparse Snapshots & Review/Redact Flow
**Objective:** Generate privacy-safe, cryptographically tamper-evident proof of work locally, with an on-demand Review & Redact UI for Audit Bundle export.

### Build
1. **Sparse Local Snapshot Vault (`traction-daemon`):**
   - Capture low-res 720p WebP frames (~15–30 KB) every 60–90s (with randomized jitter seeded from server when online) **only while a tracked project is active** and on key events (prompt submit, build/test run, focus shift).
   - Deduplicate consecutive identical screens via 64-bit `dHash` (skip encoding when Hamming distance < 3).
   - Prompt macOS Screen Recording permission **only** when the user explicitly enables visual proof mode—never on initial silent install.
   - Auto-privacy denylist: automatically suspend capture when password managers (`1Password`, `Bitwarden`, `KeePass`), private/incognito browser windows, or `.env` files are focused; run regex secret redaction before writing any local metadata.
   - Store encrypted frames locally in `~/.traction/vault/` with rolling retention (default 45 days unless pinned to an active project).
2. **Salted Hash-Chain & Offline Consistency (`traction-core` + `traction-daemon`):**
   - For each captured frame $i$: generate 16-byte cryptographic salt and compute `frame_hash = SHA256(image_bytes || salt)` (Step 3b; folded into the chain as `frame_digest`, reserved as 32 zero bytes until then).
   - Every 2 minutes (one 120 s Activity Rollup window) seal a Chain Link with the domain-separated formula `entry_hash = SHA256("traction/v1/link\0" || canonical_json(header))`, where the header holds `prev_hash` (the previous `entry_hash`, 64 zeros at genesis) and `payload_digest = SHA256("traction/v1/payload\0" || canonical_json(payload))` over the window's per-Project `rows_digest`s (Work Buckets and commit oids), the Activity Rollup digest and `frame_digest`; sign `entry_hash` with the Device Signing Key (Ed25519, `verify_strict`). Full spec in `docs/protocol.md` section 4. (Replaces the bare `H_n = SHA256(H_{n-1} || ...)` line.)
   - When offline, queue buckets in SQLite tagged `sync_mode = offline_buffered` and flush upon reconnect; track harness network type (`cloud_api` vs `local_model` vs `manual`) so impossible offline cloud sessions are detectable. Offline manual coding and local-model Harnesses (e.g. `ollama`) must track and verify fully. Non-Git Projects use 32 zero bytes for `git_diff_hash` and a file size/mtime churn digest for the shadow diff.
3. **Local Review & Redact UI (Served On-Demand at `http://127.0.0.1:41937/submit`):**
   - Lightweight HTML/JS served straight from the Rust daemon (zero Electron/WebView RAM when closed).
   - Lets the user visually scrub their project's sparse filmstrip, click to redact/blur any private frame (dropping `image_bytes` and `salt`, keeping `{ "redacted": true, "frame_hash": "<hex>" }` so hash-chain integrity stays intact), inspect `% redacted`, and export a compressed `.traction-bundle` archive (~15–25 MB per 30-hour project).

### Exit Gate (Must Pass Before Step 4)
> **Scope deviation recorded 2026-11-26:** Step 3 is split. Sub-step 3a (item 1, minus frames) is built and gated first; items 2 and 3 (and the frame-bearing parts of item 1) are deferred to 3b/3c until after Step 4, and must pass before Step 5. Item 1 and the bundle tamper suite are proven by `crates/traction-daemon/tests/tamper_red_team.rs`.

- [ ] Tampering with, deleting, or inserting any past bucket in local SQLite fails `traction-core` hash-chain verification immediately (frames: deferred to 3b). *(Proven for 3a by `tamper_red_team.rs`; ticked at human sign-off of 3a.)*
- [ ] Redacting 3 frames removes 100% of their pixel bytes and salts from the exported bundle while passing chain verification and accurately reporting `% redacted`.
- [ ] Password-manager / `.env` focus test confirms zero frames are captured during sensitive focus.
- [ ] Frame tampering (swapping, altering or removing a real frame) is caught by the same server verification path Step 4 builds (`verify_bundle` against anchored tips). *(Added by Step 4 deviation 2.)*

---

## Step 4: Centralized Backend, Hash Anchoring & Swappable `DecisionEngine` (Jev)
**Objective:** Build `traction-server` (Axum + SQLx + Partitioned Postgres + Cloudflare R2) to run at ≤$300/mo all-in at 15,488 MAU (hard cap $500/mo all-in, never exceeded even at peak) while anchoring live hashes and triaging sessions via a swappable `DecisionEngine`.

> **Before writing Jev code:** Install TypeSafe's official agent skill (`claude plugin marketplace add typesafe-ai/skills` && `claude plugin install typesafe@typesafe-ai`) so exact live request/response schemas are loaded—never guess API fields.

### Build
1. **API, Hash Anchoring & Storage (`traction-server`):**
   - Real-time 2-minute bucket + hash-tip ingestion recording monotonic `server_received_at` timestamps; merge overlapping multi-device intervals so 1 wall-clock hour never yields >1 human hour.
   - Partitioned PostgreSQL 17 tables for buckets/anchors + real-time SSE/WebSocket fanout for live profile stats.
   - Presigned Cloudflare R2 upload/download endpoints for submitted Audit Bundles ($0 egress).
   - External integration seams: define clean `AuthProvider` (working local dev provider + Hack Club OAuth2/OIDC env-driven provider) and `ProjectDbExporter` traits.
   - **Projects & GitHub linking:** Store each Project's name, absolute folder path, home-relative path, and OS username (owner/reviewer-visible only; public profiles show the Project name). API to link a Project to a GitHub repo (GitHub App / OAuth), fetch its commit history for audit alignment, and merge duplicate Projects (renamed folders, same folder on two devices). Linking is optional for tracking — unlinked and private-repo Projects are fully tracked and visible to their owner.
   - Record soft CI-signed binary provenance metadata (accepting self-compiled AGPLv3 forks gracefully and tagging them for standard human spot-check).
2. **Swappable `DecisionEngine` Seam:**
   - **`HeuristicEngine` (Default, $0, zero external APIs):** Deterministic scoring from `traction-core` so any AGPLv3 self-hoster can run Traction completely standalone.
   - **`JevEngine` (Production Hack Club Plugin):**
     - Endpoint: `POST https://api.typesafe.ai/v1/systemone` with `Authorization: Bearer <TYPESAFE_API_KEY>`, pinned to `jev-1.14.0` (never an unpinned alias).
     - Enforce a hard monthly Jev spend limit from server config; when reached, triage falls back to `HeuristicEngine` (never overspend).
     - Centralize all question definitions, criteria, and confidence routing thresholds in a **single server config file** (outside source logic so Fraud Squad can tune rubrics without code changes).
     - Run batched atomic questions (`noul` with starting `0.7`/`0.3` thresholds, `choice` **always** including an `"other"` option (superseded by a companion "does any option fit?" `noul`, see Exit Gate deviation 3), and `score` inspecting the **full level probability distribution** rather than raw averages) on anonymized 15-minute telemetry JSON first.
     - For submitted Audit Bundles / flagged anomalies only: transcode sampled WebP frames **in memory on the server** to 8-bit baseline JPEG (longest side ≤1568px), attach in the top-level `assets` map with UUID placeholders, and send with header `TypeSafe-Danger-Zone-Images: true` strictly as a secondary reviewer hint.
     - Route via 3-way confidence router (Green fast-track / Yellow fail-closed human review / Red anomaly pin)—**never auto-ban**.

### Exit Gate (Must Pass Before Step 5)
> **Scope deviations recorded 2026-11-26 (Step 4 spec, `.scratch/step-4/spec.md`; ADR `docs/adr/0006-linux-only-db-ci.md`):**
> 1. **Postgres and MinIO tests run on Linux only.** `traction-server` deploys only on Linux, and installing Postgres on Windows and macOS runners is slow and flaky. These still run on all 3 OSes: `cargo fmt`, `cargo clippy --workspace --all-targets`, every `traction-core` test, every `traction-daemon` test (including the sync client against a local test HTTP server) and every `traction-server` test that needs no database. The Postgres 17 and MinIO service-container tests run on `ubuntu-latest`. "Passes in CI" for this Step means this split.
> 2. **Real frame tampering moves to the 3b/3c gate.** Frames do not exist until 3b. Step 4 proves `frame_digest` integrity (changing a link's `frame_digest` fails verification) through `verify_bundle`; 3b/3c gain a gate item that runs real frame tampering through the same server path.
> 3. **No catch-all `"other"` choice option (recorded 2026-11-27).** Build item 2 and Gate item 4 ask for every `choice` to include an `"other"` option. TypeSafe's live `v1/systemone` guidance (the installed `typesafe@typesafe-ai` 0.6.2 skill) says a catch-all option competes with the real options for probability and distorts the distribution. Instead each `choice` is paired with a companion `noul` asking "does any of these options fit?", and "nothing fits" routes to Yellow (fail-closed human review). Gate item 4's `"other"` fallback is proven by that companion `noul` handling (`crates/traction-server/tests/jev.rs`: `no_choice_has_a_catch_all_other_and_each_has_a_companion_fits_noul`, `nothing_fits_routes_to_yellow_and_a_fitting_option_uses_its_colour`).

- [ ] Benchmark/load test simulates 15,488 MAU (110k hrs/mo peak burst) against Postgres + Axum with <50ms p95 ingest latency and low memory footprint (<512 MB server RAM).
- [ ] Itemized all-in monthly cost model (server, Postgres storage growth, R2, Jev at its spend limit, bandwidth, backups, domain/email) totals ≤$300 at 15,488 MAU and stays ≤$500 under the load test's peak burst.
- [ ] Recomputing a submitted Audit Bundle against anchored `server_received_at` tips catches backdating, `frame_digest` tampering (real frame tampering: see deviation 2), and clock-skew overlap attacks.
- [ ] `HeuristicEngine` passes full test suite with zero network calls; `JevEngine` passes contract tests verifying `v1/systemone` payload shape, WebP→baseline JPEG transcoding, `"other"` fallback (the companion `noul` "nothing fits" → Yellow, see deviation 3), and score-distribution handling.

---

## Step 5: Fraud Squad & Reviewer Audit System (YSWS Readiness)
**Objective:** Give Hack Club reviewers and Fraud Squad an audit workflow that validates or deflates a 30-hour project in under 2 minutes, then red-team every attack vector.

### Build
1. **Submission Eligibility:** A Project can be submitted to a YSWS program only when linked to a **public (open-source) GitHub repo**. Private and unlinked Projects keep full tracking and owner-visible stats but cannot be submitted (see `docs/adr/0002`).
2. **Automated Audit Verification Pipeline:**
   - On bundle upload, verify: salted hash-chain continuity against live server anchors, `% redacted`, `Impossible Offline Cloud Session` flags, synthetic HID flags, zero-AST-delta stretches, and `DecisionEngine` anomaly pins.
3. **Fraud Squad Reviewer View (API + Dedicated Reviewer UI):**
   - **4-Track Synchronized Struggle Flamegraph:**
     1. Human Active vs. Agent Compute vs. Idle Autonomous
     2. Shadow Diff / AST Velocity & GitHub Commit Alignment
     3. Friction Arcs (build/test failures turning green)
     4. Anomaly Pins (1-click jump to top suspicious stretches)
   - **Verified Frame Scrubber:** Clicking any timestamp displays the hash-verified sparse snapshot (or `REDACTED_BY_USER` badge) alongside anonymized harness, diff stats, and exit-code context.
   - **Verdict & Export:** Reviewer Approve / Deflate / Flag actions with reason codes and structured audit report export via `ProjectDbExporter` (ready for impartial third-party review and Hack Club Unified Projects DB).
4. **Full Red-Team Pentest Suite (Automated + Live):**
   - Run all 4 attacks against the live stack:
     1. Scripted LLM-to-OpenCode prompt loop
     2. AutoHotkey / `xdotool` synthetic HID jiggler
     3. Forked daemon submitting spoofed `"hid_valid": true` JSON with static/blank screenshots
     4. Local SQLite backdating / frame-swapping attempt
   - Fix any gaps until all 4 attacks trigger clear anomaly pins or chain/visual audit failures in the Fraud Squad view.

### Exit Gate (Must Pass Before Step 6)
- [ ] All 4 red-team attack tests are automatically detected and surfaced prominently in the Fraud Squad view.
- [ ] End-to-end timed walkthrough confirms an unfamiliar reviewer can audit a multi-hour test project and distinguish real struggle from a scripted LLM loop in under 2 minutes.

---

## Step 6 (STRICTLY LAST): Next.js Frontend UI/UX & Post-v1 Expansion
**Objective:** Ship the motivating teen-facing web app wired strictly through the Hack Club design system, then expand harness coverage toward **1,000 weighted projects**.

### Build
1. **Swappable Hack Club Design Seam (`docs/design-tokens.md` + `packages/ui/src/design-seam/`):**
   - Wire `@hackclub/tokens`, `@hackclub/theme` (`<hc-button arrow>` and native web components), and `@hackclub/icons` strictly through `packages/ui/src/design-seam/`.
   - Add a CI lint rule that **fails builds** if any file outside `design-seam/` uses hardcoded hex colors or imports non-Hack Club icon libraries.
2. **Core Web App (`apps/web/` — Next.js 16+ / React 19):**
   - **Landing Page & 15-Second One-Liner Install:** OS-detected install command (`irm ... | iex`, `curl ... | sh`, Homebrew) showing zero-config setup for Claude Code, OpenCode, and Codex.
   - **Auth & Connections:** Sign in / Sign up, **Login with Hack Club**, and GitHub repo linking per Project (including linking folders first tracked without Git, and merging duplicate Projects).
   - **Public-by-Default Profiles & Real-Time Dashboard:** Live SSE stats, **Build Stories** (auto-generated tough-bug milestones), **Craft Breakdown** (Human Active vs. AI Leverage vs. Debugging/Verification), deep-flow streaks, comfort-zone growth, shareable profile cards, and embeddable GitHub README badges.
   - **Comparison Page:** Objective, transparent comparison of Traction vs. Hackatime, Lapse, and other Hack Club trackers across privacy, passive tracking, AI struggle accuracy, RAM footprint on 10-year-old laptops, and reviewer audit speed.
3. **Post-v1 Expansion (Requirement #7):**
   - Add **T3 Code** causal process-tree (`PPID`) + Git worktree deduplication (never scanning/locking `statev2.sqlite`).
   - Publish the **Open Traction Ingestion Spec** (`http://127.0.0.1:41937/v1/event` + `traction ingest`) so community plugins can add Cursor, Windsurf, Zed, Aider, editors, and future AI agents.

### Exit Gate (Bounty Win Condition)
- [ ] CI passes with zero design-seam violations, full AGPLv3 compliance, and verified cross-platform builds (Windows/macOS/Linux).
- [ ] Production hosting verified at ≤$300/month all-in at Hackatime scale and never above $500/month at peak.
- [ ] Every technical requirement in `docs/criteria.md` is demonstrably met (this is the definition of done).