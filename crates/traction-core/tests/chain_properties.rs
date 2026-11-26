//! Property: for any valid set of chains, changing any single byte of the
//! serialized chains, keys or covered rows makes verification fail, never
//! panic, and fail identically on a second run. The case count and seed are
//! fixed so CI is deterministic.

mod chain_fixture;

use chain_fixture::*;
use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, RngSeed};
use traction_core::{ChainLink, KeyedChain, RowSet, VerifyPolicy, VerifyingKeyHex, verify_chains};

type Wire = (Vec<Vec<ChainLink>>, Vec<VerifyingKeyHex>, RowSet);

fn config() -> Config {
    Config {
        cases: 160,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x7AC7_3001),
        max_shrink_iters: 1024,
        failure_persistence: None,
        ..Config::default()
    }
}

fn verify(wire: &Wire) -> Result<traction_core::ChainReport, traction_core::ChainError> {
    let chains: Vec<KeyedChain> = wire
        .0
        .iter()
        .zip(&wire.1)
        .map(|(links, key)| KeyedChain {
            key: key.clone(),
            links: links.clone(),
        })
        .collect();
    verify_chains(&chains, &wire.2, &VerifyPolicy::default())
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn any_single_byte_change_is_rejected_deterministically(
        links in proptest::collection::vec(1u64..=4, 1..=3),
        position in any::<u64>(),
        replacement in 32u8..127,
    ) {
        let specs: Vec<(u8, u64)> = links
            .iter()
            .enumerate()
            .map(|(i, n)| (i as u8 + 1, *n))
            .collect();
        let fix = build(&specs);
        let wire: Wire = (fix.chains, fix.keys, fix.rows);
        prop_assert!(verify(&wire).is_ok());

        let mut bytes = serde_json::to_vec(&wire).unwrap();
        let at = (position % bytes.len() as u64) as usize;
        prop_assume!(bytes[at] != replacement);
        bytes[at] = replacement;

        // A byte change that no longer parses is rejected at the door.
        if let Ok(changed) = serde_json::from_slice::<Wire>(&bytes) {
            prop_assume!(changed != wire);
            let first = verify(&changed);
            prop_assert!(first.is_err(), "a changed byte verified");
            prop_assert_eq!(first, verify(&changed));
        }
    }
}
