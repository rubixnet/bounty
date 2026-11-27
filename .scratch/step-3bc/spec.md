# Step 3b + 3c: Snapshot Vault, Review & Redact, `.traction-bundle`

Status: ready-for-agent (approved by the human 2026-11-27: Q1–Q14, the five additions, and choices A–F with the B and E rules below)

Source: `docs/steps.md` (Step 3 Build 1 and 3, Exit Gate items 1–4), `docs/roadmap.md` (sub-steps 3b, 3c and the 2026-11-26 deferral), `docs/protocol.md` sections 3, 4 and 5, `docs/goal.md` (sparse snapshots, salted frame hashes, Redaction), `.scratch/step-3/spec.md` (3a: `frame_digest` reserved, the frame-less Audit Bundle), `.scratch/step-4/spec.md` and `.scratch/step-4/issues/05-jev-engine.md` (the Jev frame path waiting for real frames), `GLOSSARY.md` ("Audit Bundle", "Redaction"), and the AGENTS.md invariants "Zero Raw Content Persistence", "OS Permissions & Low-Spec Hygiene", "Zero Mocking of Core Mechanics" (rule 4), "Single Source of Shared Types" (rule 5), "Git & Local SQLite Safety" (single writer) and "Jev Integration (Step 4)".

Every decision below was settled in the 3b/3c grill (2026-11-27) and is listed by number under "Resolved Decisions". Anything the grill did not settle is under "Left to tickets" and is **not** decided here. Step 5 waits for this spec's gate; its own decisions are parked in `.scratch/step-5/notes.md`.

**One spec, 4 tracer slices** (Q1). S1 capture tracer, S2 capture policy, S3 bundle + verification, S4 Review & Redact UI. Each slice fits one fresh session (~150k tokens). If a slice doesn't fit, split *that* slice, not the spec. S1 rewrites `docs/roadmap.md` Step 3 to match.

## Gate items this spec owns

- [ ] **Gate 1 (frames part):** tampering with, deleting or inserting a past frame record in local SQLite, or the encrypted vault file behind it, fails verification with an exact typed error. *(S1 for records, S3 for vault bytes; `tamper_red_team.rs` gains frame rows.)*
- [ ] **Gate 2:** redacting 3 frames removes 100% of their pixel bytes and salts from the exported `.traction-bundle`, the bundle still passes chain verification, and `% redacted` is reported accurately (user redactions and expiries counted separately). *(S3 through the CLI; S4 repeats it through `/submit`.)*
- [ ] **Gate 3:** while a password manager, a private/incognito browser window or a focused `.env` file has focus, zero frames are captured. *(S2, real windows on Linux Xvfb and Windows; macOS: see deviation 3.)*
- [ ] **Gate 4:** frame tampering (swapping, altering or removing a real frame) is caught by the server's verification path (`verify_bundle` against anchored tips). *(S3, Postgres + versitygw job.)*
- [ ] **Offline item (Step 3 Build 2, third bullet):** already met by Step 4 S1 (`offline_buffered`, `network_class`). S1 adds the test references to the Step 3 section; no new code.
- [ ] **Step 4 carry-over:** `JevEngine` receives real, verified, unredacted frames from submitted bundles (replacing the `frames: Vec::new()` placeholders), with the existing 280 px skip and `TypeSafe-Danger-Zone-Images: true` rules unchanged. *(S3.)*
- [ ] All of the above pass in CI on all 3 OSes, under the Linux-only Postgres/blob split of ADR 0006 and deviation 3 below.

### Scope deviations (recorded in `docs/steps.md` and `docs/protocol.md` by the slice named)

