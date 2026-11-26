//! The frame-less Audit Bundle (`docs/protocol.md` section 5): one Project's
//! rows, every chain the device holds and a manifest signed with the current
//! Device Signing Key. Pure: the daemon loads rows and links from SQLite and
//! calls [`build_bundle`]; the `verify` CLI and the Step 4 server call
//! [`verify_bundle`].

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    ChainError, ChainLink, DeviceSigningKey, KeyId, KeyedChain, RowScope, RowSet, TAG_MANIFEST,
    VerifyPolicy, VerifyingKeyHex, canon, digest_hex, sign_manifest, verify_chains,
    verify_manifest,
};
use crate::{ActivityRollup, CommitObservation, TRACTION_RULES_1, WorkBucket};

/// The only `bundle_schema` this build writes and reads.
pub const BUNDLE_SCHEMA: u32 = 1;

/// One key's chain: its self-asserted public key and every link, genesis to
/// tip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleChain {
    pub public_key: String,
    pub links: Vec<ChainLink>,
}

/// The chain tip a manifest commits to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestTip {
    pub key_id: KeyId,
    pub seq: u64,
    pub entry_hash: String,
}

/// The signed part of the manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestBody {
    pub bundle_schema: u32,
    pub project_id: String,
    /// Step 5 fills this.
    pub linked_repo: Option<String>,
    /// The current (signing) key.
    pub key_id: KeyId,
    pub public_key: String,
    pub chain_count: usize,
    pub key_ids: Vec<KeyId>,
    pub first_window_ms: u64,
    pub tip: ManifestTip,
    pub link_count: usize,
    pub bucket_count: usize,
    pub ruleset_ids: Vec<String>,
    pub exported_at_ms: u64,
    /// `SHA-256(TAG_MANIFEST || canonical_json(chains))`, in hex.
    pub chains_digest: String,
}

/// A manifest body plus its signature (`"signature"` sits beside the body
/// fields in the JSON).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub body: ManifestBody,
    pub signature: String,
}

impl Manifest {
    fn body_value(&self) -> Value {
        serde_json::to_value(&self.body).expect("manifest body serializes")
    }
}

impl Serialize for Manifest {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut value = self.body_value();
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "signature".to_owned(),
                Value::String(self.signature.clone()),
            );
        }
        value.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Manifest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut value = Value::deserialize(deserializer)?;
        let signature = value
            .as_object_mut()
            .and_then(|map| map.remove("signature"))
            .and_then(|s| s.as_str().map(str::to_owned))
            .ok_or_else(|| serde::de::Error::custom("manifest has no signature"))?;
        let body = ManifestBody::deserialize(value).map_err(serde::de::Error::custom)?;
        Ok(Self { body, signature })
    }
}

/// One Project's rows: Work Buckets and, per window, commit object ids.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRowsSection {
    pub work_buckets: Vec<WorkBucket>,
    /// Window start (ms) -> sorted, de-duplicated commit object ids.
    pub commit_oids: BTreeMap<u64, Vec<String>>,
}

/// Device-wide Activity Rollups (counts only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceRowsSection {
    pub activity_rollups: Vec<ActivityRollup>,
}

/// The frame-less Audit Bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditBundle {
    pub manifest: Manifest,
    pub chains: Vec<BundleChain>,
    pub project_rows: ProjectRowsSection,
    pub device_rows: DeviceRowsSection,
    /// Reserved for Step 3c; empty until then.
    pub frames: Vec<Value>,
}

impl AuditBundle {
    /// The bundle as canonical JSON bytes (sorted keys, no whitespace).
    pub fn to_canonical_bytes(&self) -> Vec<u8> {
        canon(self)
    }
}

/// Why a bundle could not be built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleBuildError {
    /// The device holds no chain yet.
    NoChains,
    /// The signing key is not the key of the newest chain.
    KeyIsNotCurrent,
}

impl BundleBuildError {
    /// A stable reason code.
    pub fn code(self) -> &'static str {
        match self {
            BundleBuildError::NoChains => "no_chains",
            BundleBuildError::KeyIsNotCurrent => "key_is_not_current",
        }
    }
}

impl std::fmt::Display for BundleBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for BundleBuildError {}

/// What [`build_bundle`] packages. `chains` are ordered by genesis window.
pub struct BundleInput {
    pub project_id: String,
    pub chains: Vec<BundleChain>,
    /// This Project's rows only (the builder drops any other Project's).
    pub work_buckets: Vec<WorkBucket>,
    pub commits: Vec<CommitObservation>,
    pub rollups: Vec<ActivityRollup>,
    pub exported_at_ms: u64,
}

