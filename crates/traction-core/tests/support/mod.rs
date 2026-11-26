//! Shared test support: event builders and the seeded persona simulator
//! (`docs/protocol.md` section 2c, Step 2 Slice 4). Everything here is
//! deterministic: one seed gives one byte-identical 60-minute stream.
#![allow(dead_code)]

use traction_core::{
    AppCategory, AttributionSource, CategorySec, EndReason, EventType, HarnessId, MetricsOnly,
    MutationKind, NetworkClass, PresenceMinute, RollupInput, SCHEMA_VERSION, TRACTION_RULES_1,
    TelemetryTier, TractionEvent, WorkBucket, work_buckets,
};

pub const MIN: u64 = 60_000;
pub const T0: u64 = 1_800_000_000_000 - 1_800_000_000_000 % (2 * MIN);
pub const PROJECT: &str = "git:persona";
pub const HOUR_MIN: u64 = 60;

/// SplitMix64: tiny, seedable, identical on every OS.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    /// Uniform integer in `[lo, hi]`.
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo + 1)
    }
    pub fn chance(&mut self, p: f64) -> bool {
        self.unit() < p
    }
    /// Log-normal with the given median and sigma (Box-Muller).
    pub fn lognormal(&mut self, median: f64, sigma: f64) -> f64 {
        let (u1, u2) = (self.unit().max(1e-12), self.unit());
        let z = (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos();
        median * (sigma * z).exp()
    }
}

// ---- events ------------------------------------------------------------

#[derive(Default)]
pub struct Stream {
    pub events: Vec<TractionEvent>,
    n: u32,
}

impl Stream {
    fn push(&mut self, ts: u64, kind: EventType, session: Option<&str>, metrics: MetricsOnly) {
        self.n += 1;
        let manual = session.is_none();
        self.events.push(TractionEvent {
            schema_version: SCHEMA_VERSION,
            event_id: format!("ev-{:06}", self.n),
            timestamp_ms: ts,
            harness_id: (!manual).then_some(HarnessId::ClaudeCode),
            session_id: session.map(str::to_owned),
            parent_orchestrator_id: None,
            event_type: kind,
            end_reason: (kind == EventType::TurnEnd).then_some(EndReason::Completed),
            cwd: None,
            project_id: Some(PROJECT.to_owned()),
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
            metrics_only: metrics,
        });
    }

    pub fn prompt(&mut self, ts: u64, chars: u64, hits: u64, trace: bool) {
        self.push(
            ts,
            EventType::PromptSubmit,
            Some("s1"),
            MetricsOnly {
                prompt_char_len: Some(chars),
                repo_symbol_hits: Some(hits),
                has_stacktrace: Some(trace),
                ..MetricsOnly::default()
            },
        );
    }
    pub fn tool(&mut self, ts: u64, kind: EventType, name: &str, exit: i64) {
        self.push(
            ts,
            kind,
            Some("s1"),
            MetricsOnly {
                tool_name: Some(name.to_owned()),
                exit_code: Some(exit),
                ..MetricsOnly::default()
            },
        );
    }
    pub fn turn_end(&mut self, ts: u64) {
        self.push(ts, EventType::TurnEnd, Some("s1"), MetricsOnly::default());
    }
    pub fn mutation(&mut self, ts: u64, manual: bool, diff: i64) {
        self.push(
            ts,
            EventType::FileMutation,
            (!manual).then_some("s1"),
            MetricsOnly {
                mutation_kind: Some(MutationKind::Modified),
                diff_lines_delta: Some(diff),
                ..MetricsOnly::default()
            },
        );
    }
}

// ---- presence ----------------------------------------------------------

/// How a minute's real input behaves.
pub enum Input {
    /// Human timing: log-normal intervals around `median_ms`, occasional
    /// pauses, reaching all six sub-slots.
    Human { median_ms: f64 },
    /// Exactly these offsets (ms into the minute) of real input.
    Taps(Vec<u64>),
    /// `n` synthetic events (flagged by the OS), no real input.
    Injected(u32),
    /// Nothing.
    None,
}

