//! Seam: the credit-cap property suite through `rollup`. Whatever the event
//! flood, the overlap of sessions, Projects and duplicate or missing turn
//! closes, no clock may exceed elapsed wall-clock time and credit may never
//! exceed Human Active Time; any arrival order gives byte-identical
//! canonical JSON.
//!
//! The case count and seed are fixed so CI is deterministic. Shrunk
//! failures are kept in `tests/credit_properties.proptest-regressions`
//! (proptest replays them first) and, once understood, as named tests below.

use std::collections::BTreeMap;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, RngSeed};
use traction_core::{
    AttributionSource, CategorySec, CommitObservation, EndReason, EventType, HarnessId,
    MetricsOnly, MutationKind, NetworkClass, PresenceMinute, RollupInput, RollupOutput,
    SCHEMA_VERSION, TRACTION_RULES_1, TelemetryTier, TractionEvent, canonical_json, rollup,
};

const MIN: u64 = 60_000;
const T0: u64 = 1_800_000_000_000 - 1_800_000_000_000 % (2 * MIN);
const SPAN_MIN: u64 = 12;

fn config() -> Config {
    Config {
        cases: 96,
        rng_algorithm: RngAlgorithm::ChaCha,
        rng_seed: RngSeed::Fixed(0x7AC7_1002),
        max_shrink_iters: 4096,
        ..Config::default()
    }
}

#[derive(Debug, Clone)]
struct RawEvent {
    ts: u64,
    kind: EventType,
    session: u8,
    project: u8,
    manual: bool,
    parent: bool,
    diff: i64,
    tool: u8,
    hits: u8,
}

fn event_type() -> impl Strategy<Value = EventType> {
    // Weighted toward turn structure so overlaps and duplicate or missing
    // closes are common.
    prop_oneof![
        4 => Just(EventType::PromptSubmit),
        3 => Just(EventType::TurnEnd),
        2 => Just(EventType::ToolPre),
        2 => Just(EventType::ToolPost),
        2 => Just(EventType::ToolFail),
        2 => Just(EventType::PermissionRequest),
        2 => Just(EventType::FileMutation),
        1 => Just(EventType::StopFailure),
        1 => Just(EventType::SessionEnd),
        1 => Just(EventType::SessionStart),
        1 => Just(EventType::SubagentStart),
        1 => Just(EventType::SubagentStop),
        1 => Just(EventType::CwdChange),
    ]
}

fn timestamp() -> impl Strategy<Value = u64> {
    prop_oneof![
        // A flood point: many rows share one `timestamp_ms`.
        3 => prop::sample::select(vec![
            T0, T0 + 1, T0 + MIN - 1, T0 + MIN, T0 + 5 * MIN + 30_000,
        ]),
        6 => T0..T0 + (SPAN_MIN + 3) * MIN,
    ]
}

fn raw_event(ts: impl Strategy<Value = u64>) -> impl Strategy<Value = RawEvent> {
    (
        ts,
        event_type(),
        0u8..4,
        0u8..3,
        any::<bool>(),
        any::<bool>(),
        -200i64..200,
        0u8..2,
        0u8..5,
    )
        .prop_map(
            |(ts, kind, session, project, manual, parent, diff, tool, hits)| RawEvent {
                ts,
                kind,
                session,
                project,
                manual,
                parent,
                diff,
                tool,
                hits,
            },
        )
}

