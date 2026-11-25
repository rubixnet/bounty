//! Traction Event IDs for events the daemon itself builds: fresh UUIDv7s
//! (watcher mutations, mapped Harness hooks) and deterministic IDs for
//! events read back from a Harness log, so a reread maps to the same ID and
//! the events table's `INSERT OR IGNORE` absorbs it.

use std::sync::atomic::{AtomicUsize, Ordering};

use sha2::{Digest, Sha256};

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

/// A deterministic event ID: the first 16 bytes of the SHA-256 of `parts`
/// (each length-prefixed, so no two part lists collide by concatenation),
/// formatted as a hyphenated UUID with the RFC 9562 version-8 ("custom")
/// and variant bits set.
pub fn derived_event_id(parts: &[&[u8]]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part);
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_ids_at_the_same_millisecond_differ() {
        assert_ne!(fresh_event_id(1), fresh_event_id(1));
    }

    #[test]
    fn derived_ids_are_stable_v8_uuids_that_separate_their_parts() {
        let id = derived_event_id(&[b"claude-code", b"s", b"u", b"0", b"TOOL_PRE"]);
        assert_eq!(
            id,
            derived_event_id(&[b"claude-code", b"s", b"u", b"0", b"TOOL_PRE"])
        );
        assert_eq!(id.len(), 36);
        assert_eq!(&id[14..15], "8");
        assert!(matches!(&id[19..20], "8" | "9" | "a" | "b"));
        assert_ne!(
            derived_event_id(&[b"ab", b"c"]),
            derived_event_id(&[b"a", b"bc"])
        );
        assert_ne!(
            id,
            derived_event_id(&[b"claude-code", b"s", b"u", b"1", b"TOOL_PRE"])
        );
    }
}