fn human_offsets(rng: &mut Rng, median_ms: f64) -> Vec<u64> {
    let mut out = Vec::new();
    let mut t = rng.range(0, 1500) as f64;
    while t < 60_000.0 {
        out.push(t as u64);
        t += rng.lognormal(median_ms, 1.0).clamp(60.0, 9_000.0);
        if rng.chance(0.04) {
            t += rng.range(3_000, 8_000) as f64;
        }
    }
    out
}

/// Entropy (bits x 1000) of log2-binned real inter-event intervals.
fn cadence_entropy_milli(offsets: &[u64], min_samples: u32) -> Option<u32> {
    let gaps: Vec<u64> = offsets.windows(2).map(|w| w[1] - w[0]).collect();
    if gaps.len() < min_samples as usize {
        return None;
    }
    let mut bins = [0u32; 20];
    for g in &gaps {
        bins[(63 - (*g).max(1).leading_zeros() as usize).min(19)] += 1;
    }
    let n = gaps.len() as f64;
    let h: f64 = bins
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = f64::from(*c) / n;
            -p * p.log2()
        })
        .sum();
    Some((h * 1000.0).round() as u32)
}

fn idle_sec(offsets: &[u64]) -> u32 {
    let mut last = 0u64;
    let mut idle = 0u64;
    for t in offsets.iter().copied().chain(std::iter::once(60_000)) {
        idle += (t - last).saturating_sub(30_000) / 1000;
        last = t;
    }
    idle as u32
}

/// One Full-tier Presence Minute. `slots` is the foreground category of
/// each 10 s sub-slot.
pub fn presence_minute(
    rng: &mut Rng,
    minute_start_ms: u64,
    input: Input,
    slots: [AppCategory; 6],
) -> PresenceMinute {
    let (offsets, injected) = match input {
        Input::Human { median_ms } => (human_offsets(rng, median_ms), 0),
        Input::Taps(t) => (t, 0),
        Input::Injected(n) => (Vec::new(), n),
        Input::None => (Vec::new(), 0),
    };
    let mut subslots = [false; 6];
    for t in &offsets {
        subslots[(*t / 10_000).min(5) as usize] = true;
    }
    let mut cats = CategorySec::default();
    for c in slots {
        cats.add(c, 10);
    }
    PresenceMinute {
        minute_start_ms,
        // Counts include injected events (`docs/protocol.md` section 2b).
        key_events: Some((offsets.iter().step_by(2).count() as u32) + injected.div_ceil(2)),
        pointer_events: Some((offsets.iter().skip(1).step_by(2).count() as u32) + injected / 2),
        injected_events: Some(injected),
        active_subslots: Some(subslots.iter().filter(|s| **s).count() as u8),
        cadence_entropy_milli: cadence_entropy_milli(
            &offsets,
            TRACTION_RULES_1.min_cadence_samples,
        ),
        category_sec: Some(cats),
        category_switches: Some(slots.windows(2).filter(|w| w[0] != w[1]).count() as u32),
        idle_sec: Some(if injected > 0 { 0 } else { idle_sec(&offsets) }),
        telemetry_tier: TelemetryTier::Full,
    }
}

