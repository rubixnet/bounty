//! Seam: the submission path against a real server and Postgres 17 (Linux
//! CI; set `TRACTION_TEST_DATABASE_URL` to run locally). A red-team suite:
//! every tamper goes through the real endpoints (`POST /v1/bundles`, a `PUT`
//! to the presigned URL, `POST /v1/bundles/{id}/complete`) with bundles built
//! by the real `traction-core` chain and verified by the unchanged
//! `verify_bundle`. The stored decision is what each test reads.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use common::*;
use serde_json::Value;
use traction_core::sync::{
    BundleCompleteResponse, BundleStartRequest, BundleStartResponse, ChainTip, HeartbeatRequest,
    ProjectMeta, SyncMode,
};
use traction_core::{
    ActivityRollup, BundleChain, BundleInput, ChainLink, DeviceSigningKey, LinkPayload,
    ProjectWindowRows, WorkBucket, build_bundle,
};
use traction_server::config::Config;
use traction_server::decision::{
    BoxFuture, DecisionEngine, EngineError, Triage, TriageInput, Verdict,
};

const T0: u64 = 1_800_000_000_000;
const WINDOW: u64 = 120_000;
const RULESET: &str = "traction-rules/1";

fn user_clock(ms: Arc<AtomicU64>) -> traction_server::app::Clock {
    Arc::new(move || ms.load(Ordering::SeqCst) * 1000)
}

struct Rig {
    server: TestServer,
    now: Arc<AtomicU64>,
    _blobs: tempfile::TempDir,
}

async fn rig_with(url: &str, engine: Option<Arc<dyn DecisionEngine>>) -> Rig {
    let now = Arc::new(AtomicU64::new(T0));
    let blobs = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.blob.local_dir = blobs.path().to_string_lossy().into_owned();
    let server = start_with_engine(url, config, Some(user_clock(Arc::clone(&now))), engine).await;
    Rig {
        server,
        now,
        _blobs: blobs,
    }
}

async fn rig(url: &str) -> Rig {
    rig_with(url, None).await
}

/// A real sealed chain of `n` windows for one Project.
struct Chain {
    key: DeviceSigningKey,
    links: Vec<ChainLink>,
    buckets: Vec<Vec<WorkBucket>>,
    rollups: Vec<ActivityRollup>,
}

fn seal_chain(key: DeviceSigningKey, n: u64, buckets_of: impl Fn(u64) -> Vec<WorkBucket>) -> Chain {
    let mut links: Vec<ChainLink> = Vec::new();
    let mut all_buckets = Vec::new();
    let mut rollups = Vec::new();
    for i in 0..n {
        let start = T0 + i * WINDOW;
        let buckets = buckets_of(start);
        let rollup = rollup(start);
        let payload = LinkPayload::new(
            start,
            RULESET,
            Some(&rollup),
            &[ProjectWindowRows::new(PROJECT.into(), buckets.clone(), vec![]).unwrap()],
            None,
        )
        .unwrap();
        let link = key
            .seal(
                links.last().map(|l| &l.header),
                payload,
                start + WINDOW + 10_000,
            )
            .unwrap();
        links.push(link);
        all_buckets.push(buckets);
        rollups.push(rollup);
    }
    Chain {
        key,
        links,
        buckets: all_buckets,
        rollups,
    }
}

fn honest_buckets(start: u64) -> Vec<WorkBucket> {
    vec![bucket(start, 40), bucket(start + 60_000, 30)]
}

fn heartbeat_for(
    chain: &Chain,
    i: usize,
    buckets: Vec<WorkBucket>,
    mode: SyncMode,
    sent_at_ms: u64,
) -> HeartbeatRequest {
    let link = &chain.links[i];
    HeartbeatRequest {
        tip: ChainTip {
            key_id: chain.key.key_id(),
            seq: link.header.seq,
            entry_hash: link.entry_hash.clone(),
            window_start_ms: link.header.window_start_ms,
        },
        sync_mode: mode,
        sent_at_ms,
        work_buckets: buckets,
        activity_rollups: vec![chain.rollups[i].clone()],
        projects: vec![ProjectMeta {
            project_id: PROJECT.into(),
            project_name: "demo".into(),
            project_root_path: "/home/u/demo".into(),
            project_home_relative_path: Some("~/demo".into()),
            os_username: "u".into(),
        }],
        timeline_marks: vec![],
    }
}

