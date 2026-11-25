# 09: Create the private GitHub repo and confirm CI passes on all 3 OSes

**What to build:** The 1a gate items are confirmed on real CI on Windows, macOS and Linux. This step creates and pushes to a GitHub repo, which can't be undone, so it is **deferred until the human explicitly asks for it**. The human is working locally first. Do not start this ticket from the frontier on your own.

**Blocked by:** 01, 02, 03, 04, 05, 06, 07, 08

**Status:** resolved

- [x] The human explicitly asks for the repo to be created, and confirms the repo name and owner.
- [x] ~~The repo is created private with `gh repo create --private`~~ The human created the repo public (now `rubixnet/bounty`, mirrored to `Gitstar-OC/Bounty`), and the local branch is pushed.
- [x] The CI workflow from ticket 01 passes on `ubuntu-latest`, `macos-latest` and `windows-latest`.
- [x] The two 1a gate items are ticked in `.scratch/step-1a/spec.md` only after all 3 OSes pass:
  - the Local Ingestion Bus rejects bad tokens, `Host` and `Origin`;
  - no raw text is stored in SQLite.

## Comments

- 2026-11-25: The human created the repo themselves as **public** at https://github.com/Gitstar-OC/Bounty, overriding the "private, via `gh repo create`" plan. The remote's GitHub-generated initial `LICENSE` commit (095f4da) was merged into `main` with `--allow-unrelated-histories`. The integration branch keeps the gnu.org AGPLv3 text; the two differ only in line wrapping. `main` and `step-1a-foundation` are pushed.
- 2026-11-25: CI run 45569559809 did not start on any OS: "The job was not started because your account is locked due to a billing issue." Blocked on the GitHub account's billing. Re-run once it is unlocked; the Exit Gate items stay unticked until all 3 OSes pass.
- 2026-11-25: CI moved to **https://github.com/rubixnet/bounty** (public), now the active CI repo and `gh` default (`origin`). https://github.com/Gitstar-OC/Bounty stays as a synced mirror (`gitstar` remote). `main` and `step-1a-foundation` are pushed to both; the gate is verified with `gh run ... -R rubixnet/bounty`.
- 2026-11-25: Resolved. CI run 45572638345 on `rubixnet/bounty` (commit e86bf94) passed `cargo fmt --check`, `cargo clippy -D warnings` and `cargo test --workspace` on `ubuntu-latest`, `macos-latest` and `windows-latest`. The Windows run executed every HTTP auth-gate test (bad/missing token, foreign `Host`, foreign `Origin`) and `raw_text_sent_through_unknown_fields_never_reaches_disk`. Only the 19 Unix-only tests (UDS, signals, `0600` modes, `XDG_RUNTIME_DIR`) are compiled out there. Both 1a gate items are ticked in `spec.md`.
