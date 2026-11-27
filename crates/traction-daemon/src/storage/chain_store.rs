//! The Step 3a chain on disk: sealing a finalized batch inside the rollup
//! transaction, and mapping stored rows and links back to `traction-core`
//! types so the pure verifier can check them.
//!
//! The [`Sealer`] is owned by the single writer thread, so there is exactly
//! one sealer and the chain cannot fork. It never opens its own transaction:
//! the caller's transaction holds the window's rows, the Chain Links and the
//! `rollup_cursor`, and commits or rolls back all of them together.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params};
use traction_core::{
    ActivityRollup, CategoryShareMilli, ChainError, ChainLink, ChainReport, CommitObservation,
    DeviceSigningKey, FrameRecord, FrameTrigger, KeyId, KeyedChain, LinkHeader, LinkPayload,
    ProjectWindowRows, RowScope, RowSet, StruggleAxes, Supersedes, TRACTION_RULES_1, TelemetryTier,
    VerifyPolicy, VerifyingKeyHex, WorkBucket, canonical_json, fold_baseline, verify_chains,
};

use super::{RollupBatch, load_baseline, to_i64};
use crate::clock::now_ms_i64;
use crate::log;

pub fn user_error(error: impl std::error::Error + Send + Sync + 'static) -> rusqlite::Error {
    rusqlite::Error::ToSqlConversionFailure(Box::new(error))
}

/// The only holder of the Device Signing Key.
pub struct Sealer {
    key: DeviceSigningKey,
    /// This key has no links yet but another key's chain exists.
    successor: bool,
    warned_first_seal: bool,
    refused_logged: bool,
    /// Test-only: the n-th link insert of a batch fails (1-based).
    fail_after: Option<usize>,
}

impl Sealer {
    /// Registers `key` in `chain_key` (`INSERT OR IGNORE`) and notes whether
    /// this starts a successor chain.
    pub fn open(conn: &Connection, key: DeviceSigningKey) -> rusqlite::Result<Self> {
        let key_id = key.key_id().to_string();
        conn.execute(
            "INSERT OR IGNORE INTO chain_key (key_id, public_key, created_at_ms)
             VALUES (?1, ?2, ?3)",
            params![key_id, key.verifying_key().to_hex(), now_ms_i64()],
        )?;
        let own: i64 = conn.query_row(
            "SELECT count(*) FROM chain_link WHERE key_id = ?1",
            [&key_id],
            |row| row.get(0),
        )?;
        let others: i64 = conn.query_row(
            "SELECT count(*) FROM chain_link WHERE key_id != ?1",
            [&key_id],
            |row| row.get(0),
        )?;
        let successor = own == 0 && others > 0;
        if successor {
            log::warn(
                "signing.key missing; starting a new chain; the previous chain stays \
                 verifiable under its old key",
            );
        }
        Ok(Self {
            key,
            successor,
            warned_first_seal: false,
            refused_logged: false,
            fail_after: fail_after_from_env(),
        })
    }

    /// Test hook: the n-th link insert of every batch fails.
    #[cfg(test)]
    pub fn fail_after(&mut self, n: Option<usize>) {
        self.fail_after = n;
    }

    /// `batch` without its windows at or before the newest sealed window of
    /// any chain, or `None` when all of it comes after. Those windows are
    /// dropped whole (rows and links): a seal is never forced.
    pub fn after_tip(
        &mut self,
        conn: &Connection,
        batch: &RollupBatch,
    ) -> rusqlite::Result<Option<RollupBatch>> {
        let newest: Option<i64> =
            conn.query_row("SELECT max(window_start_ms) FROM chain_link", [], |row| {
                row.get(0)
            })?;
        let Some(newest) = newest else {
            return Ok(None);
        };
        let first_open =
            u64::try_from(newest).map_err(user_error)? + TRACTION_RULES_1.activity_rollup_ms;
        if batch.from_ms >= first_open {
            return Ok(None);
        }
        if !self.refused_logged {
            self.refused_logged = true;
            log::warn("refused to seal a window that does not come after the chain tip");
        }
        let buckets: Vec<WorkBucket> = batch
            .buckets
            .iter()
            .filter(|b| b.minute_start_ms >= first_open)
            .cloned()
            .collect();
        // `batch.baseline` already folds in the dropped minutes, which the
        // stored baseline counted when they were first sealed.
        let mut baseline = load_baseline(conn)?;
        fold_baseline(&TRACTION_RULES_1, &mut baseline, &buckets);
        Ok(Some(RollupBatch {
            ruleset_id: batch.ruleset_id.clone(),
            from_ms: first_open,
            sealed_at_ms: batch.sealed_at_ms,
            through_ms: batch.through_ms,
            buckets,
            rollups: batch
                .rollups
                .iter()
                .filter(|r| r.window_start_ms >= first_open)
                .cloned()
                .collect(),
            marks: batch
                .marks
                .iter()
                .filter(|m| m.minute_start_ms >= first_open)
                .cloned()
                .collect(),
            baseline,
        }))
    }