1. **`.traction-bundle` layout (S3, protocol §5).** The archive is `tar.zst` holding `bundle.json` (the 3a canonical JSON, now `bundle_schema: 2` with `frames` filled) plus `frames/<frame_id>.webp` and `frames/<frame_id>.salt` for unredacted frames only. This replaces §5's `manifest.json` + `buckets.jsonl` split: the server's verifier and every digest stay as they are. (Q12)
2. **Domain-tagged `frame_hash` (S1, protocol §4; `steps.md` Step 3 Build 2).** `frame_hash = SHA256("traction/v1/frame\0" || webp_bytes || salt)` instead of `SHA256(image_bytes || salt)`, so every hash in the format is domain-separated. (Q8)
3. **macOS real-capture proof is local (S1/S2).** GitHub `macos-latest` runners cannot grant Screen Recording (TCC is SIP-protected). On macOS, CI runs every step after the grab against real recorded WebP fixtures, and checks that the real ScreenCaptureKit path returns a clean `permission_denied` status, not a panic. The real grab and the macOS Gate 3 run are `#[ignore]`d tests that the human runs once locally. The result is recorded with date and OS version in `docs/steps.md`. Linux (Xvfb) and Windows run real capture in CI. (Q14)

## Terminology (read first)

| Wording | Canonical term | Notes |
|---|---|---|
| Screenshot, snapshot | **Frame** (new) | One 720p lossy WebP of the focused display, encrypted in the Vault. Its **Frame Record** (id, hash, timing, trigger; no pixels) lives in SQLite and is sealed into the chain. |
| Visual proof | **Visual Proof Mode** (new) | Opt-in capture of Frames, off by default. The only path that ever prompts for macOS Screen Recording. |
| Vault | **Vault** (new) | `<data-dir>/vault/`: one encrypted file per Frame plus `vault.key`. |
| Server jitter seed | **Capture Nonce** (new) | A per-device server secret delivered in the heartbeat reply, from which the daemon derives its scheduled capture times. The server rechecks those times at submission. |
| Expired frame | **Expired Frame** (new) | A Frame whose pixels retention deleted. Exported like a Redaction but counted separately. |

S1 adds these to `GLOSSARY.md` and extends "Audit Bundle" and "Redaction".

## Problem Statement

After Step 4, a submitted Audit Bundle proves *when* buckets were sealed and that nobody rewrote them, but nothing shows *what was on screen*. A forked daemon can synthesize plausible buckets, and a reviewer has no visual check. `frame_digest` is signed into every link as 32 zero bytes, the bundle's `frames` is `[]`, `JevEngine`'s image path has never seen a real frame, and the user has no way to look at, redact or package what would be submitted. Step 3 Exit Gate items 2–4 are open and block Step 5.

## Architecture invariants (apply to every slice)

- **The daemon never holds pixels.** Grabbing, downscaling, dHash, WebP encoding, salting, hashing and encryption run in a short-lived child process, `traction-daemon capture-frame`, which exits after one Frame (Q2). The daemon's idle RSS budget (<15 MB, measured in 1e) excludes it by construction.
- **Single writer.** The child writes only its encrypted Vault file. It never opens SQLite. It reports on stdout, and the daemon's writer thread inserts the Frame Record.
- **No text in frame metadata.** Frame Records hold ids, hashes, timestamps and closed enums (`trigger`, `category`, `schedule`). Window titles are read in memory for the denylist and dropped. A canary test proves no title, path or file name reaches SQLite, the Vault index or the bundle (Q5).
- **Fail closed.** If capture is off, permission is missing, the title can't be read where the denylist needs it, or focus changed during the grab, no Frame is taken. A missing Frame is never an error in the chain: a window with no Frames keeps 32 zero bytes.
- **Core owns the format.** The Frame Record type, `frame_hash`, `frame_digest`, the `bundle_schema: 2` types, archive reading and frame verification live in `traction-core` and are used by the daemon, the `verify` CLI and the server (rule 5).
- **No new chain format.** Link Payload and Header don't change. `frame_digest` was already signed. 3a chains and visual-proof-off devices stay valid unchanged.

## Solution

### S1: Capture tracer (child process → Vault → Frame Record → sealed `frame_digest`)

The thinnest real path: one scheduled Frame on each OS ends up in a sealed Chain Link.

