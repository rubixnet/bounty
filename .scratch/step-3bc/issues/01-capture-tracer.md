# 01: S1 Capture tracer: `capture-frame` child, encrypted Vault, Frame Records, sealed `frame_digest`

**What to build:** This ticket is the thinnest real path from the screen to the chain. One scheduled Frame on each OS:
- is grabbed, downscaled, dHashed, WebP-encoded, salted, hashed and encrypted by a short-lived `traction-daemon capture-frame` child;
- lands in the Vault as one encrypted file;
- is recorded by the daemon's writer as a Frame Record;
- is sealed into its window's Chain Link as a real `frame_digest`.

It also proves first that every new native crate builds on stock CI runners, and that GDI `BitBlt` returns real pixels on `windows-latest`. See spec "S1: Capture tracer", "Architecture invariants", deviations 2 and 3, "Testing seams" (row S1), "Policy constants" and Resolved Decisions Q1–Q3, Q7, Q8, Q14 and additions 2 and 3.

**Blocked by:** none

**Status:** ready-for-agent

Acceptance:

**Build and runner checks first (addition 3; land these before anything else in this ticket)**
- [ ] One commit adds the new deps and a minimal smoke use of each, and is green in CI on stock `macos-latest`, `ubuntu-latest` and `windows-latest` (MSVC) with no extra system packages:
  - `objc2-screen-capture-kit` (macOS only), with features for `SCScreenshotManager`;
  - new `windows-sys` features for GDI (`BitBlt`, `GetDIBits`, DCs);
  - `webp` (bundled libwebp built by `cc`), lossy encode;
  - `chacha20poly1305` (XChaCha20-Poly1305) and `hmac` (for S2's schedule, here only to prove it builds).
- [ ] **Windows `BitBlt` check (before S2 starts):** a CI test on `windows-latest` grabs the primary display through the real GDI path, after the test has drawn a window filled with a known non-black colour, and asserts the grab is non-black and contains that colour. If the runner returns black or fails, stop and raise it to the human with the run log. Don't write a deviation and don't continue to S2.
- [ ] If any crate fails to build on a runner, stop and raise it to the human. Never switch to an unvetted crate silently.

**`traction-daemon capture-frame` (internal subcommand; Q2, addition 2)**
- [ ] Arguments: `--data-dir`, `--display <id>`, `--prev-dhash <16 hex | none>`, `--trigger <scheduled|turn_start|tool_fail|focus_shift>`, `--scheduled-at-ms`, `--nonce-id <id|none>`, `--expect-focus <opaque token>`.
- [ ] It grabs the display holding the focused window (native backends: ScreenCaptureKit `SCScreenshotManager` on macOS 14+, GDI `BitBlt` on Windows, `x11rb` `GetImage` on X11). Wayland and older macOS report `unsupported`.
- [ ] It re-checks focus against `--expect-focus` right after the grab, and discards with `focus_changed` on a mismatch.
- [ ] It downscales to 720p height (never upscales) and computes a 64-bit dHash (9×8 grayscale). If the Hamming distance to `--prev-dhash` is < 3 (`DHASH_SKIP_DISTANCE`), it stops **before encoding** and prints `{"status":"duplicate","dhash":"…"}`.
- [ ] Otherwise it does a lossy WebP encode, draws a 16-byte salt, and computes `frame_hash = SHA256("traction/v1/frame\0" || webp || salt)`.
- [ ] It encrypts `salt || webp` with XChaCha20-Poly1305 under `vault.key` (random nonce, associated data `frame_id || frame_hash`) into `vault/<frame_id>.bin`, writing a temp file and then renaming it.
- [ ] It prints exactly one JSON line. On success that's `{"status":"written","frame_id","frame_hash","dhash","captured_at_ms","width","height"}`. Otherwise it's `{"status":<closed code>}`, one of `permission_denied`, `unsupported`, `focus_changed`, `error`. There's never any free text.
- [ ] The child never opens SQLite.

**Daemon side**
- [ ] The daemon spawns the child, kills it after `CAPTURE_CHILD_TIMEOUT` (10 s), keeps the last `dhash` (8 bytes) in memory, and hands a `written` result to the single writer.
- [ ] A killed or hung child leaves no record. Any orphan temp or `.bin` file without a record is removed at startup.
- [ ] Pixels are never decoded or held in the daemon process.
- [ ] `vault.key`: 32 random bytes, created on first enable with the existing `owner_only` permissions.

**Frame Record and `frame_digest` (`traction-core`, rule 5; Q8)**
- [ ] `FrameRecord { frame_id, frame_hash, captured_at_ms, trigger, scheduled_at_ms, nonce_id }`, typed with `deny_unknown_fields`. `frame_id = "<captured_at_ms>-<2-digit counter>"`, and the grammar is validated.
- [ ] `frame_digest = SHA256("traction/v1/frames\0" || canonical_json(records sorted by frame_id))` for the Frames whose `captured_at_ms` falls in the link's window, or 32 zero bytes when there are none. It includes published test vectors.
- [ ] A new append-only `frame` table holds the record fields plus `window_start_ms`, `width`, `height` and `pixels_state ∈ present|redacted|expired`. `pixels_state` is not part of the digest.
- [ ] The finalizer seals a window only when no capture child for it is in flight. A Frame that reports after its window was sealed is discarded (file deleted, never re-sealed).
- [ ] `verify --data-dir` recomputes `frame_digest` from the `frame` table. A tampered, deleted or inserted record fails with `payload_hash_mismatch part=frames`.
- [ ] 3a chains, and devices with Visual Proof Mode off, still verify unchanged (zero digest).

**Scheduling (S1 only; S2 replaces the policy)**
- [ ] `traction-daemon visual-proof enable|disable|status` and a config flag. While off: no children, an empty Vault, zero `frame_digest`.
- [ ] While on and a Project is active: scheduled captures with local jitter, `interval = 60_000 + (u64::from_be_bytes(r[..8]) % 30_000)` ms (`r` from the OS CSPRNG), with `nonce_id: null`.

**Tests (spec "Testing seams" S1)**
- [ ] **Linux (Xvfb) and Windows, real capture:** one capture gives a `.bin` that decrypts under `vault.key`, whose bytes and salt hash to the sealed record's `frame_hash`, and the window's link carries the matching `frame_digest`.
- [ ] **macOS CI:** everything after the grab runs on committed real WebP fixtures, and the real ScreenCaptureKit call returns `permission_denied` cleanly (no panic, no prompt).
- [ ] `#[ignore] macos_real_screencapturekit_grab`, for the human to run locally (deviation 3).
- [ ] An identical screen twice gives `duplicate` with no encode and no file. A hung child is killed and leaves nothing.
- [ ] `tamper_red_team.rs` gains frame rows: an edited, deleted and inserted `frame` record each fail with the exact error.
- [ ] `derived_tables_no_raw_text.rs` covers the `frame` table and the child's stdout.
- [ ] Measure the WebP quality default that lands ~15–30 KB at 720p on the fixtures, and record the result in a comment below.

**Docs**
- [ ] `docs/steps.md` Step 3: deviation 2 (domain-tagged `frame_hash`) and deviation 3 (local macOS proof). The offline item is marked as met by Step 4 S1, with test references.
- [ ] `docs/protocol.md` §4: `frame_hash`, `FrameRecord`, `frame_digest`, the Vault key's honest limit (guards copied folders and backups, not same-user malware).
- [ ] `docs/roadmap.md` Step 3 is rewritten to the four slices of this spec.
- [ ] `GLOSSARY.md`: Frame, Frame Record, Visual Proof Mode, Vault, Capture Nonce, Expired Frame. Extend "Audit Bundle" and "Redaction".

## Comments