/// The platform-degraded view of a Full minute: what a `reduced` or
/// `idle_only` backend can fill. Fields it cannot fill are `None`.
pub fn degrade(m: &PresenceMinute, tier: TelemetryTier) -> PresenceMinute {
    let mut m = m.clone();
    m.telemetry_tier = tier;
    if tier != TelemetryTier::Full {
        // What the real aggregator emits below `full`: no counts or cadence,
        // but `active_subslots` from the OS idle timer (reset inside the
        // sub-slot), where injected input cannot be told from real input,
        // and `idle_sec` in whole sub-slots. `none` reports nothing.
        let injected_slots = u8::try_from(m.injected_events.unwrap_or(0).min(6)).unwrap_or(6);
        let timer_active = m.active_subslots.unwrap_or(0).max(injected_slots);
        m.key_events = None;
        m.pointer_events = None;
        m.injected_events = None;
        m.cadence_entropy_milli = None;
        m.idle_sec = m.idle_sec.map(|i| i / 10 * 10);
        m.active_subslots = (tier != TelemetryTier::None).then_some(timer_active);
        if tier == TelemetryTier::None {
            m.idle_sec = None;
        }
    }
    if matches!(tier, TelemetryTier::IdleOnly | TelemetryTier::None) {
        m.category_sec = None;
        m.category_switches = None;
    }
    m
}

/// A sticky random walk over the four work categories.
pub fn work_slots(rng: &mut Rng, switch_p: f64, drift_p: f64) -> [AppCategory; 6] {
    use AppCategory::*;
    let mut out = [Ide; 6];
    let mut cur = Ide;
    for slot in &mut out {
        if rng.chance(switch_p) {
            cur = match rng.range(0, 9) {
                0..=3 => Ide,
                4..=5 => Terminal,
                6..=7 => AiDesktop,
                _ => Browser,
            };
        }
        if rng.chance(drift_p) {
            cur = Browser;
        }
        *slot = cur;
    }
    out
}

// ---- personas ----------------------------------------------------------

pub struct Persona {
    pub name: &'static str,
    pub events: Vec<TractionEvent>,
    pub presence: Vec<PresenceMinute>,
}

fn s(sec: u64) -> u64 {
    T0 + sec * 1000
}

/// Specific prompts, diff reading, hand edits, fail then pass tests,
/// category switching, human timing. Six-minute debug cycles.
pub fn honest_debugger(seed: u64) -> Persona {
    let mut rng = Rng::new(seed);
    let mut st = Stream::default();
    for c in 0..10u64 {
        let b = c * 360;
        st.prompt(s(b), 240, 3, true);
        st.tool(s(b + 12), EventType::ToolPre, "Bash", 0);
        st.tool(s(b + 20), EventType::ToolFail, "Bash", 1);
        st.turn_end(s(b + 60));
        for (k, off) in [100u64, 170, 240].into_iter().enumerate() {
            st.mutation(s(b + off), true, 4 + k as i64);
        }
        st.prompt(s(b + 270), 90, 2, false);
        st.tool(s(b + 280), EventType::ToolPre, "Bash", 0);
        st.mutation(s(b + 290), false, 14);
        st.tool(s(b + 310), EventType::ToolPost, "Bash", 0);
        st.turn_end(s(b + 330));
    }
    let presence = (0..HOUR_MIN)
        .map(|m| {
            let input = if m == 29 {
                Input::None
            } else {
                Input::Human {
                    median_ms: if m % 6 == 0 { 2000.0 } else { 900.0 },
                }
            };
            let slots = work_slots(&mut rng, 0.35, 0.05);
            presence_minute(&mut rng, T0 + m * MIN, input, slots)
        })
        .collect();
    Persona {
        name: "honest_debugger",
        events: st.events,
        presence,
    }
}

/// Agent runs the whole hour; one real key every 59 s.
pub fn key_tapper_59s(seed: u64) -> Persona {
    let mut rng = Rng::new(seed);
    let mut st = Stream::default();
    st.prompt(s(0), 50, 0, false);
    for k in 0..119u64 {
        st.tool(s(5 + k * 30), EventType::ToolPre, "Bash", 0);
        st.tool(s(10 + k * 30), EventType::ToolPost, "Bash", 0);
        if k % 8 == 0 {
            st.mutation(s(12 + k * 30), false, 20);
        }
    }
    st.turn_end(s(3599));
    let presence = (0..HOUR_MIN)
        .map(|m| {
            let taps: Vec<u64> = (0..61u64)
                .map(|k| k * 59_000)
                .filter(|t| *t / MIN == m)
                .map(|t| t % MIN)
                .collect();
            presence_minute(
                &mut rng,
                T0 + m * MIN,
                Input::Taps(taps),
                [AppCategory::Terminal; 6],
            )
        })
        .collect();
    Persona {
        name: "key_tapper_59s",
        events: st.events,
        presence,
    }
}