**Crates (addition 3).** Each must build on stock `macos-latest`, `ubuntu-latest` and `windows-latest` (MSVC) with no system packages. S1's first commit proves it in CI before building anything on top.
- macOS: `objc2-screen-capture-kit` (`SCScreenshotManager` single-shot, macOS 14+). It's in the same `objc2` family as the existing `objc2-app-kit`. Older macOS reports `unsupported`.
- Windows: GDI `BitBlt` through the existing `windows-sys` (new features only).
- Linux X11: `x11rb` `GetImage` (existing dep). Wayland reports `unsupported` and never tries the portal.
- WebP: the `webp` crate (bundled libwebp built by `cc`, lossy, quality set in config).
- AEAD: `chacha20poly1305` (RustCrypto, pure Rust), XChaCha20-Poly1305.
- If a crate fails this check, the slice stops and raises it to the human with the failing runner log. It never switches to an unvetted crate silently.

**`traction-daemon capture-frame` (internal subcommand, Q2/Q3, addition 2).** Arguments: `--data-dir`, `--display <id>`, `--prev-dhash <16 hex | none>`, `--trigger <scheduled|turn_start|tool_fail|focus_shift>`, `--scheduled-at-ms`, `--nonce-id <id|none>`, `--expect-focus <opaque focus token>`. Steps:
1. Grab the display holding the focused window. Re-check that focus still matches `--expect-focus` right after the grab. If it doesn't, discard (`focus_changed`).
2. Downscale to 720p height (never upscale). Compute the 64-bit dHash (9×8 grayscale). If the Hamming distance to `--prev-dhash` is < 3, stop before encoding: `{"status":"duplicate","dhash":"…"}`.
3. Lossy WebP encode, draw a 16-byte salt, compute `frame_hash`.
4. Encrypt `salt || webp` into `vault/<frame_id>.bin` (written to a temp file then renamed) under `vault.key`, with `frame_id || frame_hash` as associated data.
5. Print one JSON line: `{"status":"written","frame_id":…,"frame_hash":…,"dhash":…,"captured_at_ms":…,"width":…,"height":…}`. Any other outcome is `{"status":"<closed code>"}` (`permission_denied`, `unsupported`, `focus_changed`, `error`), with no free text.

The daemon kills the child after a hard timeout (constant, default 10 s), keeps the last `dhash` in memory (8 bytes), and hands the record to the writer.

**Vault key (Q7).** `vault.key` is 32 random bytes created on first enable, with the existing `owner_only` permissions. `protocol.md` states the honest limit: it guards copied folders and backups, not same-user malware.

**Frame Record and `frame_digest` (Q8).** A new append-only table `frame` (`frame_id`, `window_start_ms`, `captured_at_ms`, `frame_hash`, `trigger`, `scheduled_at_ms`, `nonce_id`, `width`, `height`, `pixels_state` ∈ `present|redacted|expired`). `frame_id = "<captured_at_ms>-<2-digit counter>"`. The signed part of a record is `FrameRecord { frame_id, frame_hash, captured_at_ms, trigger, scheduled_at_ms, nonce_id }`. `pixels_state` is deliberately outside the digest, because Redaction and expiry happen after sealing.

```text
frame_digest = SHA256("traction/v1/frames\0" || canonical_json([FrameRecord...] sorted by frame_id))   # Frames whose captured_at_ms is in the link's window
             = 32 zero bytes when the window has no Frames
```

**Sealing.** The finalizer seals a window only once no capture child for that window is still running (bounded by the child timeout). A Frame reported after its window was sealed is discarded: its Vault file is deleted and it is never re-sealed. Local `verify --data-dir` recomputes `frame_digest` from the `frame` table (Gate 1 records: a tampered, deleted or inserted record is `payload_hash_mismatch part=frames`).

**Scheduling in S1.** Visual Proof Mode is a config flag plus `traction-daemon visual-proof enable|disable|status` (full permission flow in S2). Scheduled captures every 60–90 s from a local CSPRNG (`nonce_id: null`, reported as `local_jitter`), only while a Project is active (S2 refines this).

