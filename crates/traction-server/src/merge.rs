//! Read-time multi-device overlap merge (spec S2, Q9). Stored buckets are
//! never changed: every read that reports Human Active Time or Struggle
//! Credit (live totals, the Project list, the timeline) goes through the
//! pure `traction_core::sync::merge_minute`, which caps one user's total at
//! 60 s per UTC minute. Agent Compute Time is never capped.

use std::collections::{BTreeMap, HashMap};

use sqlx::PgPool;
use traction_core::sync::{MinuteShare, merge_minute};

use crate::app::AppState;
use crate::fanout::Totals;

/// One stored bucket's share of a minute: `(device, project, minute, human, credit)`.
pub type ShareRow = (String, String, i64, i64, i64);

/// A bucket's merged figures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Merged {
    pub human_active_sec: u32,
    pub struggle_credit_sec: u32,
    pub multi_device_overlap: bool,
}

/// Merges `rows` minute by minute, keyed by `(device, project, minute)`.
pub fn merge_rows(rows: &[ShareRow]) -> HashMap<(String, String, i64), Merged> {
    let mut minutes: BTreeMap<i64, Vec<MinuteShare>> = BTreeMap::new();
    let clamp = |v: i64| u32::try_from(v.clamp(0, 60)).unwrap_or(0);
    for (device, project, minute, human, credit) in rows {
        minutes.entry(*minute).or_default().push(MinuteShare {
            device_id: device.clone(),
            project_id: project.clone(),
            human_active_sec: clamp(*human),
            struggle_credit_sec: clamp(*credit),
        });
    }
    let mut out = HashMap::new();
    for (minute, shares) in minutes {
        let merged = merge_minute(&shares);
        for share in merged.shares {
            out.insert(
                (share.device_id, share.project_id, minute),
                Merged {
                    human_active_sec: share.human_active_sec,
                    struggle_credit_sec: share.struggle_credit_sec,
                    multi_device_overlap: merged.multi_device_overlap,
                },
            );
        }
    }
    out
}

fn to_u64(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// The user's merged totals per Project, keyed by the Project each stored
/// Project resolves to (alias-only Project merge: buckets are never
/// rewritten). The SQL sums are raw; only minutes whose raw total exceeds the
/// cap (the only ones the merge can change) are loaded and replaced by their
/// merged figures.
pub async fn project_totals(
    pool: &PgPool,
    user_id: &str,
) -> Result<BTreeMap<String, Totals>, sqlx::Error> {
    let raw: Vec<(String, i64, i64, i64)> = sqlx::query_as(
        "SELECT project_id, SUM(human_active_sec)::bigint, SUM(struggle_credit_sec)::bigint,
                SUM(agent_compute_sec)::bigint
         FROM work_buckets WHERE user_id = $1 GROUP BY project_id",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let mut totals: BTreeMap<String, Totals> = raw
        .into_iter()
        .map(|(project, human, credit, agent)| {
            (
                project,
                Totals {
                    human_active_sec: to_u64(human),
                    struggle_credit_sec: to_u64(credit),
                    agent_compute_sec: to_u64(agent),
                },
            )
        })
        .collect();
    let overlapping: Vec<ShareRow> = sqlx::query_as(
        "SELECT device_id, project_id, minute_start_ms, human_active_sec::bigint,
                struggle_credit_sec::bigint
         FROM work_buckets
         WHERE user_id = $1 AND minute_start_ms IN (
             SELECT minute_start_ms FROM work_buckets WHERE user_id = $1
             GROUP BY minute_start_ms HAVING SUM(human_active_sec) > 60)",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?;
    let merged = merge_rows(&overlapping);
    for (device, project, minute, human, credit) in &overlapping {
        let after = merged[&(device.clone(), project.clone(), *minute)];
        if let Some(t) = totals.get_mut(project) {
            t.human_active_sec = t.human_active_sec.saturating_sub(to_u64(*human))
                + u64::from(after.human_active_sec);
            t.struggle_credit_sec = t.struggle_credit_sec.saturating_sub(to_u64(*credit))
                + u64::from(after.struggle_credit_sec);
        }
    }
    let aliases = crate::aliases::load(pool, user_id).await?;
    let mut resolved: BTreeMap<String, Totals> = BTreeMap::new();
    for (project, t) in totals {
        let slot = resolved.entry(aliases.resolve(&project)).or_default();
        slot.human_active_sec += t.human_active_sec;
        slot.struggle_credit_sec += t.struggle_credit_sec;
        slot.agent_compute_sec += t.agent_compute_sec;
    }
    Ok(resolved)
}

/// Merged Logged Hours totals of a user: what live totals report.
pub async fn user_totals(state: &AppState, user_id: &str) -> Result<Totals, sqlx::Error> {
    let by_project = project_totals(&state.pool, user_id).await?;
    Ok(by_project
        .values()
        .fold(Totals::default(), |sum, t| Totals {
            human_active_sec: sum.human_active_sec + t.human_active_sec,
            struggle_credit_sec: sum.struggle_credit_sec + t.struggle_credit_sec,
            agent_compute_sec: sum.agent_compute_sec + t.agent_compute_sec,
        }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(device: &str, project: &str, minute: i64, human: i64, credit: i64) -> ShareRow {
        (device.into(), project.into(), minute, human, credit)
    }

    #[test]
    fn minutes_are_merged_independently_and_flagged_only_when_capped() {
        let rows = [
            row("a", "p", 0, 45, 20),
            row("b", "p", 0, 45, 20),
            row("a", "p", 60_000, 30, 10),
            row("b", "p", 60_000, 20, 10),
        ];
        let merged = merge_rows(&rows);
        let a0 = merged[&("a".into(), "p".into(), 0)];
        let b0 = merged[&("b".into(), "p".into(), 0)];
        assert_eq!((a0.human_active_sec, b0.human_active_sec), (30, 30));
        assert!(a0.multi_device_overlap);
        let a1 = merged[&("a".into(), "p".into(), 60_000)];
        assert_eq!(a1.human_active_sec, 30);
        assert!(!a1.multi_device_overlap);
    }
}