impl Rig {
    /// Anchors windows `which` of `chain`, each `lag_ms` after its window
    /// ended, with the device clock off by `clock_offset_ms` (`sent_at` is
    /// the device's idea of the time).
    async fn anchor(
        &self,
        token: &str,
        chain: &Chain,
        which: impl IntoIterator<Item = usize>,
        lag_ms: u64,
        clock_offset_ms: i64,
    ) {
        for i in which {
            let received = T0 + (i as u64 + 1) * WINDOW + lag_ms;
            self.now.store(received, Ordering::SeqCst);
            let sent = u64::try_from(i64::try_from(received).unwrap() + clock_offset_ms).unwrap();
            let reply = send_heartbeat(
                &self.server,
                token,
                &heartbeat_for(chain, i, chain.buckets[i].clone(), SyncMode::Realtime, sent),
            )
            .await;
            assert_eq!(reply.status, 200, "{}", reply.body);
        }
    }

    async fn paired(&self, seed: u8) -> (DeviceSigningKey, String, String) {
        let key = device_key(seed);
        let (token, device_id) = pair(&self.server, &key, None).await;
        (key, token, device_id)
    }
}

fn bundle_bytes(chain: &Chain, upto: usize, buckets: Option<Vec<WorkBucket>>) -> Vec<u8> {
    let work_buckets = buckets.unwrap_or_else(|| chain.buckets[..upto].concat());
    build_bundle(
        &chain.key,
        BundleInput {
            project_id: PROJECT.into(),
            chains: vec![BundleChain {
                public_key: chain.key.verifying_key().to_hex(),
                links: chain.links[..upto].to_vec(),
            }],
            work_buckets,
            commits: vec![],
            rollups: chain.rollups[..upto].to_vec(),
            exported_at_ms: T0,
        },
    )
    .unwrap()
    .to_canonical_bytes()
}

async fn put(url: String, body: Vec<u8>) -> Reply {
    tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let mut response = agent.put(&url).send(&body[..]).unwrap();
        Reply {
            status: response.status().as_u16(),
            retry_after: None,
            set_cookie: None,
            location: None,
            body: response.body_mut().read_to_string().unwrap_or_default(),
        }
    })
    .await
    .unwrap()
}

async fn start_bundle(rig: &Rig, token: &str, size: u64) -> Reply {
    post_json(
        format!("{}/v1/bundles", rig.server.base),
        Some(token.to_owned()),
        serde_json::to_string(&BundleStartRequest {
            project_id: PROJECT.into(),
            size_bytes: size,
        })
        .unwrap(),
    )
    .await
}

async fn complete(rig: &Rig, token: &str, id: &str) -> Reply {
    http(
        "POST",
        format!("{}/v1/bundles/{id}/complete", rig.server.base),
        Some(token.to_owned()),
        vec![],
        None,
    )
    .await
}

/// The whole upload flow; returns the bundle id.
async fn submit(rig: &Rig, token: &str, bytes: &[u8]) -> String {
    let started = start_bundle(rig, token, bytes.len() as u64).await;
    assert_eq!(started.status, 200, "{}", started.body);
    let started: BundleStartResponse = serde_json::from_str(&started.body).unwrap();
    let uploaded = put(started.put_url.clone(), bytes.to_vec()).await;
    assert_eq!(uploaded.status, 200, "{}", uploaded.body);
    let done = complete(rig, token, &started.bundle_id).await;
    assert_eq!(done.status, 200, "{}", done.body);
    let done: BundleCompleteResponse = serde_json::from_str(&done.body).unwrap();
    assert_eq!(done.status, "submitted");
    // The submitter is not told the colour.
    assert!(!done.bundle_id.is_empty());
    started.bundle_id
}

struct Decision {
    verdict: String,
    reasons: Vec<String>,
    engine_id: String,
    failure: Option<String>,
    input: Value,
}

async fn decision(rig: &Rig, bundle_id: &str) -> Decision {
    let (verdict, reasons, engine_id, failure, input): (
        String,
        Vec<String>,
        String,
        Option<String>,
        Value,
    ) = sqlx::query_as(
        "SELECT verdict, reasons, engine_id, failure, input FROM triage_decisions
         WHERE bundle_id = $1",
    )
    .bind(bundle_id)
    .fetch_one(&rig.server.pool)
    .await
    .expect("a stored decision");
    Decision {
        verdict,
        reasons,
        engine_id,
        failure,
        input,
    }
}

