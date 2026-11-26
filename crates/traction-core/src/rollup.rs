//! The Three-Clock rollup (`docs/protocol.md` section 3): a pure,
//! deterministic function from a [`Ruleset`], Traction Events, Presence
//! Minutes and Commit Observations to 60 s Work Buckets and 120 s Activity
//! Rollups.
//!
//! It does no I/O, reads no clock and uses no randomness or floats, so the
//! daemon and the Step 4 server recompute byte-identical results. Inputs may
//! arrive in any order; they are sorted by `(timestamp, event_id)` first.
//!
//! Time conventions: an interval is half-open `[start, end)` in
//! milliseconds; a second `s` belongs to an interval when `s * 1000` lies
//! inside it, so a minute-aligned 10-minute turn is exactly 600 s.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::struggle::{self, BaselineDay, ScoreInput, StruggleAxes};
use crate::{AppCategory, EventType, PresenceMinute, Ruleset, TelemetryTier, TractionEvent};

const MINUTE_MS: u64 = 60_000;
/// Fixed-point scale for splitting a second among Projects: divisible by
/// every share count up to 12, and rounding is always down so the shares of
/// one second never sum to more than that second.
const SCALE: u64 = 27_720;

/// A commit the daemon observed (`commit_observation` table). Only the
/// object id and the observation time: never a message, author or diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommitObservation {
    pub project_id: String,
    pub commit_oid: String,
    pub observed_ms: u64,
}

/// Everything the engine reads. `from_ms` and `to_ms` are minute-aligned;
/// buckets are produced for the minutes in `[from_ms, to_ms)`. `to_ms` is
/// also the engine's "now": a turn still open there is closed at its last
/// event if it has been silent longer than the stale-turn timeout.
#[derive(Debug, Clone, Copy)]
pub struct RollupInput<'a> {
    pub events: &'a [TractionEvent],
    pub presence: &'a [PresenceMinute],
    pub commits: &'a [CommitObservation],
    /// The local comfort baseline (`struggle::BaselineDay`s, oldest first);
    /// empty for a user with no history.
    pub baseline: &'a [BaselineDay],
    pub from_ms: u64,
    pub to_ms: u64,
}

/// One Project's three clocks for one minute.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkBucket {
    pub project_id: String,
    pub minute_start_ms: u64,
    pub ruleset_id: String,
    pub agent_compute_sec: u32,
    /// Unverified Autonomous Time: a subset of `agent_compute_sec`.
    pub idle_autonomous_sec: u32,
    pub human_active_sec: u32,
    pub permission_wait_ms: u64,
    pub commits_human: u32,
    pub commits_agent: u32,
    pub injected_hid_detected: bool,
    pub auto_approver_suspected: bool,
    pub telemetry_tier: TelemetryTier,
    /// The category clause could not be checked (`idle_only`/`none` tier or
    /// no category data), so verified presence rests on input alone.
    pub tier_limited: bool,
    pub struggle_axes: StruggleAxes,
    /// Weighted mean of the four axes, 0 to 1000.
    pub verified_struggle: u32,
    /// `human_active_sec` x the anchor and comfort weight; never above
    /// `human_active_sec`.
    pub struggle_credit_sec: u32,
}

/// Per-category share of foreground time, per mille.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategoryShareMilli {
    pub ide: u32,
    pub terminal: u32,
    pub browser: u32,
    pub ai_desktop: u32,
    pub other: u32,
}

/// One device's 120 s audit window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityRollup {
    pub window_start_ms: u64,
    pub ruleset_id: String,
    /// Presence Minutes that exist in the window (0 to 2).
    pub minutes_observed: u32,
    pub category_share_milli: CategoryShareMilli,
    /// Category switches per observed minute, times 1000.
    pub switch_rate_milli: u32,
    pub mean_cadence_entropy_milli: Option<u32>,
    /// Injected share of input events, per mille; `None` when no minute
    /// carried input counts.
    pub injected_ratio_milli: Option<u32>,
    /// The weakest tier among the observed minutes.
    pub telemetry_tier: TelemetryTier,
}

/// Buckets and rollups for one range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollupOutput {
    pub work_buckets: Vec<WorkBucket>,
    pub activity_rollups: Vec<ActivityRollup>,
}

/// Work Buckets plus Activity Rollups for the whole range.
pub fn rollup(rules: &Ruleset, input: &RollupInput<'_>) -> RollupOutput {
    RollupOutput {
        work_buckets: work_buckets(rules, input),
        activity_rollups: activity_rollups(rules, input.presence, input.from_ms, input.to_ms),
    }
}

