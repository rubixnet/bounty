//! Seam: the pure chain API of `traction-core` (seal, verify, domain tags).
//! Hand-built chains hit every `ChainError` variant at least once.

mod chain_fixture;

use chain_fixture::*;
use traction_core::{
    ChainError, ChainLink, DeviceSigningKey, FRAME_DIGEST_NONE, GENESIS_PREV_HASH, KeyedChain,
    LinkHeader, LinkPayload, PayloadPart, RowScope, RowSet, SealError, Supersedes, TAG_DEVICE_ROWS,
    TAG_FRAMES, TAG_KEY_ID, TAG_LINK, TAG_MANIFEST, TAG_PAYLOAD, TAG_ROWS, TAG_SIG_LINK,
    TAG_SIG_MANIFEST, VerifyPolicy, VerifyingKeyHex, canonical_json, manifest_digest,
    sign_manifest, verify_chain, verify_chains, verify_manifest,
};
use zeroize::Zeroizing;

fn policy() -> VerifyPolicy {
    VerifyPolicy::default()
}

fn one(fix: &Fixture) -> Result<traction_core::ChainReport, ChainError> {
    verify_chain(&fix.chains[0], &fix.keys[0], &fix.rows, &policy())
}

fn keyed(chains: &[Vec<ChainLink>], keys: &[VerifyingKeyHex]) -> Vec<KeyedChain> {
    chains
        .iter()
        .zip(keys)
        .map(|(links, key)| KeyedChain {
            key: key.clone(),
            links: links.clone(),
        })
        .collect()
}

fn all(fix: &Fixture) -> Result<traction_core::ChainReport, ChainError> {
    verify_chains(&keyed(&fix.chains, &fix.keys), &fix.rows, &policy())
}

const ALL_TAGS: [&[u8]; 9] = [
    TAG_KEY_ID,
    TAG_ROWS,
    TAG_DEVICE_ROWS,
    TAG_PAYLOAD,
    TAG_LINK,
    TAG_SIG_LINK,
    TAG_MANIFEST,
    TAG_SIG_MANIFEST,
    TAG_FRAMES,
];

#[test]
fn tags_are_distinct_prefix_free_and_versioned() {
    for (i, a) in ALL_TAGS.iter().enumerate() {
        assert!(a.starts_with(b"traction/v1/") && a.ends_with(b"\0"));
        for (j, b) in ALL_TAGS.iter().enumerate() {
            if i != j {
                assert_ne!(a, b);
                assert!(!b.starts_with(a), "{a:?} is a prefix of {b:?}");
            }
        }
    }
}

#[test]
fn one_input_hashed_under_two_tags_differs() {
    let value = serde_json::json!({"a": 1});
    let bytes = canonical_json(&value).unwrap();
    let mut seen = std::collections::BTreeSet::new();
    for tag in ALL_TAGS {
        let mut input = tag.to_vec();
        input.extend_from_slice(&bytes);
        use sha2::{Digest, Sha256};
        assert!(seen.insert(Sha256::digest(&input).to_vec()));
    }
}

#[test]
fn a_payloads_canonical_bytes_do_not_parse_as_a_header() {
    let payload = window_payload(0, None);
    let bytes = serde_json::to_vec(&payload).unwrap();
    assert!(serde_json::from_slice::<LinkHeader>(&bytes).is_err());
}

#[test]
fn a_valid_chain_verifies_and_reports_one_chain() {
    let fix = build(&[(7, 4)]);
    let report = one(&fix).unwrap();
    assert_eq!(report.chains.len(), 1);
    assert!(report.key_discontinuities.is_empty());
    assert!(!report.key_pinned);
    assert_eq!(report.chains[0].links, 4);
    assert_eq!(report.chains[0].tip.0, 3);
    assert_eq!(report.clock_regressions, 0);
    assert_eq!(report.pending_rows_after_tip, 0);
    assert_eq!(report.legacy_rows_before_genesis, 0);
}

#[test]
fn seal_derives_the_linking_fields_itself() {
    let fix = build(&[(7, 3)]);
    let links = &fix.chains[0];
    assert_eq!(links[0].header.seq, 0);
    assert_eq!(links[0].header.prev_hash, GENESIS_PREV_HASH);
    assert_eq!(links[1].header.seq, 1);
    assert_eq!(links[1].header.prev_hash, links[0].entry_hash);
    assert_eq!(links[2].payload.frame_digest, FRAME_DIGEST_NONE);
    assert_eq!(links[0].header.key_id, key(7).key_id());
}

