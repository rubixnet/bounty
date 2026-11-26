//! Seam: the pure Audit Bundle builder and verifier of `traction-core`.

mod chain_fixture;

use chain_fixture::*;
use serde_json::Value;
use traction_core::{
    BundleChain, BundleError, BundleInput, BundlePolicy, ChainError, build_bundle, verify_bundle,
};

fn bundle_bytes(fix: &Fixture, signer_seed: u8) -> Vec<u8> {
    let chains = fix
        .chains
        .iter()
        .zip(&fix.keys)
        .map(|(links, key)| BundleChain {
            public_key: key.to_hex(),
            links: links.clone(),
        })
        .collect();
    build_bundle(
        &key(signer_seed),
        BundleInput {
            project_id: ALPHA.to_owned(),
            chains,
            work_buckets: fix.rows.work_buckets.clone(),
            commits: fix.rows.commits.clone(),
            rollups: fix.rows.rollups.clone(),
            exported_at_ms: 42,
        },
    )
    .expect("builds")
    .to_canonical_bytes()
}

fn chain_error(result: Result<traction_core::ChainReport, BundleError>) -> ChainError {
    match result.expect_err("must fail") {
        BundleError::Chain(error) => error,
        other => panic!("expected a chain error, got {other:?}"),
    }
}

#[test]
fn a_built_bundle_verifies_and_carries_only_its_project() {
    let fix = build(&[(7, 4)]);
    let bytes = bundle_bytes(&fix, 7);
    let report = verify_bundle(&bytes, &BundlePolicy::default()).expect("verifies");
    assert_eq!(report.chains.len(), 1);
    assert_eq!(report.chains[0].links, 4);
    let json: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["frames"], serde_json::json!([]));
    // Other Projects appear only as opaque ids inside link payloads.
    assert!(
        json["project_rows"]["work_buckets"]
            .as_array()
            .unwrap()
            .iter()
            .all(|b| b["project_id"] == ALPHA)
    );
}

#[test]
fn a_bundle_signed_by_a_key_that_is_not_the_newest_chains_is_not_built() {
    let fix = build(&[(7, 3)]);
    let chains = vec![BundleChain {
        public_key: fix.keys[0].to_hex(),
        links: fix.chains[0].clone(),
    }];
    let err = build_bundle(
        &key(9),
        BundleInput {
            project_id: ALPHA.to_owned(),
            chains,
            work_buckets: Vec::new(),
            commits: Vec::new(),
            rollups: Vec::new(),
            exported_at_ms: 1,
        },
    )
    .expect_err("foreign key");
    assert_eq!(err.code(), "key_is_not_current");
}

#[test]
fn an_edited_manifest_fails_its_signature_and_junk_is_malformed() {
    let fix = build(&[(7, 3)]);
    let mut json: Value = serde_json::from_slice(&bundle_bytes(&fix, 7)).unwrap();
    json["manifest"]["exported_at_ms"] = 43.into();
    let err = chain_error(verify_bundle(
        &serde_json::to_vec(&json).unwrap(),
        &BundlePolicy::default(),
    ));
    assert_eq!(err, ChainError::ManifestSignatureInvalid);
    assert_eq!(err.code(), "manifest_signature_invalid");

    assert_eq!(
        verify_bundle(b"not json", &BundlePolicy::default()),
        Err(BundleError::Malformed)
    );
}

#[test]
fn a_manifest_that_lists_other_chains_than_it_ships_is_a_mismatch() {
    let fix = build(&[(7, 3), (9, 2)]);
    let mut json: Value = serde_json::from_slice(&bundle_bytes(&fix, 9)).unwrap();
    json["chains"].as_array_mut().unwrap().remove(0);
    let err = chain_error(verify_bundle(
        &serde_json::to_vec(&json).unwrap(),
        &BundlePolicy::default(),
    ));
    assert_eq!(err, ChainError::ManifestChainsMismatch);
}
