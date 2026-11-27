//! Published vectors for `frame_hash` and `frame_digest` (Step 3b, spec Q8,
//! deviation 2). Cross-checked against an independent implementation
//! (Python `hashlib` + `json.dumps(sort_keys=True, separators=(",", ":"))`)
//! when committed. The server recomputes these, so a change is a format
//! break.

use traction_core::{
    FRAME_DIGEST_NONE, FrameRecord, FrameTrigger, LinkPayload, frame_digest, frame_hash_hex,
};

const WEBP: &[u8] = b"RIFF\x0c\x00\x00\x00WEBPVP8 ";
const SALT: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
const FRAME_HASH: &str = "14e0eb9eda7402f94eee31d299ad3d76a6a4920f142f218fc4022749256a1619";
const DIGEST_ONE: &str = "a46f133059b6ef5f35b92ad278e3e1fb109e7538a9ffcdb3178299f9adfedf34";
const DIGEST_TWO: &str = "6bd0d4b16d45e060afb793022dc9fcab32ff0eb3346ba040937a834b29f654c6";

fn records() -> [FrameRecord; 2] {
    [
        FrameRecord {
            frame_id: "1800000000123-00".into(),
            frame_hash: FRAME_HASH.into(),
            captured_at_ms: 1_800_000_000_123,
            trigger: FrameTrigger::Scheduled,
            scheduled_at_ms: Some(1_800_000_000_000),
            nonce_id: None,
        },
        FrameRecord {
            frame_id: "1800000060000-01".into(),
            frame_hash: "ab".repeat(32),
            captured_at_ms: 1_800_000_060_000,
            trigger: FrameTrigger::ToolFail,
            scheduled_at_ms: None,
            nonce_id: Some("n-7".into()),
        },
    ]
}

#[test]
fn frame_hash_is_domain_tagged_over_webp_then_salt() {
    assert_eq!(frame_hash_hex(WEBP, &SALT), FRAME_HASH);
    // Order matters and the tag separates it from a plain SHA-256.
    assert_ne!(frame_hash_hex(&SALT, WEBP), FRAME_HASH);
}

#[test]
fn frame_digest_vectors_sort_by_frame_id_and_zero_when_empty() {
    let [a, b] = records();
    assert_eq!(frame_digest(std::slice::from_ref(&a)), DIGEST_ONE);
    assert_eq!(frame_digest(&[a.clone(), b.clone()]), DIGEST_TWO);
    assert_eq!(
        frame_digest(&[b, a]),
        DIGEST_TWO,
        "input order is irrelevant"
    );
    assert_eq!(frame_digest(&[]), FRAME_DIGEST_NONE);
}

#[test]
fn a_payload_commits_to_its_frames_and_a_frameless_one_keeps_the_3a_bytes() {
    let base = LinkPayload::new(0, "traction-rules/1", None, &[], None).unwrap();
    assert_eq!(base.frame_digest, FRAME_DIGEST_NONE);
    assert_eq!(
        base.clone().with_frames(&[]),
        base,
        "3a payloads are unchanged"
    );
    let with = base.clone().with_frames(&records());
    assert_eq!(with.frame_digest, DIGEST_TWO);
    assert_ne!(with.digest(), base.digest());
}