/// `"continue"` / `"fix all"` every ~2 minutes, large agent turns, no own
/// edits or tests; the human is really there (watching, scrolling).
pub fn trivial_prompt_spammer(seed: u64) -> Persona {
    let mut rng = Rng::new(seed);
    let mut st = Stream::default();
    let mut t = 5u64;
    while t + 100 < 3600 {
        st.prompt(s(t), if rng.chance(0.5) { 8 } else { 7 }, 0, false);
        st.tool(s(t + 10), EventType::ToolPre, "Bash", 0);
        st.mutation(s(t + 30), false, 150);
        st.tool(s(t + 60), EventType::ToolPost, "Bash", 0);
        st.turn_end(s(t + 95));
        t += rng.range(100, 140);
    }
    let presence = (0..HOUR_MIN)
        .map(|m| {
            let slots = work_slots(&mut rng, 0.35, 0.25);
            presence_minute(
                &mut rng,
                T0 + m * MIN,
                Input::Human { median_ms: 1200.0 },
                slots,
            )
        })
        .collect();
    Persona {
        name: "trivial_prompt_spammer",
        events: st.events,
        presence,
    }
}

/// Agent runs all hour in ten-minute turns started by a terse prompt; a
/// bot replays log-normal real key/pointer cadence across every sub-slot
/// with category switching, and has no Outcome Anchors of its own (no hand
/// edits, no commits, no fail then pass loops).
pub fn steer_rhythm_faker(seed: u64, injected: bool) -> Persona {
    let mut rng = Rng::new(seed);
    let mut st = Stream::default();
    for c in 0..6u64 {
        let b = c * 600;
        st.prompt(s(b), 12, 0, false);
        for k in 0..19u64 {
            st.tool(s(b + 5 + k * 30), EventType::ToolPre, "Bash", 0);
            st.tool(s(b + 10 + k * 30), EventType::ToolPost, "Bash", 0);
            if k % 4 == 0 {
                st.mutation(s(b + 12 + k * 30), false, 40);
            }
        }
        st.turn_end(s(b + 599));
    }
    let presence = (0..HOUR_MIN)
        .map(|m| {
            let slots = work_slots(&mut rng, 0.5, 0.0);
            let input = if injected {
                Input::Injected(60)
            } else {
                Input::Human { median_ms: 700.0 }
            };
            presence_minute(&mut rng, T0 + m * MIN, input, slots)
        })
        .collect();
    Persona {
        name: if injected {
            "steer_rhythm_faker_injected"
        } else {
            "steer_rhythm_faker"
        },
        events: st.events,
        presence,
    }
}

/// Apply a tier to every minute of a persona.
pub fn at_tier(p: &Persona, tier: TelemetryTier) -> Persona {
    Persona {
        name: p.name,
        events: p.events.clone(),
        presence: p.presence.iter().map(|m| degrade(m, tier)).collect(),
    }
}

pub fn score(p: &Persona) -> Vec<WorkBucket> {
    work_buckets(
        &TRACTION_RULES_1,
        &RollupInput {
            events: &p.events,
            presence: &p.presence,
            commits: &[],
            baseline: &[],
            from_ms: T0,
            to_ms: T0 + HOUR_MIN * MIN,
        },
    )
}

/// `struggle_credit_sec` per mille of the 3600 s wall clock.
pub fn credit_milli(buckets: &[WorkBucket]) -> u32 {
    let sum: u32 = buckets.iter().map(|b| b.struggle_credit_sec).sum();
    sum * 1000 / (HOUR_MIN as u32 * 60)
}
