# 01: Core schema room on `schema_version: 1` + protocol.md update

**What to build:** `traction-core` accepts events with optional `cwd`/`project_id`, a required closed `attribution_source`, optional `harness_id`/`session_id` (manual `FILE_MUTATION`), and `metrics_only.mutation_kind`, with value-free reason codes. `docs/protocol.md` sections 2/2a document all of it in the same change. See spec "`traction-core` schema changes", "Seam 1" schema-room bullets, and "Further Notes → Protocol update" in `.scratch/step-1b/spec.md`.

**Blocked by:** None (can start immediately)

**Status:** ready-for-agent

- [ ] `cwd` and `project_id` optional; when present, 1a rules / `git:|dir:<64 hex>` still enforced.
- [ ] `attribution_source` closed enum (8 values), required; every value round-trips; unknown spelling rejected.
- [ ] Harness/session rules, `network_class: manual` rule, `file_watcher`⇔`FILE_MUTATION` rule, `mutation_kind` required on/only on `FILE_MUTATION`, each rejected with its own reason code.
- [ ] Canonical JSON golden bytes updated (1a example + `attribution_source`).
- [ ] Existing daemon code/tests compile and pass with the new required field (fixtures updated minimally).
- [ ] `docs/protocol.md` §2/2a updated: optional fields, `attribution_source`, `mutation_kind`, `LOG_TAIL_HOLD`=5 s, `LOG_TAIL_TRAILING_WINDOW`=30 s, Git timeout 5 s, alias-only migration, `git:` ID pinning, `/v1/project`, derivation formulas (formulas may be finalized by 02).
