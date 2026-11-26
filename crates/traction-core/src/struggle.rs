//! Struggle signals and credit (`docs/protocol.md` section 2c): the four
//! `struggle_axes`, `verified_struggle` and `struggle_credit_sec` of every
//! Work Bucket.
//!
//! Pure and integer-only like the rest of the rollup, so the daemon and the
//! Step 4 server agree byte for byte. Every constant is a [`Ruleset`] field.
//! No text, file name or path reaches this module: it reads event types,
//! counts and the metrics an Adapter already reduced.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::rollup::WorkBucket;
use crate::{AppCategory, EventType, PresenceMinute, Ruleset, TelemetryTier, TractionEvent};

const MINUTE_MS: u64 = 60_000;
const DAY_MS: u64 = 86_400_000;

/// Histogram bins per axis per baseline day (each 100 per mille wide). This
/// is the stored shape of [`BaselineDay`], not a tuning knob.
pub const BASELINE_BINS: usize = 10;
const BIN_WIDTH: u32 = 100;

/// The four axes of one Work Bucket, each 0 to 1000.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StruggleAxes {
    pub steering: u32,
    pub specificity: u32,
    pub friction: u32,
    pub comfort_delta: u32,
}

/// One day of the local comfort baseline: histograms of the friction and
/// specificity of the day's present minutes. Counts only, never events or
/// timestamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineDay {
    /// Days since the Unix epoch (UTC).
    pub day: u64,
    pub friction_hist: [u32; BASELINE_BINS],
    pub specificity_hist: [u32; BASELINE_BINS],
}

impl BaselineDay {
    pub fn new(day: u64) -> Self {
        Self {
            day,
            friction_hist: [0; BASELINE_BINS],
            specificity_hist: [0; BASELINE_BINS],
        }
    }

    /// Adds one present minute.
    pub fn record(&mut self, friction: u32, specificity: u32) {
        let bin = |v: u32| usize::try_from(v / BIN_WIDTH).unwrap_or(BASELINE_BINS - 1);
        let f = bin(friction).min(BASELINE_BINS - 1);
        let s = bin(specificity).min(BASELINE_BINS - 1);
        self.friction_hist[f] = self.friction_hist[f].saturating_add(1);
        self.specificity_hist[s] = self.specificity_hist[s].saturating_add(1);
    }
}

/// Folds the present minutes of `buckets` into `days` (sorted by day), then
/// prunes days older than `baseline_window_days` before the newest.
pub fn fold_baseline(rules: &Ruleset, days: &mut Vec<BaselineDay>, buckets: &[WorkBucket]) {
    for b in buckets.iter().filter(|b| b.human_active_sec > 0) {
        let day = b.minute_start_ms / DAY_MS;
        let at = match days.binary_search_by_key(&day, |d| d.day) {
            Ok(at) => at,
            Err(at) => {
                days.insert(at, BaselineDay::new(day));
                at
            }
        };
        days[at].record(b.struggle_axes.friction, b.struggle_axes.specificity);
    }
    if let Some(newest) = days.last().map(|d| d.day) {
        let keep_from = (newest + 1).saturating_sub(u64::from(rules.baseline_window_days));
        days.retain(|d| d.day >= keep_from);
    }
}

/// How often the user's own history looks like a low-friction, low-
/// specificity minute, per mille.
fn familiarity_milli(rules: &Ruleset, baseline: &[BaselineDay]) -> u32 {
    let (mut total_f, mut calm_f, mut total_s, mut low_s) = (0u64, 0u64, 0u64, 0u64);
    let low_bins = usize::try_from(rules.comfort_low_specificity_milli / BIN_WIDTH)
        .unwrap_or(BASELINE_BINS)
        .min(BASELINE_BINS);
    for d in baseline {
        total_f += d.friction_hist.iter().map(|c| u64::from(*c)).sum::<u64>();
        calm_f += u64::from(d.friction_hist[0]);
        total_s += d
            .specificity_hist
            .iter()
            .map(|c| u64::from(*c))
            .sum::<u64>();
        low_s += d.specificity_hist[..low_bins]
            .iter()
            .map(|c| u64::from(*c))
            .sum::<u64>();
    }
    let share = |part: u64, total: u64| (part * 1000).checked_div(total).unwrap_or(0);
    u32::try_from(share(calm_f, total_f).min(share(low_s, total_s))).unwrap_or(1000)
}

/// `(comfort_delta axis, comfort weight)`, both per mille. A minute with
/// friction or real specificity is never comfortable; with fewer than
/// `baseline_min_days` of history nothing is.
fn comfort(
    rules: &Ruleset,
    baseline: &[BaselineDay],
    friction: u32,
    specificity: u32,
) -> (u32, u32) {
    let days = u32::try_from(baseline.len()).unwrap_or(u32::MAX);
    if days < rules.baseline_min_days
        || friction >= BIN_WIDTH
        || specificity >= rules.comfort_low_specificity_milli
    {
        return (1000, 1000);
    }
    let familiar = familiarity_milli(rules, baseline);
    let weight = 1000 - (1000 - rules.comfort_floor_milli) * familiar / 1000;
    (1000 - familiar, weight)
}

