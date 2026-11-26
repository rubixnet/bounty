# 01: Presence Sensor: content-free Presence Minutes on Windows, macOS and Linux

**What to build:** A `presence` module in `traction-daemon` with one backend per OS, chosen at compile time. It samples the OS idle timer, the foreground App Category and real-vs-injected input in memory. Once a minute it writes one **Presence Minute** (counts, ratios and closed enums only) through the single writer into a new append-only `presence_minute` table. The `PresenceMinute` and `AppCategory` types live in `traction-core` so 02 and the server share them. See spec "Slice 1: Presence Sensor" and "Resolved Decisions" 3–5.

**Blocked by:** none

**Status:** ready-for-agent

**Start with the two spikes** (time-boxed, findings recorded under `## Comments` before any backend code is final):

- **macOS:** does a prompt-free signal (no Input Monitoring, no Accessibility) separate `CGEventPost` input from hardware input, e.g. `CGEventSourceCounterForEventType` deltas `HIDSystemState` vs `CombinedSessionState`? Yes → `full`. No → the backend is `reduced` (idle + category), and the decision is recorded.
- **Windows:** does Raw Input (message-only window, `RIDEV_INPUTSINK`, keyboard + mouse) flag `SendInput` input with `RAWINPUTHEADER.hDevice == NULL`, reading only the header? Does it raise any Defender/AV detection on an unsigned build? Clean → `full`. Otherwise → `reduced` (`GetLastInputInfo` + foreground executable name).

Acceptance:

- [ ] `traction-core` defines `AppCategory` (`ide`, `terminal`, `browser`, `ai_desktop`, `other`, closed, serde snake_case), `TelemetryTier` (`full`, `reduced`, `idle_only`, `none`) and `PresenceMinute` (`minute_start_ms`, `key_events`, `pointer_events`, `injected_events`, `active_subslots` 0–6, `cadence_entropy_milli`, `category_sec`, `category_switches`, `idle_sec`, `telemetry_tier`). Fields a tier can't fill are `null`, never `0`. Canonical JSON round-trips and unknown fields are rejected.
- [ ] Category mapping is one compiled-in table per OS (macOS bundle IDs, Windows executable names, X11 `WM_CLASS`) that resolves to an `AppCategory` in memory. Anything unknown is `other`. The raw identifier is never stored, logged or kept past the lookup. Unit tests cover each table, including case and path handling of executable names.
- [ ] Pure aggregation (input timestamps → sub-slot counts, log-binned inter-event entropy with `MIN_CADENCE_SAMPLES`, category seconds and switches) is OS-independent and unit-tested. Raw intervals are dropped when the minute closes.
- [ ] **Windows:** `GetForegroundWindow` → owning process executable name, `GetLastInputInfo` idle. No `SetWindowsHookEx` of any kind anywhere in the crate (a test greps the sources for `WH_KEYBOARD_LL`, `WH_MOUSE_LL` and `SetWindowsHookEx` and fails on a hit). Raw Input only if the spike passed, reading only `RAWINPUTHEADER` (never `VKey`, `MakeCode`, `Flags` key data or mouse coordinates).
- [ ] **macOS:** `NSWorkspace.frontmostApplication` bundle ID, `CGEventSourceSecondsSinceLastEventType` idle, and the injection signal only if the spike passed. No call can trigger an Input Monitoring, Accessibility or Screen Recording prompt. A test asserts the backend never calls `CGEventTapCreate`, `AXIsProcessTrustedWithOptions`, `CGRequestListenEventAccess` or `CGPreflightScreenCaptureAccess`.
- [ ] **Linux X11:** `_NET_ACTIVE_WINDOW` → `WM_CLASS`, XScreenSaver idle, XInput2 raw events with an XTEST source device counted as injected. No `/dev/input` access.
- [ ] **Linux Wayland / no display:** `ext-idle-notify-v1`, else `org.freedesktop.ScreenSaver`, else none → `idle_only` / `none`. Category fields `null`. Any backend error lowers the tier and logs once, and the daemon never panics (a test starts the daemon with no `DISPLAY` and no `WAYLAND_DISPLAY`).
- [ ] `presence_minute` (migration through the existing `user_version` list) has the same no-update/no-delete triggers as `traction_event`. Rows are written through the single writer, one per wall-clock minute while the daemon runs. Nothing is written for minutes the daemon was down. Tray/session pause does not stop presence (it is device-wide, not Harness data).
- [ ] Real host-OS integration tests in 3-OS CI: idle and foreground category on every OS. Injection: Linux under Xvfb + `xdotool` is flagged as injected; Windows `SendInput` is flagged when Raw Input is `full`, else the backend reports `reduced`; on headless macOS a loud skip that prints `SKIPPED (macOS injection): <reason>`, which the CI workflow greps for and surfaces in the job summary. The skip is backed by unit tests of the macOS counter-delta logic with recorded inputs.
- [ ] Budget guard until 1e: a test runs the sensor for a fixed span with no input and asserts its wakeups stay at or below one per 10 s sub-slot plus OS notifications (no busy polling).
- [ ] The 1a "no raw text" canary test is extended to `presence_minute`: typing a canary string under `xdotool` (Linux) leaves no trace of it in SQLite or the daemon log.
- [ ] `docs/protocol.md` documents `PresenceMinute`, the tiers, the per-OS backends and the spike outcomes.

## Comments
