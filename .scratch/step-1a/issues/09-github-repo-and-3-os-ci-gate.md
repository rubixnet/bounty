# 09: Create the private GitHub repo and confirm CI passes on all 3 OSes

**What to build:** The 1a gate items are confirmed on real CI on Windows, macOS and Linux. This step creates and pushes to a GitHub repo, which can't be undone, so it is **deferred until the human explicitly asks for it**. The human is working locally first. Do not start this ticket from the frontier on your own.

**Blocked by:** 01, 02, 03, 04, 05, 06, 07, 08

**Status:** ready-for-human

- [ ] The human explicitly asks for the repo to be created, and confirms the repo name and owner.
- [ ] The repo is created private with `gh repo create --private`, and the local branch is pushed.
- [ ] The CI workflow from ticket 01 passes on `ubuntu-latest`, `macos-latest` and `windows-latest`.
- [ ] The two 1a gate items are ticked in `.scratch/step-1a/spec.md` only after all 3 OSes pass:
  - the Local Ingestion Bus rejects bad tokens, `Host` and `Origin`;
  - no raw text is stored in SQLite.
