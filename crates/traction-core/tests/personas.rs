//! Seam: the persona red-team simulator through `work_buckets` with
//! `TRACTION_RULES_1`. Seeded and deterministic; pass marks live beside the
//! Ruleset (`PERSONA_MARKS_1`).

mod support;

use support::*;
use traction_core::{PERSONA_MARKS_1 as MARKS, TelemetryTier};

const TIERS: [TelemetryTier; 3] = [
    TelemetryTier::Full,
    TelemetryTier::Reduced,
    TelemetryTier::IdleOnly,
];
const SEEDS: [u64; 3] = [7, 42, 2026];

fn credit(p: &Persona, tier: TelemetryTier) -> u32 {
    credit_milli(&score(&at_tier(p, tier)))
}

#[test]
fn honest_debugger_earns_real_credit_on_every_tier() {
    for seed in SEEDS {
        let p = honest_debugger(seed);
        for tier in TIERS {
            let got = credit(&p, tier);
            let min = if tier == TelemetryTier::Full {
                MARKS.honest_min_milli
            } else {
                MARKS.honest_degraded_min_milli
            };
            assert!(got >= min, "seed {seed} {tier:?}: {got} < {min}");
        }
    }
}

#[test]
fn key_tapper_earns_nothing_on_every_tier() {
    for seed in SEEDS {
        let p = key_tapper_59s(seed);
        for tier in TIERS {
            let got = credit(&p, tier);
            assert!(
                got <= MARKS.key_tapper_max_milli,
                "seed {seed} {tier:?}: {got}"
            );
        }
    }
}

#[test]
fn trivial_prompt_spammer_earns_negligible_credit_on_every_tier() {
    for seed in SEEDS {
        let p = trivial_prompt_spammer(seed);
        for tier in TIERS {
            let got = credit(&p, tier);
            assert!(
                got <= MARKS.spammer_max_milli,
                "seed {seed} {tier:?}: {got}"
            );
        }
    }
}

#[test]
fn steer_rhythm_faker_earns_negligible_credit_on_every_tier() {
    for seed in SEEDS {
        let p = steer_rhythm_faker(seed, false);
        for tier in TIERS {
            let got = credit(&p, tier);
            assert!(
                got <= MARKS.steer_faker_max_milli,
                "seed {seed} {tier:?}: {got}"
            );
        }
    }
}

#[test]
fn injected_steer_faker_earns_zero_and_is_flagged() {
    for seed in SEEDS {
        let p = steer_rhythm_faker(seed, true);
        let full = score(&p);
        assert_eq!(credit_milli(&full), MARKS.steer_injected_max_milli);
        assert!(full.iter().any(|b| b.injected_hid_detected));
        for tier in [TelemetryTier::Reduced, TelemetryTier::IdleOnly] {
            assert!(credit(&p, tier) <= MARKS.steer_faker_max_milli);
        }
    }
}

#[test]
fn a_persona_seed_reproduces_byte_identical_streams() {
    let a = honest_debugger(7);
    let b = honest_debugger(7);
    assert_eq!(a.events, b.events);
    assert_eq!(a.presence, b.presence);
}

/// Prints the results table (run with `--nocapture`).
#[test]
fn persona_results_table() {
    for seed in SEEDS {
        for p in [
            honest_debugger(seed),
            key_tapper_59s(seed),
            trivial_prompt_spammer(seed),
            steer_rhythm_faker(seed, false),
            steer_rhythm_faker(seed, true),
        ] {
            println!(
                "TABLE seed={seed} {:<30} full={:>4} reduced={:>4} idle_only={:>4}",
                p.name,
                credit(&p, TelemetryTier::Full),
                credit(&p, TelemetryTier::Reduced),
                credit(&p, TelemetryTier::IdleOnly)
            );
        }
    }
}