    /// Seals every non-empty 120 s window of `batch`, in ascending order,
    /// inside the caller's transaction. Every window must come after the
    /// chain tip (see [`Sealer::after_tip`]).
    pub fn seal_batch(&mut self, conn: &Connection, batch: &RollupBatch) -> rusqlite::Result<()> {
        let width = TRACTION_RULES_1.activity_rollup_ms;
        let mut windows: BTreeMap<u64, WindowRows<'_>> = BTreeMap::new();
        for bucket in &batch.buckets {
            windows
                .entry(bucket.minute_start_ms / width * width)
                .or_default()
                .project(&bucket.project_id)
                .buckets
                .push(bucket.clone());
        }
        for rollup in &batch.rollups {
            windows
                .entry(rollup.window_start_ms / width * width)
                .or_default()
                .rollup = Some(rollup);
        }
        // Commit Observations are read in the transaction, never carried.
        let mut stmt = conn.prepare_cached(
            "SELECT project_id, commit_oid, observed_ms FROM commit_observation
             WHERE observed_ms >= ?1 AND observed_ms < ?2",
        )?;
        let commits = stmt.query_map(
            params![to_i64(batch.from_ms), to_i64(batch.through_ms)],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    u64_at(row, 2)?,
                ))
            },
        )?;
        for commit in commits {
            let (project, oid, observed) = commit?;
            windows
                .entry(observed / width * width)
                .or_default()
                .project(&project)
                .commit_oids
                .push(oid);
        }
        drop(stmt);
        // Frame Records too: a window holding only Frames is still sealed,
        // so no recorded Frame is ever left outside the chain.
        for frame in load_frames(conn, batch.from_ms, batch.through_ms)? {
            let window = frame.captured_at_ms / width * width;
            windows.entry(window).or_default().frames.push(frame);
        }
        if windows.is_empty() {
            return Ok(());
        }

        let key_id = self.key.key_id();
        let mut tip = read_tip(conn, &key_id)?;
        let supersedes = if tip.is_none() {
            read_other_tip(conn, &key_id)?
        } else {
            None
        };
        let mut inserted = 0usize;
        for (window, rows) in windows {
            let mut projects = Vec::with_capacity(rows.projects.len());
            for (project_id, project) in rows.projects {
                projects.push(
                    ProjectWindowRows::new(project_id, project.buckets, project.commit_oids)
                        .map_err(user_error)?,
                );
            }
            let payload = LinkPayload::new(
                window,
                &batch.ruleset_id,
                rows.rollup,
                &projects,
                if tip.is_none() {
                    supersedes.clone()
                } else {
                    None
                },
            )
            .map_err(user_error)?
            .with_frames(&rows.frames);
            let link = self
                .key
                .seal(tip.as_ref(), payload, batch.sealed_at_ms)
                .map_err(|e| user_error(io::Error::other(e.code())))?;
            inserted += 1;
            if self.fail_after == Some(inserted) {
                return Err(user_error(io::Error::other("injected seal failure")));
            }
            if self.successor && !self.warned_first_seal {
                self.warned_first_seal = true;
                log::warn(
                    "signing.key was missing: sealing the first link of a new chain; the \
                     previous chain stays verifiable under its old key",
                );
            }
            insert_link(conn, &link)?;
            tip = Some(link.header);
        }
        Ok(())
    }
}

