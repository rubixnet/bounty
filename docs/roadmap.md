# Sub-Step Roadmap

Each Step in `docs/steps.md` is split into sub-steps sized for one fresh session (under ~150k tokens). Run every sub-step the same way:

1. Start a fresh session (`/clear`).
2. `/grill-with-docs Step Nx from docs/roadmap.md` (skip this for 1a and 1b, which were grilled on 2026-11-24).
3. `/to-spec` writes `.scratch/step-Nx/spec.md`.
4. `/to-tickets` writes `.scratch/step-Nx/issues/NN-*.md`.
5. `/clear`, then run `/implement .scratch/step-Nx/issues/NN-*.md` once per ticket, clearing context between tickets.
6. At the end of the sub-step, run `/code-review` and tick its gate items. At the end of a Step, get human sign-off before moving on.

The gates below are excerpts from `docs/steps.md`. A Step is complete only when every gate item passes on all 3 OSes in CI.

## Step 1: Passive daemon and the 3 Harnesses

| Sub-step | Scope | Gate items it owns |
|---|---|---|
| **1a: Foundation** | Install Rust (`rustup`, pinned in `rust-toolchain.toml`), AGPLv3 `LICENSE`, Cargo workspace, private GitHub repo with a 3-OS CI matrix (create it only after the human confirms). `traction-core`: `TractionEvent`, canonical JSON. `traction-daemon`: single-instance lock, SQLite WAL with a single writer, ingestion bus (UDS + `127.0.0.1:41937`, bearer token, Host/Origin checks). | Ingestion bus rejects bad token, Host and Origin. No raw text in SQLite. |
| **1b: Project Router** | Project root resolution (git root → nearest marker → session start folder) and `--git-common-dir` worktree collapse without `index.lock`. `dir:`/`git:` IDs and migration. Folder exclusions. File watcher emitting `FILE_MUTATION` events (always on per ADR 0001; Harness attribution only inside exact hook windows or one documented fallback window for log-tailed Harnesses; no retroactive attribution across daemon downtime). `.tractionignore`. Hardened `git` wrapper (plumbing allowlist, `--no-optional-locks`, `core.fsmonitor=false`, `core.hooksPath=/dev/null`, strict timeout, skip `$HOME`-rooted repos). `TractionEvent` schema room on `schema_version: 1`: optional `cwd` plus a closed `attribution_source` enum reserving browser/editor Adapters (full Open Ingestion Spec stays in 6d). | 3 repos + 1 worktree + 2 non-Git folders route correctly. `git init` migrates history. Hand edits emit `FILE_MUTATION`. Red-team fixture: a repo-local `core.fsmonitor` never executes. |
| **1c: Claude Code Adapter** | Install the `claude` CLI. Installer merges HTTP hooks (`timeout: 1`, `onFailure: continue`, token via `allowedEnvVars`, never `WorktreeCreate`) with a version check. Hook `PreToolUse` alongside `PostToolUse`/`PostToolUseFailure` so every Claude tool call opens and closes an exact hook window (`TOOL_PRE`→`TOOL_POST`/`TOOL_FAIL`). Stale-turn timeout for log-tailed Harnesses: an open log-tailed turn window with no new events for one documented inactivity constant auto-closes, so a crashed or unterminated turn (Codex rollout, Claude JSONL fallback) stops claiming manual edits instead of holding until daemon restart; the shared mechanism lands here and Codex (1d) inherits it. `/stop-track` and `/resume-track` blocking. JSONL tailer fallback with offsets persisted. Fixtures from real runs that use synthetic prompts. Future polish (not 1b): a `traction --dry-run` installer flag that prints the hook merge without writing it. | Stopped daemon doesn't block Claude. `/stop-track` halts recording. Restart resumes from saved offsets. |
| **1d: OpenCode + Codex Adapters** | Zero-dependency `@traction/opencode-plugin` (`plugin` and `plugins` keys, 15 ms timeout, `pnpm test/typecheck`). Read-only `time_updated` poller that runs only while `opencode` is alive. Codex rollout tailer (event mapping, Windows share flags, Desktop vs CLI detection, never `*.sqlite`). Confirm the Codex error record type. | Zero duplicates across all 3 sets of fixtures. Stopped daemon doesn't block OpenCode. |
| **1e: Controls, Auto-start, Budget** | Tray (Pause 30m/2h/Resume), hotkey, `traction pause` / `traction resume`. User-space auto-start (HKCU Run, LaunchAgent, `systemd --user`), never a Screen Recording prompt. Measure RSS and CPU in CI; split the tray into a helper process if it pushes RSS over budget. | Tray Pause halts recording. Idle RSS <15 MB, CPU <1%. |

## Step 2: OS presence, Three Clocks, struggle signals