fn build(raw: &[RawEvent]) -> Vec<TractionEvent> {
    raw.iter()
        .enumerate()
        .map(|(i, r)| {
            let manual = r.manual && r.kind == EventType::FileMutation;
            let mut m = MetricsOnly::default();
            match r.kind {
                EventType::PromptSubmit => {
                    m.prompt_char_len = Some(u64::from(r.hits) * 20);
                    m.repo_symbol_hits = Some(u64::from(r.hits));
                    m.has_stacktrace = Some(r.parent);
                }
                EventType::ToolPre | EventType::ToolPost | EventType::ToolFail => {
                    m.tool_name = Some(format!("tool{}", r.tool));
                }
                EventType::FileMutation => {
                    m.mutation_kind = Some(MutationKind::Modified);
                    m.diff_lines_delta = Some(r.diff);
                }
                _ => {}
            }
            TractionEvent {
                schema_version: SCHEMA_VERSION,
                event_id: format!("ev-{i:06}"),
                timestamp_ms: r.ts,
                harness_id: (!manual).then_some(HarnessId::ClaudeCode),
                session_id: (!manual).then(|| format!("s{}", r.session)),
                parent_orchestrator_id: (r.parent && !manual).then(|| "s0".to_owned()),
                event_type: r.kind,
                end_reason: (r.kind == EventType::TurnEnd).then_some(EndReason::Completed),
                cwd: None,
                project_id: Some(format!("git:p{}", r.project)),
                model_id: None,
                network_class: if manual {
                    NetworkClass::Manual
                } else {
                    NetworkClass::CloudApi
                },
                attribution_source: if manual {
                    AttributionSource::FileWatcher
                } else {
                    AttributionSource::HarnessHook
                },
                metrics_only: m,
            }
        })
        .collect()
}

fn tier() -> impl Strategy<Value = TelemetryTier> {
    prop_oneof![
        Just(TelemetryTier::Full),
        Just(TelemetryTier::Reduced),
        Just(TelemetryTier::IdleOnly),
        Just(TelemetryTier::None),
    ]
}

fn presence_minute(minute: u64) -> impl Strategy<Value = PresenceMinute> {
    (
        (
            prop::option::of(0u32..200),
            prop::option::of(0u32..200),
            prop::option::of(0u32..250),
            prop::option::of(0u8..8), // 7 is out of range on purpose: the engine must not trust it
            prop::option::of(0u32..4000),
        ),
        (
            prop::option::of((0u32..60, 0u32..60, 0u32..60, 0u32..60, 0u32..60)),
            prop::option::of(0u32..40),
            prop::option::of(0u32..120),
            tier(),
        ),
    )
        .prop_map(
            move |((key, ptr, inj, slots, ent), (cats, sw, idle, tier))| PresenceMinute {
                minute_start_ms: T0 + minute * MIN,
                key_events: key,
                pointer_events: ptr,
                injected_events: inj,
                active_subslots: slots,
                cadence_entropy_milli: ent,
                category_sec: cats.map(|(ide, terminal, browser, ai_desktop, other)| CategorySec {
                    ide,
                    terminal,
                    browser,
                    ai_desktop,
                    other,
                }),
                category_switches: sw,
                idle_sec: idle,
                telemetry_tier: tier,
            },
        )
}

fn presence() -> impl Strategy<Value = Vec<PresenceMinute>> {
    // At most one row per minute, as the append-only table guarantees.
    prop::collection::vec(prop::option::weighted(0.8, 0..1u8), SPAN_MIN as usize).prop_flat_map(
        |present| {
            let rows: Vec<_> = present
                .iter()
                .enumerate()
                .filter(|(_, p)| p.is_some())
                .map(|(m, _)| presence_minute(m as u64))
                .collect();
            rows
        },
    )
}

fn commits() -> impl Strategy<Value = Vec<CommitObservation>> {
    prop::collection::vec(
        (0u8..3, 0u32..100, T0..T0 + SPAN_MIN * MIN).prop_map(|(p, n, t)| CommitObservation {
            project_id: format!("git:p{p}"),
            commit_oid: format!("{n:040}"),
            observed_ms: t,
        }),
        0..6,
    )
}

fn events() -> impl Strategy<Value = Vec<RawEvent>> {
    prop_oneof![
        3 => prop::collection::vec(raw_event(timestamp()), 0..250),
        // Thousands of events at one `timestamp_ms`.
        1 => prop::sample::select(vec![T0, T0 + MIN, T0 + 5 * MIN + 7])
            .prop_flat_map(|t| prop::collection::vec(raw_event(Just(t)), 1000..2200)),
    ]
}

