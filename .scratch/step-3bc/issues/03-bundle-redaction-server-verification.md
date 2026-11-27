# 03: S3 `.traction-bundle` archive, Redaction, frame verification (core, CLI, server), schedule check, Jev frames

**What to build:** This ticket covers the frame-bearing Audit Bundle end to end:
- the `tar.zst` `.traction-bundle` export with `bundle_schema: 2`;
- permanent user Redaction;
- frame checks in the `traction-core` verifier, shared by the `verify` CLI and the server;
- archive hardening;
- `submit` sending only the archive;
- the server's Capture Nonce schedule check;
- verified, unredacted frames feeding `JevEngine`.

It owns Gates 2 and 4 and the Step 4 Jev frame carry-over. See spec "S3", deviation 1, "Testing seams" (row S3), "Policy constants" and Resolved Decisions Q9, Q11, Q12, choices A–F and addition 1.

**Blocked by:** 01, 02

**Status:** ready-for-agent

Acceptance:

**Archive (Q12, deviation 1, choices C and D)**
- [ ] `export-bundle --project <id> --out <file.traction-bundle> [--redact <frame_id>]...` writes `tar.zst` containing:
  - `bundle.json`: canonical JSON, `bundle_schema: 2`, with `frames` holding `{ record: FrameRecord, redacted, reason: null | "user" | "expired" }` for every Frame of the exported windows;
  - `frames/<frame_id>.webp` and `frames/<frame_id>.salt` for each unredacted Frame only, decrypted from the Vault during export.
- [ ] The manifest adds `frames_digest = SHA256("traction/v1/manifest\0" || canonical_json(frames))`, plus `frame_count`, `redacted_count` and `expired_count`, and stays signed.
- [ ] The JSON-only `.traction-bundle.json` export stays. `verify` reads `bundle_schema` 1 and 2, in either container.

**Redaction (choices A and B)**
- [ ] `--redact`, and the S4 API, record the choice in a new `frame_redaction` table. Through the writer, they **permanently delete the Vault `.bin`** and set `pixels_state = redacted`. The Frame Record stays. The CLI flag's help text says the deletion is permanent.
- [ ] **Before export, the daemon decides:** a Frame whose Vault file is gone or unreadable is exported as `redacted: true` with no files. The reason is `expired` if `pixels_state = expired`, and `user` otherwise (a user Redaction, a hand-deleted file, an undecryptable file).
- [ ] **After export, the archive is fixed:** stripping or changing an unredacted entry's files fails, and so does flipping an entry's `redacted`/`reason` (`frames_digest` covers it).

**Core verification (`traction-core`, rule 5)**
- [ ] After the chain checks, each with a stable code:
  - the records listed per window recompute that link's `frame_digest` (`payload_hash_mismatch part=frames`);
  - `frames_digest` and the counts match (`manifest_frames_mismatch`);
  - every unredacted entry has both files and its `frame_hash` recomputes (`frame_missing`, `frame_hash_mismatch`);
  - no file is present without an entry (`frame_unexpected`).
- [ ] The report gives `frame_count`, `redacted_user_pct` and `expired_pct`, computed from the entries and never taken from the manifest's numbers.
- [ ] A `reason: "expired"` entry on a Frame captured less than 45 days before `exported_at_ms` is counted as a user redaction, with advisory `expired_claim_too_young`.
- [ ] **Hardening, shared by the CLI and the server:**
  - decompression capped at `blob.max_bytes` × a documented factor;
  - an entry-count cap;
  - only the exact names `bundle.json` and `frames/<frame_id>.{webp,salt}`, with the id grammar checked;
  - no links, absolute paths, `..` or duplicates.
  - Anything else is `malformed_bundle`.

**Server**
- [ ] `traction-daemon submit` sends only the archive. The blob key is `bundles/<id>.traction-bundle`.
- [ ] `complete` unpacks the archive in memory under the caps and runs the core verifier against the pinned keys and stored Anchors (unchanged logic). It stores `frame_count`, `redacted_user_pct` and `expired_pct` with the verification summary.
- [ ] **Schedule check (Q9a):** for each scheduled Frame with a `nonce_id`, the server recomputes the schedule with the shared core function and the stored nonce. It flags `frame_timing_mismatch` when `scheduled_at_ms` isn't on the schedule, when `captured_at_ms` is more than `capture_timing_tolerance_ms` after it, or when the `nonce_id` was never issued to the device. `HeuristicEngine` makes it Yellow at or above a `[heuristic]` share, and never Red on its own. Frames scheduled by local jitter are counted and shown, not flagged.
- [ ] **Jev frames (addition 1):**
  - `TriageInput.frames` gets the verified unredacted WebP bytes, sampled evenly across the submission's timeline up to `[jev] max_frames`, replacing `frames: Vec::new()` in `crates/traction-server/src/submission.rs`.
  - The `failed(...)` path keeps no frames.
  - The 280 px skip, `frame_too_small_skipped`, the all-skipped → Yellow rule and `TypeSafe-Danger-Zone-Images: true` are unchanged.
  - Frames are sent only for verified, non-Red submissions.

**Tests (spec "Testing seams" S3)**
- [ ] **All 3 OSes:**
  - core vectors for `frame_hash`, `frame_digest` and `frames_digest`;
  - CLI round trip of a real daemon's archive.
- [ ] **Red-team rows, each through the CLI and through the server on Postgres + versitygw:**
  - swapping two Frames' files → `frame_hash_mismatch`;
  - a byte flipped in a WebP or a salt → `frame_hash_mismatch`;
  - an unredacted Frame's files deleted after export → `frame_missing`;
  - the same, plus its entry flipped to `redacted: true` → `manifest_frames_mismatch`, and `manifest_signature_invalid` if the manifest is re-signed with another key;
  - an entry dropped → `manifest_frames_mismatch`;
  - a record removed from a window → `payload_hash_mismatch part=frames`;
  - an extra file → `frame_unexpected`;
  - a decompression bomb, a `../` entry, a symlink entry → `malformed_bundle`.
- [ ] **Gate 2:** redact 3 of N real Frames, then:
  - none of their WebP or salt bytes occurs anywhere in the archive's decompressed stream;
  - `verify` passes;
  - `redacted_user_pct == 3/N`.
- [ ] A Vault file deleted before export exports as `reason: "user"`, verifies, and is counted.
- [ ] The schedule check passes an honest daemon's frames. It flags shifted times, a late capture and an unknown `nonce_id`.
- [ ] In `tests/jev.rs`, a submission-path test shows real verified frames reaching the request with the Danger-Zone header, and none on Red or failed bundles.
- [ ] `e2e_daemon.rs` submits a real daemon's archive with frames.

**Docs**
- [ ] `docs/steps.md` Step 3: deviation 1 (`tar.zst` with the `bundle.json` layout).
- [ ] `docs/protocol.md` §4: Redaction and Expired Frames, replacing the 3a placeholder paragraph, including the before/after-export rule.
- [ ] `docs/protocol.md` §5: `bundle_schema: 2`, the archive layout, frame verification codes and archive caps.
- [ ] Record the decompression factor and entry cap in a comment below.

## Comments