#[tokio::test]
async fn an_honest_bundle_verifies_and_is_green_from_the_heuristic_engine() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let chain = seal_chain(key, 4, honest_buckets);
    rig.anchor(&token, &chain, 0..4, 30_000, 0).await;

    let id = submit(&rig, &token, &bundle_bytes(&chain, 4, None)).await;
    let d = decision(&rig, &id).await;
    assert_eq!(
        (d.verdict.as_str(), d.engine_id.as_str(), d.failure),
        ("green", "heuristic", None),
        "{:?}",
        d.reasons
    );
    assert!(d.reasons.is_empty(), "{:?}", d.reasons);
}

#[tokio::test]
async fn the_triage_input_holds_no_user_path_username_or_project_name() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let chain = seal_chain(key, 4, honest_buckets);
    rig.anchor(&token, &chain, 0..4, 30_000, 0).await;
    let id = submit(&rig, &token, &bundle_bytes(&chain, 4, None)).await;
    let d = decision(&rig, &id).await;
    let text = d.input.to_string();
    let user_id: String = sqlx::query_scalar("SELECT id FROM users")
        .fetch_one(&rig.server.pool)
        .await
        .unwrap();
    for forbidden in [
        user_id.as_str(),
        PROJECT,
        "demo",
        "/home/u",
        "~/demo",
        "dev-user",
        "Dev User",
        "test laptop",
    ] {
        assert!(!text.contains(forbidden), "{forbidden} in {text}");
    }
    // It does hold the aggregates the engine needs.
    assert!(d.input["slots"].as_array().is_some_and(|s| !s.is_empty()));
}

#[tokio::test]
async fn a_backdated_tip_is_late_anchor_red_when_common_and_yellow_when_rare() {
    let Some(url) = database_url() else { return };
    // Every window anchored 25 minutes late (the limit is 20) in realtime mode.
    let rig_a = rig(&url).await;
    let (key, token, _) = rig_a.paired(1).await;
    let chain = seal_chain(key, 4, honest_buckets);
    rig_a.anchor(&token, &chain, 0..4, 25 * 60_000, 0).await;
    let id = submit(&rig_a, &token, &bundle_bytes(&chain, 4, None)).await;
    let d = decision(&rig_a, &id).await;
    assert_eq!(d.verdict, "red", "{:?}", d.reasons);
    assert!(d.reasons.contains(&"late_anchor".to_owned()));
    assert_eq!(d.engine_id, "heuristic");

    // One late window in 40 (2.5%, under the 5% Red share) is Yellow.
    let rig_b = rig(&url).await;
    let (key, token, _) = rig_b.paired(1).await;
    let chain = seal_chain(key, 40, honest_buckets);
    rig_b.anchor(&token, &chain, 0..39, 30_000, 0).await;
    rig_b.anchor(&token, &chain, 39..40, 25 * 60_000, 0).await;
    let id = submit(&rig_b, &token, &bundle_bytes(&chain, 40, None)).await;
    let d = decision(&rig_b, &id).await;
    assert_eq!(d.verdict, "yellow", "{:?}", d.reasons);
    assert_eq!(d.reasons, ["late_anchor"]);
}

#[tokio::test]
async fn a_cut_tail_is_red_because_an_anchor_is_missing_from_the_bundle() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let chain = seal_chain(key, 4, honest_buckets);
    rig.anchor(&token, &chain, 0..4, 30_000, 0).await;
    // A valid, honestly signed chain, cut after window 2: verify_bundle
    // accepts it, the stored Anchors for seq 2 and 3 do not appear in it.
    let id = submit(&rig, &token, &bundle_bytes(&chain, 2, None)).await;
    let d = decision(&rig, &id).await;
    assert_eq!(d.verdict, "red");
    assert_eq!(d.failure.as_deref(), Some("anchor_missing"));
    assert_eq!(d.engine_id, "deterministic");
}

#[tokio::test]
async fn rows_that_differ_from_the_live_sync_are_red() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let chain = seal_chain(key, 4, honest_buckets);
    // The live heartbeats claim more human time than the sealed chain says.
    for i in 0..4 {
        let mut inflated = chain.buckets[i].clone();
        inflated[0].human_active_sec = 58;
        let received = T0 + (i as u64 + 1) * WINDOW + 30_000;
        rig.now.store(received, Ordering::SeqCst);
        let reply = send_heartbeat(
            &rig.server,
            &token,
            &heartbeat_for(&chain, i, inflated, SyncMode::Realtime, received),
        )
        .await;
        assert_eq!(reply.status, 200, "{}", reply.body);
    }
    let id = submit(&rig, &token, &bundle_bytes(&chain, 4, None)).await;
    let d = decision(&rig, &id).await;
    assert_eq!(d.verdict, "red");
    assert_eq!(d.failure.as_deref(), Some("rows_mismatch"));
}