fn run(
    events: &[TractionEvent],
    presence: &[PresenceMinute],
    commits: &[CommitObservation],
) -> RollupOutput {
    rollup(
        &TRACTION_RULES_1,
        &RollupInput {
            events,
            presence,
            commits,
            baseline: &[],
            from_ms: T0,
            to_ms: T0 + SPAN_MIN * MIN,
        },
    )
}

fn assert_caps(out: &RollupOutput) -> Result<(), TestCaseError> {
    let mut human: BTreeMap<u64, u32> = BTreeMap::new();
    let mut agent: BTreeMap<u64, u32> = BTreeMap::new();
    for b in &out.work_buckets {
        prop_assert!(b.idle_autonomous_sec <= b.agent_compute_sec, "{b:?}");
        prop_assert!(b.struggle_credit_sec <= b.human_active_sec, "{b:?}");
        prop_assert!(
            b.human_active_sec <= 60 && b.agent_compute_sec <= 60,
            "{b:?}"
        );
        *human.entry(b.minute_start_ms).or_default() += b.human_active_sec;
        *agent.entry(b.minute_start_ms).or_default() += b.agent_compute_sec;
        if b.injected_hid_detected {
            prop_assert_eq!(b.human_active_sec, 0, "{:?}", b);
        }
    }
    for (m, v) in &human {
        prop_assert!(*v <= 60, "human {v} s in minute {m}");
    }
    for (m, v) in &agent {
        prop_assert!(*v <= 60, "agent {v} s in minute {m}");
    }
    let range = u32::try_from(SPAN_MIN * 60).unwrap();
    prop_assert!(human.values().sum::<u32>() <= range);
    prop_assert!(agent.values().sum::<u32>() <= range);
    let credit: u32 = out.work_buckets.iter().map(|b| b.struggle_credit_sec).sum();
    prop_assert!(credit <= human.values().sum::<u32>());
    Ok(())
}

fn canonical(out: &RollupOutput) -> Vec<u8> {
    canonical_json(&serde_json::to_value(out).unwrap()).unwrap()
}

proptest! {
    #![proptest_config(config())]

    /// Every device-minute and the whole range stay within wall-clock time.
    #[test]
    fn credited_time_never_exceeds_elapsed_wall_clock(
        raw in events(),
        pm in presence(),
        cm in commits(),
    ) {
        let ev = build(&raw);
        assert_caps(&run(&ev, &pm, &cm))?;
    }

    /// Arrival order is irrelevant: byte-identical canonical JSON.
    #[test]
    fn any_arrival_order_gives_byte_identical_output(
        (raw, shuffled, pm, shuffled_pm) in (events(), presence()).prop_flat_map(|(raw, pm)| {
            let ev = build(&raw);
            (Just(raw), Just(ev).prop_shuffle(), Just(pm.clone()), Just(pm).prop_shuffle())
        }),
        cm in commits(),
    ) {
        let ev = build(&raw);
        let a = canonical(&run(&ev, &pm, &cm));
        let b = canonical(&run(&shuffled, &shuffled_pm, &cm));
        prop_assert_eq!(a, b);
    }
}

#[test]
fn a_flood_at_one_timestamp_cannot_mint_time() {
    // The shape that shrinks out of the flood generator, kept by hand: a
    // thousand prompts and turn closes sharing one millisecond.
    let raw: Vec<RawEvent> = (0..2000)
        .map(|i| RawEvent {
            ts: T0 + MIN,
            kind: if i % 2 == 0 {
                EventType::PromptSubmit
            } else {
                EventType::TurnEnd
            },
            session: (i % 4) as u8,
            project: (i % 3) as u8,
            manual: false,
            parent: i % 5 == 0,
            diff: 10,
            tool: 0,
            hits: 3,
        })
        .collect();
    let pm: Vec<PresenceMinute> = Vec::new();
    let out = run(&build(&raw), &pm, &[]);
    let agent: u32 = out.work_buckets.iter().map(|b| b.agent_compute_sec).sum();
    assert!(agent <= u32::try_from(SPAN_MIN * 60).unwrap());
    assert_caps_plain(&out);
}

fn assert_caps_plain(out: &RollupOutput) {
    assert_caps(out).expect("caps hold");
}
