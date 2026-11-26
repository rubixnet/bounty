# Step 3a: Signed Hash-Chain, Local Ledger Sealing and Offline Verification

Status: ready-for-agent (approved by the human 2026-11-26; see "Resolved Decisions" at the end)

Source: `docs/steps.md` (Step 3, Build 2 and Exit Gate item 1), `docs/roadmap.md` (sub-step 3a, plus the bundle-export part of 3c), `docs/protocol.md` sections 2c, 3, 4 and 5, `GLOSSARY.md` ("Anchor", "Audit Bundle", "Activity Rollup", "Work Bucket"), and the AGENTS.md invariants "Zero Raw Content Persistence", "Single Source of Shared Types" (rule 5), "Zero Mocking of Core Mechanics" (rule 4) and "Git & Local SQLite Safety".

**One spec, 3 vertical slices.** `docs/roadmap.md` plans three sub-steps (3a hash-chain, 3b snapshot vault, 3c offline + review UI + bundle). At the human's request, this spec covers the **hash-chain half of Step 3** in 3 vertical slices, each sized for one fresh Sonnet session (~150k tokens). If a slice doesn't fit, split *that* slice, not the spec.

Gate items this spec owns:

- [x] Tampering with, deleting or inserting any past Work Bucket, Activity Rollup or Commit Observation in local SQLite fails `traction-core` chain verification with an exact, typed error. *(`docs/steps.md` Step 3 Exit Gate item 1, minus frames; `tamper_red_team.rs::row01_…` to `::row04_…`, `::row05_…`, `::row06_…`, `::row08_…`, `::row09_…`, `::row10_…`, `::row19_…`)*

Added at the human's request:

- [x] The same tamper suite against an exported Audit Bundle: a 1-bit flip, a deleted middle link, swapped rows, a rogue-key forgery, a grafted second chain and truncated history each fail with the expected error. *(Slice 3; `tamper_red_team.rs::row02_…`, `::row05_…`, `::row07_…`, `::row11_…`, `::row12_…`, `::row13_…`, `::row14_…`, `::row15_…`, `::row16_…`, `::row17_…`, `::row20_…`, all 24 rows green)*
- [x] Restart safety: killing the daemon at any point of a sealing transaction and restarting never leaves an unsealed finalized row, a `seq` gap or a fork. *(Slice 2 `tests/chain_restart.rs::a_killed_daemon_resumes_the_same_chain_without_forking_or_double_sealing`)*
- [x] Cross-OS determinism: fixed golden vectors (key seed, payload, digests, signature) give byte-identical hex on Windows, macOS and Linux. *(Slice 1 `traction-core/tests/chain_vectors.rs::golden_vectors_are_byte_identical_on_every_os`)* 3-OS CI green on `89fb8c0`: https://github.com/rubixnet/bounty/actions/runs/45836230330.
- [x] The zero-raw-text canary gate covers `chain_link`, the key files and the exported Audit Bundle. *(Slice 3; `derived_tables_no_raw_text.rs::canary_text_never_reaches_the_derived_tables`)*

- [x] All of the above pass on Windows, macOS and Linux in CI. 3-OS CI green on `89fb8c0`: https://github.com/rubixnet/bounty/actions/runs/45836230330.

- [x] Key discontinuity is never hidden: a chain restarted after a lost `signing.key` verifies as **two chains**, and both `verify_chain` and the `verify` CLI report it explicitly (`chains=2`, a `WARN key_discontinuity` line, exit code 2). *(Slices 1–3; `tamper_red_team.rs::row21_…`, `audit_cli.rs::a_key_discontinuity_is_exit_code_two_in_the_real_binary`, `chain_successor.rs`)*

### Step 3 scope deviation (recorded, like the 1d/1e deferral in Step 2)

On 2026-11-26 the human scoped this spec as **Step 3a** and deferred **3b** (snapshot vault) and **3c** (offline sync, Review & Redact UI, zstd `.traction-bundle`) until **after Step 4**. Step 3 Exit Gate items 2 (redacting 3 frames) and 3 (zero frames during sensitive focus) therefore stay open and must pass before Step 5. This spec reserves what they need, so 3b/3c add no chain format change: `frame_digest` is signed into every Link Payload as all zeros, and the Audit Bundle has an empty `frames` section.

## Terminology (read first)

| Request wording | Canonical term | Notes |
|---|---|---|
| Signed anchor / chain link | **Chain Link** (new) | The glossary's **Anchor** is the chain *tip sent to the server* every 2 minutes (Step 4). A Chain Link is the local, signed, hash-chained record of one sealed 120 s window. An Anchor in Step 4 carries a Chain Link's `entry_hash`. |
| Chain/sign a window | **Seal** (new, verb) | Seal = compute the window's payload, link it to the previous Chain Link, sign it, and commit it with the window's rows in one transaction. |
| Per-device Ed25519 keypair | **Device Signing Key** (new) | Different from the existing **device secret** (`device.secret`, the HMAC-style key for `dir:` Project IDs). Never the same bytes. |
| Proof bundle | **Audit Bundle** (frame-less until 3c) | The glossary says to avoid "export"/"proof pack". This spec's bundle is the Audit Bundle with an empty `frames` section. |
| `activity_rollup` anchors | Chain Link per **sealed window** | A window is sealed whether it holds an Activity Rollup, Work Buckets, Commit Observations or any mix (see Slice 2). |

