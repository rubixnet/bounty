//! Seam: the red-team suite for the signed chain. Every test builds a real
//! chain through the real writer path (`finalize_settled` with an injected
//! clock: 8 sealed windows over 2 Projects), checks it verifies, then
//! attacks the SQLite file or an exported Audit Bundle and asserts the exact
//! `ChainError` variant, its stable `code()` string and, through the real
//! `verify` command code, the printed line and exit code.
//!
//! SQLite attacks drop the relevant `*_no_update` / `*_no_delete` trigger
//! first, inside the test only: the triggers stop mistakes, the chain stops
//! an attacker who removes them. Row numbers follow the spec's tamper table.

mod common;

use std::ffi::OsString;

use common::chain::{ALPHA, BETA, Device, T0, WINDOW};
use serde_json::Value;
use sha2::{Digest, Sha256};
use traction_core::{
    AuditBundle, BundleError, BundlePolicy, ChainError, ChainLink, DeviceSigningKey, KeyId,
    ManifestTip, PayloadPart, ProjectWindowRows, TAG_LINK, TAG_MANIFEST, VerifyPolicy,
    VerifyingKeyHex, canonical_json, sign_manifest, verify_bundle,
};
use traction_daemon::{VerifyDatabaseError, run_verify, verify_database, verify_database_with};
use zeroize::Zeroizing;

/// The attacked window: its Chain Link has `seq == K` (the windows are
/// contiguous from 0).
const K: u64 = 5;
const WINDOWS: u64 = 8;

fn rogue(seed: u8) -> DeviceSigningKey {
    DeviceSigningKey::from_seed(Zeroizing::new([seed; 32])).unwrap()
}

fn cli(args: &[OsString]) -> traction_daemon::Outcome {
    run_verify(args.iter().cloned()).expect("the verify command runs")
}

fn args(items: &[&str]) -> Vec<OsString> {
    items.iter().map(OsString::from).collect()
}

fn bundle_of(device: &Device, project: &str) -> AuditBundle {
    let path = device.export(project, 1_000);
    serde_json::from_slice(&std::fs::read(path).unwrap()).expect("a parseable bundle")
}

fn public_key_of(bundle: &AuditBundle) -> String {
    bundle.chains.last().unwrap().public_key.clone()
}

fn policy(pinned: Option<&str>) -> BundlePolicy {
    BundlePolicy {
        trusted_keys: pinned.map(|k| vec![VerifyingKeyHex::parse(k).unwrap()]),
        expected_tip: None,
    }
}

/// The bundle fails with exactly `expected` (variant and code), and the
/// `verify` command prints the matching `FAIL` line and exits 1.
fn assert_bundle_fails(
    device: &Device,
    bundle: &AuditBundle,
    pinned: Option<&str>,
    expected: ChainError,
    code: &str,
) {
    let bytes = bundle.to_canonical_bytes();
    match verify_bundle(&bytes, &policy(pinned)) {
        Err(BundleError::Chain(error)) => {
            assert_eq!(error, expected);
            assert_eq!(error.code(), code);
        }
        other => panic!("expected {expected:?}, got {other:?}"),
    }
    let path = device.dir().join("tampered.traction-bundle.json");
    std::fs::write(&path, &bytes).unwrap();
    let mut cli_args = vec![path.into_os_string()];
    if let Some(key) = pinned {
        cli_args.extend(args(&["--trusted-key", key]));
    }
    let out = cli(&cli_args);
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(
        out.stdout.starts_with(&format!("FAIL {code} ")),
        "{}",
        out.stdout
    );
}

/// The local database fails with exactly `expected`, and `verify --data-dir`
/// prints the matching `FAIL` line and exits 1.
fn assert_db_fails(device: &Device, expected: ChainError, code: &str) {
    match verify_database(device.dir()) {
        Err(VerifyDatabaseError::Chain(error)) => {
            assert_eq!(error, expected);
            assert_eq!(error.code(), code);
        }
        other => panic!("expected {expected:?}, got {other:?}"),
    }
    let out = cli(&args(&["--data-dir", device.dir().to_str().unwrap()]));
    assert_eq!(out.code, 1, "{}", out.stdout);
    assert!(
        out.stdout.starts_with(&format!("FAIL {code} ")),
        "{}",
        out.stdout
    );
}

fn chain_count_and_links(device: &Device) -> (i64, i64) {
    let db = device.attacker(&[]);
    (
        db.query_row("SELECT count(*) FROM chain_key", [], |r| r.get(0))
            .unwrap(),
        db.query_row("SELECT count(*) FROM chain_link", [], |r| r.get(0))
            .unwrap(),
    )
}

