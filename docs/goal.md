**The goal of this AI Bounty is to build an AI-native time tracking platform *Traction* that satisfies every requirement listed in [criteria.md](./criteria.md) using the architecture below.**

## Logic

### 1. Understanding How AI Coding Harnesses Work
Before tracking AI-assisted engineering accurately, stop thinking of an AI harness as a "chat window" and model it as a **deterministic local event loop with disk side-effects**. Every modern coding agent (Claude Code, OpenCode, Codex CLI/Desktop) executes the same 6-stage state machine per turn:

1. **Human Input (`TURN_START`):** The human types or edits a prompt and submits. Input is appended to the message array and persisted locally (JSONL or SQLite).
2. **Context Assembly & LLM Streaming (`AGENT_THINKING`):** The harness bundles system prompts, repo rules (`CLAUDE.md` / `AGENTS.md`), open files, and prior turns, then streams from the model provider. The human is waiting here unless they are simultaneously reading code, reviewing diffs, testing in a browser, or editing another file.
3. **Tool Call Proposal (`TOOL_REQUEST`):** The model emits structured tool calls (`Read`, `Edit`, `Write`, `Bash`, `Grep`, MCP tools).
4. **Permission Gate (`AWAITING_HUMAN`):** If a tool is not auto-approved, the harness blocks until the human inspects the diff or command and clicks Allow/Deny.
5. **Local Execution (`TOOL_RUNNING`):** The harness mutates files or spawns a subprocess (`bash`, `pwsh`, test runner), captures output and exit codes, and appends a `tool_result` record.
6. **Loop or Yield (`TURN_END`):** Steps 2–5 repeat 1 to 100+ times autonomously until the model yields control back to the human (or terminates on API/runtime failure).

**Core mistake to avoid:** Naive trackers merge these stages into one timer. That is why Hackatime's AI tracking breaks—it counts Step 2 + Step 5 (how long the API/agent ran) instead of how long the human struggled.

Traction runs **three parallel state-machine clocks** on every 60-second bucket:

| # | Clock | What Increments It | Counts Toward YSWS Struggle? | Shown on User Profile? |
|---|-------|--------------------|------------------------------|------------------------|
| 1 | **Human Active Clock** | Real OS hardware input (HID) in IDE, terminal, docs, `localhost`/DevTools, diff review, or actively composing/approving prompts. | **Yes** — weighted by struggle signals. | **Yes** — Primary metric. |
| 2 | **Agent Compute Clock** | Harness is between `PROMPT_SUBMIT` and `TURN_END` / `STOP_FAILURE` (streaming inference or executing tool loops). | **Only if** the human is actively supervising, testing, or working in parallel — **not** if AFK or watching videos. | **Yes** — Secondary "AI Leverage" stat. |
| 3 | **Idle / Unverified Autonomous Clock** | Agent loops for **>90s** with zero human HID, zero focus on project/test windows, or injected synthetic input. | **No** — deflated to `0`; preserved in local audit log for transparency. | **Hidden / Internal** — Audit only. |

---

### 2. Operational Definition of "Productive Struggle"
An hour of productive struggle is an hour where the **human is actively steering through uncertainty, testing hypotheses against reality, and adapting based on feedback**.

> If the user is actively reading diffs, testing `localhost:3000`, reading docs, or editing another file while the agent runs, the Human Active Clock ticks. If the user prompts "build the whole app" and walks away for 20 minutes, the Agent Compute Clock ticks 20m, and the Human Active Clock ticks 0m.

Because 0→1 for a beginner is just as challenging as 100→101 for an expert, productive struggle is defined by **four measurable, model-agnostic behavioral signals**—never by lines of code written, model tier, or project complexity:

| # | Dimension | High Productive Struggle (Counts) | Low / Zero Struggle (Deflates) | Why It's Beginner- & Budget-Fair |
|---|-----------|-----------------------------------|-------------------------------|----------------------------------|
| 1 | **Human Steering vs. Agent Drift** | Human reads diffs, runs the app, inspects errors, manually edits, or writes targeted follow-ups between AI turns. | AI agent loops autonomously while the user is AFK, tabbed out, or spamming "continue / fix all." | Tracks **Human Active Time** and **AI Compute Time** separately, so expensive frontier models give zero unfair advantage. |
| 2 | **Prompt Specificity (Local-Only Metadata)** | Prompts reference specific files, symbols, stack traces, edge cases, or constraints (e.g., "test auth bugs on `/api/login`"). | Vague one-liners ("find bugs and patch," "rewrite in Rust") triggering massive uninspected tool-call chains. | Scored locally in memory via deterministic heuristics (repo symbol/filename overlap, stack-trace presence, prompt-to-diff ratio). Raw prompt text is immediately discarded and never leaves the device; no heavy local LLM required on 10-year-old laptops. |
| 3 | **Verification & Friction Loops** | Visible cycles of: hypothesis → change → run/build/test → inspect failure/UI → refine. | Pure generation or repetitive copy-paste with zero execution, testing, or debugging cycles. | A beginner fixing a Python `SyntaxError` produces the same **fail → read docs → edit → pass** curve as an expert debugging a WebGL shader. |
| 4 | **Personal Comfort-Zone Delta** | Working with unfamiliar APIs, new error classes, iterative refactoring, or reading docs/source code. | Repeating identical boilerplate the user has already written dozens of times at flat speed with zero friction. | Measured against the user's **own rolling 30-day baseline**, not a global skill bar. |

> "1 hour of wall-clock time can produce 0m, 10m, or 60m of productive struggle, whether you're hand-coding every line or directing Claude Code." — zrl

---

### 3. How Hackatime Works Today (and What Traction Upgrades)
Hackatime receives ~2-minute heartbeats from WakaTime-compatible plugins containing file paths, line numbers, project/branch/language metadata, and newer AI client fields—without file contents or keystrokes.

During YSWS review and fraud checks, reviewers inspect three sources:
- Heartbeat streams & AI breakdowns (active stretches, file/line movement, separated AI rows).
- GitHub commit history (checking whether logged time matches incremental commits rather than a giant dump).
- Fraud / heartbeat-poisoning signals (fabricated heartbeats, impossible concurrency, undeclared AI bursts).

**Why Stardance deflated 123,400 hours to 27,600:** Heartbeats only prove an editor had focus or an AI tool emitted events—not that a human was thinking, testing, or steering. Reviewers had to guess from Git diffs and aggressively deflate unverified vibe-coding.

**The Traction upgrade:** Meet High Seas–era Hackatime passive reliability while generating verifiable proof that stands up to an impartial third party. Reviewers receive a **Session Struggle Breakdown** combining Human Active vs. AI Compute time, build/test friction arcs, shadow diff progression, and user-reviewed sparse visual proof.

---

### 4. Pentesting & Open-Source Security Model

#### Attack 1: "What stops someone from making an LLM prompt OpenCode in a loop?"
A script using a cheap LLM can feed specific-looking prompts into OpenCode or Claude Code every 2 minutes. Defeat automated LLM-to-LLM farming with three orthogonal signals:
1. **OS-Level Hardware Input (HID) Presence & Entropy:** Detect OS-injected input flags (`LLKHF_INJECTED` / `LLMHF_INJECTED` on Windows, synthetic `CGEventSource` state on macOS, evdev/uinput separation on Linux) and robotic inter-event timing/pointer absence. Sessions lacking real human HID route to Unverified Autonomous Time.
2. **Event-Triggered Sparse Local Snapshots:** Capture compressed 720p WebP frames (~15–30 KB, deduplicated with 64-bit `dHash`) strictly locally every 60–90s with server-seeded randomized jitter, **only while a tracked project is active**. An LLM loop produces a static terminal with zero human interaction across `localhost`, DevTools, docs, or IDE diffs.
3. **Execution & Inspection Correlation:** Real humans switch windows to run binaries/browser previews, scroll diffs, and read docs. Multi-hour terminal-only loops with zero runtime inspection trigger automatic anomaly pins.

#### Attack 2: Prompt-Injecting the AI Plugin ("Tell Claude to say the user is working!")
- **Zero LLM Trust Boundary:** Never use a `.md` instruction file or let any LLM self-report activity via MCP tools. Treat every model and harness as an untrusted data source.
- Tracking runs strictly outside the model in a deterministic Rust OS daemon + harness hooks/log tailers.