New glossary terms that Slice 3 adds: **Chain Link**, **Seal**, **Device Signing Key**, **Sealed Window**, **Chain Tip**.

## Problem Statement

After Step 2, the daemon writes Work Buckets, Activity Rollups and Commit Observations into append-only SQLite tables. The triggers stop an honest bug from rewriting a row, but anyone with the file can drop a trigger and edit a duration, delete an awkward hour or paste in another machine's history. Reviewers (Step 5) and the server (Step 4) have no way to tell. That is exactly how Stardance-style inflation survives review.

The chain must make every change to sealed history detectable, offline, with code the server reuses unchanged. The edges:

- **Determinism.** The server (Step 4) recomputes digests on Linux. A daemon on Windows must produce byte-identical bytes, or honest users fail verification.
- **Cross-type collisions.** One SHA-256 over concatenated fields lets a payload be reinterpreted as a link header (or a bundle manifest). Each hashed object needs its own domain.
- **Crash safety.** A crash between "rows written" and "chain extended" leaves either unsealed rows (silent gap) or a link pointing at nothing. Two writers would fork the chain.
- **Keys.** The key must live on a headless CI runner or a 10-year-old laptop with no keyring prompt, and must never sit world-readable.
- **Clock.** The user can set the wall clock back. History must never be rewritten because of it, and the rollback must be visible.
- **Honest limits.** A local chain proves "unchanged since sealing", not "honest when sealed": the user owns the machine and the key. Tail truncation of a purely local chain is undetectable without an outside reference (an earlier export or the Step 4 Anchor). The spec says so instead of pretending otherwise.

From the user's point of view: "Once a window of my work is sealed, nobody (including me) can quietly change it, and anyone can check that offline with one command."

## Solution

### Slice 1: `traction-core` pure crypto and verifier

A new `chain` module in `traction-core` (no I/O, no clock, no randomness, no OS dependency), reused by the daemon now and by the server in Step 4.

**Dependencies.** `ed25519-dalek = { version = "3.0.0", default-features = false, features = ["zeroize"] }` in `[workspace.dependencies]`. It depends on `sha2 0.11`, `curve25519-dalek 5`, `rand_core 0.10` (optional, not enabled) and `zeroize 1.5`, so `cargo tree -d` shows no duplicate `sha2`/`digest`/`rand_core` trees (a `workspace_guards.rs` test asserts this by parsing `Cargo.lock`). `zeroize` is added as a direct workspace dependency for our own buffers. The `fast` feature (precomputed tables) stays off: signing ~1 link per 2 minutes doesn't need it, and it costs binary size on low-spec machines. `traction-core` never generates keys (that needs an OS RNG); it takes a 32-byte seed.

**Domain separation.** Every hash and signature input starts with a fixed ASCII tag ending in `0x00`. Tags are distinct and none is a prefix of another, so no byte string is valid in two domains:

| Constant | Tag bytes | Input after the tag |
|---|---|---|
| `TAG_KEY_ID` | `traction/v1/key-id\0` | the 32-byte Ed25519 public key |
| `TAG_ROWS` | `traction/v1/rows\0` | canonical JSON of one Project's rows in one window (below) |
| `TAG_DEVICE_ROWS` | `traction/v1/device-rows\0` | canonical JSON of the window's Activity Rollup, or `null` |
| `TAG_PAYLOAD` | `traction/v1/payload\0` | canonical JSON of the Link Payload |
| `TAG_LINK` | `traction/v1/link\0` | canonical JSON of the Link Header |
| `TAG_SIG_LINK` | `traction/v1/sig/link\0` | the 32-byte `entry_hash` (this is what Ed25519 signs) |
| `TAG_MANIFEST` | `traction/v1/manifest\0` | canonical JSON of the Audit Bundle manifest body |
| `TAG_SIG_MANIFEST` | `traction/v1/sig/manifest\0` | the 32-byte manifest digest |
| `TAG_FRAMES` | `traction/v1/frames\0` | reserved for 3b (`frame_digest`) |

All JSON goes through the existing `canonical_json` (sorted keys, no whitespace, no floats). Hashes are SHA-256, written as lowercase hex in JSON. Signatures are 64 bytes, lowercase hex.

**Types** (all `Serialize`/`Deserialize`, no float fields, `#[serde(deny_unknown_fields)]`):