/// Everything the scorer needs besides the buckets.
pub(crate) struct ScoreInput<'a> {
    /// Events with a Project, sorted by `(timestamp, event_id)`.
    pub sorted: &'a [&'a TractionEvent],
    pub presence: &'a BTreeMap<u64, &'a PresenceMinute>,
    /// Approved permissions that earned real-input credit.
    pub permission_anchors: &'a [(u64, String)],
    pub baseline: &'a [BaselineDay],
}

struct Loop {
    start: u64,
    end: u64,
}

fn is_closer(t: EventType) -> bool {
    matches!(
        t,
        EventType::TurnEnd | EventType::StopFailure | EventType::SessionEnd
    )
}

fn minute_of(ms: u64) -> u64 {
    ms - ms % MINUTE_MS
}

/// Fills `struggle_axes`, `verified_struggle` and `struggle_credit_sec` of
/// every bucket.
pub(crate) fn score(rules: &Ruleset, input: &ScoreInput<'_>, buckets: &mut [WorkBucket]) {
    let mut by_session: BTreeMap<&str, Vec<&TractionEvent>> = BTreeMap::new();
    let mut mutations: BTreeMap<&str, Vec<&TractionEvent>> = BTreeMap::new();
    for e in input.sorted {
        let Some(project) = e.project_id.as_deref() else {
            continue;
        };
        if let Some(s) = e.session_id.as_deref() {
            by_session.entry(s).or_default().push(e);
        }
        if e.event_type == EventType::FileMutation {
            mutations.entry(project).or_default().push(e);
        }
    }

    let mut anchors: BTreeMap<String, Vec<(u64, u32)>> = BTreeMap::new();
    let mut loops: BTreeMap<String, Vec<Loop>> = BTreeMap::new();
    let mut spans: BTreeMap<String, Vec<(u64, u64, u32)>> = BTreeMap::new();
    let mut anchor = |project: &str, ts: u64, strength: u32| {
        anchors
            .entry(project.to_owned())
            .or_default()
            .push((ts, strength));
    };

    for e in mutations.values().flatten() {
        // Manual Work: a mutation no Harness session owns.
        if e.session_id.is_none() {
            anchor(
                e.project_id.as_deref().unwrap_or_default(),
                e.timestamp_ms,
                1000,
            );
        }
    }
    for (ts, p) in input.permission_anchors {
        anchor(p, *ts, 1000);
    }
    for b in buckets.iter().filter(|b| b.commits_human > 0) {
        anchor(&b.project_id, b.minute_start_ms, 1000);
    }

    for (session, events) in &by_session {
        session_signals(
            rules,
            session,
            events,
            &mutations,
            &mut anchor,
            &mut loops,
            &mut spans,
        );
    }
    // Per Project: the anchor times (for steering) and the strongest
    // strength per timestamp (for credit).
    let mut times: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    for (project, list) in &mut anchors {
        list.sort_unstable();
        list.dedup_by(|b, a| {
            let same = a.0 == b.0;
            if same {
                a.1 = a.1.max(b.1);
            }
            same
        });
        times.insert(project.clone(), list.iter().map(|(t, _)| *t).collect());
    }

    // Manual Work right after Harness churn reverts it (the watcher sees
    // no contents, so this is the only observable shape of a revert).
    let mut reverts: BTreeMap<&str, BTreeSet<u64>> = BTreeMap::new();
    for (project, list) in &mutations {
        let harness: Vec<u64> = list
            .iter()
            .filter(|e| e.session_id.is_some())
            .map(|e| e.timestamp_ms)
            .collect();
        for e in list.iter().filter(|e| e.session_id.is_none()) {
            let low = e.timestamp_ms.saturating_sub(rules.churn_revert_window_ms);
            if harness.iter().any(|t| *t >= low && *t <= e.timestamp_ms) {
                reverts
                    .entry(project)
                    .or_default()
                    .insert(minute_of(e.timestamp_ms));
            }
        }
    }

    // Present minutes per Project, for steering.
    let present: BTreeSet<(String, u64)> = buckets
        .iter()
        .filter(|b| b.human_active_sec > 0)
        .map(|b| (b.project_id.clone(), b.minute_start_ms))
        .collect();

    for b in buckets.iter_mut() {
        let project = b.project_id.as_str();
        let m = b.minute_start_ms;
        let list: &[u64] = times.get(project).map_or(&[], Vec::as_slice);
        let strong: &[(u64, u32)] = anchors.get(project).map_or(&[], Vec::as_slice);

        let friction = {
            let in_loop = loops.get(project).is_some_and(|ls| {
                ls.iter()
                    .any(|l| minute_of(l.start) <= m && m <= minute_of(l.end))
            });
            let revert = reverts.get(project).is_some_and(|set| set.contains(&m));
            if in_loop {
                1000
            } else if revert {
                rules.friction_revert_milli
            } else {
                0
            }
        };
        let specificity = spans.get(project).map_or(0, |ss| {
            ss.iter()
                .filter(|(s, e, _)| minute_of(*s) <= m && m <= minute_of(*e))
                .map(|(_, _, v)| *v)
                .max()
                .unwrap_or(0)
        });
        let steering = steering(rules, input.presence, &present, list, project, m);
        let (comfort_delta, comfort_weight) = comfort(rules, input.baseline, friction, specificity);

        let axes = StruggleAxes {
            steering,
            specificity,
            friction,
            comfort_delta,
        };
        let weighted = u64::from(axes.steering) * u64::from(rules.weight_steering_milli)
            + u64::from(axes.specificity) * u64::from(rules.weight_specificity_milli)
            + u64::from(axes.friction) * u64::from(rules.weight_friction_milli)
            + u64::from(axes.comfort_delta) * u64::from(rules.weight_comfort_milli);
        b.struggle_axes = axes;
        b.verified_struggle = u32::try_from(weighted / 1000).unwrap_or(1000).min(1000);

        let anchor_weight = anchor_weight(rules, input.presence.get(&m).copied(), strong, m);
        let mut weight = u64::from(anchor_weight) * u64::from(comfort_weight) / 1000;
        if matches!(
            b.telemetry_tier,
            TelemetryTier::Reduced | TelemetryTier::IdleOnly | TelemetryTier::None
        ) {
            weight = weight * u64::from(rules.degraded_tier_credit_milli) / 1000;
        }
        b.struggle_credit_sec = u32::try_from(u64::from(b.human_active_sec) * weight / 1000)
            .unwrap_or(0)
            .min(b.human_active_sec);
    }
}