#### Attack 3: Forging or Backdating Local Logs Before Submitting
- **Salted Local Hash-Chain + Real-Time Server Anchoring:**
  - For each captured snapshot frame $i$, generate a random 16-byte salt and compute:
    $$\text{frame\_hash}_i = \text{SHA256}(\text{image\_bytes}_i \parallel \text{salt}_i)$$
  - Every 2 minutes during active work, compute the rolling chain tip:
    $$H_n = \text{SHA256}(H_{n-1} \parallel \text{canonical\_bucket\_json}_n \parallel \text{frame\_hash}_n \parallel \text{git\_diff\_hash}_n)$$
  - Only the 32-byte $H_n$ and non-sensitive telemetry hit the server in real time (`server_received_at`).
  - **Privacy-Safe Redaction:** When reviewing before submission, redacting frame $i$ deletes `image_bytes` and `salt`, keeping only `{ "redacted": true, "frame_hash": "<hex>" }`. The server still verifies the entire chain identically, reports exact `% redacted`, and the random salt prevents brute-forcing blank-screen hashes.
- **Connectivity Consistency Check:** Offline manual coding and offline local models (`ollama`) buffer cleanly in local SQLite (`sync_mode = offline_buffered`). Claiming offline hours for cloud-only harnesses (Claude Code / Codex cloud API) triggers an automatic `Impossible Offline Cloud Session` anomaly.

