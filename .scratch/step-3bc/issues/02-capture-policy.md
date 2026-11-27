# 02: S2 Capture policy: permission flow, active-Project rule, denylist, triggers, Capture Nonce schedule, retention

**What to build:** This ticket decides when a Frame may be taken, and makes sure none is ever taken during sensitive focus. It covers:
- the opt-in permission flow;
- the active-Project and category rules;
- the fail-closed denylist (app identity plus in-memory title patterns);
- event triggers under one rate floor;
- the server-issued Capture Nonce and the exact shared schedule;
- Vault retention with pins and a disk cap.

It owns Gate 3. See spec "S2: Capture policy", "Testing seams" (row S2), "Policy constants" and Resolved Decisions Q4–Q6, Q9–Q11, Q14 and choice E.

**Blocked by:** 01

**Status:** ready-for-agent

Acceptance:

**Permission and platforms (Q6)**
- [ ] `visual-proof enable` is the **only** code path that calls `CGRequestScreenCaptureAccess`. Install, startup and every other command never prompt. A test asserts there's exactly one call site.
- [ ] `visual-proof status` reports `on | off | permission_missing | unsupported(wayland | macos_version)`.
- [ ] Wayland never tries the portal.

**When a Frame is allowed (Q4)** — all of these must hold:
- [ ] Visual Proof Mode is on.
- [ ] A Project is active: a `FILE_MUTATION` or Harness event for a tracked Project within `CAPTURE_ACTIVE_WINDOW` (5 min, one documented constant).
- [ ] The focused app's category is `ide`, `terminal`, `browser` or `ai_desktop`. `other` is never captured.
- [ ] The denylist passes.
- [ ] The session isn't paused (`/stop-track`, CLI pause).

**Denylist (Q5)**
- [ ] Password managers are matched by app identity in the category table (1Password, Bitwarden, KeePass/KeePassXC, plus a documented list per OS).
- [ ] Private/incognito windows and focused `.env` files are matched by title, in memory only, against a fixed, documented pattern set per browser and editor. The title is dropped right after the check.
- [ ] **Fail closed:** if the title of a `browser`, `ide` or `terminal` window can't be read, no Frame is taken.
- [ ] The check runs in the daemon before spawning. The child's `--expect-focus` re-check (01) closes the race.
- [ ] No title, pattern match or app name is stored anywhere. Frame metadata stays closed-enum.

**Triggers and rate (Q10)**
- [ ] Triggers: scheduled slots, `TURN_START`, `TOOL_FAIL`, and focus shifts that change category.
- [ ] One shared floor, `CAPTURE_MIN_GAP` (20 s), across all triggers. dHash dedup applies to every trigger.

**Capture Nonce and schedule (Q9a, choice E)**
- [ ] The heartbeat reply gains an optional `capture_nonce { id, nonce (32 bytes hex), issued_at_ms }` (a `traction-core` sync wire type). The server rotates it daily per device and keeps every issued nonce in a new owner-only table, which `DELETE /v1/me` removes.
- [ ] One `traction-core` function, used by both daemon and server:
  `interval = 60_000 + (u64::from_be_bytes(HMAC-SHA256(key = nonce, msg = tₖ as u64 BE)[..8]) % 30_000)`, `tₖ₊₁ = tₖ + interval`, and `t₀` = the first 120 000 ms-aligned boundary strictly after receipt. It ships with shared test vectors.
- [ ] Each scheduled Frame records `scheduled_at_ms = tₖ` and `nonce_id`. Skipped slots (inactive, denylisted, duplicate, failed child) still advance the schedule. A new nonce starts at its own `t₀`, and the old schedule stops there.
- [ ] **Before the first nonce:** local jitter with the same reduction over CSPRNG bytes, `t₀` = the first boundary after enable, `nonce_id: null`.
- [ ] **Offline:** the last nonce is used for up to `CAPTURE_NONCE_MAX_AGE` (7 days after `issued_at_ms`), then local jitter until a fresh one arrives.
- [ ] The nonce is kept only in memory and an owner-only data file, and never appears in a bundle or log.

**Retention (Q11)**
- [ ] A daily job, run by the writer, deletes Vault files older than `VAULT_RETENTION` (45 days) and sets `pixels_state = expired`.
- [ ] It skips pinned Projects (`traction-daemon vault pin|unpin <project_id>`) and any Project with a pending submission.
- [ ] It enforces `vault_max_bytes` (default 2 GB, config) oldest-first.
- [ ] Frame Records are never deleted.

**Tests (spec "Testing seams" S2)**
- [ ] **Gate 3, Linux Xvfb and Windows, real windows**, each focused through a scheduled run with zero Frames asserted:
  - a window with a password-manager identity;
  - a browser window titled with each private-mode pattern;
  - an editor window whose title holds `.env`;
  - a `browser`/`ide`/`terminal` window whose title can't be read.
- [ ] **macOS CI:** the same policy runs against recorded focus snapshots. `#[ignore] macos_real_focus_denylist` is for the human (deviation 3).
- [ ] `other` is never captured. With no Project activity, there are no Frames. The 20 s floor holds across mixed triggers.
- [ ] Schedule vectors match between daemon and server. Skipped slots advance the schedule. The fallback before the first nonce is used. A nonce older than 7 days drops to local jitter.
- [ ] Retention expires, honours pins and pending submissions, and the cap removes oldest-first, all on a real Vault.
- [ ] A heartbeat against the local test server receives and persists a nonce.

**Docs**
- [ ] `docs/protocol.md` §3: `capture_nonce` in the heartbeat reply. §4: the schedule formula, the fallbacks and the capture constants.
- [ ] Write `docs/adr/0007-server-seeded-capture-schedule.md`: why the seed is checkable, and what it proves. A forked daemon also gets the nonce, but Frames are sealed and anchored within ~2 minutes of their scheduled time, so staged screens have to exist in real time.
- [ ] Record the denylist patterns and identity lists in a comment below.

## Comments
