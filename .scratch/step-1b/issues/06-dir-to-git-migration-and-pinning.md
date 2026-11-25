# 06: Alias-only `dir:` → `git:` migration, alias-aware queries, ID pinning

**What to build:** When a registered `dir:` Project Root becomes a Git repo with a commit, the writer, in one transaction, registers the `git:` Project, records `project_alias`, and marks the `dir:` Project merged — never touching stored events. The Router resolves IDs through aliases before insert (late old-ID events land under `git:`). One alias-aware query helper/view is the only way to read a Project's events. `git:` IDs are pinned per canonical common dir. See spec "ID pinning", "Migration (alias-only…)", "Alias-aware queries", Seam 2 "Migration" and "ID pinning".

**Blocked by:** 05

**Status:** resolved

- [x] Migration test: pre-migration rows byte-for-byte unchanged; alias row exists; post-migration rows (incl. late old-`dir:` event) under `git:`; alias-aware view returns both sets; all survives a restart.
- [x] Idempotent replay; alias chains resolved to final ID; cycles refused.
- [x] `.git` appearing at a known `dir:` root invalidates the cache immediately.
- [x] Adding `origin` after first routing doesn't change the `git:` ID.