/// The distance from minute `m` to the nearest anchor in `list`; 0 when one
/// lands inside the minute.
fn nearest(list: &[u64], m: u64) -> Option<u64> {
    let i = list.partition_point(|a| *a < m);
    let after = list.get(i).map(|a| a.saturating_sub(m + MINUTE_MS - 1));
    let before = i.checked_sub(1).map(|j| m - list[j]);
    match (before, after) {
        (Some(b), Some(a)) => Some(b.min(a)),
        (b, a) => b.or(a),
    }
}

/// The anchor weight (per mille) of a present minute: the strongest anchor
/// within `anchor_window_ms`, each scaled by its own strength. A `browser` /
/// `other` minute holding no anchor of its own decays linearly from 1 to 0
/// over `drift_decay_ms` of distance, and keeps counting anchors that far.
fn anchor_weight(
    rules: &Ruleset,
    presence: Option<&PresenceMinute>,
    anchors: &[(u64, u32)],
    m: u64,
) -> u32 {
    let drifting = presence.and_then(|p| p.category_sec).is_some_and(|c| {
        let drift = u64::from(c.get(AppCategory::Browser)) + u64::from(c.get(AppCategory::Other));
        drift > u64::from(c.total()) - drift
    });
    let mut best = 0u32;
    for (ts, strength) in anchors {
        let d = if *ts < m {
            m - ts
        } else {
            ts.saturating_sub(m + MINUTE_MS - 1)
        };
        // A drifting minute decays over the whole `drift_decay_ms`; any
        // other minute counts anchors only inside `anchor_window_ms`.
        let reach = if drifting {
            rules.drift_decay_ms.max(rules.anchor_window_ms)
        } else {
            rules.anchor_window_ms
        };
        if d > reach {
            continue;
        }
        let mut w = *strength;
        if drifting && d > 0 {
            let left = rules.drift_decay_ms.saturating_sub(d);
            w = u32::try_from(u64::from(w) * left / rules.drift_decay_ms.max(1)).unwrap_or(0);
        }
        best = best.max(w);
    }
    best
}

/// The share of present minutes within two minutes of `m` that switch
/// between work categories close to an anchor.
fn steering(
    rules: &Ruleset,
    presence: &BTreeMap<u64, &PresenceMinute>,
    present: &BTreeSet<(String, u64)>,
    anchors: &[u64],
    project: &str,
    m: u64,
) -> u32 {
    let (mut total, mut interleaved) = (0u32, 0u32);
    for k in m.saturating_sub(2 * MINUTE_MS)..=m + 2 * MINUTE_MS {
        if k % MINUTE_MS != 0 || !present.contains(&(project.to_owned(), k)) {
            continue;
        }
        total += 1;
        let switching = presence.get(&k).is_some_and(|p| {
            let kinds = p.category_sec.map_or(0, |c| {
                AppCategory::ALL
                    .iter()
                    .filter(|a| **a != AppCategory::Other && c.get(**a) > 0)
                    .count()
            });
            p.category_switches.unwrap_or(0) >= 1 && kinds >= 2
        });
        if switching && nearest(anchors, k).is_some_and(|d| d <= rules.steering_window_ms) {
            interleaved += 1;
        }
    }
    (interleaved * 1000).checked_div(total).unwrap_or(0)
}

