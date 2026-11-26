//! The pure verifier: per-link checks in one fixed order, then the checks
//! that span chains and rows. The first failure in chain order is returned,
//! so the result is deterministic for any tamper.

use std::collections::{BTreeMap, BTreeSet};

use crate::{ActivityRollup, CommitObservation, TRACTION_RULES_1, WorkBucket};

use super::{
    ChainError, ChainLink, GENESIS_PREV_HASH, KeyId, PayloadPart, ProjectWindowRows, RowSet,
    TAG_LINK, TAG_SIG_LINK, VerifyingKeyHex, device_rows_digest, digest_hex, hex,
};

/// What a caller pins or expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyPolicy {
    /// Pinned public keys. `None` trusts each chain's own key (trust on
    /// first use) and the report says `key_pinned: false`.
    pub trusted_keys: Option<Vec<VerifyingKeyHex>>,
    /// The `(seq, entry_hash)` the last chain's tip must have.
    pub expected_tip: Option<(u64, String)>,
    /// The window length every link must align to.
    pub window_ms: u64,
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        Self {
            trusted_keys: None,
            expected_tip: None,
            window_ms: TRACTION_RULES_1.activity_rollup_ms,
        }
    }
}

/// One verified chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainSummary {
    pub key_id: KeyId,
    pub links: usize,
    pub first_window_ms: u64,
    /// `(seq, entry_hash)` of the last link.
    pub tip: (u64, String),
}

/// A change of signing key between two chains. Never hidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyDiscontinuity {
    pub from_key_id: KeyId,
    pub to_key_id: KeyId,
    pub at_window_ms: u64,
    /// The successor chain's genesis names the previous chain's tip.
    pub linked: bool,
}

/// What a successful verification found. There is deliberately no combined
/// tip for several chains.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainReport {
    pub chains: Vec<ChainSummary>,
    pub key_discontinuities: Vec<KeyDiscontinuity>,
    pub key_pinned: bool,
    /// Links sealed after the clock went backwards.
    pub clock_regressions: usize,
    /// Rows in windows after the last tip: not yet sealed.
    pub pending_rows_after_tip: usize,
    /// Rows in windows before the first genesis: they predate the chain.
    pub legacy_rows_before_genesis: usize,
}

/// Verify one chain signed by `key`.
pub fn verify_chain(
    links: &[ChainLink],
    key: &VerifyingKeyHex,
    rows: &RowSet,
    policy: &VerifyPolicy,
) -> Result<ChainReport, ChainError> {
    run(vec![(links, key)], rows, policy)
}

/// One chain with the self-asserted public key it claims to be signed by.
/// Pairing them in one value makes a key without a chain (or the reverse)
/// unrepresentable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedChain {
    pub key: VerifyingKeyHex,
    /// The key's links in `seq` order.
    pub links: Vec<ChainLink>,
}

/// Verify every chain a device ever had.
pub fn verify_chains(
    chains: &[KeyedChain],
    rows: &RowSet,
    policy: &VerifyPolicy,
) -> Result<ChainReport, ChainError> {
    let pairs = chains
        .iter()
        .map(|c| (c.links.as_slice(), &c.key))
        .collect::<Vec<_>>();
    run(pairs, rows, policy)
}

/// Rows grouped by window (and Project), restricted to the scope.
struct RowIndex<'a> {
    buckets: BTreeMap<(u64, &'a str), Vec<&'a WorkBucket>>,
    commits: BTreeMap<(u64, &'a str), Vec<&'a CommitObservation>>,
    rollups: BTreeMap<u64, Vec<&'a ActivityRollup>>,
}

impl<'a> RowIndex<'a> {
    fn new(rows: &'a RowSet, window_ms: u64) -> Self {
        let size = window_ms.max(1);
        let mut index = RowIndex {
            buckets: BTreeMap::new(),
            commits: BTreeMap::new(),
            rollups: BTreeMap::new(),
        };
        for bucket in &rows.work_buckets {
            if rows.in_scope(&bucket.project_id) {
                let window = bucket.minute_start_ms / size * size;
                index
                    .buckets
                    .entry((window, bucket.project_id.as_str()))
                    .or_default()
                    .push(bucket);
            }
        }
        for commit in &rows.commits {
            if rows.in_scope(&commit.project_id) {
                let window = commit.observed_ms / size * size;
                index
                    .commits
                    .entry((window, commit.project_id.as_str()))
                    .or_default()
                    .push(commit);
            }
        }
        for rollup in &rows.rollups {
            index
                .rollups
                .entry(rollup.window_start_ms)
                .or_default()
                .push(rollup);
        }
        index
    }

    /// Projects with rows in `window`.
    fn projects_in(&self, window: u64) -> BTreeSet<&'a str> {
        let buckets = self
            .buckets
            .range((window, "")..)
            .take_while(|((w, _), _)| *w == window)
            .map(|((_, p), _)| *p);
        let commits = self
            .commits
            .range((window, "")..)
            .take_while(|((w, _), _)| *w == window)
            .map(|((_, p), _)| *p);
        buckets.chain(commits).collect()
    }

    /// Every window that holds a row, with its row count.
    fn window_counts(&self) -> BTreeMap<u64, usize> {
        let mut counts = BTreeMap::new();
        for ((w, _), rows) in &self.buckets {
            *counts.entry(*w).or_insert(0) += rows.len();
        }
        for ((w, _), rows) in &self.commits {
            *counts.entry(*w).or_insert(0) += rows.len();
        }
        for (w, rows) in &self.rollups {
            *counts.entry(*w).or_insert(0) += rows.len();
        }
        counts
    }
}