| Sub-step | Scope |
|---|---|
| **2a: OS Presence** | Foreground window categories (without storing raw private window titles), idle timers, Wayland degradation, a way to map the focused IDE to its Project. Resolve the macOS Input Monitoring permission question. Emit privacy-safe activity rollup buckets every 1–5 minutes: window-switch rate, app-category share (`ide`, `terminal`, `browser`, `ai_desktop`, `other`; reconcile with the `steps.md` Step 2 category list in the 2a grill), key/mouse cadence and inter-event timing entropy, HID-injection ratio (filled by 2b's detector), and `telemetry_tier` so Wayland and other reduced-signal sessions degrade cleanly instead of reporting zeros. Counts and ratios only, never keystrokes, titles or URLs. |
| **2b: Injected HID + Cadence** | Detection of `LLKHF_INJECTED`, synthetic `CGEventSource` input and evdev/uinput input. Entropy check for robotic timing. Fixture showing jiggler time routed to Unverified Autonomous Time. |
| **2c: Three-Clock Aggregator** | 60-second Work Buckets, the >90 s rule, `STOP_FAILURE`/`SESSION_END` close open timers, simulation test (10m agent / 0m human). |
| **2d: Struggle Signals** | Prompt specificity (repo symbol index), friction loops, rolling 30-day comfort baseline. Anti-spoof scoring over the 2a rollup buckets: dev-loop interleaving credit (`ide`/`terminal`/`browser`/`ai_desktop` switching that sits around real work) vs. decay for unanchored `browser`/`other` drift, plus downstream outcome anchoring (credit is confirmed by later `FILE_MUTATION`s, commits and Harness turns). Gate: the verified-struggle simulation. |

## Step 3: Vault, hash-chain, review and redaction

| Sub-step | Scope |
|---|---|
| **3a: Hash-Chain in core** | Salted frame hashes, `H_n`, canonical bucket JSON, verification. Gate: tampering, deletion and insertion all fail. |
| **3b: Snapshot Vault** | Opt-in visual proof mode, 720p WebP, dHash dedup, encrypted vault, 45-day retention, denylist (password managers, incognito, `.env`). Gate: zero frames captured while sensitive windows are focused. |
| **3c: Offline + Review UI + Bundle** | `offline_buffered` sync, `network_class`, `/submit` filmstrip UI, Redaction, `.traction-bundle` export. Gate: redacting 3 frames still verifies. |

## Step 4: Server, Anchors, DecisionEngine

| Sub-step | Scope |
|---|---|
| **4a: Server Core** | Axum + SQLx, partitioned Postgres 17, `/v1/anchors`, `server_received_at`, merging of multi-device overlap, SSE fanout, `AuthProvider` (dev + Hack Club OIDC). |
| **4b: Projects + GitHub + R2** | Project metadata (paths, username, owner/reviewer visibility), GitHub linking and commit fetch, Project merge, presigned R2, build provenance. Session timeline API that time-aligns the 2a activity rollup buckets with `FILE_MUTATION`s, Git commits and Harness turns (data source for the 5b Audit Strip). |
| **4c: HeuristicEngine + Load/Cost** | Default engine with zero network calls, load test (<50 ms p95, <512 MB), itemized cost model ≤$300 (cap $500). Gate: backdating and clock-skew detection. |
| **4d: JevEngine** | Install the TypeSafe skill first. `v1/systemone` contract tests, `"other"` choice, score distributions, WebP→JPEG, hard spend limit with fallback, 3-way router. |

## Step 5: Fraud Squad and red team

| Sub-step | Scope |
|---|---|
| **5a: Audit Pipeline + Eligibility** | Public Linked Repo required for submission, chain/anchor verification on upload, anomaly flags. |
| **5b: Reviewer View** | 4-track flamegraph, frame scrubber, verdicts, `ProjectDbExporter`. Session Audit Strip: a time-aligned strip overlaying the activity rollup buckets (window-switch rate, app-category share, cadence entropy, HID-injection ratio, `telemetry_tier`) against `FILE_MUTATION`s, Git commits and Harness turns, so a reviewer can tell a debugging loop from unanchored browsing or synthetic input at a glance. A minimal reviewer UI is allowed here; the Hack Club design seam is not. |
| **5c: Red Team** | All 4 attacks automated against the live stack, plus a timed walkthrough that a reviewer finishes in under 2 minutes. |

Future polish (Step 5, not Step 1b): `traction doctor` (checks daemon, token, hooks, plugin and tailers, and reports what is wrong) and a clean `traction uninstall` that removes hooks, plugin registration, auto-start entries and local data without touching anything Traction did not write.

## Step 6: Frontend and expansion

| Sub-step | Scope |
|---|---|
| **6a: Design Seam + CI Lint** | `packages/ui/src/design-seam/`, lint rule for seam violations. |
| **6b: Landing + Install + Auth** | Install one-liners for each OS, Login with Hack Club, GitHub linking UI, Project merge UI. |
| **6c: Profiles + Live Dashboard** | Public-by-default profiles (names only, never paths), Build Stories, Craft Breakdown, streaks, badges. |
| **6d: Comparison + Ingestion Spec + T3 Code** | Comparison page, Open Traction Ingestion Spec, T3 Code dedup. Lightweight reference MV3 browser extension Adapter (turns on the reserved `browser_extension` `attribution_source`): classifies the active tab's hostname strictly locally into closed buckets (`dev_docs`, `localhost_preview`, `web_ai`, `other_web`), sending only the bucket, never URLs or titles. `dev_docs` and `web_ai` time earns extended research credit, exempt from 2d's unanchored-drift decay. Gate: everything in `criteria.md`. |
