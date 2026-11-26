//! Pure, OS-independent aggregation of one Presence Minute.
//!
//! Input timestamps become sub-slot activity, counts and a log-binned
//! inter-event histogram; foreground samples become category seconds and
//! switches. Only the histogram (16 counters) is kept for cadence: individual
//! intervals are never stored, so nothing about them survives the minute.

use traction_core::{
    AppCategory, CategorySec, PresenceMinute, SUBSLOTS_PER_MINUTE, TRACTION_RULES_1, TelemetryTier,
};

const SLOTS: usize = SUBSLOTS_PER_MINUTE as usize;
const BINS: usize = 16;

/// One input event, content-free: only its kind and whether the OS flagged
/// it as synthetic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(target_os = "linux"),
    allow(dead_code, reason = "only the X11 backend pushes input")
)]
pub enum InputKind {
    Key,
    Pointer,
}

/// What a backend reads once per sub-slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Sample {
    /// Milliseconds since the last user input, per the OS idle timer.
    pub idle_ms: Option<u64>,
    /// The foreground application's category. `None` when the platform
    /// cannot tell (Wayland, a backend error).
    pub category: Option<AppCategory>,
}

pub struct Aggregator {
    minute_start_ms: u64,
    slot_ms: u64,
    tier: TelemetryTier,
    keys: u32,
    pointers: u32,
    injected: u32,
    slot_input: [bool; SLOTS],
    slot_idle_active: [bool; SLOTS],
    last_real_ms: Option<u64>,
    bins: [u32; BINS],
    intervals: u32,
    category_sec: CategorySec,
    category_seen: bool,
    switches: u32,
    previous_category: Option<AppCategory>,
    idle_sec: u32,
    idle_seen: bool,
}

impl Aggregator {
    /// `previous_category` is the last foreground category of the minute
    /// before, so a switch across the minute boundary is counted.
    pub fn new(
        minute_start_ms: u64,
        slot_ms: u64,
        tier: TelemetryTier,
        previous_category: Option<AppCategory>,
    ) -> Self {
        Self {
            minute_start_ms,
            slot_ms: slot_ms.max(1),
            tier,
            keys: 0,
            pointers: 0,
            injected: 0,
            slot_input: [false; SLOTS],
            slot_idle_active: [false; SLOTS],
            last_real_ms: None,
            bins: [0; BINS],
            intervals: 0,
            category_sec: CategorySec::default(),
            category_seen: false,
            switches: 0,
            previous_category,
            idle_sec: 0,
            idle_seen: false,
        }
    }

    fn slot_of(&self, t_ms: u64) -> usize {
        let slot = t_ms.saturating_sub(self.minute_start_ms) / self.slot_ms;
        usize::try_from(slot).unwrap_or(SLOTS - 1).min(SLOTS - 1)
    }

    /// Records one input event at `t_ms`. Ignored unless the tier is `Full`.
    pub fn input(&mut self, t_ms: u64, kind: InputKind, injected: bool) {
        if self.tier != TelemetryTier::Full {
            return;
        }
        if injected {
            self.injected = self.injected.saturating_add(1);
            return;
        }
        match kind {
            InputKind::Key => self.keys = self.keys.saturating_add(1),
            InputKind::Pointer => self.pointers = self.pointers.saturating_add(1),
        }
        let slot = self.slot_of(t_ms);
        self.slot_input[slot] = true;
        if let Some(last) = self.last_real_ms {
            let interval = t_ms.saturating_sub(last).max(1);
            let bin = usize::try_from(interval.ilog2()).unwrap_or(BINS - 1);
            self.bins[bin.min(BINS - 1)] += 1;
            self.intervals += 1;
        }
        self.last_real_ms = Some(t_ms);
    }

    /// Closes sub-slot `slot` with its sample.
    pub fn slot_end(&mut self, slot: usize, sample: Sample) {
        let slot = slot.min(SLOTS - 1);
        let secs = u32::try_from(self.slot_ms / 1000).unwrap_or(u32::MAX);
        if let Some(category) = sample.category {
            self.category_seen = true;
            self.category_sec.add(category, secs);
            if self.previous_category.is_some_and(|prev| prev != category) {
                self.switches += 1;
            }
            self.previous_category = Some(category);
        }
        if let Some(idle_ms) = sample.idle_ms {
            self.idle_seen = true;
            if idle_ms >= u64::from(TRACTION_RULES_1.idle_threshold_sec) * 1000 {
                self.idle_sec = self.idle_sec.saturating_add(secs);
            }
            // The timer reset inside this sub-slot, so some input happened.
            self.slot_idle_active[slot] = idle_ms < self.slot_ms;
        }
    }