**Tests.** Real capture on Linux (Xvfb) and Windows: a capture produces a decryptable Vault file whose bytes hash to the sealed record. macOS CI: everything after the grab, using committed real WebP fixtures, plus `permission_denied` returned cleanly. `#[ignore] macos_real_screencapturekit_grab` for the human. A duplicate screen is skipped before encoding. A killed or hung child leaves no record and no orphan file. The canary test covers `frame`. A wakeup/RSS guard confirms the daemon process never maps libwebp code paths at idle (no capture in the daemon process).

### S2: Capture policy (who, when, never)

**Permission and platforms (Q6).** `visual-proof enable` is the only code path that calls `CGRequestScreenCaptureAccess`. Install, startup and every other command never prompt (a test greps for the call site). `status` reports `on|off|permission_missing|unsupported(wayland|macos_version)`. While it's off: no children, an empty Vault, zero `frame_digest`.

**When a Frame is allowed (Q4).** All must hold: Visual Proof Mode is on; a Project is **active**, meaning a `FILE_MUTATION` or Harness event for a tracked Project in the last `CAPTURE_ACTIVE_WINDOW` (one documented constant, 5 min); the focused app's category is `ide`, `terminal`, `browser` or `ai_desktop` (never `other`); the denylist passes; the session isn't paused (`/stop-track`, tray or CLI pause).

**Denylist (Q5).** Password managers are matched by app identity in the category table (1Password, Bitwarden, KeePass/KeePassXC, plus a documented list). Private/incognito windows and `.env` focus are matched on the title in memory against a fixed, documented pattern set. If the title can't be read for a `browser`, `ide` or `terminal` window, capture fails closed. The check runs in the daemon before spawning, and the child's focus re-check closes the race.

**Triggers and rate (Q10).** Scheduled captures, plus `TURN_START`, `TOOL_FAIL` and category-changing focus shifts. One shared floor of one Frame per 20 s (`CAPTURE_MIN_GAP`). dHash dedup applies to every trigger.

**Capture Nonce issuance (Q9a).** The heartbeat reply gains an optional `capture_nonce { id, nonce, issued_at_ms }`, where `nonce` is 32 random bytes in lowercase hex. The server rotates it daily per device and keeps every issued nonce (new table, owner-only, deleted by `DELETE /v1/me`).

**Schedule (exact, shared vectors in `traction-core`; choice E).** One function in `traction-core` is used by both the daemon and the server:

```text
mac      = HMAC-SHA256(key = nonce (32 raw bytes), msg = tₖ as u64 big-endian (8 bytes))
interval = 60_000 + (u64::from_be_bytes(mac[..8]) % 30_000)          # ms, in [60_000, 89_999]
tₖ₊₁     = tₖ + interval
t₀       = the first 120_000 ms-aligned window boundary strictly after the daemon received the nonce
```

Each scheduled Frame records `scheduled_at_ms = tₖ` and `nonce_id`. A skipped slot (inactive, denylisted, duplicate, failed child) still advances the schedule. A new nonce takes effect at its own `t₀`, and the old schedule stops at that boundary.

