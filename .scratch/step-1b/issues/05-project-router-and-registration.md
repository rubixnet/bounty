# 05: Project Router, exclusions, user ignore file, sessions, `/v1/project` (routing gate)

**What to build:** Every accepted event passes through the Project Router before the writer: `cwd` (or the session's last `cwd`) resolves to a Project Root via Git root (worktrees collapsed), nearest marker, session start folder, or none; exclusions and the global ignore file apply; the registry (`project`, `session`) is written via the single writer; resolutions cached ~30 s; Adapter `project_id` replaced. `POST /v1/project` registers a path. Owns the routing Exit Gate item and the integration-level red-team test. See spec "Project Router (daemon)" (all bullets except Migration/Alias-aware queries/ID pinning), "Project registration endpoint", "Daemon config additions", and Seam 2 "Routing gate", "No index.lock", "Exclusions", "Red team", "Git timeout and missing Git", "Registration endpoint", "Bus policy" (routing parts).

**Blocked by:** 02, 03, 04

**Status:** resolved

- [x] Routing gate test: 3 Git repos (one with origin) + a worktree + 2 non-Git folders, interleaved events incl. `CWD_CHANGE`; correct IDs, worktree shares main ID, exactly 5 Projects registered.
- [x] No `.git/index.lock` ever appears (polled), `.git/index` bytes/mtime unchanged.
- [x] Exclusions: fake `$HOME`, OS folder → no `project_id`; `node_modules/<pkg>` → enclosing Project; dotfile repo at `$HOME` doesn't capture subfolders; global-ignore folder → `202`, nothing stored.
- [x] Red-team integration test through the daemon: hostile fsmonitor/hooks repo + planted bare repo; no sentinel, no `dev/null` on all OSes.
- [x] Missing `git` on `PATH` → repo folder routes as `dir:`, daemon healthy.
- [x] `/v1/project`: subfolder registers repo root (`200` + id); `$HOME`, OS folder, `node_modules`, user-excluded, missing path → `422`; auth/Host/Origin failures match `/v1/event`.
- [x] Routing never blocks the bus response; test `TestDaemon` helper extended with fake `home_dir`, fixture builder and poll-until helper.