#[test]
fn two_chains_are_reported_as_two_with_one_discontinuity() {
    let fix = build(&[(7, 3), (9, 2)]);
    let report = all(&fix).unwrap();
    assert_eq!(report.chains.len(), 2);
    assert_eq!(report.key_discontinuities.len(), 1);
    let d = &report.key_discontinuities[0];
    assert!(d.linked);
    assert_eq!(d.from_key_id, key(7).key_id());
    assert_eq!(d.to_key_id, key(9).key_id());
    assert_eq!(d.at_window_ms, T0 + 3 * WINDOW);
}

#[test]
fn a_successor_without_supersedes_is_reported_unlinked_not_rejected() {
    let mut fix = build(&[(7, 2), (9, 2)]);
    // Re-seal the successor genesis without `supersedes`.
    let signer = key(9);
    let g = signer
        .seal(None, window_payload(2, None), T0 + 2 * WINDOW + 1_000)
        .unwrap();
    let l1 = signer
        .seal(
            Some(&g.header),
            window_payload(3, None),
            T0 + 3 * WINDOW + 1_000,
        )
        .unwrap();
    fix.chains[1] = vec![g, l1];
    let report = all(&fix).unwrap();
    assert_eq!(report.chains.len(), 2);
    assert!(!report.key_discontinuities[0].linked);
}

#[test]
fn chain_order_does_not_depend_on_argument_order() {
    let fix = build(&[(7, 2), (9, 2)]);
    let chains = vec![fix.chains[1].clone(), fix.chains[0].clone()];
    let keys = vec![fix.keys[1].clone(), fix.keys[0].clone()];
    let report = verify_chains(&keyed(&chains, &keys), &fix.rows, &policy()).unwrap();
    assert_eq!(report.chains[0].key_id, key(7).key_id());
}

fn mutated(mutate: impl FnOnce(&mut Fixture)) -> ChainError {
    let mut fix = build(&[(7, 4)]);
    mutate(&mut fix);
    one(&fix).unwrap_err()
}

#[test]
fn unsupported_schema() {
    let err = mutated(|f| f.chains[0][2].header.schema_version = 2);
    assert_eq!(err, ChainError::UnsupportedSchema { seq: 2 });
    assert_eq!(err.code(), "unsupported_schema");
}

#[test]
fn truncated_genesis() {
    let err = mutated(|f| {
        f.chains[0].remove(0);
    });
    assert_eq!(err, ChainError::TruncatedGenesis { first_seq: 1 });
    assert_eq!(err.code(), "truncated_genesis");
}

#[test]
fn duplicate_seq() {
    let err = mutated(|f| {
        let again = f.chains[0][1].clone();
        f.chains[0].insert(2, again);
    });
    assert_eq!(err, ChainError::DuplicateSeq { seq: 1 });
    assert_eq!(err.code(), "duplicate_seq");
}

#[test]
fn seq_gap() {
    let err = mutated(|f| {
        f.chains[0].remove(1);
    });
    assert_eq!(
        err,
        ChainError::SeqGap {
            expected: 1,
            found: 2
        }
    );
    assert_eq!(err.code(), "seq_gap");
}

#[test]
fn unknown_key_when_the_pinned_set_lacks_it() {
    let fix = build(&[(7, 2)]);
    let policy = VerifyPolicy {
        trusted_keys: Some(vec![key(8).verifying_key().clone()]),
        ..VerifyPolicy::default()
    };
    let err = verify_chain(&fix.chains[0], &fix.keys[0], &fix.rows, &policy).unwrap_err();
    assert_eq!(
        err,
        ChainError::UnknownKey {
            seq: 0,
            key_id: key(7).key_id()
        }
    );
    assert_eq!(err.code(), "unknown_key");
    let pinned = VerifyPolicy {
        trusted_keys: Some(vec![fix.keys[0].clone()]),
        ..VerifyPolicy::default()
    };
    let report = verify_chain(&fix.chains[0], &fix.keys[0], &fix.rows, &pinned).unwrap();
    assert!(report.key_pinned);
}