/// Build and sign a bundle. `key` must be the newest chain's key.
pub fn build_bundle(
    key: &DeviceSigningKey,
    input: BundleInput,
) -> Result<AuditBundle, BundleBuildError> {
    let last = input.chains.last().ok_or(BundleBuildError::NoChains)?;
    let first = input.chains.first().ok_or(BundleBuildError::NoChains)?;
    if last.public_key != key.verifying_key().to_hex() {
        return Err(BundleBuildError::KeyIsNotCurrent);
    }
    let tip_link = last.links.last().ok_or(BundleBuildError::NoChains)?;
    let first_link = first.links.first().ok_or(BundleBuildError::NoChains)?;
    let width = TRACTION_RULES_1.activity_rollup_ms;
    let first_window = first_link.header.window_start_ms;

    let mut work_buckets: Vec<WorkBucket> = input
        .work_buckets
        .into_iter()
        .filter(|b| b.project_id == input.project_id && b.minute_start_ms >= first_window)
        .collect();
    work_buckets.sort_by_key(|b| b.minute_start_ms);
    let mut commit_oids: BTreeMap<u64, Vec<String>> = BTreeMap::new();
    for commit in input
        .commits
        .iter()
        .filter(|c| c.project_id == input.project_id && c.observed_ms >= first_window)
    {
        commit_oids
            .entry(commit.observed_ms / width * width)
            .or_default()
            .push(commit.commit_oid.clone());
    }
    for oids in commit_oids.values_mut() {
        oids.sort();
        oids.dedup();
    }
    let mut activity_rollups: Vec<ActivityRollup> = input
        .rollups
        .into_iter()
        .filter(|r| r.window_start_ms >= first_window)
        .collect();
    activity_rollups.sort_by_key(|r| r.window_start_ms);

    let mut ruleset_ids: Vec<String> = input
        .chains
        .iter()
        .flat_map(|c| c.links.iter().map(|l| l.header.ruleset_id.clone()))
        .collect();
    ruleset_ids.sort();
    ruleset_ids.dedup();

    let body = ManifestBody {
        bundle_schema: BUNDLE_SCHEMA,
        project_id: input.project_id,
        linked_repo: None,
        key_id: key.key_id(),
        public_key: key.verifying_key().to_hex(),
        chain_count: input.chains.len(),
        key_ids: input
            .chains
            .iter()
            .filter_map(|c| VerifyingKeyHex::parse(&c.public_key).ok())
            .map(|k| k.key_id())
            .collect(),
        first_window_ms: first_window,
        tip: ManifestTip {
            key_id: tip_link.header.key_id.clone(),
            seq: tip_link.header.seq,
            entry_hash: tip_link.entry_hash.clone(),
        },
        link_count: input.chains.iter().map(|c| c.links.len()).sum(),
        bucket_count: work_buckets.len(),
        ruleset_ids,
        exported_at_ms: input.exported_at_ms,
        chains_digest: digest_hex(TAG_MANIFEST, &input.chains),
    };
    let body_value = serde_json::to_value(&body).expect("manifest body serializes");
    let signature = sign_manifest(key, &body_value).expect("manifest body has no floats");
    Ok(AuditBundle {
        manifest: Manifest { body, signature },
        chains: input.chains,
        project_rows: ProjectRowsSection {
            work_buckets,
            commit_oids,
        },
        device_rows: DeviceRowsSection { activity_rollups },
        frames: Vec::new(),
    })
}

/// Why a bundle failed to verify.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BundleError {
    /// The bytes are not a well-formed bundle of this schema.
    Malformed,
    /// The bundle parsed but failed a manifest or chain check.
    Chain(ChainError),
}

impl BundleError {
    /// A stable reason code.
    pub fn code(&self) -> &'static str {
        match self {
            BundleError::Malformed => "malformed_bundle",
            BundleError::Chain(error) => error.code(),
        }
    }
}

impl From<ChainError> for BundleError {
    fn from(error: ChainError) -> Self {
        BundleError::Chain(error)
    }
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for BundleError {}

/// What a reviewer pins or expects when verifying a bundle.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundlePolicy {
    /// Pinned public keys; `None` is trust on first use.
    pub trusted_keys: Option<Vec<VerifyingKeyHex>>,
    /// A tip learned out of band: `(key_id, seq, entry_hash)`.
    pub expected_tip: Option<(KeyId, u64, String)>,
}

