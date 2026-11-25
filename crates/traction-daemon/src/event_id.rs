//! Fresh Traction Event IDs for events the daemon itself builds (watcher
//! mutations, mapped Harness hooks).

use std::sync::atomic::{AtomicUsize, Ordering};

/// A fresh event ID for `ms`: [`traction_core::uuid_v7`] with OS randomness.
pub fn fresh_event_id(ms: u64) -> String {
    let mut random = [0u8; 10];
    if getrandom::fill(&mut random).is_err() {
        // No OS randomness: still unique enough within one daemon.
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed) as u64;
        random[2..].copy_from_slice(&n.to_be_bytes());
    }
    traction_core::uuid_v7(ms, random)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_ids_at_the_same_millisecond_differ() {
        assert_ne!(fresh_event_id(1), fresh_event_id(1));
    }
}
