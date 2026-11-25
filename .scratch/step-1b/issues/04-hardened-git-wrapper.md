# 04: Hardened Git wrapper + AGENTS.md invariant update

**What to build:** One daemon module, the only place `std::process::Command` runs `git`, exposing a closed set of typed queries (`CommonDir`, `TopLevel`, `RootCommits`, `OriginUrl`) with the fixed hardened argv/env, per-OS `core.hooksPath` null device (`/dev/null` / `NUL` via `cfg(windows)`), 5 s kill-and-reap timeout, stdout cap, stderr discarded, `$HOME` refusal and "git missing → unavailable, logged once". Update the AGENTS.md "Untrusted-Repo Git Hardening" invariant to name the per-OS null device and `safe.bareRepository=explicit`. See spec "Hardened Git wrapper" and "Narrow internal test, allowed by exception", plus Further Notes "AGENTS.md wording".

**Blocked by:** None (can start immediately)

**Status:** resolved

- [x] Unit tests assert the exact argv and env for each query (incl. `safe.bareRepository=explicit` and per-OS hooks path).
- [x] A real child hanging past the timeout is killed and reaped (test may use a test-only timeout override; production value is one public constant = 5 s).
- [x] Wrapper-level red-team test: a real repo with hostile `core.fsmonitor`, `core.hooksPath` and `.git/hooks/*` (sh on Unix, `.bat`/`.cmd` on Windows) — running every query never creates a sentinel, and no `dev/null` path appears.
- [x] A bare repo with hostile config, reached by `-C` inside it, is refused (treated as not-a-repo).
- [x] `$HOME`-rooted repo refused; missing `git` binary returns unavailable without panicking.
- [x] AGENTS.md invariant wording updated.