#[tokio::test]
async fn a_rewritten_chain_with_the_same_key_fails_against_the_stored_anchors() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let honest = seal_chain(key, 4, honest_buckets);
    rig.anchor(&token, &honest, 0..4, 30_000, 0).await;
    // The device owner re-seals a whole chain with inflated rows: internally
    // consistent and validly signed, so verify_bundle passes, but the
    // entry_hashes the server anchored live are gone.
    let rewritten = seal_chain(device_key(1), 4, |start| {
        vec![bucket(start, 60), bucket(start + 60_000, 60)]
    });
    let id = submit(&rig, &token, &bundle_bytes(&rewritten, 4, None)).await;
    let d = decision(&rig, &id).await;
    assert_eq!(d.verdict, "red");
    assert_eq!(d.failure.as_deref(), Some("anchor_mismatch"));
}

#[tokio::test]
async fn a_changed_frame_digest_is_red() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let mut chain = seal_chain(key, 4, honest_buckets);
    rig.anchor(&token, &chain, 0..4, 30_000, 0).await;
    // Tamper with one link's frame_digest after it was sealed and anchored.
    chain.links[1].payload.frame_digest = "1".repeat(64);
    let id = submit(&rig, &token, &bundle_bytes(&chain, 4, None)).await;
    let d = decision(&rig, &id).await;
    assert_eq!(d.verdict, "red");
    assert_eq!(d.engine_id, "deterministic");
    assert!(d.failure.is_some(), "a verification failure code is stored");
}

#[tokio::test]
async fn two_device_clock_skew_is_yellow_with_clock_skew_and_multi_device_overlap() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    // Device A's clock runs 5 minutes ahead; device B (the same user) works
    // in the same minutes with a correct clock.
    let (key_a, token_a, _) = rig.paired(1).await;
    let (key_b, token_b, _) = rig.paired(2).await;
    let chain_a = seal_chain(key_a, 4, honest_buckets);
    let chain_b = seal_chain(key_b, 4, honest_buckets);
    rig.anchor(&token_b, &chain_b, 0..4, 30_000, 0).await;
    rig.anchor(&token_a, &chain_a, 0..4, 40_000, 5 * 60_000)
        .await;
    let id = submit(&rig, &token_a, &bundle_bytes(&chain_a, 4, None)).await;
    let d = decision(&rig, &id).await;
    assert_eq!(d.verdict, "yellow", "{:?} {:?}", d.reasons, d.failure);
    assert_eq!(d.reasons, ["clock_skew", "multi_device_overlap"]);
}

struct Always(Verdict);

impl DecisionEngine for Always {
    fn engine_id(&self) -> &'static str {
        "always"
    }
    fn triage<'a>(&'a self, _: &'a TriageInput) -> BoxFuture<'a, Result<Triage, EngineError>> {
        let verdict = self.0;
        Box::pin(async move {
            Ok(Triage {
                verdict,
                reasons: vec![],
                engine_id: "always".into(),
                rubric_version: "t".into(),
            })
        })
    }
}

#[tokio::test]
async fn a_deterministic_failure_stays_red_whatever_the_engine_returns() {
    let Some(url) = database_url() else { return };
    let rig = rig_with(&url, Some(Arc::new(Always(Verdict::Green)))).await;
    let (key, token, _) = rig.paired(1).await;
    let chain = seal_chain(key, 4, honest_buckets);
    rig.anchor(&token, &chain, 0..4, 30_000, 0).await;

    // The engine says Green about an honest bundle and the engine is recorded.
    let honest = submit(&rig, &token, &bundle_bytes(&chain, 4, None)).await;
    let d = decision(&rig, &honest).await;
    assert_eq!(
        (d.verdict.as_str(), d.engine_id.as_str()),
        ("green", "always")
    );

    // But a cut tail is Red, decided before (and without) the engine.
    let cut = submit(&rig, &token, &bundle_bytes(&chain, 2, None)).await;
    let d = decision(&rig, &cut).await;
    assert_eq!(
        (d.verdict.as_str(), d.engine_id.as_str()),
        ("red", "deterministic")
    );
}