**Before the first nonce, and offline.** Until a device has ever received a nonce (a fresh install, or never paired), the schedule is **local jitter**: `interval = 60_000 + (u64::from_be_bytes(r[..8]) % 30_000)` with `r` from the OS CSPRNG, `t₀` = the first window boundary after Visual Proof Mode is turned on, and `nonce_id: null`. Offline, the daemon keeps using its most recent nonce for up to `CAPTURE_NONCE_MAX_AGE` (7 days after `issued_at_ms` by the daemon's clock), because the server still holds it. After that it falls back to local jitter until a fresh nonce arrives. The nonce lives only in daemon memory and an owner-only data file, and never appears in a bundle.

**Retention (Q11).** A daily job, run by the writer, deletes Vault files older than `VAULT_RETENTION` (45 days) and sets `pixels_state = expired`. It skips pinned Projects (`traction-daemon vault pin|unpin <project_id>`) and any Project with a pending submission, and enforces `vault_max_bytes` (default 2 GB) oldest-first. Frame Records are never deleted.

**Tests.** Gate 3 on Linux Xvfb and Windows with real windows (a window with a password-manager identity; browser windows titled with each private-mode pattern; an editor titled `… .env …`): zero Frames over a scheduled run. On macOS, the same policy is tested against recorded focus snapshots, and the real-focus run is `#[ignore]`d (deviation 3). Further tests: `other` is never captured; no Project activity, no Frames; the 20 s floor; the deterministic schedule from a fixed nonce (shared vectors in `traction-core`); retention expires, pins and caps correctly; there is no Screen Recording call outside `enable`.

### S3: `.traction-bundle`, Redaction, server frame verification, Jev frames

**Archive (Q12, deviation 1).** `export-bundle --project <id> --out <file.traction-bundle> [--redact <frame_id>]...` writes `tar.zst` containing:
- `bundle.json`: canonical JSON, `bundle_schema: 2`. `frames` holds `{ record: FrameRecord, redacted: bool, reason: null | "user" | "expired" }` for every Frame of the exported windows.
- `frames/<frame_id>.webp` and `frames/<frame_id>.salt` for each unredacted Frame only (decrypted from the Vault during export).
- The manifest gains `frames_digest` (`SHA256("traction/v1/manifest\0" || canonical_json(frames))`), `frame_count`, `redacted_count` and `expired_count`, and stays signed.
- The JSON-only `.traction-bundle.json` export stays for `verify` and for 3a compatibility. `verify` reads `bundle_schema` 1 and 2, in either container.

**Redaction (Q12, Gate 2; choice A).** `--redact` (and S4's UI) persists the choice in a new `frame_redaction` table and **permanently deletes that Frame's Vault `.bin` file** (pixels and salt) through the writer, while the SQLite Frame Record stays, so `frame_digest` stays verifiable. This needs explicit confirmation in the UI and the CLI flag says so.

**What becomes `redacted` (choice B, one explicit rule).**
- **Before export, the daemon decides.** A Frame whose Vault file is gone or unreadable at export time is exported as `{redacted: true}` with no files. Its `reason` is `"expired"` if retention deleted it (`pixels_state = expired`), and `"user"` in every other case: a user Redaction, a file deleted by hand, or an undecryptable file. User cases count in `redacted_user_pct`.
- **After export, the archive is fixed.** An entry with `redacted: false` must have both `frames/<id>.webp` and `frames/<id>.salt`, and they must hash to its record. Stripping or changing them after export **fails** verification (`frame_missing`, `frame_hash_mismatch`) because the archive was tampered with. The signed manifest's `frames_digest` covers every entry's `redacted` and `reason`, so flipping an entry to `redacted: true` after export fails too (`manifest_frames_mismatch`, or `manifest_signature_invalid` if the manifest was edited to match).

**Core verification (rule 5).** `verify_bundle` gains frame checks after the chain checks, each with a stable code: every listed record is in its link's `frame_digest` (`payload_hash_mismatch part=frames`); `frames_digest` and the counts match (`manifest_frames_mismatch`); every unredacted entry has both files and its `frame_hash` recomputes from them (`frame_missing`, `frame_hash_mismatch`); no file lacks an entry (`frame_unexpected`). The report gives `redacted_user_pct` and `expired_pct`, computed from entries and never taken from the manifest. A `reason: "expired"` claim on a Frame newer than 45 days before `exported_at_ms` is counted as a user redaction (`expired_claim_too_young`, advisory).

**Archive hardening (server and CLI).** Size-capped decompression (decompressed total ≤ `blob.max_bytes` × a documented factor), an entry-count cap, and only the exact names `bundle.json` and `frames/<frame_id>.{webp,salt}` (ids matching the record grammar). No paths, links, `..` or duplicates. Anything else is `malformed_bundle`.

**Server.** `submit` always sends the archive, under blob key `bundles/<id>.traction-bundle`. `complete` unpacks it in memory under the caps, runs the core verifier against pinned keys and stored Anchors (unchanged), and stores `frame_count`, `redacted_user_pct` and `expired_pct` with the verification summary. Then it does two new things:
- **Capture schedule check (Q9a).** For each scheduled Frame with a `nonce_id`, the server recomputes the schedule from the stored nonce and flags `frame_timing_mismatch` if `scheduled_at_ms` isn't on it or `captured_at_ms` is more than `capture_timing_tolerance_ms` after it. It's Yellow when its share passes a `[heuristic]` threshold, and never Red on its own. A `nonce_id` the device was never issued is `frame_timing_mismatch` too. `local_jitter` frames are counted and shown, not flagged.
- **Jev frames (addition 1).** `TriageInput.frames` gets the verified, unredacted WebP bytes, sampled evenly across the submission's timeline up to `[jev] max_frames`. This replaces `frames: Vec::new()` in `submission.rs`. The `failed(...)` path keeps no frames. Jev's existing 280 px skip, `frame_too_small_skipped`, the all-skipped → Yellow rule and the Danger-Zone header are unchanged. Frames never go out unless the bundle verified, and never on Red.

**Tests (Gate 2, Gate 4).**
- Core vectors for `frame_hash`/`frame_digest`.
- Red-team rows: swap two Frames' files (`frame_hash_mismatch`); flip a byte in a WebP or salt (`frame_hash_mismatch`); delete an unredacted Frame's files (`frame_missing`); drop an entry (`manifest_frames_mismatch`) or a record from the digest (`payload_hash_mismatch part=frames`); add a file (`frame_unexpected`); strip an unredacted Frame's files from an exported archive and flip its entry to `redacted: true` (`manifest_frames_mismatch`; re-signing with another key is `manifest_signature_invalid`); delete a Vault file *before* export (exports as `reason: "user"`, verifies, counted in `redacted_user_pct`); a zip-bomb or `../` entry (`malformed_bundle`). Each runs through the CLI and through the server's `complete` on Postgres + versitygw.
- Gate 2: redact 3 of N real Frames, then assert that none of their bytes or salts occurs anywhere in the archive's decompressed stream, that verification passes, and that `redacted_user_pct = 3/N`.
- The schedule check passes an honest daemon and flags shifted times and unknown nonces.
- `tests/jev.rs` gets a submission-path test showing real verified frames reach the request with the header, and none on Red.
- `e2e_daemon.rs` submits a real daemon's archive with frames.

### S4: Review & Redact UI (`/submit`)

**Session (Q13, addition 4).** `traction-daemon submit --review --project <id>` asks the running daemon (bearer token over the bus) for a one-time 128-bit code, builds `http://127.0.0.1:41937/submit?k=<code>`, and opens it with the existing `open_in_browser`. It first checks the URL with `traction_core::secure_http_url` and refuses to open anything that fails, so the URL policy is enforced at the call site and not only inside the opener. The daemon swaps the code (single use, 60 s to redeem) for an `HttpOnly; SameSite=Strict; Path=/submit` cookie valid for 30 minutes. The existing Host and Origin checks still apply to every `/submit` request. A same-origin `Origin` is allowed only on `/submit` routes with the session cookie. `--no-browser` prints the URL.

**UI.** Vanilla HTML/JS/CSS embedded in the daemon binary (no deps, no build step, nothing in `apps/web` or `packages/ui`). It shows a filmstrip of one Project's Frames by time, with expired and redacted badges. Frames are decrypted on demand per request and served with `Cache-Control: no-store`. Clicking a Frame toggles redaction, and the page shows live `% redacted` (user) and `% expired`. Confirming applies the redactions (irreversible, as in S3). **Export** writes the archive to `<data-dir>/exports/<project>-<ts>.traction-bundle` and shows where. **Submit** runs the existing submit flow with that archive. The daemon serves nothing under `/submit` while no session exists, and zero RAM is held while the page is closed (frames are never cached in the daemon).

**Tests.** The code is single-use and expires; requests with no cookie, a foreign Origin or a bad Host are refused; `secure_http_url` rejects a non-loopback `http://` before opening; every decrypted-frame response has `no-store`; redacting 3 Frames through the HTTP API and then exporting gives the Gate 2 result; the canary covers the UI's JSON endpoints (no titles or paths).

## Testing seams, by slice

| Slice | Real | Fixture |
|---|---|---|
| S1 | capture on Xvfb and Windows, libwebp, AEAD, the writer, sealing, `verify --data-dir` | recorded real WebP frames on macOS CI |
| S2 | real windows and titles on Xvfb and Windows, the heartbeat nonce against a local test server, retention on a real Vault | recorded focus snapshots on macOS CI |
| S3 | the archive, core verification, the server on Postgres + versitygw, the Jev request against the local `v1/systemone` server | — |
| S4 | the daemon's HTTP server, cookies, the browser-facing API (no real browser needed) | — |

## Policy constants

Daemon (documented in `protocol.md`): `CAPTURE_ACTIVE_WINDOW` 5 min, `CAPTURE_MIN_GAP` 20 s, scheduled interval 60 000–89 999 ms, `DHASH_SKIP_DISTANCE` 3, `CAPTURE_CHILD_TIMEOUT` 10 s, `CAPTURE_NONCE_MAX_AGE` 7 days, `VAULT_RETENTION` 45 days, `vault_max_bytes` 2 GB (config), WebP quality (config), the 720p target height. Server config: Capture Nonce rotation (1 day), `capture_timing_tolerance_ms`, the `frame_timing_mismatch` Yellow share, the archive decompression factor and entry cap.

## Documentation changes

- `docs/steps.md` Step 3: deviations 1–3; the offline item marked as met by Step 4 S1 with test references; gate items ticked only at human sign-off.
- `docs/protocol.md` §3: `capture_nonce` in the heartbeat reply. §4: `frame_hash` (deviation 2), `FrameRecord`, `frame_digest`, the schedule formula, Redaction and Expired Frames (replacing the 3a placeholder paragraph), the Vault key's honest limit. §5: `bundle_schema: 2`, the `tar.zst` layout (deviation 1), frame verification codes, archive caps.
- `docs/roadmap.md` Step 3: rewritten to these four slices.
- `GLOSSARY.md`: the terms above.
- ADR `0007-server-seeded-capture-schedule.md`: why the seed is checkable and what it does and doesn't prove (a forked daemon also gets the nonce, but because Frames are sealed and anchored within ~2 minutes of their scheduled time, staged screens have to exist in real time).

## User Stories

1. As a user, nothing asks me for screen permission until I turn on visual proof myself.
2. As a user, no Frame is ever taken while my password manager, a private browser window or a `.env` file has focus, or while I'm in a non-dev app.
3. As a user, I can look at every Frame before submitting, redact any of them for good, and see exactly what share I redacted.
4. As a user on a 10-year-old laptop, visual proof adds nothing to the daemon's idle memory.
5. As a reviewer, I know the Frames I see are the ones sealed and anchored at the time, taken on a schedule the server chose, and I can tell user redactions from old frames that expired.
6. As Fraud Squad, Jev's image hint runs on real verified frames, never on unverified or Red submissions.

## Out of Scope

- Reviewer-side frame viewing, the Frame Scrubber and static/blank-screen detection (Step 5).
- OS keychain storage for `vault.key`; Wayland portal capture; capturing more than the focused display.
- A real browser test of the `/submit` page (an HTTP-level test is enough here).
- 1d, 1e and everything in Step 5 (`.scratch/step-5/notes.md`).

## Left to tickets (not settled in the grill)

- The exact private-mode title patterns per browser and the password-manager identity list (S2 documents both).
- Whether GDI `BitBlt` returns real (non-black) pixels on the GitHub `windows-latest` session. S1 checks this in CI first, before S2 starts. If it doesn't, S1 raises it to the human before any deviation is written.
- The WebP quality default that hits ~15–30 KB at 720p on the recorded fixtures (S1 measures it).
- The archive decompression factor and entry cap (S3 sets and documents them).

## Resolved Decisions (human, 2026-11-27)

- **Q1:** one spec, 4 tracer slices; 3c's offline item is met by Step 4 S1.
- **Q2:** capture in a short-lived `capture-frame` child; the daemon only schedules. **Addition 2:** the child takes `--prev-dhash` and returns the new dHash or a `duplicate` status.
- **Q3:** native backends (ScreenCaptureKit `SCScreenshotManager`, GDI `BitBlt`, `x11rb` `GetImage`) plus lossy `webp` (libwebp via `cc`); not `xcap`. **Addition 3:** S1 proves all of them build on stock runners first.
- **Q4:** focused display only, 720p, an active Project within 5 min, categories `ide`/`terminal`/`browser`/`ai_desktop` only.
- **Q5:** denylist by app identity plus in-memory title patterns, fail closed, closed-enum metadata, canary test.
- **Q6:** off by default; `visual-proof enable` is the only prompt path; Wayland reports `unsupported`.
- **Q7:** per-Frame XChaCha20-Poly1305 files under an `owner_only` `vault.key`; honest limit documented.
- **Q8:** domain-tagged `frame_hash`, `frame_digest` over sorted Frame Records per window, zero when empty, content-free `frame_id`.
- **Q9:** (a) a server-issued Capture Nonce, an HMAC-derived schedule, rechecked at submission as `frame_timing_mismatch` (Yellow, never Red); `local_jitter` offline.
- **Q10:** `TURN_START`, `TOOL_FAIL` and focus-shift triggers; a 20 s floor; dHash < 3 skipped.
- **Q11:** records kept forever; pixels expire at 45 days except when pinned or pending; a 2 GB cap; `expired` counted apart from user redactions.
- **Q12:** `tar.zst` with `bundle.json` plus frame files; manifest `frames_digest` and counts; the server computes `% redacted`. **Addition 5:** deviations recorded in `steps.md`/`protocol.md`.
- **Q13:** a one-time code → `SameSite=Strict` cookie, `no-store`, vanilla embedded UI. **Addition 4:** `submit --review` checks the URL with `traction_core::secure_http_url` before `open_in_browser`.
- **Q14:** real capture in CI on Linux (Xvfb) and Windows; macOS post-grab fixtures plus a local `#[ignore]`d ScreenCaptureKit proof, recorded as a deviation.
- **Addition 1:** S3 feeds verified unredacted frames into `JevEngine` through `TriageInput.frames` (the engine is `JevEngine`; no type is named `SystemOneEngine`).

### Choices made while writing this spec (approved 2026-11-27)

- **A. Redaction deletes the Vault file irreversibly**, rather than only leaving the Frame out of exports. It's the stronger privacy choice, and the UI confirms before applying it.
- **B.** Redaction and expiry stay outside the signed `frame_digest`, because they happen after sealing. A Vault file missing before export becomes `reason: "user"` and counts in `redacted_user_pct`. Stripping files or flipping `redacted` after export fails verification, because the manifest's `frames_digest` covers each entry (see S3).
- **C. `bundle_schema` bumps to 2.** The chain format doesn't change, and `verify` still reads schema 1.
- **D. The server accepts only the archive from `submit`.** The JSON-only bundle stays for offline `verify`.
- **E.** `interval = 60_000 + u64::from_be_bytes(HMAC-SHA256(nonce, tₖ as u64 BE)[..8]) % 30_000`. `t₀` is the first window boundary after receipt, and skipped slots still advance the schedule. Before the first nonce, local jitter uses the same reduction. Offline, the last nonce is used for up to 7 days (see S2).
- **F. ADR 0007** for the server-seeded capture schedule.
