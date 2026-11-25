# 01: Claude Code fixture capture script + clean, version-tagged fixtures

**What to build:** A repeatable capture script, run locally and never in CI, that drives the real `claude -p` CLI with synthetic prompts on a throwaway Git repo. A capturing HTTP listener records every hook payload, and the script then copies each session transcript. The result is clean, version-tagged fixtures committed under the daemon's test data, which tickets 03 and 07 build their tests on. The script must never hang and never stall the ticket when nested `claude -p` calls cannot run (for example inside a headless subagent worktree with no login, no TTY or no network). See spec "Fixtures" and "Pause: verification task".

**Blocked by:** None (can start immediately).

**Status:** ready-for-agent

### Capture script
- [ ] One script, with a short usage note in the fixture folder's README, captures every scenario: normal multi-tool session, tool failure, permission request, subagent, `cwd` change, interrupted turn, and API-error stop (simulated if it can't be triggered, and clearly marked as simulated).
- [ ] Each run uses a fresh temp Git repo, a temp `CLAUDE_CONFIG_DIR` holding only the capture hooks, and the Traction command files from the spec, so neither the developer's own settings nor their transcripts are ever read. Prompts are synthetic and checked into the script.
- [ ] The capturing listener binds an ephemeral loopback port, answers `200 {}` to every hook, and writes each payload unchanged to the scenario's fixture folder.
- [ ] Every scenario sets non-interactive flags and gets a hard wall-clock timeout that kills `claude` and its children. A failed or timed-out scenario is reported and skipped; the remaining scenarios still run.
- [ ] A **preflight** step runs before any scenario: `claude` on `PATH`, `claude --version` parseable, and one short authenticated `claude -p` call, all with a short timeout. On failure it exits non-zero within seconds with one clear reason (`claude-missing`, `not-logged-in`, `no-network`, `timeout`) and writes nothing to the fixtures folder.
- [ ] A `--check` mode validates the committed fixtures without calling `claude`, so CI and later agents can run it: every scenario is present, every hook payload parses, transcripts are valid JSONL, and the cleanliness rules below hold.

### Clean, version-tagged fixtures
- [ ] Each scenario folder holds a manifest with the Claude Code version, capture date, OS, scenario name, and whether it was simulated.
- [ ] A scrub pass replaces machine-specific values with stable placeholders: home dir, temp paths, username, hostname, and any auth or account identifiers. Session IDs and record UUIDs are kept, because dedup and the deterministic `event_id` depend on them. `--check` fails if any of these values are left in.
- [ ] Prompts and tool content are synthetic, with known canary strings that later canary tests can look for.

### Findings recorded (in the fixture README and the Comments below)
- [ ] Whether transcripts contain explicit turn-end and API-error records (this decides the tailer's `TURN_END` and `STOP_FAILURE` mapping in ticket 07).
- [ ] What text `UserPromptSubmit` receives for `/stop-track` when the command files are installed (the literal command or the expanded body), and whether a `{"decision":"block"}` answer stops the model call. Ticket 04 copies this finding into `docs/protocol.md`.

### Not stalling
- [ ] If preflight fails in the agent's environment, the agent does **not** retry in a loop, fabricate payloads, or hand-write fixtures. It commits the script, the README and `--check`, appends the preflight reason and the one exact command a human should run under `## Comments`, and sets `Status: ready-for-human`. Once the human run commits the fixtures and `--check` passes, the ticket is resolved.
- [ ] No CI job runs the capture. `--check` may run in CI.