#[test]
fn a_chain_signed_under_another_keys_id_is_an_unknown_key() {
    let fix = build(&[(7, 2)]);
    let err =
        verify_chain(&fix.chains[0], key(8).verifying_key(), &fix.rows, &policy()).unwrap_err();
    assert_eq!(err.code(), "unknown_key");
}

#[test]
fn weak_keys_are_rejected() {
    // The identity point (order 1) and a point of order 2.
    let identity = format!("01{}", "00".repeat(31));
    let order_two = format!("ecff{}7f", "ff".repeat(29));
    for text in [identity, order_two, "zz".to_owned(), "ab".repeat(31)] {
        let err = VerifyingKeyHex::parse(&text).unwrap_err();
        assert_eq!(err, ChainError::WeakKey);
        assert_eq!(err.code(), "weak_key");
    }
    // Uppercase is never accepted.
    let upper = key(7).verifying_key().to_hex().to_uppercase();
    assert!(VerifyingKeyHex::parse(&upper).is_err());
    let good = key(7).verifying_key().to_hex();
    assert_eq!(
        VerifyingKeyHex::parse(&good).unwrap(),
        *key(7).verifying_key()
    );
}

#[test]
fn payload_hash_mismatch_names_the_part() {
    let err = mutated(|f| f.chains[0][1].payload.ruleset_id = "other".to_owned());
    assert_eq!(
        err,
        ChainError::PayloadHashMismatch {
            seq: 1,
            part: PayloadPart::Payload
        }
    );
    assert_eq!(err.code(), "payload_hash_mismatch");

    let err = mutated(|f| f.rows.rollups[2].switch_rate_milli += 1);
    assert_eq!(
        err,
        ChainError::PayloadHashMismatch {
            seq: 2,
            part: PayloadPart::DeviceRows
        }
    );

    let err = mutated(|f| f.rows.work_buckets[0].human_active_sec += 1);
    assert_eq!(
        err,
        ChainError::PayloadHashMismatch {
            seq: 0,
            part: PayloadPart::Project(ALPHA.to_owned())
        }
    );
}

#[test]
fn a_row_with_no_payload_entry_and_an_entry_with_no_row_both_fail() {
    // Window 1 holds only alpha; a beta bucket appears with no entry.
    let err = mutated(|f| {
        let w = T0 + WINDOW;
        f.rows.work_buckets.push(bucket(BETA, w + 60_000, 1));
    });
    assert_eq!(
        err,
        ChainError::PayloadHashMismatch {
            seq: 1,
            part: PayloadPart::Project(BETA.to_owned())
        }
    );
    // Window 0 has a beta entry; remove its rows.
    let err = mutated(|f| {
        f.rows.work_buckets.retain(|b| b.project_id != BETA);
        f.rows.commits.retain(|c| c.project_id != BETA);
    });
    assert_eq!(
        err,
        ChainError::PayloadHashMismatch {
            seq: 0,
            part: PayloadPart::Project(BETA.to_owned())
        }
    );
}

#[test]
fn out_of_scope_projects_are_checked_only_through_the_payload_digest() {
    let mut fix = build(&[(7, 4)]);
    // A bundle for alpha alone: beta's rows are not shipped.
    fix.rows.work_buckets.retain(|b| b.project_id == ALPHA);
    fix.rows.commits.retain(|c| c.project_id == ALPHA);
    fix.rows.scope = RowScope::Project(ALPHA.to_owned());
    one(&fix).unwrap();
    // Alpha's own rows still count.
    fix.rows.work_buckets[0].agent_compute_sec += 1;
    assert_eq!(one(&fix).unwrap_err().code(), "payload_hash_mismatch");
}

#[test]
fn an_uppercase_or_malformed_commit_oid_in_the_rows_fails() {
    let err = mutated(|f| f.rows.commits[0].commit_oid = OID_A.to_uppercase());
    assert_eq!(err.code(), "payload_hash_mismatch");
    let err = mutated(|f| f.rows.commits[0].commit_oid = "abc".to_owned());
    assert_eq!(err.code(), "payload_hash_mismatch");
}

#[test]
fn entry_hash_mismatch() {
    let err = mutated(|f| f.chains[0][1].header.sealed_at_ms += 1);
    assert_eq!(err, ChainError::EntryHashMismatch { seq: 1 });
    assert_eq!(err.code(), "entry_hash_mismatch");
}

