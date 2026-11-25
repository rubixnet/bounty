# Traction — Agent Instructions

Traction is an AGPLv3, AI-native passive time tracking platform built to measure verifiable productive struggle across Windows, macOS, and Linux on low-spec hardware (<15 MB headless daemon RAM, <1% idle CPU) within a hard all-in cap of $500/month at Hackatime scale (15,488 MAU / ~110k hrs/mo), designed to run at ≤$300/month so peak growth in MAU stays under the cap.

## Source-of-Truth Docs (Read Before Acting)
- `docs/criteria.md` — Verbatim bounty requirements and acceptance criteria. "Done" = every technical requirement in this file is met.
- `docs/goal.md` — First-principles architecture, Three Clocks model, productive struggle signals, and security/pentest model.
- `docs/steps.md` — Staged execution roadmap (Steps 1–6) with mandatory Exit Gates.
- `docs/protocol.md` — Canonical wire protocol (`TractionEvent`, local ingestion bus, salted hash-chain, `.traction-bundle` format).
- `GLOSSARY.md` + `docs/adr/` — Canonical domain vocabulary and recorded decisions. Use glossary terms in code, tests, and tickets.
- `docs/design-tokens.md` — Locked Hack Club design-system seam contract (do not touch until Step 6).

## Target Workspace Layout
```text
./  (this repo root is the workspace root)
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
│   └── web/                      # Next.js 16+ (React 19) — LOCKED UNTIL STEP 6
└── docs/
```

## Non-Negotiable Execution Rules
1. **Strictly One Step at a Time:** Follow `docs/steps.md` in order. Never start Step N+1 until every Exit Gate checkbox in Step N passes real automated tests on all 3 OSes in CI (Windows, macOS, Linux). Split each Step into sub-steps (1a, 1b, …) with their own spec and tickets under `.scratch/`, sized so a session stays under ~150k tokens. Stop at each Exit Gate for human sign-off.
2. **No Early Frontend:** Do NOT create, scaffold, or install packages in `apps/web/` or `packages/ui/` during Steps 1–5.
3. **No v1 Scope Creep:** Step 1 targets **Claude Code, OpenCode, and Codex CLI/Desktop** only. Do NOT add WakaTime compatibility, T3 Code, Cursor, or extra harnesses until Step 6.
4. **Zero Mocking of Core Mechanics:** Never stub OS HID/window checks, harness parsers, SQLite WAL persistence, or hash-chain verification with `todo!()` or hardcoded `true` values on the host OS. Every step requires real unit + integration tests using realistic harness fixtures and red-team test cases.
5. **Single Source of Shared Types:** All protocol structs, canonical JSON serialization, and hash-chain math MUST live in `crates/traction-core` and be shared by both `traction-daemon` and `traction-server`.

## Hard Technical & Security Invariants
- **Zero LLM Trust Boundary:** Never create `.md` prompt instructions or MCP tools asking an LLM to self-report user activity. All tracking runs deterministically outside the model.
- **Zero Raw Content Persistence:** Raw prompt text, file contents, secrets, `.env` values, and raw tool `stdout`/`stderr` must NEVER enter `TractionEvent`, SQLite, or server payloads. Compute specificity metrics in memory and discard raw text immediately.
- **Claude Code Hooks:**
  - ALWAYS set `"onFailure": "continue"` and `"timeout": 1` on HTTP hooks (Claude Code hook `timeout` is in **seconds**) so a stopped daemon never blocks Claude Code. `onFailure` needs Claude Code ≥ v2.1.295; older versions are already non-blocking by default, so the installer warns but still installs.
  - Pass the daemon bearer token via `headers` with `$TRACTION_TOKEN` listed in `allowedEnvVars`; never write the raw token into `settings.json`.
  - **NEVER hook `WorktreeCreate`** (it replaces default Git worktree creation and breaks `--worktree`).
  - Handle `/stop-track` and `/resume-track` inside `UserPromptSubmit` ONLY for exact user commands using `{ "decision": "block", "reason": "..." }`—never block automated turns.
  - Always hook `StopFailure` and `SessionEnd` so open Agent Compute timers close cleanly on API errors.