/// `TRACTION_TEST_FAIL_SEAL_AFTER=<n>`, honoured in debug builds only.
fn fail_after_from_env() -> Option<usize> {
    if cfg!(debug_assertions) {
        std::env::var("TRACTION_TEST_FAIL_SEAL_AFTER")
            .ok()
            .and_then(|v| v.parse().ok())
    } else {
        None
    }
}

#[derive(Default)]
struct WindowRows<'a> {
    rollup: Option<&'a ActivityRollup>,
    /// Per Project: its Work Buckets and commit object ids in the window.
    projects: BTreeMap<String, ProjectRows>,
    frames: Vec<FrameRecord>,
}

#[derive(Default)]
struct ProjectRows {
    buckets: Vec<WorkBucket>,
    commit_oids: Vec<String>,
}

impl WindowRows<'_> {
    fn project(&mut self, id: &str) -> &mut ProjectRows {
        self.projects.entry(id.to_owned()).or_default()
    }
}

fn insert_link(conn: &Connection, link: &ChainLink) -> rusqlite::Result<()> {
    let payload = serde_json::to_value(&link.payload).map_err(user_error)?;
    let payload_json =
        String::from_utf8(canonical_json(&payload).map_err(user_error)?).map_err(user_error)?;
    let h = &link.header;
    conn.prepare_cached(
        "INSERT INTO chain_link
             (key_id, seq, window_start_ms, ruleset_id, prev_hash, payload_digest,
              entry_hash, sealed_at_ms, clock_regressed, payload_json, signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?
    .execute(params![
        h.key_id.to_string(),
        to_i64(h.seq),
        to_i64(h.window_start_ms),
        h.ruleset_id,
        h.prev_hash,
        h.payload_digest,
        link.entry_hash,
        to_i64(h.sealed_at_ms),
        h.clock_regressed,
        payload_json,
        link.signature,
    ])?;
    Ok(())
}

fn link_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ChainLink> {
    let key_id: String = row.get("key_id")?;
    let payload_json: String = row.get("payload_json")?;
    let payload: LinkPayload = serde_json::from_str(&payload_json).map_err(user_error)?;
    let to_u64 = |name: &str| -> rusqlite::Result<u64> {
        u64::try_from(row.get::<_, i64>(name)?).map_err(user_error)
    };
    Ok(ChainLink {
        header: LinkHeader {
            schema_version: 1,
            seq: to_u64("seq")?,
            prev_hash: row.get("prev_hash")?,
            payload_digest: row.get("payload_digest")?,
            key_id: serde_json::from_value(serde_json::Value::String(key_id))
                .map_err(user_error)?,
            ruleset_id: row.get("ruleset_id")?,
            window_start_ms: to_u64("window_start_ms")?,
            sealed_at_ms: to_u64("sealed_at_ms")?,
            clock_regressed: row.get("clock_regressed")?,
        },
        payload,
        entry_hash: row.get("entry_hash")?,
        signature: row.get("signature")?,
    })
}

const LINK_COLUMNS: &str = "key_id, seq, window_start_ms, ruleset_id, prev_hash, payload_digest,
    entry_hash, sealed_at_ms, clock_regressed, payload_json, signature";

/// The current key's highest link header (the Chain Tip).
fn read_tip(conn: &Connection, key_id: &KeyId) -> rusqlite::Result<Option<LinkHeader>> {
    conn.query_row(
        &format!(
            "SELECT {LINK_COLUMNS} FROM chain_link WHERE key_id = ?1
             ORDER BY seq DESC LIMIT 1"
        ),
        [key_id.to_string()],
        link_from_row,
    )
    .optional()
    .map(|link| link.map(|l| l.header))
}

/// The highest-window link of any other key: what a successor chain's
/// genesis commits to.
fn read_other_tip(conn: &Connection, key_id: &KeyId) -> rusqlite::Result<Option<Supersedes>> {
    conn.query_row(
        &format!(
            "SELECT {LINK_COLUMNS} FROM chain_link WHERE key_id != ?1
             ORDER BY window_start_ms DESC LIMIT 1"
        ),
        [key_id.to_string()],
        link_from_row,
    )
    .optional()
    .map(|link| {
        link.map(|l| Supersedes {
            key_id: l.header.key_id,
            seq: l.header.seq,
            entry_hash: l.entry_hash,
        })
    })
}

/// Every chain the database holds, one `Vec` per key in `seq` order, with the
/// matching public keys, ordered by each chain's first window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredChains {
    pub chains: Vec<KeyedChain>,
}

pub fn load_chains(conn: &Connection) -> rusqlite::Result<StoredChains> {
    let mut keys_stmt = conn.prepare("SELECT key_id, public_key FROM chain_key")?;
    let registered: Vec<(String, String)> = keys_stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let mut links_stmt = conn.prepare(&format!(
        "SELECT {LINK_COLUMNS} FROM chain_link WHERE key_id = ?1 ORDER BY seq"
    ))?;
    let mut found: Vec<KeyedChain> = Vec::new();
    for (key_id, public_key) in registered {
        let links = links_stmt
            .query_map([&key_id], link_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        if links.is_empty() {
            continue;
        }
        let key = VerifyingKeyHex::parse(&public_key)
            .map_err(|e| user_error(io::Error::other(e.code())))?;
        found.push(KeyedChain { key, links });
    }
    found.sort_by_key(|c| c.links[0].header.window_start_ms);
    Ok(StoredChains { chains: found })
}

/// The stored Work Buckets, Commit Observations and Activity Rollups whose
/// minute, observation time or window lies in `[from, to)`, as the types the
/// verifier takes (a local database is complete for every Project).
pub fn load_sealed_rows(conn: &Connection, from: u64, to: u64) -> rusqlite::Result<RowSet> {
    let bounds = params![to_i64(from), to_i64(to)];
    let mut buckets = conn.prepare(
        "SELECT project_id, minute_start_ms, ruleset_id, agent_compute_sec,
                idle_autonomous_sec, human_active_sec, permission_wait_ms, commits_human,
                commits_agent, injected_hid_detected, auto_approver_suspected,
                telemetry_tier, tier_limited, axis_steering, axis_specificity,
                axis_friction, axis_comfort_delta, verified_struggle, struggle_credit_sec
         FROM work_bucket WHERE minute_start_ms >= ?1 AND minute_start_ms < ?2
         ORDER BY minute_start_ms, project_id",
    )?;
    let work_buckets = buckets
        .query_map(bounds, |row| {
            Ok(WorkBucket {
                project_id: row.get(0)?,
                minute_start_ms: u64_at(row, 1)?,
                ruleset_id: row.get(2)?,
                agent_compute_sec: row.get(3)?,
                idle_autonomous_sec: row.get(4)?,
                human_active_sec: row.get(5)?,
                permission_wait_ms: u64_at(row, 6)?,
                commits_human: row.get(7)?,
                commits_agent: row.get(8)?,
                injected_hid_detected: row.get(9)?,
                auto_approver_suspected: row.get(10)?,
                telemetry_tier: tier_at(row, 11)?,
                tier_limited: row.get(12)?,
                struggle_axes: StruggleAxes {
                    steering: row.get(13)?,
                    specificity: row.get(14)?,
                    friction: row.get(15)?,
                    comfort_delta: row.get(16)?,
                },
                verified_struggle: row.get(17)?,
                struggle_credit_sec: row.get(18)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut commit_stmt = conn.prepare(
        "SELECT project_id, commit_oid, observed_ms FROM commit_observation
         WHERE observed_ms >= ?1 AND observed_ms < ?2 ORDER BY observed_ms, project_id",
    )?;
    let commits = commit_stmt
        .query_map(bounds, |row| {
            Ok(CommitObservation {
                project_id: row.get(0)?,
                commit_oid: row.get(1)?,
                observed_ms: u64_at(row, 2)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut rollup_stmt = conn.prepare(
        "SELECT window_start_ms, ruleset_id, minutes_observed, category_share_milli,
                switch_rate_milli, mean_cadence_entropy_milli, injected_ratio_milli,
                telemetry_tier
         FROM activity_rollup WHERE window_start_ms >= ?1 AND window_start_ms < ?2
         ORDER BY window_start_ms",
    )?;
    let rollups = rollup_stmt
        .query_map(bounds, |row| {
            let share: String = row.get(3)?;
            Ok(ActivityRollup {
                window_start_ms: u64_at(row, 0)?,
                ruleset_id: row.get(1)?,
                minutes_observed: row.get(2)?,
                category_share_milli: serde_json::from_str::<CategoryShareMilli>(&share)
                    .map_err(user_error)?,
                switch_rate_milli: row.get(4)?,
                mean_cadence_entropy_milli: row.get(5)?,
                injected_ratio_milli: row.get(6)?,
                telemetry_tier: tier_at(row, 7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let frames = load_frames(conn, from, to)?;
    Ok(RowSet {
        scope: RowScope::AllProjects,
        work_buckets,
        commits,
        rollups,
        frames: Some(frames),
    })
}

/// The Frame Records captured in `[from, to)`, as the signed type.
pub fn load_frames(conn: &Connection, from: u64, to: u64) -> rusqlite::Result<Vec<FrameRecord>> {
    let mut stmt = conn.prepare_cached(
        "SELECT frame_id, frame_hash, captured_at_ms, trigger, scheduled_at_ms, nonce_id
         FROM frame WHERE captured_at_ms >= ?1 AND captured_at_ms < ?2
         ORDER BY frame_id",
    )?;
    stmt.query_map(params![to_i64(from), to_i64(to)], |row| {
        let trigger: String = row.get(3)?;
        let scheduled: Option<i64> = row.get(4)?;
        Ok(FrameRecord {
            frame_id: row.get(0)?,
            frame_hash: row.get(1)?,
            captured_at_ms: u64_at(row, 2)?,
            trigger: FrameTrigger::parse(&trigger)
                .ok_or_else(|| user_error(io::Error::other("unknown frame trigger")))?,
            scheduled_at_ms: scheduled
                .map(u64::try_from)
                .transpose()
                .map_err(user_error)?,
            nonce_id: row.get(5)?,
        })
    })?
    .collect()
}

fn u64_at(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(row.get::<_, i64>(index)?).map_err(user_error)
}

fn tier_at(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<TelemetryTier> {
    let text: String = row.get(index)?;
    serde_json::from_value(serde_json::Value::String(text)).map_err(user_error)
}

/// Opens the database `mode=ro` with `PRAGMA query_only = ON`: safe next to a
/// running daemon (WAL readers never block its writer).
pub fn open_read_only(data_dir: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        data_dir.join(super::DB_FILE),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.pragma_update(None, "query_only", "ON")?;
    Ok(conn)
}

/// Why [`verify_database`] failed.
#[derive(Debug)]
pub enum VerifyDatabaseError {
    Database(rusqlite::Error),
    Chain(ChainError),
}

/// Verifies every chain in the database under `data_dir` against every
/// stored row (trust on first use). A read-only helper for tests and, later,
/// the `verify` CLI.
#[doc(hidden)]
pub fn verify_database(data_dir: &Path) -> Result<ChainReport, VerifyDatabaseError> {
    verify_database_with(data_dir, &VerifyPolicy::default())
        .map(|(report, _)| report)
        .map_err(|(error, _)| error)
}

/// [`verify_database`] under a caller's policy; also returns the chains read
/// so a caller can name the key behind a failure.
pub fn verify_database_with(
    data_dir: &Path,
    policy: &VerifyPolicy,
) -> Result<(ChainReport, StoredChains), (VerifyDatabaseError, Option<StoredChains>)> {
    let db_error = |e| (VerifyDatabaseError::Database(e), None);
    let conn = open_read_only(data_dir).map_err(db_error)?;
    let stored = load_chains(&conn).map_err(db_error)?;
    let rows = load_sealed_rows(&conn, 0, u64::try_from(i64::MAX).unwrap_or(u64::MAX))
        .map_err(db_error)?;
    match verify_chains(&stored.chains, &rows, policy) {
        Ok(report) => Ok((report, stored)),
        Err(error) => Err((VerifyDatabaseError::Chain(error), Some(stored))),
    }
}
