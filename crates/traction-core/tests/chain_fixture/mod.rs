//! Shared builders for the chain tests: hand-made rows and chains sealed
//! with fixed seeds. Everything is deterministic.
#![allow(dead_code)]

use traction_core::{
    ActivityRollup, CategoryShareMilli, ChainLink, CommitObservation, DeviceSigningKey,
    LinkPayload, ProjectWindowRows, RowSet, StruggleAxes, Supersedes, TelemetryTier, WorkBucket,
};
use traction_core::{TRACTION_RULES_1, VerifyingKeyHex};
use zeroize::Zeroizing;

pub const WINDOW: u64 = 120_000;
pub const T0: u64 = 1_800_000_000_000 - 1_800_000_000_000 % WINDOW;
pub const ALPHA: &str = "git:alpha";
pub const BETA: &str = "git:beta";
pub const OID_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
pub const OID_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

pub fn key(seed: u8) -> DeviceSigningKey {
    DeviceSigningKey::from_seed(Zeroizing::new([seed; 32])).expect("seed gives a strong key")
}

pub fn bucket(project: &str, minute_start_ms: u64, seed: u32) -> WorkBucket {
    WorkBucket {
        project_id: project.to_owned(),
        minute_start_ms,
        ruleset_id: TRACTION_RULES_1.ruleset_id.to_owned(),
        agent_compute_sec: 40 + seed,
        idle_autonomous_sec: seed,
        human_active_sec: 20,
        permission_wait_ms: 0,
        commits_human: 0,
        commits_agent: seed % 2,
        injected_hid_detected: false,
        auto_approver_suspected: false,
        telemetry_tier: TelemetryTier::Full,
        tier_limited: false,
        struggle_axes: StruggleAxes {
            steering: 500,
            specificity: 400,
            friction: 300,
            comfort_delta: 200,
        },
        verified_struggle: 350,
        struggle_credit_sec: 18,
    }
}

pub fn rollup(window_start_ms: u64) -> ActivityRollup {
    ActivityRollup {
        window_start_ms,
        ruleset_id: TRACTION_RULES_1.ruleset_id.to_owned(),
        minutes_observed: 2,
        category_share_milli: CategoryShareMilli::default(),
        switch_rate_milli: 500,
        mean_cadence_entropy_milli: Some(700),
        injected_ratio_milli: None,
        telemetry_tier: TelemetryTier::Full,
    }
}

pub fn commit(project: &str, oid: &str, observed_ms: u64) -> CommitObservation {
    CommitObservation {
        project_id: project.to_owned(),
        commit_oid: oid.to_owned(),
        observed_ms,
    }
}

/// The rows of window index `n` (alpha always; beta on even windows).
pub fn rows_of_window(n: u64) -> (Vec<WorkBucket>, Vec<CommitObservation>, ActivityRollup) {
    let w = T0 + n * WINDOW;
    let mut buckets = vec![
        bucket(ALPHA, w, n as u32),
        bucket(ALPHA, w + 60_000, n as u32 + 1),
    ];
    let mut commits = vec![commit(ALPHA, OID_A, w + 5_000)];
    if n.is_multiple_of(2) {
        buckets.push(bucket(BETA, w + 60_000, n as u32 + 2));
        commits.push(commit(BETA, OID_B, w + 70_000));
    }
    (buckets, commits, rollup(w))
}

pub fn window_payload(n: u64, supersedes: Option<Supersedes>) -> LinkPayload {
    let (buckets, commits, rollup) = rows_of_window(n);
    let w = T0 + n * WINDOW;
    let mut projects = Vec::new();
    for project in [ALPHA, BETA] {
        let b: Vec<_> = buckets
            .iter()
            .filter(|b| b.project_id == project)
            .cloned()
            .collect();
        let c: Vec<_> = commits
            .iter()
            .filter(|c| c.project_id == project)
            .map(|c| c.commit_oid.clone())
            .collect();
        if !b.is_empty() || !c.is_empty() {
            projects.push(ProjectWindowRows::new(project.to_owned(), b, c).unwrap());
        }
    }
    LinkPayload::new(
        w,
        TRACTION_RULES_1.ruleset_id,
        Some(&rollup),
        &projects,
        supersedes,
    )
    .unwrap()
}

pub struct Fixture {
    pub chains: Vec<Vec<ChainLink>>,
    pub keys: Vec<VerifyingKeyHex>,
    pub rows: RowSet,
}

/// One chain per `(seed, link count)`, windows contiguous across chains; each
/// later chain's genesis names the previous chain's tip.
pub fn build(specs: &[(u8, u64)]) -> Fixture {
    let mut chains = Vec::new();
    let mut keys = Vec::new();
    let mut rows = RowSet::empty();
    let mut n = 0u64;
    for (c, (seed, count)) in specs.iter().enumerate() {
        let signer = key(*seed);
        let mut links: Vec<ChainLink> = Vec::new();
        for i in 0..*count {
            let supersedes = if i == 0 && c > 0 {
                let tip = chains
                    .last()
                    .and_then(|l: &Vec<ChainLink>| l.last())
                    .unwrap();
                Some(Supersedes {
                    key_id: tip.header.key_id.clone(),
                    seq: tip.header.seq,
                    entry_hash: tip.entry_hash.clone(),
                })
            } else {
                None
            };
            let payload = window_payload(n, supersedes);
            let link = signer
                .seal(
                    links.last().map(|l| &l.header),
                    payload,
                    T0 + n * WINDOW + 1_000,
                )
                .unwrap();
            links.push(link);
            let (b, co, ro) = rows_of_window(n);
            rows.work_buckets.extend(b);
            rows.commits.extend(co);
            rows.rollups.push(ro);
            n += 1;
        }
        chains.push(links);
        keys.push(signer.verifying_key().clone());
    }
    Fixture { chains, keys, rows }
}
