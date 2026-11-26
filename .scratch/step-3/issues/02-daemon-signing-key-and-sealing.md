# 02: `traction-daemon` Device Signing Key, `chain_link` storage and atomic sealing in the finalizer

**What to build:** The daemon creates and protects a per-device Ed25519 key with no keyring or GUI prompt, stores Chain Links in new append-only tables, and seals every finalized 120 s window in the **same SQLite transaction** that writes its Work Buckets, Activity Rollups and `rollup_cursor`, through the single writer. It survives crashes, restarts, wall-clock rollback and a lost key (successor chain) without forking or rewriting history. See spec "Slice 2: `traction-daemon` key lifecycle, storage and sealing in the finalizer" and "Resolved Decision" 3.

**Blocked by:** 01

**Status:** resolved

Acceptance:

- [x] New `signing_key.rs` (following `secret.rs`): `<data dir>/signing.key` holds the 32 raw seed bytes from `getrandom::fill` straight into `Zeroizing<[u8; 32]>`, written with `fsutil::write_atomically`; `<data dir>/signing.pub` holds the public key as lowercase hex + newline. Every intermediate buffer is `Zeroizing`. The key is never logged, formatted or sent anywhere, and never uses the macOS Keychain or the Linux Secret Service.
- [x] **Unix:** `signing.key` is created `0600`. A load that finds group/other bits set tightens it to `0600` and logs one warning. Test: create, check mode; `chmod 0644`, restart, mode back to `0600`.
- [x] **Windows:** `signing.key` is created with a protected DACL holding exactly one ACE (full control, current user's SID), no inherited ACEs (`windows-sys` gains `Win32_Security`, `Win32_Security_Authorization`). Test reads it back with `GetNamedSecurityInfoW` and asserts that one ACE. 3-OS CI green on `89fb8c0`: https://github.com/rubixnet/bounty/actions/runs/45836230330. (`signing_key.rs::the_key_file_has_one_protected_owner_ace` ran on Windows.)
- [x] A **malformed** `signing.key` (wrong length, weak public key) fails startup with a one-line error and is left untouched (test).
- [x] A **missing** `signing.key` with existing `chain_link` rows starts a successor chain: new key and `key_id`, genesis `seq 0`, `supersedes` = the latest link of any other key. A loud `log::warn` fires at start and on the first seal. The old links are untouched. Test: seal, delete the key, restart, seal; `verify_chains` over the DB gives 2 chains and one `linked: true` discontinuity.
- [x] Migration 10 (appended to `MIGRATIONS`, no shipped migration edited) creates `chain_key` and `chain_link` exactly as in the spec (`PRIMARY KEY (key_id, seq)`, `UNIQUE (window_start_ms, ruleset_id)`, `entry_hash UNIQUE`, `STRICT`), with no-update/no-delete triggers on both. The daemon inserts its key's `chain_key` row (`INSERT OR IGNORE`) at start. `tests/schema.rs` / `tests/migration.rs` cover the new version.
- [x] `storage::load_sealed_rows(conn, from, to) -> RowSet` and `storage::load_chains(conn)` map rows back to `traction-core` types. A round-trip test proves `row → WorkBucket / ActivityRollup → canonical JSON` equals the engine struct's canonical JSON for every column (including `category_share_milli`).
- [x] The writer thread is the only holder of the `DeviceSigningKey` (moved in at start). `insert_rollup` seals inside its existing transaction: for each aligned 120 s window of the batch, in ascending order, that holds any Work Bucket, Activity Rollup or Commit Observation (read in-tx), it builds the Link Payload from the **engine's in-memory rows**, reads the current key's tip, calls `seal`, and inserts the `chain_link` before advancing `rollup_cursor`. Empty windows get no link. Any error rolls back rows, links and cursor together.
- [x] A seal that would break `window_start_ms` order is refused (that window's rows and link dropped, logged once), never forced.
- [x] Fault injection: test-only `TRACTION_TEST_FAIL_SEAL_AFTER=<n>` makes the n-th link insert of a batch error. Test: the batch rolls back entirely (no row, link or cursor change), the next pass seals it, and `verify_chains` passes with contiguous `seq`.
- [x] `tests/chain_restart.rs`: a real daemon process is killed (`kill -9` on Unix, `TerminateProcess` on Windows) after some windows are sealed, then restarted; asserts every finalized window is sealed exactly once, `seq` is contiguous, there is one chain, and verification passes. Waits use `poll_until`, never `sleep` assertions.
- [x] Wall-clock rollback through the finalizer's injected `now_ms` (`finalize_once`-style entry point): the clock goes 10 minutes back, then forward. No window is re-finalized, the cursor never moves back, `sealed_at_ms` is stored unclamped, the first link sealed with a lower `sealed_at_ms` than the tip has `clock_regressed = true`, and the chain verifies with `clock_regressions == 1`.
- [x] `INSERT OR IGNORE` conflict: a test pre-inserts a different `work_bucket` row for a minute about to be finalized; after sealing, `verify_chains` reports `PayloadHashMismatch` for that window (the conflict is detected, never silently re-sealed).
- [x] No new thread, no new wakeup; sealing adds ~1 signature per sealed window.
- [x] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` green locally (macOS); 3-OS CI green on `89fb8c0`: https://github.com/rubixnet/bounty/actions/runs/45836230330.

## Comments

Review fixes (step-3-review-fixes):

- `chain_restart.rs` gained `a_seal_that_fails_mid_batch_leaves_nothing_unsealed_across_a_kill_and_restart`: a child daemon with `TRACTION_TEST_FAIL_SEAL_AFTER=2` fails a sealing transaction mid-batch, is killed and restarted; no link, rollup or cursor row survives the failure, then one gap-free chain verifies.
- `chain_successor.rs` now reads the child's stderr and asserts the successor-chain warning at start and on first seal (once each, none on a later pass).
- Refused seals: only the windows at or before the newest sealed window are dropped; newer windows in the same batch are sealed, and the stored `rollup_cursor` advances past the batch even when nothing is left, so restarts don't retry it (`chain_successor.rs`: `a_batch_straddling_the_tip_…`, `a_batch_wholly_behind_the_tip_…`, `windows_after_an_all_old_batch_are_sealed`, `a_new_key_with_a_lost_cursor_…`; `finalizer.rs::a_lost_cursor_never_counts_sealed_minutes_into_the_baseline_twice`). This closes the earlier open design question.
- `signing_key.rs` has one `parse_seed` helper; `ProjectRows` replaces a tuple in `seal_batch`; the commit time uses `u64_at` (a negative value errors); the Windows `owner_only.rs` dead code is gone.
- Windows ACL box and 3-OS CI box ticked: 3-OS CI green on `89fb8c0`: https://github.com/rubixnet/bounty/actions/runs/45836230330.