fn project_error(seq: u64, project: &str) -> ChainError {
    ChainError::PayloadHashMismatch {
        seq,
        part: PayloadPart::Project(project.to_owned()),
    }
}

// ---------------------------------------------------------------------------
// Forging helpers: what an attacker with the whole bundle and a key of their
// own can build. Everything here uses the public `traction-core` API.

fn sha256_hex(tag: &[u8], bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tag);
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Recomputes every derived manifest field from the (tampered) content and
/// signs it with `signer`, so only the chain checks can catch the forgery.
fn forge(mut bundle: AuditBundle, signer: &DeviceSigningKey) -> AuditBundle {
    let key_ids: Vec<KeyId> = bundle
        .chains
        .iter()
        .map(|c| VerifyingKeyHex::parse(&c.public_key).unwrap().key_id())
        .collect();
    let tip = bundle
        .chains
        .last()
        .and_then(|c| c.links.last())
        .expect("a tip");
    let tip = ManifestTip {
        key_id: tip.header.key_id.clone(),
        seq: tip.header.seq,
        entry_hash: tip.entry_hash.clone(),
    };
    let chains_json = canonical_json(&serde_json::to_value(&bundle.chains).unwrap()).unwrap();
    let body = &mut bundle.manifest.body;
    body.public_key = signer.verifying_key().to_hex();
    body.key_id = signer.key_id();
    body.chain_count = bundle.chains.len();
    body.key_ids = key_ids;
    body.first_window_ms = bundle.chains[0].links[0].header.window_start_ms;
    body.tip = tip;
    body.link_count = bundle.chains.iter().map(|c| c.links.len()).sum();
    body.bucket_count = bundle.project_rows.work_buckets.len();
    body.chains_digest = sha256_hex(TAG_MANIFEST, &chains_json);
    let value = serde_json::to_value(&bundle.manifest.body).unwrap();
    bundle.manifest.signature = sign_manifest(signer, &value).unwrap();
    bundle
}

/// Re-seals `payloads` from a genesis link under `signer`.
fn reseal(signer: &DeviceSigningKey, links: &[ChainLink]) -> Vec<ChainLink> {
    let mut out: Vec<ChainLink> = Vec::new();
    for link in links {
        let mut payload = link.payload.clone();
        payload.supersedes = None;
        let sealed = signer
            .seal(
                out.last().map(|l| &l.header),
                payload,
                link.header.sealed_at_ms,
            )
            .unwrap();
        out.push(sealed);
    }
    out
}

/// Changes one Work Bucket of `ALPHA` in window `window_index` and makes the
/// window's payload say so (what an attacker who rewrites a window does).
fn rewrite_window(bundle: &mut AuditBundle, window_index: u64) {
    let start = Device::window(window_index);
    for bucket in &mut bundle.project_rows.work_buckets {
        if bucket.minute_start_ms >= start && bucket.minute_start_ms < start + WINDOW {
            bucket.agent_compute_sec ^= 1;
        }
    }
    let buckets = bundle
        .project_rows
        .work_buckets
        .iter()
        .filter(|b| b.minute_start_ms >= start && b.minute_start_ms < start + WINDOW)
        .cloned()
        .collect();
    let oids = bundle
        .project_rows
        .commit_oids
        .get(&start)
        .cloned()
        .unwrap_or_default();
    let rows = ProjectWindowRows::new(ALPHA.to_owned(), buckets, oids).unwrap();
    let link = &mut bundle.chains[0].links[usize::try_from(window_index).unwrap()];
    for entry in &mut link.payload.projects {
        if entry.project_id == ALPHA {
            entry.rows_digest = rows.rows_digest();
        }
    }
}

/// Re-signs an entry hash after a header edit (what a forger can do), so the
/// `entry_hash` is consistent but the signature is not.
fn rehash_entry(link: &mut ChainLink) {
    let header = canonical_json(&serde_json::to_value(&link.header).unwrap()).unwrap();
    link.entry_hash = sha256_hex(TAG_LINK, &header);
}

// ---------------------------------------------------------------------------