/// One session's anchors, friction loops and prompt specificity spans.
fn session_signals(
    rules: &Ruleset,
    session: &str,
    events: &[&TractionEvent],
    mutations: &BTreeMap<&str, Vec<&TractionEvent>>,
    anchor: &mut impl FnMut(&str, u64, u32),
    loops: &mut BTreeMap<String, Vec<Loop>>,
    spans: &mut BTreeMap<String, Vec<(u64, u64, u32)>>,
) {
    let stale = rules.stale_turn_timeout_ms;
    let mut open = false;
    let mut last = 0u64;
    let mut fail: Option<(&str, u64)> = None;
    let mut prompt_strength = 0u32;
    for (i, e) in events.iter().enumerate() {
        let project = e.project_id.as_deref().unwrap_or_default();
        if open && e.timestamp_ms.saturating_sub(last) > stale {
            open = false;
        }
        last = e.timestamp_ms;
        match e.event_type {
            EventType::PromptSubmit => {
                open = true;
                let (end, diff) = turn_extent(events, i, stale);
                let value = prompt_specificity(rules, e, diff);
                prompt_strength = value;
                spans
                    .entry(project.to_owned())
                    .or_default()
                    .push((e.timestamp_ms, end, value));
            }
            t if is_closer(t) => open = false,
            // An agent edit is only as strong an anchor as the prompt that
            // asked for it: a terse "continue" proves nothing.
            EventType::FileMutation if open => anchor(project, e.timestamp_ms, prompt_strength),
            EventType::ToolFail => {
                if let Some(name) = e.metrics_only.tool_name.as_deref() {
                    match fail {
                        Some((n, _)) if n == name => {}
                        _ => fail = Some((name, e.timestamp_ms)),
                    }
                }
            }
            EventType::ToolPost => {
                let name = e.metrics_only.tool_name.as_deref();
                if let (Some((n, start)), Some(name)) = (fail, name)
                    && n == name
                {
                    fail = None;
                    let edited = e.timestamp_ms.saturating_sub(start) <= stale
                        && mutations.get(project).is_some_and(|ms| {
                            ms.iter().any(|m| {
                                m.timestamp_ms >= start
                                    && m.timestamp_ms <= e.timestamp_ms
                                    && (m.session_id.is_none()
                                        || m.session_id.as_deref() == Some(session))
                            })
                        });
                    if edited {
                        loops.entry(project.to_owned()).or_default().push(Loop {
                            start,
                            end: e.timestamp_ms,
                        });
                        anchor(project, e.timestamp_ms, 1000);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The end of the turn a prompt at `events[i]` starts, and the lines its
/// agent changed.
fn turn_extent(events: &[&TractionEvent], i: usize, stale: u64) -> (u64, u64) {
    let mut end = events[i].timestamp_ms;
    let mut diff = 0u64;
    for e in &events[i + 1..] {
        if e.timestamp_ms.saturating_sub(end) > stale
            || e.event_type == EventType::PromptSubmit
            || is_closer(e.event_type)
        {
            if is_closer(e.event_type) && e.timestamp_ms.saturating_sub(end) <= stale {
                end = e.timestamp_ms;
            }
            break;
        }
        end = e.timestamp_ms;
        diff = diff.saturating_add(e.metrics_only.diff_lines_delta.map_or(0, i64::unsigned_abs));
    }
    (end, diff)
}

/// One prompt's specificity: distinct repo symbols it names plus a pasted
/// stack trace, cut back when a terse prompt launched a large turn.
fn prompt_specificity(rules: &Ruleset, prompt: &TractionEvent, turn_diff_lines: u64) -> u32 {
    let m = &prompt.metrics_only;
    let hits = m.repo_symbol_hits.unwrap_or(0).min(1000);
    let mut value = u32::try_from(hits)
        .unwrap_or(1000)
        .saturating_mul(rules.specificity_symbol_milli);
    if m.has_stacktrace == Some(true) {
        value = value.saturating_add(rules.specificity_stacktrace_milli);
    }
    value = value.min(1000);
    let terse = m
        .prompt_char_len
        .is_none_or(|n| n < rules.terse_prompt_chars);
    if terse && turn_diff_lines >= rules.large_turn_diff_lines {
        value = value * rules.terse_big_turn_keep_milli / 1000;
    }
    value
}
