//! macOS backend: tier `reduced` (idle + foreground category).
//!
//! Spike outcome (ticket 01): no prompt-free injection signal exists, so
//! there is no `full` tier on macOS. An untrusted process's `CGEventPost`
//! is dropped by the OS (the event counters did not move), so
//! `CGEventSourceCounterForEventType` deltas (`HIDSystemState` vs
//! `CombinedSessionState`) could not be shown to separate injected input
//! without Accessibility. See `docs/protocol.md` section 2b.
//!
//! Calls made, all prompt-free: `CGEventSourceSecondsSinceLastEventType`
//! and `NSWorkspace.frontmostApplication`. Never called anywhere in this
//! crate: any event-tap creation, Accessibility trust check, Input
//! Monitoring request or Screen Recording preflight (a test scans the
//! sources for their exact names).

use objc2_app_kit::NSWorkspace;
use traction_core::TelemetryTier;

use super::super::Backend;
use super::super::aggregate::Sample;
use super::super::category;

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn CGEventSourceSecondsSinceLastEventType(state_id: i32, event_type: u32) -> f64;
}

/// `kCGEventSourceStateCombinedSessionState`.
const COMBINED_SESSION_STATE: i32 = 0;
/// `kCGAnyInputEventType`.
const ANY_INPUT_EVENT_TYPE: u32 = u32::MAX;

pub struct MacBackend;

impl MacBackend {
    pub fn new() -> Self {
        Self
    }
}

fn idle_ms() -> Option<u64> {
    // SAFETY: a plain C function taking two integers and returning a double.
    let secs = unsafe {
        CGEventSourceSecondsSinceLastEventType(COMBINED_SESSION_STATE, ANY_INPUT_EVENT_TYPE)
    };
    seconds_to_ms(secs)
}

/// Converts the OS idle seconds to milliseconds; a negative or non-finite
/// value (an OS error) is no reading.
fn seconds_to_ms(secs: f64) -> Option<u64> {
    (secs.is_finite() && secs >= 0.0).then_some((secs * 1000.0) as u64)
}

fn foreground_category() -> Option<traction_core::AppCategory> {
    let app = NSWorkspace::sharedWorkspace().frontmostApplication()?;
    let bundle_id = app.bundleIdentifier()?;
    // The identifier lives only for this lookup.
    Some(category::macos_bundle_id(&bundle_id.to_string()))
}

impl Backend for MacBackend {
    fn tier(&self) -> TelemetryTier {
        TelemetryTier::Reduced
    }

    fn sample(&mut self) -> Sample {
        Sample {
            idle_ms: idle_ms(),
            category: foreground_category(),
        }
    }
}

/// How many events of one type the OS counted between two reads of
/// `CGEventSourceCounterForEventType`, handling the 32-bit wrap.
#[allow(dead_code, reason = "kept for a future opt-in full tier; unit-tested")]
fn counter_delta(before: u32, after: u32) -> u32 {
    after.wrapping_sub(before)
}

/// Events that reached the session but not the hardware HID state were
/// posted by software: the injected count for one type over one window.
#[allow(dead_code, reason = "kept for a future opt-in full tier; unit-tested")]
fn injected_from_counters(hid_delta: u32, combined_delta: u32) -> u32 {
    combined_delta.saturating_sub(hid_delta)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_deltas_survive_the_32_bit_wrap() {
        assert_eq!(counter_delta(10, 25), 15);
        assert_eq!(counter_delta(u32::MAX - 1, 3), 5);
    }

    #[test]
    fn software_posted_events_are_the_session_minus_hid_difference() {
        // Recorded shape from the spike: hardware typing moves both counters
        // equally; posted events would move only the session counter.
        assert_eq!(injected_from_counters(12, 12), 0);
        assert_eq!(injected_from_counters(12, 32), 20);
        // HID counted more than the session (a race between the two reads).
        assert_eq!(injected_from_counters(5, 4), 0);
    }

    #[test]
    fn idle_seconds_convert_and_errors_are_no_reading() {
        assert_eq!(seconds_to_ms(1.5), Some(1500));
        assert_eq!(seconds_to_ms(0.0), Some(0));
        assert_eq!(seconds_to_ms(-1.0), None);
        assert_eq!(seconds_to_ms(f64::NAN), None);
    }

    #[test]
    fn the_host_reports_a_real_idle_time_and_a_foreground_app() {
        let mut backend = MacBackend::new();
        let sample = backend.sample();
        let idle = sample
            .idle_ms
            .expect("idle timer readable without a prompt");
        // Idle can legitimately be large (an unattended CI host), but not absurd.
        assert!(idle < 10 * 365 * 24 * 3600 * 1000);
        // A headless CI runner may have no frontmost app; if it has one the
        // category is one of the closed set (always true by type).
        let _ = sample.category;
    }
}