#[test]
fn the_real_chain_verifies_before_any_attack() {
    let device = Device::sealed(WINDOWS);
    let (keys, links) = chain_count_and_links(&device);
    assert_eq!((keys, u64::try_from(links).unwrap()), (1, WINDOWS));
    let report = verify_database(device.dir()).expect("an untouched chain verifies");
    assert_eq!(report.chains.len(), 1);
    assert!(report.key_discontinuities.is_empty());
    // Two Projects really are in it.
    let db = device.attacker(&[]);
    let projects: i64 = db
        .query_row(
            "SELECT count(DISTINCT project_id) FROM work_bucket",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(projects, 2);
    for project in [ALPHA, BETA] {
        let bundle = bundle_of(&device, project);
        verify_bundle(&bundle.to_canonical_bytes(), &policy(None)).expect("a bundle verifies");
    }
    let out = cli(&args(&["--data-dir", device.dir().to_str().unwrap()]));
    assert_eq!(out.code, 0, "{}", out.stdout);
    let _ = T0;
}

// Row 1
#[test]
fn row01_a_bit_flip_in_a_work_bucket_in_sqlite_names_the_window_and_project() {
    let device = Device::sealed(WINDOWS);
    let db = device.attacker(&["work_bucket_no_update"]);
    let changed = db
        .execute(
            "UPDATE work_bucket
             SET agent_compute_sec = agent_compute_sec + 1 - 2 * (agent_compute_sec & 1)
             WHERE project_id = ?1 AND minute_start_ms >= ?2 AND minute_start_ms < ?3",
            rusqlite::params![
                ALPHA,
                i64::try_from(Device::window(K)).unwrap(),
                i64::try_from(Device::window(K) + WINDOW).unwrap()
            ],
        )
        .unwrap();
    assert!(changed > 0);
    drop(db);
    assert_db_fails(&device, project_error(K, ALPHA), "payload_hash_mismatch");
}

// Row 2
#[test]
fn row02_the_same_flip_in_a_bundles_project_rows_fails_the_same_way() {
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    let start = Device::window(K);
    let bucket = bundle
        .project_rows
        .work_buckets
        .iter_mut()
        .find(|b| b.minute_start_ms >= start && b.minute_start_ms < start + WINDOW)
        .expect("a bucket in the window");
    bucket.agent_compute_sec ^= 1;
    assert_bundle_fails(
        &device,
        &bundle,
        None,
        project_error(K, ALPHA),
        "payload_hash_mismatch",
    );
}

// Row 3
#[test]
fn row03_a_bit_flip_in_an_activity_rollup_names_the_device_rows() {
    let device = Device::sealed(WINDOWS);
    let db = device.attacker(&["activity_rollup_no_update"]);
    db.execute(
        "UPDATE activity_rollup
         SET switch_rate_milli = switch_rate_milli + 1 - 2 * (switch_rate_milli & 1)
         WHERE window_start_ms = ?1",
        [i64::try_from(Device::window(K)).unwrap()],
    )
    .unwrap();
    drop(db);
    assert_db_fails(
        &device,
        ChainError::PayloadHashMismatch {
            seq: K,
            part: PayloadPart::DeviceRows,
        },
        "payload_hash_mismatch",
    );
}

// Row 4
#[test]
fn row04_dropping_a_project_entry_from_a_links_payload_fails_the_payload_digest() {
    let device = Device::sealed(WINDOWS);
    let db = device.attacker(&["chain_link_no_update"]);
    let json: String = db
        .query_row(
            "SELECT payload_json FROM chain_link WHERE seq = ?1",
            [i64::try_from(K).unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    let mut payload: Value = serde_json::from_str(&json).unwrap();
    assert_eq!(payload["projects"].as_array().unwrap().len(), 2);
    payload["projects"].as_array_mut().unwrap().remove(1);
    db.execute(
        "UPDATE chain_link SET payload_json = ?1 WHERE seq = ?2",
        rusqlite::params![payload.to_string(), i64::try_from(K).unwrap()],
    )
    .unwrap();
    drop(db);
    assert_db_fails(
        &device,
        ChainError::PayloadHashMismatch {
            seq: K,
            part: PayloadPart::Payload,
        },
        "payload_hash_mismatch",
    );
}

// Row 5
#[test]
fn row05_a_deleted_middle_link_is_a_sequence_gap_in_sqlite_and_in_a_bundle() {
    let expected = ChainError::SeqGap {
        expected: K,
        found: K + 1,
    };
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    bundle.chains[0].links.remove(usize::try_from(K).unwrap());
    assert_bundle_fails(&device, &bundle, None, expected.clone(), "seq_gap");

    let db = device.attacker(&["chain_link_no_delete"]);
    db.execute(
        "DELETE FROM chain_link WHERE seq = ?1",
        [i64::try_from(K).unwrap()],
    )
    .unwrap();
    drop(db);
    assert_db_fails(&device, expected, "seq_gap");
}

// Row 6
#[test]
fn row06_deleting_a_link_and_renumbering_the_rest_breaks_the_entry_hash() {
    let device = Device::sealed(WINDOWS);
    let db = device.attacker(&["chain_link_no_delete", "chain_link_no_update"]);
    let k = i64::try_from(K).unwrap();
    db.execute("DELETE FROM chain_link WHERE seq = ?1", [k])
        .unwrap();
    db.execute("UPDATE chain_link SET seq = seq - 1 WHERE seq > ?1", [k])
        .unwrap();
    drop(db);
    assert_db_fails(
        &device,
        ChainError::EntryHashMismatch { seq: K },
        "entry_hash_mismatch",
    );
}

// Row 7
#[test]
fn row07_swapping_two_adjacent_links_in_a_bundle_is_a_sequence_gap() {
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    let k = usize::try_from(K).unwrap();
    bundle.chains[0].links.swap(k, k + 1);
    assert_bundle_fails(
        &device,
        &bundle,
        None,
        ChainError::SeqGap {
            expected: K,
            found: K + 1,
        },
        "seq_gap",
    );
}

// Row 8
#[test]
fn row08_swapping_two_work_buckets_across_windows_names_the_earlier_window() {
    type Values = (i64, i64, i64, i64, i64);
    let device = Device::sealed(WINDOWS);
    let db = device.attacker(&["work_bucket_no_update"]);
    // The commit in window 1 shows in its bucket; window 5 has none.
    let early = Device::window(1);
    let late = Device::window(5);
    let read = |minute: u64| -> Values {
        db.query_row(
            "SELECT commits_human, commits_agent, human_active_sec, struggle_credit_sec,
                    verified_struggle
             FROM work_bucket WHERE project_id = ?1 AND minute_start_ms = ?2",
            rusqlite::params![ALPHA, i64::try_from(minute).unwrap()],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
    };
    let (a, b) = (read(early), read(late));
    assert_ne!(a, b, "the buckets must differ for a swap to mean anything");
    let write = |minute: u64, v: Values| {
        db.execute(
            "UPDATE work_bucket SET commits_human = ?3, commits_agent = ?4,
                    human_active_sec = ?5, struggle_credit_sec = ?6, verified_struggle = ?7
             WHERE project_id = ?1 AND minute_start_ms = ?2",
            rusqlite::params![
                ALPHA,
                i64::try_from(minute).unwrap(),
                v.0,
                v.1,
                v.2,
                v.3,
                v.4
            ],
        )
        .unwrap();
    };
    write(early, b);
    write(late, a);
    drop(db);
    assert_db_fails(&device, project_error(1, ALPHA), "payload_hash_mismatch");
}

const INSERT_BUCKET_LIKE: &str = "INSERT INTO work_bucket
        (project_id, minute_start_ms, ruleset_id, agent_compute_sec, idle_autonomous_sec,
         human_active_sec, permission_wait_ms, commits_human, commits_agent,
         injected_hid_detected, auto_approver_suspected, telemetry_tier, tier_limited,
         axis_steering, axis_specificity, axis_friction, axis_comfort_delta,
         verified_struggle, struggle_credit_sec)
    SELECT ?1, ?2, ruleset_id, 55, idle_autonomous_sec, human_active_sec,
           permission_wait_ms, commits_human, commits_agent, injected_hid_detected,
           auto_approver_suspected, telemetry_tier, tier_limited, axis_steering,
           axis_specificity, axis_friction, axis_comfort_delta, verified_struggle,
           struggle_credit_sec
    FROM work_bucket WHERE project_id = ?3 AND minute_start_ms = ?4";

// Row 9
#[test]
fn row09_inserting_a_work_bucket_into_a_sealed_window_fails_that_window() {
    let device = Device::sealed(WINDOWS);
    let db = device.attacker(&[]);
    // Beta has no row in even windows; add one in window 2.
    db.execute(
        INSERT_BUCKET_LIKE,
        rusqlite::params![
            BETA,
            i64::try_from(Device::window(2) + 60_000).unwrap(),
            ALPHA,
            i64::try_from(Device::window(2)).unwrap()
        ],
    )
    .unwrap();
    drop(db);
    assert_db_fails(&device, project_error(2, BETA), "payload_hash_mismatch");
}

// Row 10
#[test]
fn row10_inserting_a_work_bucket_into_an_unsealed_gap_window_is_unanchored() {
    let mut device = Device::sealed(4);
    device.skip_windows(1);
    device.seal_windows(3);
    let gap = Device::window(4);
    verify_database(device.dir()).expect("the gap itself is honest");
    let db = device.attacker(&[]);
    db.execute(
        INSERT_BUCKET_LIKE,
        rusqlite::params![
            ALPHA,
            i64::try_from(gap).unwrap(),
            ALPHA,
            i64::try_from(Device::window(2)).unwrap()
        ],
    )
    .unwrap();
    drop(db);
    assert_db_fails(
        &device,
        ChainError::UnanchoredRow {
            window_start_ms: gap,
        },
        "unanchored_row",
    );
}

/// The bundle with window `K` rewritten and every link from `K` re-sealed by
/// a rogue key (its own key id), spliced behind the device's links `0..K`.
fn rogue_tail_forgery(device: &Device) -> (AuditBundle, DeviceSigningKey, String) {
    let mut bundle = bundle_of(device, ALPHA);
    let device_key = public_key_of(&bundle);
    rewrite_window(&mut bundle, K);
    let signer = rogue(0x66);
    let resealed = reseal(&signer, &bundle.chains[0].links);
    let k = usize::try_from(K).unwrap();
    bundle.chains[0].links.splice(k.., resealed[k..].to_vec());
    (forge(bundle, &signer), signer, device_key)
}

// Row 11
#[test]
fn row11_a_window_rewritten_and_re_signed_with_a_rogue_key_is_an_unknown_key() {
    let device = Device::sealed(WINDOWS);
    let (bundle, signer, device_key) = rogue_tail_forgery(&device);
    let expected = ChainError::UnknownKey {
        seq: K,
        key_id: signer.key_id(),
    };
    assert_bundle_fails(
        &device,
        &bundle,
        Some(&device_key),
        expected.clone(),
        "unknown_key",
    );
    // The chain's own key is what the links are checked against, so the
    // forgery fails the same way unpinned.
    assert_bundle_fails(&device, &bundle, None, expected, "unknown_key");
}

// Row 12
#[test]
fn row12_keeping_the_devices_key_id_on_rogue_signed_links_is_an_invalid_signature() {
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    let device_key = public_key_of(&bundle);
    let device_id = bundle.chains[0].links[0].header.key_id.clone();
    rewrite_window(&mut bundle, K);
    let signer = rogue(0x66);
    let mut resealed = reseal(&signer, &bundle.chains[0].links);
    for link in &mut resealed {
        link.header.key_id = device_id.clone();
        rehash_entry(link);
    }
    let k = usize::try_from(K).unwrap();
    bundle.chains[0].links.splice(k.., resealed[k..].to_vec());
    let bundle = forge(bundle, &signer);
    assert_bundle_fails(
        &device,
        &bundle,
        Some(&device_key),
        ChainError::InvalidSignature { seq: K },
        "invalid_signature",
    );
}

// Row 13
#[test]
fn row13_a_whole_chain_rebuilt_with_a_rogue_key_passes_unpinned_and_fails_pinned() {
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    let device_key = public_key_of(&bundle);
    let device_id = bundle.chains[0].links[0].header.key_id.clone();
    let signer = rogue(0x66);
    let links = reseal(&signer, &bundle.chains[0].links);
    bundle.chains[0].links = links;
    bundle.chains[0].public_key = signer.verifying_key().to_hex();
    let bundle = forge(bundle, &signer);

    // The documented limit of trust on first use: nothing ties the chain to
    // the device, so an unpinned verify passes, with a different key id.
    let bytes = bundle.to_canonical_bytes();
    let report = verify_bundle(&bytes, &policy(None)).expect("passes unpinned");
    assert!(!report.key_pinned);
    assert_ne!(report.chains[0].key_id, device_id);
    let path = device.dir().join("rebuilt.traction-bundle.json");
    std::fs::write(&path, &bytes).unwrap();
    let out = cli(&[path.into_os_string()]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    assert!(out.stdout.contains("pinned=false"), "{}", out.stdout);
    assert!(out.stdout.contains("self-asserted"), "{}", out.stdout);

    // With the device's public key pinned, the rebuilt chain is rejected.
    assert_bundle_fails(
        &device,
        &bundle,
        Some(&device_key),
        ChainError::UnknownKey {
            seq: 0,
            key_id: signer.key_id(),
        },
        "unknown_key",
    );
}

// Row 14
#[test]
fn row14_grafting_another_devices_chain_after_a_link_repeats_a_sequence_number() {
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    let other = reseal(&rogue(0x77), &bundle.chains[0].links);
    let k = usize::try_from(K).unwrap();
    bundle.chains[0].links.splice(k + 1..k + 1, other);
    assert_bundle_fails(
        &device,
        &bundle,
        None,
        ChainError::DuplicateSeq { seq: 0 },
        "duplicate_seq",
    );
}

// Row 15
#[test]
fn row15_a_renumbered_graft_signed_by_the_second_key_is_an_unknown_key() {
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    let signer = rogue(0x77);
    let other = reseal(&signer, &bundle.chains[0].links);
    let k = usize::try_from(K).unwrap();
    // Their links K+1.. take the place of the device's.
    bundle.chains[0]
        .links
        .splice(k + 1.., other[k + 1..].to_vec());
    assert_bundle_fails(
        &device,
        &bundle,
        None,
        ChainError::UnknownKey {
            seq: K + 1,
            key_id: signer.key_id(),
        },
        "unknown_key",
    );
}

// Row 16
#[test]
fn row16_truncating_the_head_is_a_truncated_genesis_in_sqlite_and_in_a_bundle() {
    let expected = ChainError::TruncatedGenesis { first_seq: K };
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    bundle.chains[0].links.drain(..usize::try_from(K).unwrap());
    assert_bundle_fails(
        &device,
        &bundle,
        None,
        expected.clone(),
        "truncated_genesis",
    );

    let db = device.attacker(&["chain_link_no_delete"]);
    db.execute(
        "DELETE FROM chain_link WHERE seq < ?1",
        [i64::try_from(K).unwrap()],
    )
    .unwrap();
    drop(db);
    assert_db_fails(&device, expected, "truncated_genesis");
}

// Row 17
#[test]
fn row17_truncating_a_bundles_tail_is_a_tip_mismatch_and_editing_the_tip_breaks_the_signature() {
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    bundle.chains[0].links.pop();
    assert_bundle_fails(
        &device,
        &bundle,
        None,
        ChainError::TipMismatch {
            expected_seq: WINDOWS - 1,
            found_seq: WINDOWS - 2,
        },
        "tip_mismatch",
    );
    // Editing the manifest's tip to match the cut needs the device's key.
    let last = bundle.chains[0].links.last().unwrap().clone();
    bundle.manifest.body.tip = ManifestTip {
        key_id: last.header.key_id,
        seq: last.header.seq,
        entry_hash: last.entry_hash,
    };
    assert_bundle_fails(
        &device,
        &bundle,
        None,
        ChainError::ManifestSignatureInvalid,
        "manifest_signature_invalid",
    );
}

// Row 18
#[test]
fn row18_a_truncated_tail_in_sqlite_needs_an_outside_tip_to_be_caught() {
    let device = Device::sealed(WINDOWS);
    let dir = device.dir().to_str().unwrap().to_owned();
    let before = cli(&args(&["--data-dir", &dir]));
    assert_eq!(before.code, 0, "{}", before.stdout);
    let tip = before
        .stdout
        .split_whitespace()
        .find_map(|word| word.strip_prefix("tip="))
        .expect("a tip in the OK line")
        .to_owned();

    let db = device.attacker(&["chain_link_no_delete"]);
    db.execute(
        "DELETE FROM chain_link WHERE seq = ?1",
        [i64::try_from(WINDOWS - 1).unwrap()],
    )
    .unwrap();
    drop(db);

    // The documented local limit: without an outside tip it still passes,
    // and the cut window's rows now show as pending.
    let without = cli(&args(&["--data-dir", &dir]));
    assert_eq!(without.code, 0, "{}", without.stdout);
    assert!(!without.stdout.contains("pending=0"), "{}", without.stdout);

    let with = cli(&args(&["--data-dir", &dir, "--expect-tip", &tip]));
    assert_eq!(with.code, 1, "{}", with.stdout);
    assert!(
        with.stdout.starts_with("FAIL tip_mismatch "),
        "{}",
        with.stdout
    );
    // The same check through the library: the exact variant and code.
    let (rest, hash) = tip.rsplit_once(':').expect("tip is key_id:seq:hash");
    assert!(rest.ends_with(&format!(":{}", WINDOWS - 1)), "{tip}");
    let hash = hash.to_owned();
    let policy = VerifyPolicy {
        expected_tip: Some((WINDOWS - 1, hash)),
        ..VerifyPolicy::default()
    };
    match verify_database_with(device.dir(), &policy) {
        Err((VerifyDatabaseError::Chain(error), _)) => {
            assert_eq!(
                error,
                ChainError::TipMismatch {
                    expected_seq: WINDOWS - 1,
                    found_seq: WINDOWS - 2,
                }
            );
            assert_eq!(error.code(), "tip_mismatch");
        }
        other => panic!("expected a tip mismatch, got {other:?}"),
    }
}

// Row 19
#[test]
fn row19_a_forked_link_with_a_repeated_sequence_number_is_a_duplicate_seq() {
    let device = Device::sealed(WINDOWS);
    let db = device.attacker(&[]);
    // Rebuild the table without its primary key so a second `seq 3` fits.
    db.execute_batch(
        "CREATE TABLE chain_link_forked (
             key_id TEXT NOT NULL, seq INTEGER NOT NULL, window_start_ms INTEGER NOT NULL,
             ruleset_id TEXT NOT NULL, prev_hash TEXT NOT NULL, payload_digest TEXT NOT NULL,
             entry_hash TEXT NOT NULL, sealed_at_ms INTEGER NOT NULL,
             clock_regressed INTEGER NOT NULL, payload_json TEXT NOT NULL,
             signature TEXT NOT NULL
         ) STRICT;
         INSERT INTO chain_link_forked SELECT * FROM chain_link;
         INSERT INTO chain_link_forked SELECT * FROM chain_link WHERE seq = 5;
         DROP TABLE chain_link;
         ALTER TABLE chain_link_forked RENAME TO chain_link;",
    )
    .unwrap();
    drop(db);
    assert_db_fails(
        &device,
        ChainError::DuplicateSeq { seq: K },
        "duplicate_seq",
    );
}

/// `S + l`, the malleated twin of an Ed25519 signature (`l` is the group
/// order, little endian).
fn malleate(signature_hex: &str) -> String {
    const L: [u8; 32] = [
        0xed, 0xd3, 0xf5, 0x5c, 0x1a, 0x63, 0x12, 0x58, 0xd6, 0x9c, 0xf7, 0xa2, 0xde, 0xf9, 0xde,
        0x14, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x00, 0x10,
    ];
    let mut bytes: Vec<u8> = (0..64)
        .map(|i| u8::from_str_radix(&signature_hex[2 * i..2 * i + 2], 16).unwrap())
        .collect();
    let mut carry = 0u16;
    for (i, limb) in L.iter().enumerate() {
        let sum = u16::from(bytes[32 + i]) + u16::from(*limb) + carry;
        bytes[32 + i] = u8::try_from(sum & 0xff).unwrap();
        carry = sum >> 8;
    }
    assert_eq!(carry, 0, "S + l must still fit 256 bits");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// Row 20
#[test]
fn row20_a_malleated_signature_or_a_small_order_key_is_rejected() {
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    let k = usize::try_from(K).unwrap();
    let genuine = bundle.chains[0].links[k].signature.clone();
    bundle.chains[0].links[k].signature = malleate(&genuine);
    assert_bundle_fails(
        &device,
        &bundle,
        None,
        ChainError::InvalidSignature { seq: K },
        "invalid_signature",
    );

    // The identity point is a small-order public key.
    let mut weak = bundle_of(&device, ALPHA);
    weak.chains[0].public_key = format!("01{}", "00".repeat(31));
    assert_bundle_fails(&device, &weak, None, ChainError::WeakKey, "weak_key");
}

// Row 21
#[test]
fn row21_an_honestly_lost_key_verifies_as_two_chains_with_exit_code_two() {
    let mut device = Device::sealed(4);
    device.lose_the_signing_key();
    device.seal_windows(3);

    let report = verify_database(device.dir()).expect("both chains verify");
    assert_eq!(report.chains.len(), 2);
    assert_eq!(report.key_discontinuities.len(), 1);
    assert!(report.key_discontinuities[0].linked);
    let out = cli(&args(&["--data-dir", device.dir().to_str().unwrap()]));
    assert_eq!(out.code, 2, "{}", out.stdout);
    assert!(
        out.stdout.starts_with("OK chains=2 links=7 "),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout.contains("WARN key_discontinuity from=ed25519:")
            && out.stdout.contains(" linked=true"),
        "{}",
        out.stdout
    );

    let bundle = bundle_of(&device, ALPHA);
    let path = device.dir().join("two-chains.traction-bundle.json");
    std::fs::write(&path, bundle.to_canonical_bytes()).unwrap();
    let out = cli(&[path.into_os_string()]);
    assert_eq!(out.code, 2, "{}", out.stdout);
    assert!(out.stdout.starts_with("OK chains=2 "), "{}", out.stdout);
    assert!(
        out.stdout.contains("WARN key_discontinuity"),
        "{}",
        out.stdout
    );
}

// Row 22
#[test]
fn row22_deleting_the_old_chains_last_link_breaks_the_successors_commitment() {
    let mut device = Device::sealed(4);
    device.lose_the_signing_key();
    device.seal_windows(3);
    let db = device.attacker(&["chain_link_no_delete"]);
    let new_key: String = db
        .query_row(
            "SELECT key_id FROM chain_link ORDER BY window_start_ms DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let deleted = db
        .execute(
            "DELETE FROM chain_link WHERE key_id != ?1 AND seq = 3",
            [&new_key],
        )
        .unwrap();
    assert_eq!(deleted, 1);
    drop(db);
    let key_id: KeyId = serde_json::from_value(Value::String(new_key)).unwrap();
    assert_db_fails(
        &device,
        ChainError::SupersedesMismatch { key_id },
        "supersedes_mismatch",
    );
}

fn current_key(device: &Device) -> DeviceSigningKey {
    let seed = std::fs::read(device.dir().join(traction_daemon::SIGNING_KEY_FILE)).unwrap();
    DeviceSigningKey::from_seed(Zeroizing::new(
        <[u8; 32]>::try_from(seed.as_slice()).unwrap(),
    ))
    .unwrap()
}

// Row 23
#[test]
fn row23_dropping_the_old_chain_from_a_bundle_never_passes() {
    let mut device = Device::sealed(4);
    device.lose_the_signing_key();
    device.seal_windows(3);
    let bundle = bundle_of(&device, ALPHA);
    assert_eq!(bundle.chains.len(), 2);
    let new_key_id = VerifyingKeyHex::parse(&public_key_of(&bundle))
        .unwrap()
        .key_id();

    // The attacker holds the new key (it is the device's current key), so the
    // re-signed manifest is genuine. Rows of the old windows are kept: they
    // have no link any more.
    let signer = current_key(&device);
    let mut dropped = bundle.clone();
    dropped.chains.remove(0);
    let dropped = forge(dropped, &signer);
    assert_bundle_fails(
        &device,
        &dropped,
        None,
        ChainError::UnanchoredRow {
            window_start_ms: Device::window(0),
        },
        "unanchored_row",
    );

    // With the old windows' rows removed too, the new genesis still names the
    // old tip.
    let cut = Device::window(4);
    let mut scrubbed = bundle;
    scrubbed.chains.remove(0);
    scrubbed
        .project_rows
        .work_buckets
        .retain(|b| b.minute_start_ms >= cut);
    scrubbed.project_rows.commit_oids.retain(|w, _| *w >= cut);
    scrubbed
        .device_rows
        .activity_rollups
        .retain(|r| r.window_start_ms >= cut);
    let scrubbed = forge(scrubbed, &signer);
    assert_bundle_fails(
        &device,
        &scrubbed,
        None,
        ChainError::SupersedesMismatch { key_id: new_key_id },
        "supersedes_mismatch",
    );
}

// Row 24
#[test]
fn row24_a_successor_whose_windows_overlap_the_old_chain_is_a_chain_overlap() {
    let device = Device::sealed(WINDOWS);
    let mut bundle = bundle_of(&device, ALPHA);
    let signer = rogue(0x88);
    let overlapping = reseal(&signer, &bundle.chains[0].links);
    bundle.chains.push(traction_core::BundleChain {
        public_key: signer.verifying_key().to_hex(),
        links: overlapping,
    });
    let bundle = forge(bundle, &signer);
    assert_bundle_fails(
        &device,
        &bundle,
        None,
        ChainError::ChainOverlap {
            key_id: signer.key_id(),
        },
        "chain_overlap",
    );
}

#[test]
fn an_untouched_bundle_prints_one_ok_line_and_exits_zero() {
    let device = Device::sealed(WINDOWS);
    let path = device.export(ALPHA, 1_000);
    let out = cli(&[path.into_os_string()]);
    assert_eq!(out.code, 0, "{}", out.stdout);
    let first = out.stdout.lines().next().unwrap();
    assert!(
        first.starts_with("OK chains=1 links=8 tip=ed25519:") && first.contains(":7:"),
        "{first}"
    );
    assert!(
        first.ends_with("pinned=false clock_regressions=0 pending=0 legacy=0"),
        "{first}"
    );
}
