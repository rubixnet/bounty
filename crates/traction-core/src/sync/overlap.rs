//! Multi-device overlap merge (spec S2, Q9): one user's devices together may
//! report at most 60 s of Human Active Time in one UTC minute. The cap is
//! applied at read time over stored rows, which are never changed.
//!
//! Pure and deterministic: the result depends only on the multiset of inputs,
//! not on their order, because shares are sorted by `(device_id, project_id)`
//! before any rounding decision.
//!
//! - `human_active_sec` shrinks by the same proportion for every share
//!   (`60 / total`) using integer math and the largest-remainder rule, so the
//!   merged total is exactly 60 when the cap applies. Ties between equal
//!   remainders go to the share that sorts first.
//! - `struggle_credit_sec` is scaled by the same proportion with a floor
//!   (credit is a share of Human Active Time, not a second time budget, so
//!   it gets no remainder redistribution), and never exceeds the share's
//!   merged human time.
//! - Agent Compute Time is not an input: it is never capped.

use crate::MINUTE_MS;

/// The longest total Human Active Time one user can have in one minute.
pub const MINUTE_CAP_SEC: u32 = (MINUTE_MS / 1000) as u32;

/// One stored bucket's share of a user's minute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MinuteShare {
    pub device_id: String,
    pub project_id: String,
    pub human_active_sec: u32,
    pub struggle_credit_sec: u32,
}

/// The merged minute: shares sorted by `(device_id, project_id)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergedMinute {
    pub shares: Vec<MinuteShare>,
    /// `true` when the cap changed anything (the `multi_device_overlap` flag).
    pub multi_device_overlap: bool,
}

