# 09: Step 1c Exit Gate: 3-OS CI and human sign-off

**What to build:** Evidence that the Step 1c items in the Step 1 Exit Gate pass: all automated tests green on the Windows, macOS and Linux CI matrix, docs consistent, plus the human's one-off local `claude -p` check. See spec "Gate items this sub-step owns" and "Gate evidence plan".

**Blocked by:** 02, 06, 08

**Status:** ready-for-agent

- [ ] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` pass locally, and the fixture `--check` from 01 passes.
- [ ] The 3-OS CI run is green, and its link is recorded under `## Comments`. No CI job calls the Anthropic API.
- [ ] `docs/protocol.md`, `GLOSSARY.md`, the roadmap and `docs/steps.md` match what shipped (route, cap, pause, dedup, `STALE_TURN_TIMEOUT`).
- [ ] The 1c gate items are ticked in the spec. The "resumes JSONL tailing" item in `docs/steps.md` stays unticked until 1d delivers Codex.
- [ ] A sign-off checklist for the human is written here: install, then run `claude -p` with the daemon stopped (Claude unaffected), then running (events appear in SQLite), then `/stop-track` (paused, nothing stored). Then set `Status: ready-for-human` and stop for sign-off.