- `KeyId`: `"ed25519:" || hex(SHA-256(TAG_KEY_ID || public_key)[..16])`. Derived, never chosen.
- `ProjectWindowRows { project_id, work_buckets: Vec<WorkBucket>, commit_oids: Vec<String> }`. Buckets sorted by `minute_start_ms`; commit oids are the Project's Commit Observations with `observed_ms` in the window, sorted and de-duplicated (lowercase 40/64-hex only; anything else is a type error). Commit oids replace protocol §4's `git_diff_hash`: they are what a Step 4 reviewer can cross-check against the Linked Repo, and reading them needs no diff.
- `LinkPayload { window_start_ms, ruleset_id, device_rows_digest, projects: Vec<{ project_id, rows_digest }>, frame_digest, supersedes: Option<{ key_id, seq, entry_hash }> }`. `projects` sorted by `project_id`, unique. `frame_digest` is 32 zero bytes (hex) until 3b fills it; it is already part of every signed payload, so 3b needs no format change. `supersedes` is `null` except on the genesis link of a **successor chain** (a chain started after the previous key was lost, Slice 2), where it names the previous chain's tip. The new key commits to the old tip, so the old chain's tail can't be cut after the restart without breaking the new chain. `supersedes` on a non-genesis link is a type error.
- `LinkHeader { schema_version: 1, seq: u64, prev_hash, payload_digest, key_id, ruleset_id, window_start_ms, sealed_at_ms, clock_regressed: bool }`. `prev_hash` is the **genesis sentinel** (64 `'0'` hex chars) exactly when `seq == 0`, and never otherwise. `key_id` and `ruleset_id` sit in every header so rotating either later is a new header value, not a new format.
- `ChainLink { header: LinkHeader, payload: LinkPayload, entry_hash, signature }`. `entry_hash = SHA-256(TAG_LINK || canonical_json(header))`; `signature = Ed25519(TAG_SIG_LINK || entry_hash)`.
- `DeviceSigningKey`: wraps `ed25519_dalek::SigningKey` (which is `ZeroizeOnDrop`). Built only from `Zeroizing<[u8; 32]>`. Not `Debug`, `Display`, `Clone`, `Serialize`. `fn seal(&self, prev: Option<&LinkHeader>, payload, sealed_at_ms) -> ChainLink` computes `seq`, `prev_hash` and `clock_regressed` (`sealed_at_ms < prev.sealed_at_ms`) itself, so callers can't build an unlinked header.
- `VerifyingKeyHex`: a public key that has passed `VerifyingKey::from_bytes` **and** `!is_weak()`.

**Verifier.** `fn verify_chain(links: &[ChainLink], rows: &RowSet, policy: &VerifyPolicy) -> Result<ChainReport, ChainError>`:

- `RowSet` holds the Work Buckets, Commit Observations and Activity Rollups the caller has (all of them for a local DB; one Project's for a bundle) plus which Projects it is complete for (`RowScope::AllProjects` or `RowScope::Project(id)`).
- `VerifyPolicy { trusted_keys: Option<Vec<VerifyingKeyHex>>, expected_tip: Option<(u64, Hash)> }`. With `trusted_keys: None`, the genesis link's key is the only trusted key (trust on first use, reported as `key_pinned: false`).
- Signatures are checked with `VerifyingKey::verify_strict` only (rejects small-order keys and non-canonical `S`). A test proves a malleated signature (`S + ℓ`) and a small-order public key both fail.
- Checks run per link in this fixed order, and the **first** failure in chain order is returned. That makes the result deterministic for any tamper:
  1. `schema_version == 1`, else `UnsupportedSchema { seq }`.
  2. Position: link 0 must have `seq == 0` and the genesis sentinel, else `TruncatedGenesis { first_seq }`. Link `i > 0` with `seq < i` is `DuplicateSeq { seq }`; with `seq > i` is `SeqGap { expected: i, found }`.
  3. `key_id` names a trusted key, else `UnknownKey { seq, key_id }`.
  4. `payload_digest == SHA-256(TAG_PAYLOAD || canonical(payload))`, the payload's `window_start_ms`/`ruleset_id` equal the header's, and every digest the `RowSet` covers recomputes (`device_rows_digest` always; `rows_digest` for each Project in scope; a Project in scope that has rows in the window but no entry, or an entry but no rows, also fails), else `PayloadHashMismatch { seq, part }` where `part` is `Payload | DeviceRows | Project(project_id)`.
  5. `entry_hash == SHA-256(TAG_LINK || canonical(header))`, else `EntryHashMismatch { seq }`.
  6. `verify_strict(TAG_SIG_LINK || entry_hash, signature)`, else `InvalidSignature { seq }`.
  7. `prev_hash == links[i-1].entry_hash` (for `i > 0`), else `BrokenPrevHash { seq }`.
  8. `window_start_ms > links[i-1].window_start_ms`, and aligned to `activity_rollup_ms`, else `WindowOrder { seq }`.
- After the walk: any row in scope whose window lies between the genesis window and the tip window but has no Chain Link is `UnanchoredRow { window_start_ms }` (an inserted window). With `expected_tip` set, a different last `(seq, entry_hash)` is `TipMismatch { expected_seq, found_seq }` (tail truncation or extension).
- `ChainError` is a closed `#[non_exhaustive]` enum with a stable `code()` string per variant (the `CanonicalJsonError` pattern). It carries seqs, window starts and Project IDs only, never row values.
- `ChainReport { chains: Vec<ChainSummary>, key_discontinuities: Vec<KeyDiscontinuity>, key_pinned, clock_regressions, pending_rows_after_tip, legacy_rows_before_genesis }`, with `ChainSummary { key_id, links, first_window_ms, tip: (seq, entry_hash) }`. Rows after the last tip are not yet sealed (reported, not an error). Rows before the first genesis window predate Step 3 (reported as legacy; they are not proof and Slice 3's export leaves them out).

**Multiple chains (key discontinuity).** A device has one chain per Device Signing Key it ever held. The top-level entry point is `fn verify_chains(chains: &[KeyedChain], rows, policy) -> Result<ChainReport, ChainError>`, where each `KeyedChain` pairs one key's links (in `seq` order) with that chain's self-asserted public key. `verify_chain` (one chain) is the building block. On top of the per-link checks above:

- Chains are ordered by their genesis `window_start_ms`. Each chain's first window must be after the previous chain's tip window, else `ChainOverlap { key_id }`. Two chains with the same `key_id` are `DuplicateChain { key_id }`.
- A successor chain's genesis `supersedes` must equal the previous chain's `(key_id, tip seq, tip entry_hash)`, else `SupersedesMismatch { key_id }` (the old chain's tail was cut or changed after the restart). A successor genesis with `supersedes: null` is **not** an error, because a key lost together with the database can't name a tip, but its `KeyDiscontinuity { from_key_id, to_key_id, at_window_ms, linked: false }` is reported as unlinked.
- With `policy.trusted_keys: None`, each chain's own key is trusted for that chain and the report says `key_pinned: false`. With pinned keys, every chain's key must be pinned, else `UnknownKey { seq: 0 }` on the first unpinned chain.
- `UnanchoredRow` and `expected_tip` apply across the union of chains: a row between the first genesis window and the last tip window that no chain covers is `UnanchoredRow`, and `expected_tip` is compared with the **last** chain's tip.
- **Never a silent merge:** one chain gives `chains.len() == 1` and an empty `key_discontinuities`; anything else lists every discontinuity. There is no API that returns a single combined tip for several chains.

**Manifest helpers** (used by Slice 3, pure): `manifest_digest(body)` and `sign_manifest`/`verify_manifest` with `TAG_MANIFEST`/`TAG_SIG_MANIFEST`.

### Slice 2: `traction-daemon` key lifecycle, storage and sealing in the finalizer

**Device Signing Key file.** `<data dir>/signing.key`: the 32-byte Ed25519 seed, raw. `<data dir>/signing.pub`: the 32-byte public key as lowercase hex plus newline (for the user to copy to a reviewer; not secret). A new `signing_key.rs` follows `secret.rs`:

- Created with 32 bytes from `getrandom::fill` straight into a `Zeroizing<[u8; 32]>`, written through `fsutil::write_atomically`. Read into `Zeroizing<Vec<u8>>`; every intermediate buffer is zeroized.
- **Unix:** mode `0600` at creation. On load, a file whose mode grants group/other any bit is tightened to `0600` and `log::warn`ed.
- **Windows:** created with an explicit protected DACL holding one ACE (full control for the current user's SID), no inherited ACEs. This is stricter than `daemon.token`, which relies on the profile ACL. `windows-sys` gains the `Win32_Security` and `Win32_Security_Authorization` features. A test reads the DACL back with `GetNamedSecurityInfoW` and asserts exactly that one ACE.
- Never the macOS Keychain or the Linux Secret Service (both can prompt or fail on headless CI). Never logged, formatted or sent anywhere.
- A **malformed** `signing.key` (wrong length, weak public key) is never regenerated: startup fails with a one-line error, like `device.secret`, and leaves the file for the user to inspect.
- A **missing** `signing.key` while `chain_link` already has rows starts a **successor chain**: a new key (new `key_id`), a new genesis at `seq 0`, and `supersedes` set to the current tip read from `chain_link` (the old key is gone, so only the new key can sign this commitment). The daemon emits a loud `log::warn` at start (`signing.key missing; starting a new chain; the previous chain stays verifiable under its old key`) and again on the first seal. The old links stay untouched and verifiable under their old public key.

**Migration 10** (append, never edit a shipped migration):

```sql
CREATE TABLE chain_key (
    key_id          TEXT PRIMARY KEY NOT NULL,
    public_key      TEXT NOT NULL UNIQUE,   -- 32-byte Ed25519 public key, lowercase hex
    created_at_ms   INTEGER NOT NULL
) STRICT;
-- chain_key_no_update / chain_key_no_delete triggers

CREATE TABLE chain_link (
    key_id          TEXT NOT NULL REFERENCES chain_key (key_id),
    seq             INTEGER NOT NULL CHECK (seq >= 0),
    window_start_ms INTEGER NOT NULL,
    ruleset_id      TEXT NOT NULL,
    prev_hash       TEXT NOT NULL,
    payload_digest  TEXT NOT NULL,
    entry_hash      TEXT NOT NULL UNIQUE,
    sealed_at_ms    INTEGER NOT NULL,
    clock_regressed INTEGER NOT NULL CHECK (clock_regressed IN (0, 1)),
    payload_json    TEXT NOT NULL,   -- canonical JSON of the Link Payload
    signature       TEXT NOT NULL,
    PRIMARY KEY (key_id, seq),
    UNIQUE (window_start_ms, ruleset_id)
) STRICT;
-- plus chain_link_no_update / chain_link_no_delete triggers (the traction_event pattern)
```

The daemon inserts the current key's `chain_key` row (`INSERT OR IGNORE`) at start, so every chain's public key stays in the database after its `signing.key` is gone. The Chain Tip is the current key's highest `seq` (`WHERE key_id = ? ORDER BY seq DESC LIMIT 1`); the successor's `supersedes` is the highest-`window_start_ms` link of any other key. No mutable "tip" table is added, so the protocol's list of non-append-only tables stays `struggle_baseline` and `rollup_cursor`.

**Row ↔ core round-trip.** `storage.rs` gets `load_sealed_rows(conn, from, to) -> RowSet` that maps `work_bucket`, `activity_rollup` and `commit_observation` rows back to the `traction-core` types. A test proves `row → WorkBucket/ActivityRollup → canonical JSON` equals the canonical JSON of the struct the engine produced, for every field (including `category_share_milli`, stored as JSON text).

**Sealing in the writer.** `WriteOp::Rollup` stays the one finalizer write. `insert_rollup` (already one transaction with the cursor) seals inside that same transaction:

1. Insert the batch's Work Buckets, Activity Rollups and baseline as today.
2. Group the batch's rows by aligned 120 s window (the batch already spans whole windows). Commit Observations for each window are read from `commit_observation` inside the transaction.
3. For each window, in ascending order, that holds any Work Bucket, Activity Rollup or Commit Observation: build the Link Payload from the **engine's in-memory rows** (not from what `INSERT OR IGNORE` left), read the Chain Tip, `seal`, insert the `chain_link`. A window with nothing in it gets no link (empty windows are not proof of anything and would bloat the chain).
4. Advance `rollup_cursor`, commit.

Any error rolls back rows, links and cursor together, so there is never an unsealed finalized row and never a link without its rows. Because the engine's rows are what get sealed, an `INSERT OR IGNORE` that kept an older, different row leaves a mismatch the verifier reports as `PayloadHashMismatch`; it is never silently re-sealed. The writer thread is the only holder of the `DeviceSigningKey` (moved in at start), so there is one sealer and the chain can't fork. Commit Observations with `observed_ms` in a window that was already sealed (a late observation) are left unsealed and reported by the verifier as `UnanchoredRow`; the commit observer records with `observed_ms = now`, so this only happens on clock rollback (below).

**Restart.** Nothing new: the cursor and the tip are committed together, so a restart resumes from the last committed `(seq, entry_hash)`. The test kills the daemon process (real `kill -9` / `TerminateProcess`) with a fault-injection hook (test-only `TRACTION_TEST_FAIL_SEAL_AFTER=<n>` that returns an error from the n-th link insert, so the transaction rolls back deterministically) and asserts after restart: `verify_chain` passes, `seq` is contiguous, every finalized window is sealed exactly once.

**Wall-clock rollback.** History is never rewritten:

- The finalizer's `rollup_cursor` only moves forward (`max(...)` today). If the clock goes back, nothing new settles until the clock passes the cursor again; no window is re-finalized.
- `window_start_ms` must strictly increase along the chain (the verifier's `WindowOrder`), and the writer refuses (logs once) a seal that would break it: a batch reaching back to the newest sealed window of any chain (e.g. a lost `rollup_cursor`) drops the windows at or before it (rows and links), seals the newer ones, folds only those into the baseline, and still advances the cursor past the whole batch.
- `sealed_at_ms` is the wall clock at sealing, stored as is, never clamped. If it is earlier than the previous link's, the link sets `clock_regressed: true`. This is advisory: it is signed, counted in `ChainReport.clock_regressions`, and surfaced to Step 4's `DecisionEngine`; it never fails verification on its own.
- Tests use an injected clock: `finalizer::finalize_once(conn, rules, now_ms, …)` (the existing pure entry point) is driven with a clock that goes 10 minutes back and forward again. No `sleep`-based assertion.

**Low-spec budget.** Signing is ~1 Ed25519 signature per sealed window (~50 µs). No new thread, no new wakeup.

### Slice 3: Audit Bundle export, offline `verify` CLI, red-team tamper suite and docs

**Audit Bundle (frame-less).** `traction-daemon export-bundle --project <project_id> --out <file> [--data-dir <path>]` writes one canonical JSON file, `<file>` (suggested extension `.traction-bundle.json`). It opens the database `mode=ro` with `PRAGMA query_only = ON`, so it is safe while the daemon runs. Contents:

```text
{
  "manifest": { body…, "signature": "<hex>" },
  "chains": [ { "public_key": "<hex>", "links": [ every Chain Link of that key, genesis to tip ] }, ... ],   // every chain the device holds, device-wide
  "project_rows": { "work_buckets": [...], "commit_oids": {window: [...]} },   // this Project only
  "device_rows":  { "activity_rollups": [...] },                               // device-wide, counts only
  "frames": []                                                                 // reserved for 3c
}
```

Manifest body: `bundle_schema: 1`, `project_id`, `linked_repo: null` (Step 5 fills it), `key_id` and `public_key` (the current, signing key), `chain_count`, `key_ids` (every chain's, in order), `first_window_ms`, `tip: { key_id, seq, entry_hash }` (the last chain's), `link_count`, `bucket_count`, `ruleset_ids`, `exported_at_ms`, `chains_digest` (SHA-256 under `TAG_MANIFEST` over the canonical `chains` array, so dropping or adding a link or a whole chain in the file breaks the manifest). The manifest is signed with the current Device Signing Key. The verifier checks the manifest signature first (`ManifestSignatureInvalid`), checks that `chain_count`/`key_ids` match the `chains` array (`ManifestChainsMismatch`), then runs `verify_chains` with `RowScope::Project(project_id)` and `expected_tip` = the manifest tip. Rows before the genesis window are not exported. 3c wraps this same JSON as `manifest.json` + `buckets.jsonl` inside the zstd `.traction-bundle`; the digests don't change.

Other Projects appear in the bundle only as `{ project_id, rows_digest }` entries inside each Link Payload (needed to recompute `payload_digest`). This is the accepted cost of one chain per device (Resolved Decision 2), and `docs/protocol.md` must spell it out: a bundle reveals, per sealed window, how many other Projects were active and their opaque `project_id`s, never their rows. `dir:` IDs are keyed by `device.secret` and can't be linked to a path. A `git:` ID is `SHA-256(root_commit, normalized origin)`, so anyone who already knows a repo's root commit and origin can confirm that the user worked on it in that window. Users who don't want that should not export while a sensitive `git:` Project is tracked; a blinded per-bundle encoding is a possible later change (new `bundle_schema`), not part of 3a.

**Offline verifier.** `traction-daemon verify <bundle-file> [--trusted-key <hex>]... [--expect-tip <key_id>:<seq>:<entry_hash>]` and `traction-daemon verify --data-dir <path>` (the local SQLite, `mode=ro`, `RowScope::AllProjects`, chains and keys from `chain_link`/`chain_key`). Both call the same `traction-core` `verify_chains`, need no daemon and no network. Output is stable lines:

- `OK chains=<n> links=<n> tip=<key_id>:<seq>:<hash> pinned=<bool> clock_regressions=<n> pending=<n> legacy=<n>`
- one `WARN key_discontinuity from=<key_id> to=<key_id> at_window=<ms> linked=<bool>` line per discontinuity
- or `FAIL <code> key=<key_id> seq=<n>`

Exit codes: **0** = verified, exactly one chain; **2** = verified, but more than one chain (key discontinuity, so scripts can't mistake it for a clean pass); **1** = verification failed or the input could not be read. Without `--trusted-key` it prints that the keys are self-asserted (trust on first use).

**Red-team tamper suite** (`crates/traction-daemon/tests/tamper_red_team.rs`, all 3 OSes). Each test builds a real chain through a real daemon writer path (injected clock, at least 6 sealed windows over 2 Projects), checks it verifies, then attacks. SQLite attacks drop the relevant `*_no_update`/`*_no_delete` trigger first, inside the test only. Expected results (first failure in chain order, `seq = k` is the attacked window's link):

| # | Attack | Where | Expected |
|---|---|---|---|
| 1 | Flip 1 bit in a Work Bucket's `agent_compute_sec` | SQLite | `PayloadHashMismatch { seq: k, part: Project(p) }` |
| 2 | Same flip | bundle `project_rows` | same |
| 3 | Flip 1 bit in an Activity Rollup field | SQLite | `PayloadHashMismatch { part: DeviceRows }` |
| 4 | Edit a link's `payload_json` (drop a Project entry) | SQLite | `PayloadHashMismatch { part: Payload }` |
| 5 | Delete a middle link | SQLite and bundle | `SeqGap { expected: k, found: k+1 }` |
| 6 | Delete a middle link and renumber later `seq`s | SQLite | `EntryHashMismatch { seq: k }` |
| 7 | Swap two adjacent links' positions | bundle | `SeqGap { expected: k, found: k+1 }` |
| 8 | Swap the values of two Work Buckets in different windows | SQLite | `PayloadHashMismatch` at the earlier window |
| 9 | Insert a Work Bucket into a sealed window | SQLite | `PayloadHashMismatch { part: Project(p) }` |
| 10 | Insert a Work Bucket into an unsealed gap window between two links | SQLite | `UnanchoredRow { window_start_ms }` |
| 11 | Rewrite window k and re-sign k..tip with a **rogue key** (its own `key_id`) | bundle, key pinned | `UnknownKey { seq: k }` |
| 12 | Same, but keep the device's `key_id` | bundle | `InvalidSignature { seq: k }` |
| 13 | Rebuild the whole chain with a rogue key, unpinned verify | bundle | passes with `key_pinned: false` and a different `key_id`; with `--trusted-key` it is `UnknownKey { seq: 0 }`. This is the documented limit of trust-on-first-use. |
| 14 | Graft a second device's chain after link k | bundle | `DuplicateSeq { seq: 0 }` |
| 15 | Graft it renumbered and re-linked, signed by the second key | bundle | `UnknownKey { seq: k+1 }` |
| 16 | Truncate the head (delete `seq` 0..k) | SQLite and bundle | `TruncatedGenesis { first_seq: k+1 }` |
| 17 | Truncate the tail | bundle (manifest tip) | `TipMismatch`; and `ManifestSignatureInvalid` if the manifest's tip is edited to match |
| 18 | Truncate the tail | SQLite with `--expect-tip` from an earlier export | `TipMismatch`; without `--expect-tip` it passes, documented as the local limit |
| 19 | Fork: insert a second link with an existing `seq` (bypassing the primary key by rebuilding the table) | SQLite | `DuplicateSeq { seq }` |
| 20 | Malleate a signature (`S + ℓ`) / swap in a small-order public key | bundle | `InvalidSignature` / bundle rejected as `WeakKey` at load |
| 21 | Delete `signing.key`, restart, seal more windows (honest key loss) | SQLite and bundle | verifies with `chains=2`, one `linked=true` discontinuity, CLI exit **2** |
| 22 | After 21, delete the old chain's last link | SQLite | `SupersedesMismatch { key_id: <new> }` |
| 23 | Drop the old chain from a bundle and re-sign the manifest with the new key | bundle | `UnanchoredRow` for the old chain's windows if their rows are kept; with them also removed, `SupersedesMismatch` (the new genesis still names the old tip) |
| 24 | Present a successor chain whose windows overlap the old chain | bundle | `ChainOverlap { key_id }` |

Each test asserts the `code()` string as well as the variant, so the CLI's output is pinned too. A pure, seeded `proptest` in `traction-core` adds: for any valid chain and any single-byte change to any link field or row value, `verify_chain` fails (never panics) and returns the same error on every run.

**Zero raw text.** `derived_tables_no_raw_text.rs` is extended: after its canary prompt, file name, file body and commit message flow through a real daemon and the finalizer, the test also seals, exports a bundle for the canary Project and asserts none of the canaries appears in `chain_link` (every column), `signing.pub`, or any byte of the exported bundle. It also asserts the bundle holds no path: neither the Project Root nor the data folder path appears.

**Never `~/.wakatime.cfg`.** The existing source-grep guard covers the new modules; no change needed beyond confirming it scans them.

**Docs.**

- `docs/protocol.md` §4 is rewritten: the domain tags, the Link Payload/Header/Chain Link formulas, the genesis sentinel, `verify_strict`, the check order and error codes, `clock_regressed`, successor chains (`supersedes`, `chains=<n>`, exit code 2), the `git:` ID disclosure of device-wide bundles (above), and the honest-limits paragraph (local chain = tamper-evident since sealing; tail truncation needs an outside tip; trust on first use without a pinned key). The old `H_n = SHA256(H_(n-1) || …)` formula is replaced, with a note that `frame_hash_n` became `frame_digest` (reserved for 3b) and `git_diff_hash_n` became the Commit Observation oids inside `rows_digest`.
- §2c "Tables" lists `chain_link` as append-only and states again that the only non-append-only tables are `struggle_baseline` and `rollup_cursor`. The two key files are listed under a new "Device Signing Key" paragraph.
- §5 gets the frame-less Audit Bundle layout and how 3c wraps it.
- §3 (Anchor heartbeat): `chain_tip_hex` is the tip link's `entry_hash`.
- `GLOSSARY.md` gets the new terms; "Anchor" gets a line saying it carries a Chain Link's `entry_hash`.
- `docs/steps.md` Step 3 Build 2 gets the domain-separated formula in place of the bare `H_n` line.

## User Stories

1. As a user, I want each 2-minute window of my tracked work sealed as it is finalized, so that my history can't be changed afterwards, even by me.
2. As a user, I want to export a single Project's proof without exposing my other Projects' buckets, so that sharing one Project doesn't leak the rest.
3. As a user on a headless Linux box or an old Windows laptop, I want the signing key created silently with no keyring prompt, so that install stays passive.
4. As a user, I want a crash or power cut never to corrupt my chain, so that an honest user never fails verification.
5. As a user whose clock drifted or was changed, I want that shown as a flag, not as broken history, so that I'm not accused of tampering for a CMOS battery.
6. As a reviewer, I want one offline command that tells me "OK" or the exact failure and where, so that I don't have to trust the user's machine or the server.
7. As a reviewer, I want to pin the user's public key, so that a chain rebuilt with a fresh key fails.
8. As the Step 4 server, I want the exact verifier the daemon uses, so that the two can never disagree.
9. As Fraud Squad, I want bit flips, deletions, insertions, row swaps, rogue keys, grafted chains and truncations all caught with a specific error in CI, so that the open-source format isn't a forgery guide.
10. As a privacy-conscious user, I want the chain and the bundle to hold only counts, hashes and opaque IDs, never prompts, file names, paths or commit messages.

## Implementation Decisions

- **Device-wide chain, one link per sealed 120 s window** (Resolved Decision 2). One sealer (the writer thread), one chain per Device Signing Key, successor chains only after key loss.
- **Seal from engine output, inside the existing rollup transaction.** No second writer, no second transaction, no recompute-on-read.
- **`traction-core` signs but never generates keys.** Key generation needs an OS RNG and stays in the daemon; the server never needs to sign links.
- **`verify_strict` everywhere**, plus `is_weak()` rejection when a public key is loaded.
- **Schema stays `schema_version: 1`.** Chain Links are daemon-local; no new `event_type`, no `TractionEvent` change.
- **Traction Events and Presence Minutes are not sealed in Step 3.** The sealed rows are the derived outputs (Work Buckets, Activity Rollups) and Commit Observations. Sealing raw-ish event streams would grow the bundle and is not needed for the gate; Step 4 recomputes buckets from events it receives itself.
- **Bundle format is canonical JSON, uncompressed**, no new archive dependency until 3c.
- **Key rotation is out of scope**, but `key_id` per header and `trusted_keys: Vec` leave room: a later rotation link signed by the old key that names the new key is a payload extension, not a format break.

## Testing Decisions

- Slice 1: golden vectors (fixed seed `[7u8; 32]`, a fixed two-Project payload) with committed hex for every digest, the `key_id`, `entry_hash` and signature; identical on all 3 OSes. Unit tests per `ChainError` variant on hand-built chains. Domain-separation test: the same bytes hashed under two tags differ, and a payload's canonical bytes fed as a header fail to parse (`deny_unknown_fields`). `verify_strict` malleability and small-order tests. Proptest: single-byte mutation always fails, deterministically.
- Slice 2: key file created `0600` (Unix) / single-ACE protected DACL (Windows), reused on restart, malformed file refuses start, a too-open Unix mode is tightened. Seal path: a batch of N windows writes exactly the non-empty windows' links, contiguous `seq`, and `verify_chains` over the DB passes. A deleted `signing.key` gives a successor chain with `supersedes` set and a loud warning. Fault injection rolls back rows + links + cursor together. Real process kill + restart resumes the chain. Injected-clock rollback: no re-finalization, `clock_regressed` set on the first link sealed after the clock passes the cursor with `sealed_at_ms` lower than the tip's, chain still verifies. Row round-trip test for every column.
- Slice 3: the 24-row tamper table above; CLI exit codes and output lines; export while the daemon is running (read-only connection, no `SQLITE_BUSY` for the writer); canary test extension.
- No `sleep`-based assertions anywhere; daemon-level tests poll with the existing `poll_until` helper and drive time through the finalizer's injected `now_ms`.

## Out of Scope

- Snapshot frames, the encrypted vault, dHash dedup, the privacy denylist and the Screen Recording prompt (3b).
- `offline_buffered` sync, the `/submit` Review & Redact UI, Redaction tombstones and the zstd `.traction-bundle` archive (3c).
- Sending Anchors to a server, `server_received_at`, server-side verification and key registration (Step 4).
- Key rotation, key backup/recovery and hardware-backed keys.
- Any UI. WakaTime compatibility and any other Harness.

## Resolved Decisions (human, 2026-11-26)

1. **Scope:** this spec is **Step 3a**. 3b (snapshot vault) and 3c (offline sync, Review & Redact UI, zstd bundle) are deferred until after Step 4, like 1d/1e were deferred past Step 2. Step 3 Exit Gate items 2 and 3 stay open. `frame_digest` stays reserved as all zeros so 3b drops in with no format change.
2. **One chain per device:** approved. It keeps cross-Project wall-clock conservation, keeps `dir:` → `git:` migrations simple and gives Step 4 one tip per device to anchor. The bundle's disclosure of other Projects' opaque IDs (confirmable for `git:` IDs) is documented in `docs/protocol.md`.
3. **Lost `signing.key`:** a missing key while a chain exists starts a visible successor chain with a new `key_id` and a loud warning; a malformed key file still refuses start. Safeguard: `verify_chains` and the `verify` CLI always surface multiple chains / key discontinuity explicitly (`chains=<n>`, `WARN key_discontinuity`, exit code 2) and never report several chains as one unbroken chain.
