//! Visual Proof Mode (Step 3b, S1). Everything that touches pixels runs in
//! the short-lived `traction-daemon capture-frame` child ([`child`]); the
//! daemon only schedules it ([`scheduler`]), reads its one JSON line
//! ([`runner`]) and hands the Frame Record to the single writer.

use std::time::Duration;

pub mod child;
pub mod grab;
pub mod image;
pub mod runner;
pub mod scheduler;
pub mod settings;
pub mod vault;

/// A new screen whose dHash is within this many bits of the previous
/// Frame's is a duplicate: skipped before encoding.
pub const DHASH_SKIP_DISTANCE: u32 = 3;

/// The daemon kills a `capture-frame` child after this long.
pub const CAPTURE_CHILD_TIMEOUT: Duration = Duration::from_secs(10);

/// Scheduled captures happen only while a Project had activity this recently
/// (S2 refines the policy).
pub const CAPTURE_ACTIVE_WINDOW: Duration = Duration::from_secs(5 * 60);

/// Shortest scheduled interval, and the jitter span above it:
/// `interval = 60_000 + (u64::from_be_bytes(r[..8]) % 30_000)` ms.
pub const SCHEDULE_BASE_MS: u64 = 60_000;
/// See [`SCHEDULE_BASE_MS`].
pub const SCHEDULE_JITTER_MS: u64 = 30_000;

/// Lossy WebP quality (0–100). Measured on 720p frames: see the ticket's
/// comment and `docs/protocol.md` section 4.
pub const DEFAULT_WEBP_QUALITY: f32 = 15.0;

/// The local-jitter interval for the 32 random bytes `r` (S1; before any
/// Capture Nonce).
pub fn local_jitter_interval_ms(r: &[u8; 32]) -> u64 {
    let head: [u8; 8] = r[..8].try_into().expect("8 bytes");
    SCHEDULE_BASE_MS + u64::from_be_bytes(head) % SCHEDULE_JITTER_MS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_jitter_stays_in_60_to_90_seconds() {
        let mut r = [0u8; 32];
        assert_eq!(local_jitter_interval_ms(&r), 60_000);
        r[..8].copy_from_slice(&29_999u64.to_be_bytes());
        assert_eq!(local_jitter_interval_ms(&r), 89_999);
        r[..8].copy_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(local_jitter_interval_ms(&r), 60_000 + u64::MAX % 30_000);
        r[8..].fill(0xFF);
        r[..8].fill(0);
        assert_eq!(
            local_jitter_interval_ms(&r),
            60_000,
            "only the first 8 bytes"
        );
    }
}