fn mismatch(seq: u64, part: PayloadPart) -> ChainError {
    ChainError::PayloadHashMismatch { seq, part }
}

/// Step 4: the payload and every digest the row set covers.
fn check_payload_and_rows(
    index_in_chain: usize,
    link: &ChainLink,
    index: &RowIndex<'_>,
    rows: &RowSet,
) -> Result<(), ChainError> {
    let seq = link.header.seq;
    let payload = &link.payload;
    if payload.digest() != link.header.payload_digest
        || payload.window_start_ms != link.header.window_start_ms
        || payload.ruleset_id != link.header.ruleset_id
        || payload
            .projects
            .windows(2)
            .any(|w| w[0].project_id >= w[1].project_id)
        || (index_in_chain > 0 && payload.supersedes.is_some())
    {
        return Err(mismatch(seq, PayloadPart::Payload));
    }
    let window = payload.window_start_ms;

    let device = match index.rollups.get(&window).map(Vec::as_slice) {
        None => device_rows_digest(None),
        Some([one]) => device_rows_digest(Some(one)),
        Some(_) => return Err(mismatch(seq, PayloadPart::DeviceRows)),
    };
    if device != payload.device_rows_digest {
        return Err(mismatch(seq, PayloadPart::DeviceRows));
    }

    let mut projects = index.projects_in(window);
    for entry in &payload.projects {
        if rows.in_scope(&entry.project_id) {
            projects.insert(entry.project_id.as_str());
        }
    }
    for project in projects {
        let part = || PayloadPart::Project(project.to_owned());
        let buckets = index
            .buckets
            .get(&(window, project))
            .map(|v| v.iter().map(|b| (*b).clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        let oids = index
            .commits
            .get(&(window, project))
            .map(|v| v.iter().map(|c| c.commit_oid.clone()).collect::<Vec<_>>())
            .unwrap_or_default();
        let entry = payload.projects.iter().find(|p| p.project_id == project);
        if buckets.is_empty() && oids.is_empty() {
            // An entry that no rows back (the caller is complete for it).
            return Err(mismatch(seq, part()));
        }
        let Ok(window_rows) = ProjectWindowRows::new(project.to_owned(), buckets, oids) else {
            return Err(mismatch(seq, part()));
        };
        match entry {
            Some(entry) if entry.rows_digest == window_rows.rows_digest() => {}
            _ => return Err(mismatch(seq, part())),
        }
    }
    Ok(())
}

/// Steps 1 to 8 for every link of one chain.
fn walk(
    links: &[ChainLink],
    key: &VerifyingKeyHex,
    index: &RowIndex<'_>,
    rows: &RowSet,
    policy: &VerifyPolicy,
) -> Result<(), ChainError> {
    let key_id = key.key_id();
    for (i, link) in links.iter().enumerate() {
        let header = &link.header;
        let seq = header.seq;
        if header.schema_version != 1 {
            return Err(ChainError::UnsupportedSchema { seq });
        }
        let position = i as u64;
        if i == 0 {
            if seq != 0 || header.prev_hash != GENESIS_PREV_HASH {
                return Err(ChainError::TruncatedGenesis { first_seq: seq });
            }
        } else if seq < position {
            return Err(ChainError::DuplicateSeq { seq });
        } else if seq > position {
            return Err(ChainError::SeqGap {
                expected: position,
                found: seq,
            });
        }
        let pinned = policy
            .trusted_keys
            .as_ref()
            .is_none_or(|trusted| trusted.contains(key));
        if header.key_id != key_id || !pinned {
            return Err(ChainError::UnknownKey {
                seq,
                key_id: header.key_id.clone(),
            });
        }
        check_payload_and_rows(i, link, index, rows)?;
        let entry_hash = digest_hex(TAG_LINK, header);
        if entry_hash != link.entry_hash {
            return Err(ChainError::EntryHashMismatch { seq });
        }
        let entry_bytes =
            hex::decode::<32>(&entry_hash).ok_or(ChainError::EntryHashMismatch { seq })?;
        if !key.verify_tagged(TAG_SIG_LINK, &entry_bytes, &link.signature) {
            return Err(ChainError::InvalidSignature { seq });
        }
        if i > 0 {
            let prev = &links[i - 1];
            if header.prev_hash != prev.entry_hash {
                return Err(ChainError::BrokenPrevHash { seq });
            }
            if header.window_start_ms <= prev.header.window_start_ms {
                return Err(ChainError::WindowOrder { seq });
            }
        }
        if policy.window_ms == 0 || header.window_start_ms % policy.window_ms != 0 {
            return Err(ChainError::WindowOrder { seq });
        }
    }
    Ok(())
}

fn run(
    mut chains: Vec<(&[ChainLink], &VerifyingKeyHex)>,
    rows: &RowSet,
    policy: &VerifyPolicy,
) -> Result<ChainReport, ChainError> {
    if chains.iter().any(|(links, _)| links.is_empty()) {
        return Err(ChainError::TruncatedGenesis { first_seq: 0 });
    }
    chains.sort_by_key(|(links, _)| links[0].header.window_start_ms);

    let mut seen = BTreeSet::new();
    for (_, key) in &chains {
        if !seen.insert(key.key_id()) {
            return Err(ChainError::DuplicateChain {
                key_id: key.key_id(),
            });
        }
    }

    let index = RowIndex::new(rows, policy.window_ms);
    let mut report = ChainReport {
        chains: Vec::new(),
        key_discontinuities: Vec::new(),
        key_pinned: policy.trusted_keys.is_some(),
        clock_regressions: 0,
        pending_rows_after_tip: 0,
        legacy_rows_before_genesis: 0,
    };
    let mut linked_windows = BTreeSet::new();

    for (links, key) in &chains {
        walk(links, key, &index, rows, policy)?;
        let first = &links[0];
        let last = &links[links.len() - 1];
        let summary = ChainSummary {
            key_id: key.key_id(),
            links: links.len(),
            first_window_ms: first.header.window_start_ms,
            tip: (last.header.seq, last.entry_hash.clone()),
        };
        if let Some(prev) = report.chains.last() {
            let prev_tip_window = linked_windows.last().copied().unwrap_or(0);
            if first.header.window_start_ms <= prev_tip_window {
                return Err(ChainError::ChainOverlap {
                    key_id: summary.key_id,
                });
            }
            let linked = match &first.payload.supersedes {
                Some(s)
                    if s.key_id == prev.key_id
                        && s.seq == prev.tip.0
                        && s.entry_hash == prev.tip.1 =>
                {
                    true
                }
                Some(_) => {
                    return Err(ChainError::SupersedesMismatch {
                        key_id: summary.key_id,
                    });
                }
                None => false,
            };
            report.key_discontinuities.push(KeyDiscontinuity {
                from_key_id: prev.key_id.clone(),
                to_key_id: summary.key_id.clone(),
                at_window_ms: first.header.window_start_ms,
                linked,
            });
        }
        report.clock_regressions += links.iter().filter(|l| l.header.clock_regressed).count();
        linked_windows.extend(links.iter().map(|l| l.header.window_start_ms));
        report.chains.push(summary);
    }

    // A first chain whose genesis names a predecessor is a successor chain
    // shown without it: rows before it should have been covered by that
    // predecessor, so they are not "legacy".
    let first_is_successor = chains
        .first()
        .is_some_and(|(links, _)| links[0].payload.supersedes.is_some());
    let first_window = report.chains.first().map(|c| c.first_window_ms);
    let last_window = linked_windows.last().copied();
    for (window, count) in index.window_counts() {
        match (first_window, last_window) {
            (Some(first), _) if window < first && first_is_successor => {
                return Err(ChainError::UnanchoredRow {
                    window_start_ms: window,
                });
            }
            (Some(first), _) if window < first => report.legacy_rows_before_genesis += count,
            (Some(_), Some(last)) if window > last => report.pending_rows_after_tip += count,
            (Some(_), Some(_)) if !linked_windows.contains(&window) => {
                return Err(ChainError::UnanchoredRow {
                    window_start_ms: window,
                });
            }
            (Some(_), _) => {}
            (None, _) => report.legacy_rows_before_genesis += count,
        }
    }

    if first_is_successor && let Some(first) = report.chains.first() {
        return Err(ChainError::SupersedesMismatch {
            key_id: first.key_id.clone(),
        });
    }

    if let (Some((expected_seq, expected_hash)), Some(last)) =
        (&policy.expected_tip, report.chains.last())
        && (last.tip.0 != *expected_seq || last.tip.1 != *expected_hash)
    {
        return Err(ChainError::TipMismatch {
            expected_seq: *expected_seq,
            found_seq: last.tip.0,
        });
    }
    Ok(report)
}