#### Attack 4: The Open-Source Bypass ("Paste the AGPLv3 Repo into Claude and Fork a Spoofed Client")
Because Traction must be AGPLv3 open source, an attacker can read every rule and compile a forked daemon sending fake `"hid_valid": true` JSON.
- **Defense — Evidence Over Claims (Kerckhoffs's Principle):**
  1. Client-reported HID/struggle scores are **triage signals, never final proof**. Final proof is the real-time server-anchored Audit Bundle (visually consistent sparse screenshots + incremental Git diffs + shipped working code) verified by Fraud Squad.
  2. **Open Code, Private Ops Config:** All client/server source code is AGPLv3. Production Jev question rubrics, anomaly thresholds, and Fraud Squad weights live in server config and evolve as new bypass patterns emerge.
  3. **Build Provenance (Soft Signal, Not DRM):** Official CI-signed release binaries carry verifiable build provenance. Custom AGPLv3 forks work 100% freely, but route to standard human audit review instead of fast-track green approval.
  4. **Economic Security:** Synthesizing 30 hours of real-time, hash-anchored, visually coherent IDE/browser/debugging progression matching a real shipped repo costs more effort than actually building the project.

---

### 5. Initial Target Harnesses (v1 Focus)

Initial v1 focus is **strictly AI-native tracking** across the top 3 harnesses. Do **not** dilute v1 by building WakaTime compatibility or broad third-party editor plugins on Day 1.

1. **Claude Code**
   - **Under the hood:** Native lifecycle hooks in `~/.claude/settings.json` / `.claude/settings.json` supporting command hooks and zero-overhead HTTP hooks, plus append-only JSONL transcripts in `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`.
   - **Primary (Non-Blocking HTTP Hooks):** Register HTTP hooks with `"onFailure": "continue"` and `"timeout": 1` (seconds) (so a stopped daemon **never** interrupts Claude Code) on: `SessionStart`, `UserPromptSubmit`, `PermissionRequest`, `PostToolUse`, `PostToolUseFailure`, `SubagentStart`, `SubagentStop`, `CwdChanged`, `DirectoryAdded`, `Stop`, `StopFailure`, and `SessionEnd`.
   - **CRITICAL — Never hook `WorktreeCreate`:** Claude Code's `WorktreeCreate` hook replaces default Git worktree creation behavior and will break users' `--worktree` flag.
   - **Stop-Track UX:** On `UserPromptSubmit`, match **only** exact user commands `/stop-track` and `/resume-track` (never block automated agent turns), toggle session privacy locally, and return `{ "decision": "block", "reason": "Traction tracking paused for this session." }` so zero Anthropic tokens are wasted.
   - **Fallback:** Passive inode + byte-offset JSONL tailer on `~/.claude/projects/`.

2. **OpenCode**
   - **Under the hood:** JS/TS Plugin API configured via `"plugin"` (1.x) or `"plugins"` (newer majors) in `opencode.json` (or `.opencode/plugins/*.js`), exposing hooks (`event`, `chat.message`, `tool.execute.before`, `tool.execute.after`, `permission.ask`, `command.execute.before`, `experimental.chat.messages.transform`, `experimental.text.complete`); TUI commands arrive as events via `event`. SQLite persistence (WAL) at `~/.local/share/opencode/opencode.db`.
   - **Primary (`@traction/opencode-plugin`):** Zero-dependency plugin computing prompt specificity and tool timings in-memory, stripping secrets, catching `/stop-track` via TUI command hooks, and fire-and-forgetting an async `POST` to the local daemon (strict 15ms timeout).
   - **Fallback:** Read-only cursor poller (`mode=ro`, `PRAGMA query_only = ON`, cursor `WHERE time_updated > ?`) active **only** while an `opencode` process is alive.

3. **OpenAI Codex CLI & Codex Desktop**
   - **Under the hood:** Both Codex CLI and Codex Desktop stream live turn events continuously into append-only rollout JSONL files at `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` (archived to `~/.codex/archived_sessions/`), indexed in `~/.codex/session_index.jsonl`, alongside `$CODEX_HOME/config.toml` (`notify`) and private SQLite runtime DBs (`state_5.sqlite`).
   - **Primary (Append-Only Rollout JSONL Tailer):** Tail `rollout-*.jsonl` by inode + byte offset in Rust. Zero subprocess spawn overhead (unlike Codex's synchronous command-only hooks, which slow down 10-year-old Windows laptops) and covers both CLI and Desktop passively.
   - **Not in v1:** Codex `notify` wrapper (its payload carries raw prompt text and the slot may already be owned by another tool) and Codex lifecycle hooks.
   - **HARD WARNING:** Never open, query, or lock `~/.codex/state_5.sqlite` directly—external locks or symlinks on its WAL files break Codex startup.

---

### 6. Locked Tech Stack & Architecture

- **Pure Rust Monorepo Workspace (`Cargo.toml`):**
  - `crates/traction-core`: Shared `TractionEvent`, `WorkBucket`, canonical JSON serialization, salted hash-chain math, and deterministic struggle heuristics (zero schema drift between daemon and server).
  - `crates/traction-daemon`: Single ~8–10 MB static binary (<15 MB RAM, <1% CPU idle), SQLite WAL via dedicated single-writer thread, native system tray (`tray-icon`), on-demand localhost Review UI (`http://127.0.0.1:41937`), OS auto-start without admin prompts (`HKCU\...\Run` on Windows, `LaunchAgent` on macOS, `systemd --user` on Linux).
  - `crates/traction-server`: Axum + SQLx API + partitioned PostgreSQL 17 + Cloudflare R2 ($0 egress for Audit Bundles). Targets ≤$300/mo all-in at 15,488 MAU / 110,000 hrs/mo (hard cap $500/mo all-in at peak, never exceeded), e.g. one ~$60–$85/mo dedicated server + ~$2/mo R2 + a hard-capped Jev budget.
- **Swappable `DecisionEngine` Seam (Server-Side):**
  - `HeuristicEngine` (Default, $0, zero external APIs—guarantees full standalone AGPLv3 self-hosting).
  - `JevEngine` (Production Hack Club plugin calling TypeSafe System One at `POST https://api.typesafe.ai/v1/systemone`, pinned to `jev-1.14.0`, ~$0.042/1M input tokens ≈ $10–$40/mo at Hackatime scale). Uses batched atomic `noul` / `choice` (always including `"other"`) / `score` (evaluating full level probability distributions) over anonymized 15-minute telemetry first; server-side transcodes sampled WebP Audit Bundle frames to 8-bit baseline JPEG (longest side ≤1568px) only for experimental `TypeSafe-Danger-Zone-Images: true` triage on submitted bundles. Never auto-bans; routes on confidence to Green / Yellow / Red for human Fraud Squad confirmation.
- **Frontend & Design Mandate (Strictly Last — Step 6):**
  - Next.js 16+ (React 19, native web component support for `<hc-button arrow>` and friends) with **all** `@hackclub/tokens`, `@hackclub/theme`, and `@hackclub/icons` imports strictly isolated inside `packages/ui/src/design-seam/` (enforced by CI lint rules per `docs/design-tokens.md`).

---

### 7. Post-v1 Expansion (After Core AI Tracking & Fraud Audit Are Proven)
- **T3 Code Meta-Harness Support:** Process-tree (`PPID`) + Git worktree causal deduplication so T3-orchestrated Claude/Codex/OpenCode sessions never double-count and `statev2.sqlite` is never locked/scanned.
- **Universal Plugin Spec (Requirement #7):** Documented open local ingestion protocol (`http://127.0.0.1:41937/v1/event` + `traction ingest`) enabling community adapters for Cursor, Windsurf, Zed, Aider, editors, and future AI agents.