#[tokio::test]
async fn garbage_and_foreign_bundles_are_red_not_errors() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let chain = seal_chain(key, 2, honest_buckets);
    rig.anchor(&token, &chain, 0..2, 30_000, 0).await;
    let id = submit(&rig, &token, b"{\"not\":\"a bundle\"}").await;
    let d = decision(&rig, &id).await;
    assert_eq!(d.verdict, "red");
    assert_eq!(d.failure.as_deref(), Some("malformed_bundle"));

    // A bundle signed by a key the device never pinned.
    let stranger = seal_chain(device_key(9), 2, honest_buckets);
    let id = submit(&rig, &token, &bundle_bytes(&stranger, 2, None)).await;
    let d = decision(&rig, &id).await;
    assert_eq!(d.verdict, "red");
    assert!(d.failure.is_some());
}

#[tokio::test]
async fn a_device_with_no_anchors_cannot_be_checked_and_is_red() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let chain = seal_chain(key, 2, honest_buckets);
    let id = submit(&rig, &token, &bundle_bytes(&chain, 2, None)).await;
    let d = decision(&rig, &id).await;
    assert_eq!(d.verdict, "red");
    assert_eq!(d.failure.as_deref(), Some("no_anchors"));
}

#[tokio::test]
async fn windows_sealed_after_the_newest_anchor_are_yellow_not_tampering() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let chain = seal_chain(key, 4, honest_buckets);
    // Only the first 3 windows reached the server before the export.
    rig.anchor(&token, &chain, 0..3, 30_000, 0).await;
    let id = submit(&rig, &token, &bundle_bytes(&chain, 4, None)).await;
    let d = decision(&rig, &id).await;
    assert_eq!(d.verdict, "yellow", "{:?} {:?}", d.reasons, d.failure);
    assert_eq!(d.reasons, ["unanchored_tail"]);
}

#[tokio::test]
async fn the_upload_endpoints_enforce_the_url_the_length_and_ownership() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (_, token, _) = rig.paired(1).await;
    let (_, other_token, _) = rig.paired(2).await;

    // Over the 64 MB cap and zero bytes are refused up front.
    assert_eq!(
        start_bundle(&rig, &token, 64 * 1024 * 1024 + 1)
            .await
            .status,
        422
    );
    assert_eq!(start_bundle(&rig, &token, 0).await.status, 422);
    assert_eq!(start_bundle(&rig, "bad-token", 10).await.status, 401);

    let started = start_bundle(&rig, &token, 5).await;
    let started: BundleStartResponse = serde_json::from_str(&started.body).unwrap();
    // Not uploaded yet.
    assert_eq!(complete(&rig, &token, &started.bundle_id).await.status, 409);
    // Another device cannot complete it.
    assert_eq!(
        complete(&rig, &other_token, &started.bundle_id)
            .await
            .status,
        404
    );
    // The exact Content-Length: a different body is refused, nothing stored.
    assert_eq!(
        put(started.put_url.clone(), b"toolong".to_vec())
            .await
            .status,
        400
    );
    // A forged signature is refused.
    let forged = started.put_url.replace("sig=", "sig=00");
    assert_eq!(put(forged, b"hello".to_vec()).await.status, 401);
    assert_eq!(complete(&rig, &token, &started.bundle_id).await.status, 409);
    // After the expiry the URL stops working.
    rig.now.store(T0 + 16 * 60_000, Ordering::SeqCst);
    assert_eq!(
        put(started.put_url.clone(), b"hello".to_vec()).await.status,
        403
    );
}

#[tokio::test]
async fn delete_me_also_deletes_the_users_blobs() {
    let Some(url) = database_url() else { return };
    let rig = rig(&url).await;
    let (key, token, _) = rig.paired(1).await;
    let chain = seal_chain(key, 2, honest_buckets);
    rig.anchor(&token, &chain, 0..2, 30_000, 0).await;
    let id = submit(&rig, &token, &bundle_bytes(&chain, 2, None)).await;
    let pending = start_bundle(&rig, &token, 5).await;
    assert_eq!(pending.status, 200);
    let file = rig._blobs.path().join("bundles").join(format!("{id}.json"));
    assert!(file.exists());

    let gone = http(
        "DELETE",
        format!("{}/v1/me", rig.server.base),
        Some(token),
        vec![],
        None,
    )
    .await;
    assert_eq!(gone.status, 204);
    assert!(
        !file.exists(),
        "the bundle object is deleted with the account"
    );
    for table in ["bundles", "triage_decisions"] {
        let n: i64 =
            sqlx::query_scalar(sqlx::AssertSqlSafe(format!("SELECT count(*) FROM {table}")))
                .fetch_one(&rig.server.pool)
                .await
                .unwrap();
        assert_eq!(n, 0, "{table}");
    }
}
