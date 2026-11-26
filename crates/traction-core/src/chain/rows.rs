//! The rows a Chain Link covers: one Project's Work Buckets and Commit
//! Observations in one window, and the set a verifier is handed.

use serde::{Deserialize, Serialize};

use super::{TAG_ROWS, digest_hex};
use crate::{ActivityRollup, CommitObservation, WorkBucket};

/// A Project's rows could not be assembled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowsError {
    /// A commit object id is not 40 or 64 lowercase hex characters.
    InvalidCommitOid,
}

impl std::fmt::Display for RowsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid_commit_oid")
    }
}

impl std::error::Error for RowsError {}

fn valid_oid(oid: &str) -> bool {
    matches!(oid.len(), 40 | 64) && oid.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// One Project's rows in one window: Work Buckets sorted by minute, commit
/// object ids sorted and de-duplicated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProjectWindowRows {
    pub project_id: String,
    pub work_buckets: Vec<WorkBucket>,
    pub commit_oids: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProjectWindowRows {
    project_id: String,
    work_buckets: Vec<WorkBucket>,
    commit_oids: Vec<String>,
}

impl<'de> Deserialize<'de> for ProjectWindowRows {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawProjectWindowRows::deserialize(deserializer)?;
        Self::new(raw.project_id, raw.work_buckets, raw.commit_oids)
            .map_err(serde::de::Error::custom)
    }
}

impl ProjectWindowRows {
    /// Normalize (sort, de-duplicate) and validate.
    pub fn new(
        project_id: String,
        mut work_buckets: Vec<WorkBucket>,
        mut commit_oids: Vec<String>,
    ) -> Result<Self, RowsError> {
        if !commit_oids.iter().all(|oid| valid_oid(oid)) {
            return Err(RowsError::InvalidCommitOid);
        }
        work_buckets.sort_by_key(|b| b.minute_start_ms);
        commit_oids.sort();
        commit_oids.dedup();
        Ok(Self {
            project_id,
            work_buckets,
            commit_oids,
        })
    }

    /// `SHA-256(TAG_ROWS || canonical_json(self))`, in hex.
    pub fn rows_digest(&self) -> String {
        digest_hex(TAG_ROWS, self)
    }
}

/// Which Projects a [`RowSet`] is complete for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub enum RowScope {
    /// Every Project (a local database).
    AllProjects,
    /// One Project only (a bundle for one Project).
    Project(String),
}

/// The rows a caller holds, to be checked against the chain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RowSet {
    pub scope: RowScope,
    pub work_buckets: Vec<WorkBucket>,
    pub commits: Vec<CommitObservation>,
    pub rollups: Vec<ActivityRollup>,
}

impl RowSet {
    /// No rows, complete for every Project.
    pub fn empty() -> Self {
        Self {
            scope: RowScope::AllProjects,
            work_buckets: Vec::new(),
            commits: Vec::new(),
            rollups: Vec::new(),
        }
    }

    pub(crate) fn in_scope(&self, project_id: &str) -> bool {
        match &self.scope {
            RowScope::AllProjects => true,
            RowScope::Project(id) => id == project_id,
        }
    }
}