    /// The minute's record and the last foreground category seen.
    pub fn finish(self) -> (PresenceMinute, Option<AppCategory>) {
        let full = self.tier == TelemetryTier::Full;
        let none = self.tier == TelemetryTier::None;
        let active = if full {
            self.slot_input.iter().filter(|a| **a).count()
        } else {
            self.slot_idle_active.iter().filter(|a| **a).count()
        };
        let minute = PresenceMinute {
            minute_start_ms: self.minute_start_ms,
            key_events: full.then_some(self.keys),
            pointer_events: full.then_some(self.pointers),
            injected_events: full.then_some(self.injected),
            active_subslots: (!none && (full || self.idle_seen))
                .then(|| u8::try_from(active).unwrap_or(SUBSLOTS_PER_MINUTE)),
            cadence_entropy_milli: if full {
                entropy_milli(&self.bins, self.intervals)
            } else {
                None
            },
            category_sec: self.category_seen.then_some(self.category_sec),
            category_switches: self.category_seen.then_some(self.switches),
            idle_sec: (!none && self.idle_seen).then_some(self.idle_sec),
            telemetry_tier: self.tier,
        };
        (minute, self.previous_category)
    }
}

/// Shannon entropy in bits, times 1000, of the interval histogram; `None`
/// below `min_cadence_samples`.
fn entropy_milli(bins: &[u32; BINS], total: u32) -> Option<u32> {
    if total < TRACTION_RULES_1.min_cadence_samples {
        return None;
    }
    let total = f64::from(total);
    let bits: f64 = bins
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = f64::from(*count) / total;
            -p * p.log2()
        })
        .sum();
    // Bounded by log2(16) = 4 bits, so the cast cannot overflow.
    Some((bits * 1000.0).round() as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT: u64 = 10_000;
    const START: u64 = 600_000;

    fn full() -> Aggregator {
        Aggregator::new(START, SLOT, TelemetryTier::Full, None)
    }

    fn close_all(agg: &mut Aggregator, category: Option<AppCategory>, idle_ms: u64) {
        for slot in 0..SLOTS {
            agg.slot_end(
                slot,
                Sample {
                    idle_ms: Some(idle_ms),
                    category,
                },
            );
        }
    }

    #[test]
    fn counts_split_by_kind_and_injected_events_are_not_real_input() {
        let mut agg = full();
        agg.input(START + 100, InputKind::Key, false);
        agg.input(START + 200, InputKind::Pointer, false);
        agg.input(START + 300, InputKind::Pointer, false);
        agg.input(START + 400, InputKind::Key, true);
        let (m, _) = agg.finish();
        assert_eq!(m.key_events, Some(1));
        assert_eq!(m.pointer_events, Some(2));
        assert_eq!(m.injected_events, Some(1));
        assert_eq!(m.active_subslots, Some(1));
    }

    #[test]
    fn only_injected_input_leaves_every_subslot_inactive() {
        let mut agg = full();
        for i in 0..50 {
            agg.input(START + i * 1000, InputKind::Pointer, true);
        }
        let (m, _) = agg.finish();
        assert_eq!(m.active_subslots, Some(0));
        assert_eq!(m.injected_events, Some(50));
        assert_eq!(m.cadence_entropy_milli, None);
    }

    #[test]
    fn a_tap_every_59_seconds_fills_one_subslot() {
        let mut agg = full();
        agg.input(START + 59_000, InputKind::Key, false);
        let (m, _) = agg.finish();
        assert_eq!(m.active_subslots, Some(1));
    }

    #[test]
    fn input_in_each_subslot_counts_six_and_a_late_stamp_clamps_to_the_last() {
        let mut agg = full();
        for slot in 0..6 {
            agg.input(START + slot * SLOT + 5, InputKind::Key, false);
        }
        agg.input(START + 999_999, InputKind::Key, false);
        let (m, _) = agg.finish();
        assert_eq!(m.active_subslots, Some(6));
    }

    #[test]
    fn cadence_is_null_below_the_sample_floor_and_zero_for_a_metronome() {
        let mut agg = full();
        for i in 0..TRACTION_RULES_1.min_cadence_samples {
            agg.input(START + u64::from(i) * 100, InputKind::Key, false);
        }
        // 8 events make 7 intervals: still below the floor.
        assert_eq!(agg.finish().0.cadence_entropy_milli, None);

        let mut agg = full();
        for i in 0..=TRACTION_RULES_1.min_cadence_samples {
            agg.input(START + u64::from(i) * 100, InputKind::Key, false);
        }
        assert_eq!(agg.finish().0.cadence_entropy_milli, Some(0));
    }

    #[test]
    fn cadence_over_four_equally_used_bins_is_two_bits() {
        let mut agg = full();
        // Intervals of 10, 20, 40 and 80 ms (bins 3, 4, 5, 6), two each.
        let mut t = START;
        agg.input(t, InputKind::Key, false);
        for interval in [10, 20, 40, 80, 10, 20, 40, 80] {
            t += interval;
            agg.input(t, InputKind::Key, false);
        }
        assert_eq!(agg.finish().0.cadence_entropy_milli, Some(2000));
    }

    #[test]
    fn category_seconds_and_switches_follow_the_samples() {
        let mut agg = Aggregator::new(START, SLOT, TelemetryTier::Reduced, None);
        let cats = [
            AppCategory::Ide,
            AppCategory::Ide,
            AppCategory::Terminal,
            AppCategory::Browser,
            AppCategory::Browser,
            AppCategory::Other,
        ];
        for (slot, category) in cats.into_iter().enumerate() {
            agg.slot_end(
                slot,
                Sample {
                    idle_ms: Some(0),
                    category: Some(category),
                },
            );
        }
        let (m, last) = agg.finish();
        let sec = m.category_sec.unwrap();
        assert_eq!(
            (sec.ide, sec.terminal, sec.browser, sec.other),
            (20, 10, 20, 10)
        );
        assert_eq!(m.category_switches, Some(3));
        assert_eq!(last, Some(AppCategory::Other));
    }

    #[test]
    fn a_switch_across_the_minute_boundary_is_counted() {
        let mut agg = Aggregator::new(START, SLOT, TelemetryTier::Reduced, Some(AppCategory::Ide));
        close_all(&mut agg, Some(AppCategory::Terminal), 0);
        assert_eq!(agg.finish().0.category_switches, Some(1));
    }

    #[test]
    fn reduced_tier_fills_idle_and_category_but_nulls_input_fields() {
        let mut agg = Aggregator::new(START, SLOT, TelemetryTier::Reduced, None);
        // A backend would never send input at this tier; it must be ignored.
        agg.input(START, InputKind::Key, false);
        close_all(&mut agg, Some(AppCategory::Ide), 45_000);
        let (m, _) = agg.finish();
        assert_eq!(m.telemetry_tier, TelemetryTier::Reduced);
        assert_eq!(m.key_events, None);
        assert_eq!(m.pointer_events, None);
        assert_eq!(m.injected_events, None);
        assert_eq!(m.cadence_entropy_milli, None);
        assert_eq!(m.idle_sec, Some(60));
        assert_eq!(m.active_subslots, Some(0));
        assert!(m.category_sec.is_some());
    }

    #[test]
    fn reduced_tier_derives_active_subslots_from_the_idle_timer() {
        let mut agg = Aggregator::new(START, SLOT, TelemetryTier::Reduced, None);
        for slot in 0..SLOTS {
            let idle_ms = if slot % 2 == 0 { 1_500 } else { 25_000 };
            agg.slot_end(
                slot,
                Sample {
                    idle_ms: Some(idle_ms),
                    category: None,
                },
            );
        }
        let (m, _) = agg.finish();
        assert_eq!(m.active_subslots, Some(3));
        assert_eq!(m.idle_sec, Some(0));
        assert_eq!(m.category_sec, None);
        assert_eq!(m.category_switches, None);
    }

    #[test]
    fn idle_only_has_no_category_and_none_has_nothing() {
        let mut agg = Aggregator::new(START, SLOT, TelemetryTier::IdleOnly, None);
        close_all(&mut agg, None, 90_000);
        let (m, _) = agg.finish();
        assert_eq!(m.idle_sec, Some(60));
        assert_eq!(m.category_sec, None);

        let mut agg = Aggregator::new(START, SLOT, TelemetryTier::None, None);
        close_all(&mut agg, None, 90_000);
        let (m, _) = agg.finish();
        assert_eq!(m.idle_sec, None);
        assert_eq!(m.active_subslots, None);
        assert_eq!(m.key_events, None);
    }

    #[test]
    fn a_flood_of_events_in_one_millisecond_cannot_overflow_or_panic() {
        let mut agg = full();
        for _ in 0..10_000 {
            agg.input(START, InputKind::Pointer, false);
        }
        let (m, _) = agg.finish();
        assert_eq!(m.pointer_events, Some(10_000));
        assert_eq!(m.active_subslots, Some(1));
        assert_eq!(m.cadence_entropy_milli, Some(0));
    }
}