- **OpenCode Plugin:** Register under the `"plugin"` key (OpenCode 1.x) or `"plugins"` key (newer majors) in `opencode.json`, or drop into `.opencode/plugins/` / `~/.config/opencode/plugins/`; support both. Real hook names: `event`, `chat.message`, `tool.execute.before`, `tool.execute.after`, `command.execute.before`, `permission.ask`, `experimental.chat.messages.transform`, `experimental.text.complete` (TUI commands arrive as events through `event`, not as hooks). Enforce a strict 15 ms fire-and-forget HTTP timeout. Fallback SQLite poller MUST use `mode=ro` + `PRAGMA query_only = ON` and run only while an `opencode` process is alive; cursor on `time_updated` (not `updated_at`).
- **Codex CLI & Desktop:** Tail append-only `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` by inode + byte offset (using `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` on Windows). **NEVER open, query, or lock `~/.codex/state_5.sqlite`** (or any other `~/.codex/*.sqlite`). Turn boundaries are `event_msg` with `payload.type` `task_started` / `task_complete` / `turn_aborted`; Desktop vs CLI is told apart by `session_meta.originator`. Do NOT install a Codex `notify` wrapper in v1 (it carries raw prompt text and may already be owned by another tool).
- **Git & Local SQLite Safety:** Never acquire `.git/index.lock` when resolving canonical repos (`--git-common-dir`). Enforce a single-instance OS daemon lock and funnel all local SQLite WAL writes through a single dedicated writer thread/channel.
- **Untrusted-Repo Git Hardening:** Treat every repo the daemon touches as hostile: its `.git/config` can name programs Git will run. Funnel every daemon `git` call through one wrapper that permits only an allowlist of read-only plumbing (`rev-parse`, `rev-list`, `config --get`; nothing that touches the index or worktree) and always passes `--no-optional-locks -c core.fsmonitor=false -c core.hooksPath=/dev/null`, with a strict timeout that kills the child on expiry. Skip any repo rooted at `$HOME` (dotfile repos). A red-team fixture test must prove a repo-local `core.fsmonitor` command never executes.
- **Project Tracking Scope:** Track every Project folder the user works in, via an AI Harness or by hand, whether or not it is a Git repo (see `docs/adr/0001`). Never watch or track system, home-root, cache, or dependency folders (`node_modules`, `target`, `.git`, OS dirs).
- **`FILE_MUTATION` Attribution:** File watching of known Projects is always on (ADR 0001); every mutation is recorded, and only its attribution varies. Attribute a mutation to a Harness only inside that Harness's window: the exact hook window (`TOOL_PRE`→`TOOL_POST`, open turn) when the Harness has hooks, else a short fallback window around log-tailed activity, defined as one documented constant. Outside any window it is manual work. Attribution is live-only: mutations that happened while the daemon was down are never backfilled or attributed retroactively.
- **`TractionEvent` Schema Room:** Stay on `schema_version: 1` while leaving room for non-CLI sources: `cwd` is optional (for example, a browser tab has none) and every event carries an `attribution_source` drawn from a closed enum whose values cover today's sources and reserve future browser/editor Adapters, so adding those needs no schema migration. The full Open Ingestion Spec stays in Step 6d.
- **Local Ingestion Bus Hardening:** Besides the bearer token, reject any request carrying a foreign `Origin` (browser CSRF) and, on loopback HTTP, any request whose `Host` is not `127.0.0.1:41937`/`localhost:41937` (DNS rebinding). Unix-socket requests skip only the `Host` check (no TCP origin; socket is `0600`; see `docs/adr/0003-uds-skips-host-check.md`).
- **Offline & Local Models:** Offline work and local-model Harnesses (e.g. `ollama`) must track fully and buffer locally; only cloud-only Harnesses claiming offline hours are anomalous.
- **OS Permissions & Low-Spec Hygiene:** Never require admin/UAC privileges for user-space auto-start. Never trigger macOS Screen Recording prompts during Step 1 passive install (prompt only when visual proof mode is explicitly enabled in Step 3). Degrade gracefully on Linux Wayland without panicking.
- **Jev Integration (Step 4):** Use `POST https://api.typesafe.ai/v1/systemone` pinned to `jev-1.14.0`, always include an `"other"` choice, inspect full `score` probability distributions, transcode WebP frames to 8-bit baseline JPEG (<=1568px) server-side before `TypeSafe-Danger-Zone-Images: true`, keep all rubrics/thresholds and a hard monthly Jev spend limit in server config (falling back to `HeuristicEngine` when reached), and never auto-ban.

## Commands & Verification
Run before finishing any task or Exit Gate:
- Rust format & lint: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings`
- Rust tests: `cargo test --workspace`
- OpenCode plugin (`packages/opencode-plugin`): `pnpm test && pnpm typecheck` (once initialized in Step 1)