fn minute_of(ms: u64) -> u64 {
    ms - ms % MINUTE_MS
}

fn is_work_category(c: AppCategory) -> bool {
    !matches!(c, AppCategory::Other)
}

/// Per-minute facts derived from one Presence Minute.
#[derive(Clone, Copy)]
struct MinuteFacts {
    tier: TelemetryTier,
    flagged: bool,
    verified: bool,
    tier_limited: bool,
}

const MISSING: MinuteFacts = MinuteFacts {
    tier: TelemetryTier::None,
    flagged: false,
    verified: false,
    tier_limited: true,
};

fn injected_ratio_milli(m: &PresenceMinute) -> Option<u32> {
    let injected = u64::from(m.injected_events?);
    let real = u64::from(m.key_events.unwrap_or(0)) + u64::from(m.pointer_events.unwrap_or(0));
    let total = injected + real;
    (total > 0).then(|| u32::try_from(injected * 1000 / total).unwrap_or(1000))
}

fn facts(rules: &Ruleset, m: &PresenceMinute) -> MinuteFacts {
    let ratio_flag = injected_ratio_milli(m).is_some_and(|r| r >= rules.injected_ratio_flag_milli);
    let robotic = m
        .cadence_entropy_milli
        .is_some_and(|e| e < rules.robotic_entropy_max_milli);
    let flagged = ratio_flag || robotic;
    let category_unknown = matches!(
        m.telemetry_tier,
        TelemetryTier::IdleOnly | TelemetryTier::None
    ) || m.category_sec.is_none();
    let category_ok = category_unknown
        || m.category_sec.is_some_and(|c| {
            AppCategory::ALL
                .iter()
                .any(|a| is_work_category(*a) && c.get(*a) > 0)
        });
    let enough_input = m
        .active_subslots
        .is_some_and(|n| n >= rules.min_active_subslots);
    MinuteFacts {
        tier: m.telemetry_tier,
        flagged,
        verified: !flagged && enough_input && category_ok,
        tier_limited: category_unknown,
    }
}

/// One closed or still-open turn interval, `[start, end)`.
struct Turn<'a> {
    project: &'a str,
    start: u64,
    end: u64,
}

fn sorted_events(events: &[TractionEvent]) -> Vec<&TractionEvent> {
    let mut sorted: Vec<&TractionEvent> =
        events.iter().filter(|e| e.project_id.is_some()).collect();
    sorted.sort_by(|a, b| (a.timestamp_ms, &a.event_id).cmp(&(b.timestamp_ms, &b.event_id)));
    sorted
}

fn sessions<'a>(sorted: &[&'a TractionEvent]) -> BTreeMap<&'a str, Vec<&'a TractionEvent>> {
    let mut by_session: BTreeMap<&str, Vec<&TractionEvent>> = BTreeMap::new();
    for e in sorted {
        if let Some(s) = e.session_id.as_deref() {
            by_session.entry(s).or_default().push(e);
        }
    }
    by_session
}

fn turns<'a>(
    rules: &Ruleset,
    by_session: &BTreeMap<&str, Vec<&'a TractionEvent>>,
    now_ms: u64,
) -> Vec<Turn<'a>> {
    let stale = rules.stale_turn_timeout_ms;
    let mut out = Vec::new();
    for events in by_session.values() {
        let mut open: Option<(&str, u64)> = None;
        let mut last = 0u64;
        for e in events {
            if let Some((p, s)) = open
                && e.timestamp_ms.saturating_sub(last) > stale
            {
                out.push(Turn {
                    project: p,
                    start: s,
                    end: last,
                });
                open = None;
            }
            match e.event_type {
                EventType::PromptSubmit if open.is_none() => {
                    open = e.project_id.as_deref().map(|p| (p, e.timestamp_ms));
                }
                EventType::TurnEnd | EventType::StopFailure | EventType::SessionEnd => {
                    if let Some((p, s)) = open.take() {
                        out.push(Turn {
                            project: p,
                            start: s,
                            end: e.timestamp_ms,
                        });
                    }
                }
                _ => {}
            }
            last = e.timestamp_ms;
        }
        if let Some((p, s)) = open {
            let end = if now_ms.saturating_sub(last) > stale {
                last
            } else {
                now_ms.max(last)
            };
            out.push(Turn {
                project: p,
                start: s,
                end,
            });
        }
    }
    out
}

