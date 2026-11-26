# 01: Presence Sensor: content-free Presence Minutes on Windows, macOS and Linux

**What to build:** A `presence` module in `traction-daemon` with one backend per OS, chosen at compile time. It samples the OS idle timer, the foreground App Category and real-vs-injected input in memory. Once a minute it writes one **Presence Minute** (counts, ratios and closed enums only) through the single writer into a new append-only `presence_minute` table. The `PresenceMinute` and `AppCategory` types live in `traction-core` so 02 and the server share them. See spec "Slice 1: Presence Sensor" and "Resolved Decisions" 3–5.

**Blocked by:** none

**Status:** ready-for-agent

**Start with the two spikes** (time-boxed, findings recorded under `## Comments` before any backend code is final):

- **macOS:** does a prompt-free signal (no Input Monitoring, no Accessibility) separate `CGEventPost` input from hardware input, e.g. `CGEventSourceCounterForEventType` deltas `HIDSystemState` vs `CombinedSessionState`? Yes → `full`. No → the backend is `reduced` (idle + category), and the decision is recorded.
- **Windows:** does Raw Input (message-only window, `RIDEV_INPUTSINK`, keyboard + mouse) flag `SendInput` input with `RAWINPUTHEADER.hDevice == NULL`, reading only the header? Does it raise any Defender/AV detection on an unsigned build? Clean → `full`. Otherwise → `reduced` (`GetLastInputInfo` + foreground executable name).

Acceptance:

- [x] `traction-core` defines `AppCategory` (`ide`, `terminal`, `browser`, `ai_desktop`, `other`, closed, serde snake_case), `TelemetryTier` (`full`, `reduced`, `idle_only`, `none`) and `PresenceMinute` (`minute_start_ms`, `key_events`, `pointer_events`, `injected_events`, `active_subslots` 0–6, `cadence_entropy_milli`, `category_sec`, `category_switches`, `idle_sec`, `telemetry_tier`). Fields a tier can't fill are `null`, never `0`. Canonical JSON round-trips and unknown fields are rejected.
- [x] Category mapping is one compiled-in table per OS (macOS bundle IDs, Windows executable names, X11 `WM_CLASS`) that resolves to an `AppCategory` in memory. Anything unknown is `other`. The raw identifier is never stored, logged or kept past the lookup. Unit tests cover each table, including case and path handling of executable names.
- [x] Pure aggregation (input timestamps → sub-slot counts, log-binned inter-event entropy with `MIN_CADENCE_SAMPLES`, category seconds and switches) is OS-independent and unit-tested. Raw intervals are dropped when the minute closes.
- [ ] **Windows:** `GetForegroundWindow` → owning process executable name, `GetLastInputInfo` idle. No `SetWindowsHookEx` of any kind anywhere in the crate (a test greps the sources for `WH_KEYBOARD_LL`, `WH_MOUSE_LL` and `SetWindowsHookEx` and fails on a hit). Raw Input only if the spike passed, reading only `RAWINPUTHEADER` (never `VKey`, `MakeCode`, `Flags` key data or mouse coordinates).
- [x] **macOS:** `NSWorkspace.frontmostApplication` bundle ID, `CGEventSourceSecondsSinceLastEventType` idle, and the injection signal only if the spike passed. No call can trigger an Input Monitoring, Accessibility or Screen Recording prompt. A test asserts the backend never calls `CGEventTapCreate`, `AXIsProcessTrustedWithOptions`, `CGRequestListenEventAccess` or `CGPreflightScreenCaptureAccess`.
- [ ] **Linux X11:** `_NET_ACTIVE_WINDOW` → `WM_CLASS`, XScreenSaver idle, XInput2 raw events with an XTEST source device counted as injected. No `/dev/input` access.
- [ ] **Linux Wayland / no display:** `ext-idle-notify-v1`, else `org.freedesktop.ScreenSaver`, else none → `idle_only` / `none`. Category fields `null`. Any backend error lowers the tier and logs once, and the daemon never panics (a test starts the daemon with no `DISPLAY` and no `WAYLAND_DISPLAY`).
- [x] `presence_minute` (migration through the existing `user_version` list) has the same no-update/no-delete triggers as `traction_event`. Rows are written through the single writer, one per wall-clock minute while the daemon runs. Nothing is written for minutes the daemon was down. Tray/session pause does not stop presence (it is device-wide, not Harness data).
- [ ] Real host-OS integration tests in 3-OS CI: idle and foreground category on every OS. Injection: Linux under Xvfb + `xdotool` is flagged as injected; Windows `SendInput` is flagged when Raw Input is `full`, else the backend reports `reduced`; on headless macOS a loud skip that prints `SKIPPED (macOS injection): <reason>`, which the CI workflow greps for and surfaces in the job summary. The skip is backed by unit tests of the macOS counter-delta logic with recorded inputs.
- [x] Budget guard until 1e: a test runs the sensor for a fixed span with no input and asserts its wakeups stay at or below one per 10 s sub-slot plus OS notifications (no busy polling).
- [ ] The 1a "no raw text" canary test is extended to `presence_minute`: typing a canary string under `xdotool` (Linux) leaves no trace of it in SQLite or the daemon log.
- [x] `docs/protocol.md` documents `PresenceMinute`, the tiers, the per-OS backends and the spike outcomes.