/// Verify bundle bytes: manifest signature, manifest against the chains,
/// then [`verify_chains`] over this Project's rows with the manifest tip,
/// and finally every manifest count and digest.
pub fn verify_bundle(
    bytes: &[u8],
    policy: &BundlePolicy,
) -> Result<super::ChainReport, BundleError> {
    let bundle: AuditBundle = serde_json::from_slice(bytes).map_err(|_| BundleError::Malformed)?;
    let body = &bundle.manifest.body;
    if body.bundle_schema != BUNDLE_SCHEMA || !bundle.frames.is_empty() {
        return Err(BundleError::Malformed);
    }

    // 1. The manifest signature, under the manifest's own (self-asserted) key.
    let manifest_key = VerifyingKeyHex::parse(&body.public_key)?;
    let body_value = serde_json::to_value(body).map_err(|_| BundleError::Malformed)?;
    if manifest_key.key_id() != body.key_id {
        return Err(ChainError::ManifestSignatureInvalid.into());
    }
    verify_manifest(&manifest_key, &body_value, &bundle.manifest.signature)?;

    // 2. The manifest names the chains it ships.
    let keys = bundle
        .chains
        .iter()
        .map(|c| VerifyingKeyHex::parse(&c.public_key))
        .collect::<Result<Vec<_>, _>>()?;
    let key_ids: Vec<KeyId> = keys.iter().map(VerifyingKeyHex::key_id).collect();
    if body.chain_count != bundle.chains.len() || body.key_ids != key_ids {
        return Err(ChainError::ManifestChainsMismatch.into());
    }

    // 3. The rows: this Project's only; the bundle is complete for it.
    if bundle
        .project_rows
        .work_buckets
        .iter()
        .any(|b| b.project_id != body.project_id)
    {
        return Err(BundleError::Malformed);
    }
    let width = TRACTION_RULES_1.activity_rollup_ms;
    let mut commits = Vec::new();
    for (window, oids) in &bundle.project_rows.commit_oids {
        if window % width != 0 {
            return Err(BundleError::Malformed);
        }
        for oid in oids {
            commits.push(CommitObservation {
                project_id: body.project_id.clone(),
                commit_oid: oid.clone(),
                observed_ms: *window,
            });
        }
    }
    let rows = RowSet {
        scope: RowScope::Project(body.project_id.clone()),
        work_buckets: bundle.project_rows.work_buckets.clone(),
        commits,
        rollups: bundle.device_rows.activity_rollups.clone(),
    };

    // 4. The chains against the rows, pinned to the manifest tip.
    let chains: Vec<KeyedChain> = bundle
        .chains
        .iter()
        .zip(&keys)
        .map(|(c, key)| KeyedChain {
            key: key.clone(),
            links: c.links.clone(),
        })
        .collect();
    let chain_policy = VerifyPolicy {
        trusted_keys: policy.trusted_keys.clone(),
        expected_tip: Some((body.tip.seq, body.tip.entry_hash.clone())),
        ..VerifyPolicy::default()
    };
    let report = verify_chains(&chains, &rows, &chain_policy)?;

    // 5. Everything else the manifest counted or digested.
    let first_window = report.chains.first().map(|c| c.first_window_ms);
    let link_count: usize = chains.iter().map(|c| c.links.len()).sum();
    if Some(&body.tip.key_id) != key_ids.last()
        || body.link_count != link_count
        || body.bucket_count != bundle.project_rows.work_buckets.len()
        || Some(body.first_window_ms) != first_window
        || body.chains_digest != digest_hex(TAG_MANIFEST, &bundle.chains)
    {
        return Err(ChainError::ManifestChainsMismatch.into());
    }

    // 6. With pinned keys, the manifest's signer must be pinned too.
    if let Some(trusted) = &policy.trusted_keys
        && !trusted.contains(&manifest_key)
    {
        return Err(ChainError::UnknownKey {
            seq: body.tip.seq,
            key_id: body.key_id.clone(),
        }
        .into());
    }

    // 7. A tip the reviewer learned elsewhere.
    if let (Some((key_id, seq, hash)), Some(last)) = (&policy.expected_tip, report.chains.last())
        && (*key_id != last.key_id || *seq != last.tip.0 || *hash != last.tip.1)
    {
        return Err(ChainError::TipMismatch {
            expected_seq: *seq,
            found_seq: last.tip.0,
        }
        .into());
    }

    Ok(report)
}