#[derive(Default)]
struct Acc {
    agent_units: u64,
    idle_units: u64,
    human_sec: u32,
    permission_wait_ms: u64,
    commits_human: u32,
    commits_agent: u32,
    fast_approvals: u32,
    no_input_approval: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Input {
    Real,
    Absent,
    Unknown,
}

fn input_state(m: &PresenceMinute, f: MinuteFacts) -> Input {
    if f.flagged {
        return Input::Absent;
    }
    match (m.key_events, m.pointer_events) {
        (Some(k), Some(p)) => {
            if k + p > 0 {
                Input::Real
            } else {
                Input::Absent
            }
        }
        _ => match m.active_subslots {
            Some(0) => Input::Absent,
            Some(_) => Input::Real,
            None => Input::Unknown,
        },
    }
}

/// The 60 s Work Buckets for the minutes in `[from_ms, to_ms)`.
pub fn work_buckets(rules: &Ruleset, input: &RollupInput<'_>) -> Vec<WorkBucket> {
    let (from, to) = (input.from_ms, input.to_ms);
    let sorted = sorted_events(input.events);
    let by_session = sessions(&sorted);
    let turn_list = turns(rules, &by_session, to);

    let mut presence: BTreeMap<u64, &PresenceMinute> = BTreeMap::new();
    for m in input.presence {
        presence.entry(m.minute_start_ms).or_insert(m);
    }
    let minute_facts: BTreeMap<u64, MinuteFacts> = presence
        .iter()
        .map(|(k, m)| (*k, facts(rules, m)))
        .collect();
    let facts_at = |minute: u64| minute_facts.get(&minute).copied().unwrap_or(MISSING);

    let mut accs: BTreeMap<(u64, String), Acc> = BTreeMap::new();

    // Fresh ranges: verified minutes and unflagged prompt submissions keep
    // the following `unverified_after_sec` seconds verified.
    let grace_ms = u64::from(rules.unverified_after_sec) * 1000;
    let mut fresh: Vec<(u64, u64)> = Vec::new();
    for (minute, f) in &minute_facts {
        if f.verified {
            fresh.push((*minute, minute + MINUTE_MS + grace_ms));
        }
    }
    for e in &sorted {
        if e.event_type == EventType::PromptSubmit && !facts_at(minute_of(e.timestamp_ms)).flagged {
            fresh.push((e.timestamp_ms, e.timestamp_ms + grace_ms));
        }
    }
    fresh.sort_unstable();
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for (s, e) in fresh {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    let is_fresh = |ms: u64| {
        let i = merged.partition_point(|(s, _)| *s <= ms);
        i > 0 && ms < merged[i - 1].1
    };

    // Agent seconds: per second, the Projects with an open turn.
    let mut active: BTreeMap<u64, BTreeSet<&str>> = BTreeMap::new();
    for t in &turn_list {
        let start = t.start.max(from);
        let end = t.end.min(to);
        if start >= end {
            continue;
        }
        for sec in start.div_ceil(1000)..end.div_ceil(1000) {
            active.entry(sec).or_default().insert(t.project);
        }
    }
    for (sec, projects) in &active {
        let minute = minute_of(sec * 1000);
        let share = SCALE / projects.len() as u64;
        let idle = !is_fresh(sec * 1000);
        for p in projects {
            let a = accs.entry((minute, (*p).to_owned())).or_default();
            a.agent_units += share;
            if idle {
                a.idle_units += share;
            }
        }
    }

    // Presence goes to exactly one Project.
    let mut anchors: Vec<(u64, &str)> = sorted
        .iter()
        .filter_map(|e| e.project_id.as_deref().map(|p| (e.timestamp_ms, p)))
        .collect();
    anchors.extend(
        input
            .commits
            .iter()
            .map(|c| (c.observed_ms, c.project_id.as_str())),
    );
    let assigned = |minute: u64| -> Option<&str> {
        let end = minute + MINUTE_MS;
        let low = end.saturating_sub(rules.presence_project_lookback_ms);
        anchors
            .iter()
            .filter(|(ts, _)| *ts >= low && *ts < end)
            .max_by(|a, b| a.0.cmp(&b.0).then_with(|| b.1.cmp(a.1)))
            .map(|(_, p)| *p)
    };
    let mut permission_anchors: Vec<(u64, String)> = Vec::new();
    let mut human_used: BTreeMap<u64, u32> = BTreeMap::new();
    let mut flagged_keys: BTreeSet<(u64, String)> = BTreeSet::new();
    for minute in (from..to).step_by(MINUTE_MS as usize) {
        let Some(f) = minute_facts.get(&minute) else {
            continue;
        };
        let Some(project) = assigned(minute) else {
            continue;
        };
        if f.flagged {
            flagged_keys.insert((minute, project.to_owned()));
        }
        if f.verified {
            accs.entry((minute, project.to_owned()))
                .or_default()
                .human_sec = 60;
            human_used.insert(minute, 60);
        }
    }

    // Permission waits.
    for events in by_session.values() {
        for (i, e) in events.iter().enumerate() {
            if e.event_type != EventType::PermissionRequest
                || e.timestamp_ms < from
                || e.timestamp_ms >= to
            {
                continue;
            }
            let Some(project) = e.project_id.as_deref() else {
                continue;
            };
            let Some(end) = events[i + 1..]
                .iter()
                .find(|n| {
                    matches!(
                        n.event_type,
                        EventType::ToolPost
                            | EventType::ToolFail
                            | EventType::PromptSubmit
                            | EventType::TurnEnd
                            | EventType::StopFailure
                            | EventType::SessionEnd
                    )
                })
                .map(|n| n.timestamp_ms)
            else {
                continue;
            };
            let wait_ms = end
                .saturating_sub(e.timestamp_ms)
                .min(rules.permission_latency_cap_ms);
            let minute = minute_of(e.timestamp_ms);
            // Real input anywhere in the span (the span's minutes, clamped
            // to the capped wait).
            let span_end = e.timestamp_ms + wait_ms;
            let mut state = Input::Absent;
            let mut any_row = false;
            for m in (minute..=minute_of(span_end)).step_by(MINUTE_MS as usize) {
                match presence.get(&m) {
                    Some(pm) => {
                        any_row = true;
                        match input_state(pm, facts_at(m)) {
                            Input::Real => state = Input::Real,
                            Input::Unknown if state != Input::Real => state = Input::Unknown,
                            _ => {}
                        }
                    }
                    None if state != Input::Real => state = Input::Unknown,
                    None => {}
                }
            }
            if !any_row {
                state = Input::Unknown;
            }
            let a = accs.entry((minute, project.to_owned())).or_default();
            a.permission_wait_ms += wait_ms;
            let fast = wait_ms < rules.permission_latency_floor_ms;
            if fast {
                a.fast_approvals += 1;
            }
            if state == Input::Absent {
                a.no_input_approval = true;
            }
            let suspected = a.no_input_approval || a.fast_approvals >= rules.auto_approver_min_fast;
            if state == Input::Real && !suspected && !fast && !facts_at(minute).flagged {
                permission_anchors.push((e.timestamp_ms, project.to_owned()));
                let used = human_used.entry(minute).or_insert(0);
                let credit = u32::try_from(wait_ms / 1000).unwrap_or(60).min(60 - *used);
                *used += credit;
                a.human_sec += credit;
            }
        }
    }

    // Commits.
    for c in input.commits {
        if c.observed_ms < from || c.observed_ms >= to {
            continue;
        }
        let in_turn = turn_list.iter().any(|t| {
            t.project == c.project_id && t.start <= c.observed_ms && c.observed_ms <= t.end
        });
        let low = c.observed_ms.saturating_sub(rules.agent_quiet_ms);
        let recent = sorted.iter().any(|e| {
            e.harness_id.is_some()
                && e.project_id.as_deref() == Some(c.project_id.as_str())
                && e.timestamp_ms >= low
                && e.timestamp_ms <= c.observed_ms
        });
        let a = accs
            .entry((minute_of(c.observed_ms), c.project_id.clone()))
            .or_default();
        if in_turn || recent {
            a.commits_agent += 1;
        } else {
            a.commits_human += 1;
        }
    }

    let mut out = Vec::new();
    for key in flagged_keys {
        accs.entry(key).or_default();
    }
    for ((minute, project), a) in accs {
        if minute < from || minute >= to {
            continue;
        }
        let f = facts_at(minute);
        let agent = u32::try_from(a.agent_units / SCALE).unwrap_or(60);
        let idle = u32::try_from(a.idle_units / SCALE).unwrap_or(60).min(agent);
        let suspected = a.no_input_approval || a.fast_approvals >= rules.auto_approver_min_fast;
        let nothing = agent == 0
            && a.human_sec == 0
            && a.permission_wait_ms == 0
            && a.commits_human == 0
            && a.commits_agent == 0
            && !f.flagged;
        if nothing {
            continue;
        }
        out.push(WorkBucket {
            project_id: project,
            minute_start_ms: minute,
            ruleset_id: rules.ruleset_id.to_owned(),
            agent_compute_sec: agent,
            idle_autonomous_sec: idle,
            human_active_sec: if f.flagged { 0 } else { a.human_sec },
            permission_wait_ms: a.permission_wait_ms,
            commits_human: a.commits_human,
            commits_agent: a.commits_agent,
            injected_hid_detected: f.flagged,
            auto_approver_suspected: suspected,
            telemetry_tier: f.tier,
            tier_limited: f.tier_limited,
            struggle_axes: StruggleAxes::default(),
            verified_struggle: 0,
            struggle_credit_sec: 0,
        });
    }
    struggle::score(
        rules,
        &ScoreInput {
            sorted: &sorted,
            presence: &presence,
            permission_anchors: &permission_anchors,
            baseline: input.baseline,
        },
        &mut out,
    );
    out
}

fn tier_rank(t: TelemetryTier) -> u8 {
    match t {
        TelemetryTier::Full => 0,
        TelemetryTier::Reduced => 1,
        TelemetryTier::IdleOnly => 2,
        TelemetryTier::None => 3,
    }
}

/// The 120 s Activity Rollups for every whole, aligned window inside
/// `[from_ms, to_ms)` that holds at least one Presence Minute.
pub fn activity_rollups(
    rules: &Ruleset,
    presence: &[PresenceMinute],
    from_ms: u64,
    to_ms: u64,
) -> Vec<ActivityRollup> {
    let width = rules.activity_rollup_ms;
    let mut by_minute: BTreeMap<u64, &PresenceMinute> = BTreeMap::new();
    for m in presence {
        by_minute.entry(m.minute_start_ms).or_insert(m);
    }
    let mut out = Vec::new();
    let mut window = from_ms.div_ceil(width) * width;
    while window + width <= to_ms {
        let minutes: Vec<&PresenceMinute> = by_minute
            .range(window..window + width)
            .map(|(_, m)| *m)
            .collect();
        if !minutes.is_empty() {
            out.push(window_rollup(rules, window, &minutes));
        }
        window += width;
    }
    out
}

fn window_rollup(rules: &Ruleset, window: u64, minutes: &[&PresenceMinute]) -> ActivityRollup {
    let n = u64::try_from(minutes.len()).unwrap_or(1);
    let mut total = crate::CategorySec::default();
    for m in minutes {
        if let Some(c) = m.category_sec {
            for cat in AppCategory::ALL {
                total.add(cat, c.get(cat));
            }
        }
    }
    let all = u64::from(total.total());
    let share = |c: AppCategory| {
        (u64::from(total.get(c)) * 1000)
            .checked_div(all)
            .map_or(0, |v| u32::try_from(v).unwrap_or(1000))
    };
    let switches: u64 = minutes
        .iter()
        .map(|m| u64::from(m.category_switches.unwrap_or(0)))
        .sum();
    let entropies: Vec<u64> = minutes
        .iter()
        .filter_map(|m| m.cadence_entropy_milli.map(u64::from))
        .collect();
    let mean_entropy = (!entropies.is_empty()).then(|| {
        let n = entropies.len() as u64;
        u32::try_from((entropies.iter().sum::<u64>() + n / 2) / n).unwrap_or(u32::MAX)
    });
    let (mut injected, mut counted) = (0u64, 0u64);
    let mut has_counts = false;
    for m in minutes {
        if let Some(i) = m.injected_events {
            has_counts = true;
            injected += u64::from(i);
            counted += u64::from(i)
                + u64::from(m.key_events.unwrap_or(0))
                + u64::from(m.pointer_events.unwrap_or(0));
        }
    }
    let tier = minutes
        .iter()
        .map(|m| m.telemetry_tier)
        .max_by_key(|t| tier_rank(*t))
        .unwrap_or(TelemetryTier::None);
    ActivityRollup {
        window_start_ms: window,
        ruleset_id: rules.ruleset_id.to_owned(),
        minutes_observed: u32::try_from(n).unwrap_or(u32::MAX),
        category_share_milli: CategoryShareMilli {
            ide: share(AppCategory::Ide),
            terminal: share(AppCategory::Terminal),
            browser: share(AppCategory::Browser),
            ai_desktop: share(AppCategory::AiDesktop),
            other: share(AppCategory::Other),
        },
        switch_rate_milli: u32::try_from(switches * 1000 / n).unwrap_or(u32::MAX),
        mean_cadence_entropy_milli: mean_entropy,
        injected_ratio_milli: has_counts.then(|| {
            (injected * 1000)
                .checked_div(counted)
                .map_or(0, |v| u32::try_from(v).unwrap_or(1000))
        }),
        telemetry_tier: tier,
    }
}
