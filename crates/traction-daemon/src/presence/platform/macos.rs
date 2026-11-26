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

#[cfg(test)]
mod tests {
    use super::*;

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