#[test]
fn invalid_signature() {
    let err = mutated(|f| {
        let sig = f.chains[0][3].signature.clone();
        f.chains[0][3].signature = if sig.starts_with('0') {
            sig.replacen('0', "1", 1)
        } else {
            sig.replacen(&sig[..1], "0", 1)
        };
    });
    assert_eq!(err, ChainError::InvalidSignature { seq: 3 });
    assert_eq!(err.code(), "invalid_signature");
}

#[test]
fn a_malleated_signature_fails() {
    // S' = S + l (l is the group order) is the classic malleation.
    const L: [u8; 32] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde,
        0x14, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x10,
    ];
    let err = mutated(|f| {
        let link = &mut f.chains[0][0];
        let mut sig: Vec<u8> = (0..64)
            .map(|i| u8::from_str_radix(&link.signature[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        let mut carry = 0u16;
        for i in 0..32 {
            let sum = u16::from(sig[32 + i]) + u16::from(L[i]) + carry;
            sig[32 + i] = sum as u8;
            carry = sum >> 8;
        }
        assert_eq!(carry, 0);
        link.signature = sig.iter().map(|b| format!("{b:02x}")).collect();
    });
    assert_eq!(err, ChainError::InvalidSignature { seq: 0 });
}

#[test]
fn broken_prev_hash() {
    let fix = build(&[(7, 2)]);
    let signer = key(7);
    // Seal link 1 against a doctored copy of link 0's header.
    let mut doctored = fix.chains[0][0].header.clone();
    doctored.sealed_at_ms += 5;
    let forged = signer
        .seal(
            Some(&doctored),
            window_payload(1, None),
            T0 + WINDOW + 1_000,
        )
        .unwrap();
    let mut chain = fix.chains[0].clone();
    chain[1] = forged;
    let err = verify_chain(&chain, &fix.keys[0], &fix.rows, &policy()).unwrap_err();
    assert_eq!(err, ChainError::BrokenPrevHash { seq: 1 });
    assert_eq!(err.code(), "broken_prev_hash");
}

#[test]
fn window_order_rejects_a_misaligned_window() {
    let fix = build(&[(7, 1)]);
    let signer = key(7);
    let payload = LinkPayload::new(
        T0 + 1_000,
        traction_core::TRACTION_RULES_1.ruleset_id,
        None,
        &[],
        None,
    )
    .unwrap();
    let link = signer.seal(None, payload, T0).unwrap();
    let err = verify_chain(&[link], &fix.keys[0], &RowSet::empty(), &policy()).unwrap_err();
    assert_eq!(err, ChainError::WindowOrder { seq: 0 });
    assert_eq!(err.code(), "window_order");
}

#[test]
fn window_order_rejects_a_repeated_window() {
    // Hand-forge link 1 with the same window as link 0, validly signed and
    // linked: only the window check can catch it.
    let fix = build(&[(7, 1)]);
    let payload = window_payload(0, None);
    let mut header = fix.chains[0][0].header.clone();
    header.seq = 1;
    header.prev_hash = fix.chains[0][0].entry_hash.clone();
    header.payload_digest = payload.digest();
    let forged = reseal(7, header, payload);
    let chain = vec![fix.chains[0][0].clone(), forged];
    let err = verify_chain(&chain, &fix.keys[0], &fix.rows, &policy()).unwrap_err();
    assert_eq!(err, ChainError::WindowOrder { seq: 1 });
}

/// Sign an arbitrary (possibly dishonest) header: what a hostile device
/// that owns its key could do. The tests hold the seed, so they sign with
/// dalek directly.
fn reseal(seed: u8, header: LinkHeader, payload: LinkPayload) -> ChainLink {
    use ed25519_dalek::{Signer, SigningKey};
    use sha2::{Digest, Sha256};
    let value = serde_json::to_value(&header).unwrap();
    let entry = Sha256::digest([TAG_LINK, &canonical_json(&value).unwrap()].concat());
    let message = [TAG_SIG_LINK, entry.as_slice()].concat();
    let signature = SigningKey::from_bytes(&[seed; 32])
        .sign(&message)
        .to_bytes();
    let hex = |bytes: &[u8]| bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
    ChainLink {
        header,
        payload,
        entry_hash: hex(entry.as_slice()),
        signature: hex(&signature),
    }
}

#[test]
fn unanchored_row_in_a_window_no_link_covers() {
    let signer = key(7);
    let mut links: Vec<ChainLink> = Vec::new();
    let mut rows = RowSet::empty();
    for n in [0u64, 1, 3] {
        let link = signer
            .seal(
                links.last().map(|l| &l.header),
                window_payload(n, None),
                T0 + n * WINDOW + 1,
            )
            .unwrap();
        links.push(link);
        let (b, c, r) = rows_of_window(n);
        rows.work_buckets.extend(b);
        rows.commits.extend(c);
        rows.rollups.push(r);
    }
    let report = verify_chain(&links, signer.verifying_key(), &rows, &policy()).unwrap();
    assert_eq!(report.chains[0].links, 3);
    // An inserted window between two sealed ones.
    let (b, c, r) = rows_of_window(2);
    rows.work_buckets.extend(b);
    rows.commits.extend(c);
    rows.rollups.push(r);
    let err = verify_chain(&links, signer.verifying_key(), &rows, &policy()).unwrap_err();
    assert_eq!(
        err,
        ChainError::UnanchoredRow {
            window_start_ms: T0 + 2 * WINDOW
        }
    );
    assert_eq!(err.code(), "unanchored_row");
}

#[test]
fn rows_before_the_genesis_and_after_the_tip_are_reported_not_rejected() {
    let mut fix = build(&[(7, 2)]);
    // Window -1 (legacy) and window 2 (pending, not yet sealed).
    fix.rows.rollups.push(rollup(T0 - WINDOW));
    fix.rows.rollups.push(rollup(T0 + 2 * WINDOW));
    fix.rows
        .work_buckets
        .push(bucket(ALPHA, T0 + 2 * WINDOW, 9));
    let report = one(&fix).unwrap();
    assert_eq!(report.legacy_rows_before_genesis, 1);
    assert_eq!(report.pending_rows_after_tip, 2);
}

#[test]
fn tip_mismatch_catches_truncation() {
    let fix = build(&[(7, 4)]);
    let tip = fix.chains[0][3].entry_hash.clone();
    let full = VerifyPolicy {
        expected_tip: Some((3, tip.clone())),
        ..VerifyPolicy::default()
    };
    verify_chain(&fix.chains[0], &fix.keys[0], &fix.rows, &full).unwrap();
    // Cut the last link (and its rows) off.
    let mut cut = build(&[(7, 4)]);
    cut.chains[0].pop();
    let w = T0 + 3 * WINDOW;
    cut.rows.work_buckets.retain(|b| b.minute_start_ms < w);
    cut.rows.commits.retain(|c| c.observed_ms < w);
    cut.rows.rollups.retain(|r| r.window_start_ms < w);
    let err = verify_chain(&cut.chains[0], &cut.keys[0], &cut.rows, &full).unwrap_err();
    assert_eq!(
        err,
        ChainError::TipMismatch {
            expected_seq: 3,
            found_seq: 2
        }
    );
    assert_eq!(err.code(), "tip_mismatch");
}

#[test]
fn chain_overlap() {
    let fix = build(&[(7, 4)]);
    // A second key that claims windows the first chain already covers.
    let other = key(9);
    let g = other
        .seal(None, window_payload(1, None), T0 + WINDOW + 1)
        .unwrap();
    let chains = vec![fix.chains[0].clone(), vec![g]];
    let keys = vec![fix.keys[0].clone(), other.verifying_key().clone()];
    let err = verify_chains(&keyed(&chains, &keys), &fix.rows, &policy()).unwrap_err();
    assert_eq!(
        err,
        ChainError::ChainOverlap {
            key_id: other.key_id()
        }
    );
    assert_eq!(err.code(), "chain_overlap");
}

#[test]
fn duplicate_chain() {
    let fix = build(&[(7, 2)]);
    let chains = vec![fix.chains[0].clone(), fix.chains[0].clone()];
    let keys = vec![fix.keys[0].clone(), fix.keys[0].clone()];
    let err = verify_chains(&keyed(&chains, &keys), &fix.rows, &policy()).unwrap_err();
    assert_eq!(
        err,
        ChainError::DuplicateChain {
            key_id: key(7).key_id()
        }
    );
    assert_eq!(err.code(), "duplicate_chain");
}

#[test]
fn supersedes_mismatch_when_the_old_tail_was_cut() {
    let fix = build(&[(7, 3), (9, 2)]);
    // Drop the old chain's last link (and its rows): the successor still
    // names the old tip, so the cut is caught.
    let mut chains = fix.chains.clone();
    chains[0].pop();
    let mut rows = fix.rows.clone();
    let w = T0 + 2 * WINDOW;
    rows.work_buckets
        .retain(|b| b.minute_start_ms < w || b.minute_start_ms >= w + WINDOW);
    rows.commits
        .retain(|c| c.observed_ms < w || c.observed_ms >= w + WINDOW);
    rows.rollups.retain(|r| r.window_start_ms != w);
    let err = verify_chains(&keyed(&chains, &fix.keys), &rows, &policy()).unwrap_err();
    assert_eq!(
        err,
        ChainError::SupersedesMismatch {
            key_id: key(9).key_id()
        }
    );
    assert_eq!(err.code(), "supersedes_mismatch");
}

#[test]
fn pinned_keys_must_cover_every_chain() {
    let fix = build(&[(7, 2), (9, 2)]);
    let only_first = VerifyPolicy {
        trusted_keys: Some(vec![fix.keys[0].clone()]),
        ..VerifyPolicy::default()
    };
    let err = verify_chains(&keyed(&fix.chains, &fix.keys), &fix.rows, &only_first).unwrap_err();
    assert_eq!(
        err,
        ChainError::UnknownKey {
            seq: 0,
            key_id: key(9).key_id()
        }
    );
    let both = VerifyPolicy {
        trusted_keys: Some(fix.keys.clone()),
        ..VerifyPolicy::default()
    };
    assert!(
        verify_chains(&keyed(&fix.chains, &fix.keys), &fix.rows, &both)
            .unwrap()
            .key_pinned
    );
}

#[test]
fn clock_regressions_are_counted() {
    let signer = key(7);
    let g = signer
        .seal(None, window_payload(0, None), 10_000_000)
        .unwrap();
    let l1 = signer
        .seal(Some(&g.header), window_payload(1, None), 5_000_000)
        .unwrap();
    assert!(!g.header.clock_regressed);
    assert!(l1.header.clock_regressed);
    let mut rows = RowSet::empty();
    for n in [0, 1] {
        let (b, c, r) = rows_of_window(n);
        rows.work_buckets.extend(b);
        rows.commits.extend(c);
        rows.rollups.push(r);
    }
    let report = verify_chain(&[g, l1], signer.verifying_key(), &rows, &policy()).unwrap();
    assert_eq!(report.clock_regressions, 1);
}

#[test]
fn seal_refuses_dishonest_inputs() {
    let signer = key(7);
    let g = signer.seal(None, window_payload(1, None), 1).unwrap();
    // Window not after the previous one.
    assert_eq!(
        signer
            .seal(Some(&g.header), window_payload(1, None), 2)
            .unwrap_err(),
        SealError::WindowOrder
    );
    assert_eq!(
        signer
            .seal(Some(&g.header), window_payload(0, None), 2)
            .unwrap_err(),
        SealError::WindowOrder
    );
    // `supersedes` on a non-genesis link.
    let supersedes = Supersedes {
        key_id: key(9).key_id(),
        seq: 0,
        entry_hash: g.entry_hash.clone(),
    };
    assert_eq!(
        signer
            .seal(Some(&g.header), window_payload(2, Some(supersedes)), 2)
            .unwrap_err(),
        SealError::SupersedesOnNonGenesis
    );
    // A previous header from another key.
    let theirs = key(9).seal(None, window_payload(0, None), 1).unwrap();
    assert_eq!(
        signer
            .seal(Some(&theirs.header), window_payload(1, None), 2)
            .unwrap_err(),
        SealError::ForeignPrev
    );
}

#[test]
fn a_supersedes_on_a_non_genesis_link_fails_verification() {
    let fix = build(&[(7, 2)]);
    let mut chain = fix.chains[0].clone();
    chain[1].payload.supersedes = Some(Supersedes {
        key_id: key(9).key_id(),
        seq: 0,
        entry_hash: chain[0].entry_hash.clone(),
    });
    let err = verify_chain(&chain, &fix.keys[0], &fix.rows, &policy()).unwrap_err();
    assert_eq!(err.code(), "payload_hash_mismatch");
}

#[test]
fn an_empty_chain_is_a_truncated_genesis() {
    let fix = build(&[(7, 1)]);
    let err = verify_chain(&[], &fix.keys[0], &fix.rows, &policy()).unwrap_err();
    assert_eq!(err, ChainError::TruncatedGenesis { first_seq: 0 });
}

#[test]
fn project_entries_must_be_sorted_and_unique_to_build() {
    let rows = traction_core::ProjectWindowRows::new(ALPHA.to_owned(), vec![], vec![]).unwrap();
    let dup = LinkPayload::new(T0, "r", None, &[rows.clone(), rows], None);
    assert!(dup.is_err());
    assert!(
        traction_core::ProjectWindowRows::new(ALPHA.to_owned(), vec![], vec!["XYZ".to_owned()])
            .is_err()
    );
    // Oids are sorted and de-duplicated.
    let r = traction_core::ProjectWindowRows::new(
        ALPHA.to_owned(),
        vec![],
        vec![OID_A.to_owned(), OID_A.to_owned()],
    )
    .unwrap();
    assert_eq!(r.commit_oids, vec![OID_A.to_owned()]);
}

#[test]
fn manifest_signatures_verify_strictly() {
    let signer = key(7);
    let body = serde_json::json!({"chains": ["ed25519:abc"], "version": 1});
    let digest = manifest_digest(&body).unwrap();
    assert_eq!(digest.len(), 64);
    let sig = sign_manifest(&signer, &body).unwrap();
    verify_manifest(signer.verifying_key(), &body, &sig).unwrap();
    let other = serde_json::json!({"chains": [], "version": 1});
    let err = verify_manifest(signer.verifying_key(), &other, &sig).unwrap_err();
    assert_eq!(err, ChainError::ManifestSignatureInvalid);
    assert_eq!(err.code(), "manifest_signature_invalid");
    // The manifest signature is not a link signature (different domain).
    assert!(verify_manifest(key(8).verifying_key(), &body, &sig).is_err());
    assert_eq!(
        ChainError::ManifestChainsMismatch.code(),
        "manifest_chains_mismatch"
    );
}

#[test]
fn the_chain_source_never_uses_non_strict_verification() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/chain");
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("verify("),
            "{} calls a non-strict verify",
            path.display()
        );
    }
}

