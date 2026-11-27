# 04: S4 Review & Redact UI at `/submit`

**What to build:** This ticket adds the daemon-served Review & Redact page:
- `submit --review` opens a one-time-code URL, checked by `traction_core::secure_http_url`;
- the code becomes a strict session cookie;
- a vanilla, dependency-free filmstrip lets the user redact Frames (permanently, through the 03 path), see live `% redacted` and `% expired`, export the `.traction-bundle`, and submit it.

It repeats Gate 2 through the HTTP API. See spec "S4: Review & Redact UI", "Testing seams" (row S4) and Resolved Decisions Q13, addition 4 and choice A.

**Blocked by:** 03

**Status:** ready-for-agent

Acceptance:

**Session (Q13, addition 4)**
- [ ] `traction-daemon submit --review --project <id> [--no-browser]` asks the running daemon, over the bus with the bearer token, for a one-time 128-bit code, and builds `http://127.0.0.1:41937/submit?k=<code>`.
- [ ] It checks that URL with `traction_core::secure_http_url` at the call site and refuses (exit 1, value-free message) if the check fails, before calling the existing `open_in_browser`. `--no-browser` prints the URL instead.
- [ ] The code is single-use and redeemable for 60 s. It's swapped for an `HttpOnly; SameSite=Strict; Path=/submit` cookie valid for 30 minutes.
- [ ] The existing Host and Origin checks apply to every `/submit` request. A same-origin `Origin` is accepted only on `/submit` routes carrying a valid session cookie.
- [ ] With no session, `/submit` serves nothing but a "run `traction-daemon submit --review`" page.

**UI (vanilla HTML/JS/CSS embedded in the daemon binary; nothing in `apps/web` or `packages/ui`)**
- [ ] A filmstrip of one Project's Frames ordered by time, with `REDACTED` and `EXPIRED` badges.
- [ ] Frames are decrypted on demand per request and served with `Cache-Control: no-store`. The daemon never caches decrypted frames, and holds nothing while the page is closed.
- [ ] Clicking toggles a pending redaction. The page shows live `% redacted (user)` and `% expired`.
- [ ] **Apply** asks for explicit confirmation, then permanently deletes the Vault files through the 03 Redaction path (Frame Records stay).
- [ ] **Export** writes `<data-dir>/exports/<project>-<ts>.traction-bundle` and shows the path. **Submit** runs the existing submit flow with that archive.
- [ ] The JSON endpoints return only ids, timestamps, closed enums, counts and percentages (no titles, paths or file names).

**Tests (spec "Testing seams" S4; HTTP level, no real browser)**
- [ ] The code is single-use and expires after 60 s. The cookie expires after 30 min.
- [ ] Each is refused: a request with no cookie, a foreign `Origin`, a bad `Host`.
- [ ] `secure_http_url` rejects a non-loopback `http://` URL before `open_in_browser` is reached.
- [ ] Every decrypted-frame response carries `no-store`.
- [ ] **Gate 2 via the API:** redact 3 Frames through the HTTP API, export, and get the same result as 03's Gate 2 test (no bytes or salts left, `verify` passes, `redacted_user_pct == 3/N`).
- [ ] `derived_tables_no_raw_text.rs` (or a sibling) covers the UI's JSON endpoints.

**Close-out**
- [ ] Run `/code-review` over 01–04. Fill in the Step 3 Exit Gate evidence in `docs/steps.md`, with test names and CI run, for items 1–4. Items are ticked only at human sign-off.
- [ ] List the human's local macOS `#[ignore]` runs (`macos_real_screencapturekit_grab`, `macos_real_focus_denylist`) as pending sign-off items.

## Comments
