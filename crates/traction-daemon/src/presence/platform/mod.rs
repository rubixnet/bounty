//! One backend per OS, chosen at compile time.

use traction_core::TelemetryTier;

use super::Backend;
use super::aggregate::Sample;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;

/// The backend for this OS. Never fails: a platform that cannot observe
/// anything gets [`Unavailable`] (tier `none`).
pub fn detect() -> Box<dyn Backend> {
    #[cfg(target_os = "macos")]
    return Box::new(macos::MacBackend::new());
    #[cfg(windows)]
    return Box::new(windows::WindowsBackend::new());
    #[cfg(target_os = "linux")]
    return linux::detect();
    #[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
    return Box::new(Unavailable);
}

/// No usable presence source (no display, an unsupported OS, a backend that
/// failed). Reports tier `none` and nothing else.
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only Linux can lack a display")
)]
pub struct Unavailable;

impl Backend for Unavailable {
    fn tier(&self) -> TelemetryTier {
        TelemetryTier::None
    }

    fn sample(&mut self) -> Sample {
        Sample::default()
    }
}

/// Milliseconds between two readings of a wrapping 32-bit millisecond tick
/// counter (Windows `GetTickCount`, wraps every ~49.7 days).
#[allow(
    dead_code,
    reason = "used by the Windows backend; unit-tested everywhere"
)]
fn tick_delta_ms(now: u32, last_input: u32) -> u64 {
    u64::from(now.wrapping_sub(last_input))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tick_deltas_survive_the_wrap() {
        assert_eq!(tick_delta_ms(5_000, 2_000), 3_000);
        assert_eq!(tick_delta_ms(100, u32::MAX - 899), 1_000);
        assert_eq!(tick_delta_ms(7, 7), 0);
    }

    #[test]
    fn an_unavailable_backend_is_tier_none_and_reads_nothing() {
        let mut backend = Unavailable;
        assert_eq!(backend.tier(), TelemetryTier::None);
        assert_eq!(backend.sample(), Sample::default());
    }
}
