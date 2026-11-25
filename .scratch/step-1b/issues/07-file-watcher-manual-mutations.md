# 07: File watcher: ignore rules, coalescing, startup re-attach, live-only `FILE_MUTATION` (hand-edit gate)

**What to build:** The daemon watches every non-merged, non-excluded Project (on registration and re-attached from SQLite on startup) with `notify` OS backends (recursive on macOS/Windows, per-folder inotify on Linux with graceful limit handling), filters dependency folders, `.git`, `.gitignore`/`.tractionignore` and user exclusions, holds/coalesces mutations for `LOG_TAIL_HOLD`, and emits metadata-only `FILE_MUTATION` events (manual work in this ticket: no Harness, `network_class: manual`, `attribution_source: file_watcher`) to the writer via the internal validated path. Owns the hand-edit Exit Gate item. See spec "File watcher (daemon)", Seam 2 "Hand edits", "Watcher persistence", "No raw text".

**Blocked by:** 05

**Status:** resolved

- [x] Hand edits in a Project registered via `/v1/project` produce rows with correct `mutation_kind` (created/modified/removed), no Harness/session, `manual`.
- [x] `.gitignore`d, `.tractionignore`d, `node_modules`, `.git` edits produce no rows; a burst of saves → one row; created-then-removed → no row.
- [x] Restart with 2 registered Projects (one via routed event, one via endpoint) → edits in both produce rows; a deleted root doesn't stop startup; edits during downtime produce no rows.
- [x] Sentinel scan: file name and contents with a sentinel never reach any table, DB or WAL file.
- [x] Graceful shutdown flushes pending mutations; no polling loop at idle; per-Project pending cap.
- [x] Hold is a single public constant with a test-only config override.