/// Merges the shares of one user in one UTC minute.
pub fn merge_minute(shares: &[MinuteShare]) -> MergedMinute {
    let mut sorted = shares.to_vec();
    sorted.sort_by(|a, b| (&a.device_id, &a.project_id).cmp(&(&b.device_id, &b.project_id)));
    let total: u64 = sorted.iter().map(|s| u64::from(s.human_active_sec)).sum();
    let cap = u64::from(MINUTE_CAP_SEC);
    if total <= cap {
        return MergedMinute {
            shares: sorted,
            multi_device_overlap: false,
        };
    }
    // floor(h * cap / total) and its remainder, per share.
    let mut floors: Vec<u64> = Vec::with_capacity(sorted.len());
    let mut remainders: Vec<(u64, usize)> = Vec::with_capacity(sorted.len());
    for (index, share) in sorted.iter().enumerate() {
        let scaled = u64::from(share.human_active_sec) * cap;
        floors.push(scaled / total);
        remainders.push((scaled % total, index));
    }
    let leftover = usize::try_from(cap - floors.iter().sum::<u64>()).unwrap_or(0);
    // Largest remainder first; equal remainders keep sorted (index) order.
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, index) in remainders.into_iter().take(leftover) {
        floors[index] += 1;
    }
    let merged = sorted
        .into_iter()
        .zip(floors)
        .map(|(share, human)| {
            let credit = u64::from(share.struggle_credit_sec) * cap / total;
            let human = u32::try_from(human).unwrap_or(MINUTE_CAP_SEC);
            MinuteShare {
                human_active_sec: human,
                struggle_credit_sec: u32::try_from(credit).unwrap_or(u32::MAX).min(human),
                ..share
            }
        })
        .collect();
    MergedMinute {
        shares: merged,
        multi_device_overlap: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn share(device: &str, project: &str, human: u32, credit: u32) -> MinuteShare {
        MinuteShare {
            device_id: device.into(),
            project_id: project.into(),
            human_active_sec: human,
            struggle_credit_sec: credit,
        }
    }

    fn humans(m: &MergedMinute) -> Vec<u32> {
        m.shares.iter().map(|s| s.human_active_sec).collect()
    }

    #[test]
    fn a_minute_within_the_cap_is_unchanged_and_unflagged() {
        let merged = merge_minute(&[share("b", "p", 30, 10), share("a", "p", 30, 5)]);
        assert!(!merged.multi_device_overlap);
        assert_eq!(humans(&merged), [30, 30]);
        assert_eq!(merged.shares[0].device_id, "a");
    }

    #[test]
    fn two_devices_at_45_each_share_the_minute_evenly() {
        let merged = merge_minute(&[share("a", "p", 45, 20), share("b", "p", 45, 20)]);
        assert!(merged.multi_device_overlap);
        assert_eq!(humans(&merged), [30, 30]);
        // credit: floor(20 * 60 / 90) = 13 each
        assert_eq!(merged.shares[0].struggle_credit_sec, 13);
    }

    #[test]
    fn the_largest_remainder_gets_the_spare_second_and_ties_go_to_the_first() {
        // Three shares of 40 (total 120): 20 each, no remainder.
        let even = merge_minute(&[
            share("a", "p", 40, 0),
            share("b", "p", 40, 0),
            share("c", "p", 40, 0),
        ]);
        assert_eq!(humans(&even), [20, 20, 20]);
        // 1, 1, 1 and 59 (total 62): floors 0, 0, 0, 57; remainders 60, 60,
        // 60, 6; three spare seconds go to the three equal remainders.
        let tie = merge_minute(&[
            share("a", "p", 1, 0),
            share("b", "p", 1, 0),
            share("c", "p", 1, 0),
            share("d", "p", 59, 0),
        ]);
        assert_eq!(humans(&tie), [1, 1, 1, 57]);
        // 30 and 31 (total 61): floors 29 (r 31) and 30 (r 30); the spare
        // second goes to the larger remainder, "a".
        let pair = merge_minute(&[share("b", "p", 31, 0), share("a", "p", 30, 0)]);
        assert_eq!(humans(&pair), [30, 30]);
        // Equal remainders: 7 shares of 10 (total 70), floors 8 r 40 each,
        // 56 allotted, 4 spare seconds go to the first four.
        let seven: Vec<MinuteShare> = (0..7)
            .map(|i| share(&format!("d{i}"), "p", 10, 0))
            .collect();
        assert_eq!(humans(&merge_minute(&seven)), [9, 9, 9, 9, 8, 8, 8]);
    }

    #[test]
    fn credit_never_exceeds_the_merged_human_time() {
        let merged = merge_minute(&[share("a", "p", 60, 60), share("b", "p", 60, 60)]);
        assert_eq!(humans(&merged), [30, 30]);
        assert_eq!(merged.shares[0].struggle_credit_sec, 30);
    }

    fn arb_shares() -> impl Strategy<Value = Vec<MinuteShare>> {
        prop::collection::vec(("[a-c]", "[x-z]", 0u32..=60, 0u32..=60), 0..8).prop_map(|rows| {
            rows.into_iter()
                .enumerate()
                .map(|(i, (d, p, human, credit))| MinuteShare {
                    // The index keeps every (device, project) pair unique.
                    device_id: format!("{d}{i}"),
                    project_id: p,
                    human_active_sec: human,
                    struggle_credit_sec: credit.min(human),
                })
                .collect()
        })
    }

    proptest! {
        #[test]
        fn merged_totals_are_capped_deterministic_and_credit_bounded(shares in arb_shares()) {
            let merged = merge_minute(&shares);
            let total: u32 = merged.shares.iter().map(|s| s.human_active_sec).sum();
            let raw: u32 = shares.iter().map(|s| s.human_active_sec).sum();
            prop_assert!(total <= MINUTE_CAP_SEC);
            prop_assert_eq!(merged.multi_device_overlap, raw > MINUTE_CAP_SEC);
            if raw > MINUTE_CAP_SEC {
                prop_assert_eq!(total, MINUTE_CAP_SEC);
            } else {
                prop_assert_eq!(total, raw);
            }
            let credit: u32 = merged.shares.iter().map(|s| s.struggle_credit_sec).sum();
            prop_assert!(credit <= total);
            for s in &merged.shares {
                prop_assert!(s.struggle_credit_sec <= s.human_active_sec);
            }
            // Same result on a second run and for any input order.
            prop_assert_eq!(&merge_minute(&shares), &merged);
            let mut reversed = shares.clone();
            reversed.reverse();
            prop_assert_eq!(&merge_minute(&reversed), &merged);
            // No share grows.
            for s in &merged.shares {
                let original = shares
                    .iter()
                    .find(|o| o.device_id == s.device_id && o.project_id == s.project_id)
                    .unwrap();
                prop_assert!(s.human_active_sec <= original.human_active_sec);
            }
        }
    }
}