#[test]
fn seeds_give_distinct_stable_key_ids() {
    let a = DeviceSigningKey::from_seed(Zeroizing::new([7u8; 32])).unwrap();
    let b = DeviceSigningKey::from_seed(Zeroizing::new([9u8; 32])).unwrap();
    assert_ne!(a.key_id(), b.key_id());
    assert!(a.key_id().as_str().starts_with("ed25519:"));
    assert_eq!(a.key_id().as_str().len(), "ed25519:".len() + 32);
}

#[test]
fn a_successor_chain_presented_without_its_predecessor_is_never_a_clean_pass() {
    let fix = build(&[(7, 3), (9, 2)]);
    // The old chain is dropped but its rows are kept: they have no link.
    let err = verify_chains(
        &keyed(&fix.chains[1..], &fix.keys[1..]),
        &fix.rows,
        &policy(),
    )
    .unwrap_err();
    assert_eq!(
        err,
        ChainError::UnanchoredRow {
            window_start_ms: T0
        }
    );
    assert_eq!(err.code(), "unanchored_row");

    // The old rows are dropped too: the genesis still names the old tip.
    let mut rows = fix.rows.clone();
    let cut = T0 + 3 * WINDOW;
    rows.work_buckets.retain(|b| b.minute_start_ms >= cut);
    rows.commits.retain(|c| c.observed_ms >= cut);
    rows.rollups.retain(|r| r.window_start_ms >= cut);
    let err =
        verify_chains(&keyed(&fix.chains[1..], &fix.keys[1..]), &rows, &policy()).unwrap_err();
    assert_eq!(
        err,
        ChainError::SupersedesMismatch {
            key_id: key(9).key_id()
        }
    );
}
