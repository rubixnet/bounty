# 03: Frame-less Audit Bundle export, offline `verify` CLI, tamper red-team suite, canary extension and docs

**What to build:** `traction-daemon export-bundle` writes one Project's frame-less Audit Bundle (canonical JSON, every device chain, signed manifest). `traction-daemon verify` checks a bundle or a local data folder offline with the same `traction-core` verifier and always surfaces key discontinuity. A red-team suite tampers with SQLite and bundles and asserts the exact error for each attack on all 3 OSes. The zero-raw-text canary covers the new tables, key files and bundle. Docs are updated, closing the Step 3a gate. See spec "Slice 3", the tamper table and "Resolved Decisions" 1–3.

**Blocked by:** 01, 02

**Status:** resolved

Acceptance:

- [x] `traction-daemon export-bundle --project <project_id> --out <file> [--data-dir <path>]` opens SQLite `mode=ro` + `PRAGMA query_only = ON` (safe while the daemon runs; a test exports during live sealing with no `SQLITE_BUSY` for the writer) and writes one canonical JSON document: `manifest` (body + signature), `chains` (each `{public_key, links}`, every chain the device holds, in order), `project_rows` (this Project's Work Buckets and per-window commit oids only), `device_rows` (Activity Rollups), `frames: []`. Rows before the first genesis window are not exported. An unknown Project ID is an error, exit 1.
- [x] Manifest body exactly as in the spec (`bundle_schema: 1`, `project_id`, `linked_repo: null`, current `key_id`/`public_key`, `chain_count`, `key_ids`, `first_window_ms`, `tip {key_id, seq, entry_hash}`, `link_count`, `bucket_count`, `ruleset_ids`, `exported_at_ms`, `chains_digest`), signed with the current Device Signing Key via `sign_manifest`.
- [x] `traction-daemon verify <bundle> [--trusted-key <hex>]... [--expect-tip <key_id>:<seq>:<entry_hash>]` checks the manifest signature, `chain_count`/`key_ids` against `chains`, then `verify_chains` with `RowScope::Project` and the manifest tip. `traction-daemon verify --data-dir <path>` checks local SQLite (`mode=ro`) with `RowScope::AllProjects`. No daemon and no network needed.
- [x] Output lines and exit codes exactly as in the spec: `OK chains=… links=… tip=… pinned=… clock_regressions=… pending=… legacy=…`; one `WARN key_discontinuity from=… to=… at_window=… linked=…` per discontinuity; `FAIL <code> key=… seq=…`. Exit **0** = one chain verified, **2** = verified with more than one chain, **1** = failure or unreadable input. Without `--trusted-key` a line states the keys are self-asserted. CLI tests pin each format.
- [x] `crates/traction-daemon/tests/tamper_red_team.rs` implements all 24 rows of the spec's tamper table on a real chain built through the real writer path (injected clock, ≥ 6 sealed windows, 2 Projects). SQLite attacks drop the needed trigger inside the test only. Each test asserts both the `ChainError` variant and its `code()` (and, for CLI-level rows, the exit code). Rows 13 and 18 also assert the documented limit (passes unpinned / without `--expect-tip`).
- [x] `derived_tables_no_raw_text.rs` is extended: after the canary prompt, file name, file body and commit message flow through a real daemon and the finalizer, it seals, exports a bundle for the canary Project and asserts none of the canaries appears in any column of `chain_link`/`chain_key`, in `signing.pub`, or in any byte of the bundle. The bundle also contains neither the Project Root path nor the data folder path.
- [x] The `.wakatime` source-grep guard is confirmed to scan the new modules (no new hit).
- [x] `docs/protocol.md`: §4 rewritten (domain tags, payload/header/link formulas, genesis sentinel, `verify_strict`, check order and error codes, `clock_regressed`, successor chains with `supersedes` and exit code 2, honest limits: tamper-evident since sealing only, tail truncation needs an outside tip, trust on first use without a pinned key; old `H_n` formula replaced, `frame_hash_n` → reserved `frame_digest`, `git_diff_hash_n` → commit oids in `rows_digest`). §2c "Tables" lists `chain_key` and `chain_link` as append-only and restates that only `struggle_baseline` and `rollup_cursor` are mutable; a "Device Signing Key" paragraph covers the two key files, `0600` / owner-only DACL, no keyring. §5 documents the frame-less bundle and how 3c wraps it. A clearly headed paragraph documents the **`git:` ID disclosure** trade-off of device-wide bundles. §3: `chain_tip_hex` is the tip link's `entry_hash`.
- [x] `GLOSSARY.md` gains **Chain Link**, **Seal**, **Device Signing Key**, **Sealed Window**, **Chain Tip** (and **Successor Chain**); **Anchor** notes it carries a Chain Link's `entry_hash`.
- [x] `docs/steps.md` Step 3 Build 2 gets the domain-separated formula in place of the bare `H_n` line, and the Step 3 Exit Gate notes that items 2 and 3 are deferred to 3b/3c after Step 4 (`docs/roadmap.md` Step 3 table says the same).
- [x] The spec's gate checkboxes are ticked with the test names that prove them (as in `.scratch/step-2/spec.md`).
- [x] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` green locally (macOS). 3-OS CI green on `89fb8c0`: https://github.com/rubixnet/bounty/actions/runs/45836230330. The Step 3a sign-off is **left for the human**.

## Comments

Notes (implementer):

- `traction-core` gained `chain::bundle` (`build_bundle`, `verify_bundle`, `AuditBundle`, `BundleError`, `BundlePolicy`) so the server reuses the exact verifier. The daemon's `audit.rs` is only I/O and output lines.
- Verifier change for tamper row 23: a first chain whose genesis names a `supersedes` is a successor shown without its predecessor; rows before it are `UnanchoredRow`, and with those removed `SupersedesMismatch`.
- Bundle check order: manifest signature, chain_count/key_ids, chain checks with the manifest tip, then counts/digest/tip key, then (when pinned) the manifest signer, then `--expect-tip`. This is what makes rows 11 and 17 return the spec's errors.
- `FAIL` lines print `key=-`/`seq=-` where a field does not apply; `part=`/`window=` follow for `payload_hash_mismatch`/`unanchored_row`.

Review fixes (step-3-review-fixes):

- Row 18 now also asserts the `ChainError::TipMismatch` variant and `code()` through `verify_database_with` (now exported `#[doc(hidden)]`), not just the CLI prefix.
- 3-OS CI box ticked: 3-OS CI green on `89fb8c0`: https://github.com/rubixnet/bounty/actions/runs/45836230330.