## Comments

### Spike findings (2026-11-26, copied to `docs/protocol.md` section 2b)

- **macOS: no prompt-free injection signal, backend is `reduced`.** On macOS 26 with no Accessibility/Input Monitoring, 40 `CGEventPost` calls (session tap and HID tap) from an untrusted process were dropped by the OS: `CGEventSourceCounterForEventType` for `HIDSystemState` and `CombinedSessionState` both stayed flat, so a counter delta cannot separate injected from hardware input without a permission the product must not request. Idle (`CGEventSourceSecondsSinceLastEventType`) and `NSWorkspace.frontmostApplication` work with no prompt. The delta helper is kept and unit-tested for a future opt-in.
- **Windows: spike NOT run** (macOS dev host). Raw Input `hDevice == NULL` for `SendInput` and the Defender check on an unsigned build are unverified, so Windows ships `reduced`. Cross-checked only with `cargo check`/`clippy --target x86_64-pc-windows-gnu`.

### Built and verified

- macOS: `cargo fmt`, `clippy -D warnings` and `cargo test --workspace` green on the host (real idle and frontmost app read; reduced tier through a live daemon).
- Linux and Windows backends compile and pass `clippy -D warnings --all-targets` under `cargo check --target x86_64-unknown-linux-gnu` / `x86_64-pc-windows-gnu` (C builds stubbed with `CC=true AR=true`). Their tests were NOT run.

### Gaps (acceptance boxes left open)

- **Windows:** backend code (foreground exe, `GetLastInputInfo`) and the source-scan test exist but have never run on Windows; Raw Input spike not done.
- **Linux X11:** backend (XInput2 raw events, XTEST = injected, `WM_CLASS`, XScreenSaver idle) is unrun: no Xvfb/xdotool here. The `xdotool` injection + canary test (`tests/presence.rs`) and the CI Xvfb step are written but unexercised.
- **Linux Wayland:** `ext-idle-notify-v1` and `org.freedesktop.ScreenSaver` idle are NOT built; Wayland and no-display both report `none`. No-display start is tested (`tests/binary.rs`, unit tests).
- **3-OS CI:** only macOS was run locally; first CI run must confirm Windows and Linux.
- **Canary:** the "no raw text" extension runs only on Linux/Xvfb; the daemon-log half is structural (log messages are `&'static str`), not asserted.
- Seconds resolution is 10 s (category sampled once per sub-slot).
- `IDLE_THRESHOLD_SEC` = 30 and `MIN_CADENCE_SAMPLES` = 8 are my choices; Slice 2 should fold them into the `Ruleset`.
- **Review fix pass:** the macOS counter-delta helpers (`counter_delta`, `injected_from_counters`) and their unit tests were deleted as dead code; the spike result above stands (no prompt-free injection signal, macOS stays `reduced`).
