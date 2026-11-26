# 01: `traction-core` chain: domain-separated hashing, Ed25519 Chain Links and a pure multi-chain verifier

**What to build:** A new `chain` module in `traction-core` (no I/O, no clock, no randomness, no OS dependency) that defines the Chain Link format, seals a Link Payload into a signed, hash-linked Chain Link from a 32-byte seed, and verifies one or more chains plus the rows they cover, returning typed, deterministic errors. The daemon (02) seals with it, the CLI and the Step 4 server verify with it unchanged. See spec "Slice 1: `traction-core` pure crypto and verifier" and "Resolved Decisions" 2 and 3.

**Blocked by:** none

**Status:** resolved

Acceptance:

- [x] `ed25519-dalek = { version = "3.0.0", default-features = false, features = ["zeroize"] }` and `zeroize` are added to `[workspace.dependencies]` and to `traction-core`. The `fast` and `rand_core` features stay off. A `workspace_guards.rs` test parses `Cargo.lock` and fails if `sha2`, `digest`, `rand_core` or `curve25519-dalek` appears in more than one version.
- [x] The domain tags from the spec table (`TAG_KEY_ID`, `TAG_ROWS`, `TAG_DEVICE_ROWS`, `TAG_PAYLOAD`, `TAG_LINK`, `TAG_SIG_LINK`, `TAG_MANIFEST`, `TAG_SIG_MANIFEST`, reserved `TAG_FRAMES`) are `pub const` byte strings, each `traction/v1/<name>\0`. A test asserts they are pairwise distinct and none is a prefix of another, and that the same input hashed under two tags gives different digests.
- [x] Every digest is `SHA-256(tag || canonical_json(value))` through the existing `canonical_json`; hashes and signatures are lowercase hex in JSON.
- [x] Types (`Serialize`/`Deserialize`, `#[serde(deny_unknown_fields)]`, no floats): `KeyId` (`"ed25519:" || hex(SHA-256(TAG_KEY_ID || pk)[..16])`), `ProjectWindowRows` (`project_id`, sorted `work_buckets`, sorted/deduped `commit_oids`, oids must be 40 or 64 lowercase hex), `LinkPayload` (`window_start_ms`, `ruleset_id`, `device_rows_digest`, `projects` sorted and unique by `project_id`, `frame_digest`, `supersedes: Option<{key_id, seq, entry_hash}>`), `LinkHeader` (`schema_version: 1`, `seq`, `prev_hash`, `payload_digest`, `key_id`, `ruleset_id`, `window_start_ms`, `sealed_at_ms`, `clock_regressed`), `ChainLink` (`header`, `payload`, `entry_hash`, `signature`). Feeding a payload's canonical bytes where a header is expected fails to deserialize.
- [x] `FRAME_DIGEST_NONE` (64 `'0'` hex) and `GENESIS_PREV_HASH` (64 `'0'` hex) constants. Until 3b, every payload builder sets `frame_digest = FRAME_DIGEST_NONE`.
- [x] `DeviceSigningKey` is built only from `Zeroizing<[u8; 32]>`, wraps `ed25519_dalek::SigningKey` (`ZeroizeOnDrop`) and is not `Debug`, `Display`, `Clone` or `Serialize`. `DeviceSigningKey::seal(&self, prev: Option<&LinkHeader>, payload, sealed_at_ms) -> Result<ChainLink, SealError>` derives `seq` (0 or prev+1), `prev_hash` (genesis sentinel or prev `entry_hash`), `key_id` and `clock_regressed` (`sealed_at_ms < prev.sealed_at_ms`) itself. It refuses a `window_start_ms` not after the previous one (`SealError::WindowOrder`), a `supersedes` on a non-genesis link, and a `prev` signed by another key.
- [x] `VerifyingKeyHex` parses 32-byte hex, rejects `VerifyingKey::from_bytes` failures and `is_weak()` keys (`ChainError::WeakKey`).
- [x] Signatures are verified with `VerifyingKey::verify_strict` only (a source grep test fails on any non-strict `verify(` call in `chain`). A test proves a malleated signature (`S + ℓ`) and a small-order public key both fail.
- [x] `verify_chain` runs the per-link checks in the spec's fixed order (schema → position → key → payload/rows → entry hash → signature → prev hash → window order) and returns the first failure in chain order. `verify_chains(chains: &[KeyedChain], rows, policy)` (each chain paired with its key) adds `ChainOverlap`, `DuplicateChain`, `SupersedesMismatch`, unlinked-successor reporting, pinned-key checks per chain, cross-chain `UnanchoredRow` and `expected_tip` against the last chain.
- [x] `ChainError` is a closed `#[non_exhaustive]` enum with a stable `code()` per variant: `unsupported_schema`, `truncated_genesis`, `duplicate_seq`, `seq_gap`, `unknown_key`, `weak_key`, `payload_hash_mismatch`, `entry_hash_mismatch`, `invalid_signature`, `broken_prev_hash`, `window_order`, `unanchored_row`, `tip_mismatch`, `chain_overlap`, `duplicate_chain`, `supersedes_mismatch`, `manifest_signature_invalid`, `manifest_chains_mismatch`. Variants carry seqs, key IDs, window starts and Project IDs only, never row values.
- [x] `ChainReport` holds `chains: Vec<ChainSummary>`, `key_discontinuities: Vec<KeyDiscontinuity { from_key_id, to_key_id, at_window_ms, linked }>`, `key_pinned`, `clock_regressions`, `pending_rows_after_tip`, `legacy_rows_before_genesis`. There is no API returning one combined tip for several chains. A test with two chains asserts `chains.len() == 2` and one discontinuity; with one chain, an empty `key_discontinuities`.
- [x] `RowSet` with `RowScope::AllProjects | RowScope::Project(id)`: a Project in scope with rows in a window but no payload entry (or an entry but no rows) fails `PayloadHashMismatch { part: Project(id) }`; out-of-scope Projects are checked only through `payload_digest`.
- [x] Manifest helpers: `manifest_digest`, `sign_manifest`, `verify_manifest` under `TAG_MANIFEST` / `TAG_SIG_MANIFEST`.
- [x] Golden vectors (`tests/chain_vectors.rs`): seed `[7u8; 32]`, a fixed two-Project, two-window payload; committed hex for each `rows_digest`, `device_rows_digest`, `payload_digest`, `key_id`, `entry_hash` and signature, plus a successor chain from seed `[9u8; 32]` with `supersedes`. Identical on Windows, macOS and Linux CI. 3-OS CI green on `89fb8c0`: https://github.com/rubixnet/bounty/actions/runs/45836230330.
- [x] Unit tests on hand-built chains hit every `ChainError` variant at least once.
- [x] Seeded `proptest`: for any valid chain (1–3 chains, 1–12 links) and any single-byte change to any header field, payload field, signature or covered row value, `verify_chains` returns `Err` (never panics), and the same `Err` on a second run.
- [x] `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace` green; `traction-core` still has no OS, I/O or async dependency.

## Comments

Review fixes (step-3-review-fixes):

- `verify_chains` now takes `&[KeyedChain]` (a chain paired with its key), so a chain/key count mismatch is unrepresentable and no longer reports a misleading `WeakKey`; the closed `ChainError` list is unchanged. `StoredChains` and the bundle verifier use it.
- `sign_tagged` / `verify_tagged` share one `tagged_input` helper for `tag || message`.
- Golden-vector box ticked: 3-OS CI green on `89fb8c0`: https://github.com/rubixnet/bounty/actions/runs/45836230330.
