# 03: SQLite migration 2 (append-only), device secret, bus policy for reserved/internal sources

**What to build:** Migration 2 rebuilds `traction_event` with nullable harness/session/cwd/project_id, non-null `attribution_source`, nullable `mutation_kind`, creates `project`, `project_alias`, `session` tables + indexes, and adds abort triggers making events append-only. The daemon creates a per-device secret file (separate from the bus token). The bus rejects externally sent `file_watcher`, `browser_extension`, `editor_extension`, `manual_pick` and any `FILE_MUTATION` with `400` + distinct reason codes. See spec "SQLite", "Device secret", "Local Ingestion Bus policy changes" (first two bullets), and Seam 2 "Append-only events" / "Bus policy".

**Blocked by:** 01

**Status:** resolved

- [x] Migration 2 appended (migration 1 untouched); existing rows backfilled `harness_hook`; a 1a-era database upgrades cleanly.
- [x] `UPDATE`/`DELETE` on `traction_event` via a separate RW connection after shutdown is aborted.
- [x] Device secret: 32 CSPRNG bytes in the data folder, atomic `0600` on Unix, reused across restarts, never logged.
- [x] Bus policy 400s with reason codes for the 4 sources and external `FILE_MUTATION`; 1a tests still green.
