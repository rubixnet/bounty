//! Seam (spec "Testing seams" S4): `JevEngine` behind the `DecisionEngine`
//! seam, against a local `v1/systemone` server built from TypeSafe's
//! published schema (`common/typesafe.rs`). No database and no real network,
//! so it runs on all 3 OSes. The API key is a test value passed in; the
//! `TYPESAFE_API_KEY` environment variable is never read.

#[path = "common/typesafe.rs"]
mod typesafe;

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use traction_server::config::{Config, Heuristic, Jev};
use traction_server::decision::{
    AnchorFlags, DecisionEngine, ReviewContext, Slot, Triage, TriageInput, Verdict,
    VerificationSummary, decide,
};
use traction_server::jev::JevEngine;
use typesafe::{API_KEY, FakeTypeSafe, calm_answer, choice, noul, score};

const TINY: &[u8] = include_bytes!("data/frames/tiny_200x150.webp");
const NORMAL: &[u8] = include_bytes!("data/frames/normal_1280x800.webp");
const LARGE: &[u8] = include_bytes!("data/frames/large_2400x1500.webp");

/// 2026-11-27T12:00:00Z.
const NOW_MS: u64 = 1_795_780_800_000;

fn jev_config(fake: &FakeTypeSafe, ledger: &tempfile::TempDir) -> Jev {
    let mut jev = Config::parse("").unwrap().jev;
    jev.enabled = true;
    jev.base_url = fake.base.clone();
    jev.spend_ledger_file = ledger
        .path()
        .join("spend.json")
        .to_string_lossy()
        .into_owned();
    jev
}

fn engine_with(jev: Jev) -> JevEngine {
    JevEngine::new(
        jev,
        Heuristic::default(),
        API_KEY.into(),
        Arc::new(|| NOW_MS),
    )
    .unwrap()
}

struct Rig {
    fake: FakeTypeSafe,
    dir: tempfile::TempDir,
}

impl Rig {
    async fn new() -> Self {
        Self {
            fake: typesafe::start().await,
            dir: tempfile::tempdir().unwrap(),
        }
    }
    fn config(&self) -> Jev {
        jev_config(&self.fake, &self.dir)
    }
    fn engine(&self) -> JevEngine {
        engine_with(self.config())
    }
    fn bodies(&self) -> Vec<Value> {
        self.fake.seen().into_iter().map(|s| s.body).collect()
    }
}

fn honest_slot(i: u64) -> Slot {
    Slot {
        slot_start_ms: NOW_MS - 3 * 3_600_000 + i * 15 * 60_000,
        minutes: 15,
        human_active_sec: 600,
        agent_compute_sec: 120,
        struggle_credit_sec: 400,
        mutations_manual: 4,
        mutations_harness: 2,
        turns_started: 2,
        tool_failures: 1,
        commits_human: 1,
        rollup_windows: 3,
        switch_rate_milli_sum: 4_500,
        ..Slot::default()
    }
}

/// A submission `HeuristicEngine` rates Green.
fn green_input() -> TriageInput {
    TriageInput {
        submission_ref: "sref-7f3a9c".into(),
        verification: VerificationSummary {
            chains: 1,
            links: 9,
            key_pinned: true,
            ..VerificationSummary::default()
        },
        flags: AnchorFlags {
            realtime_windows: 20,
            active_minutes: 45,
            ..AnchorFlags::default()
        },
        slots: (0..3).map(honest_slot).collect(),
        review: ReviewContext::Submitted,
        frames: Vec::new(),
    }
}

async fn triage(engine: &JevEngine, input: &TriageInput) -> Triage {
    decide(engine, input).await
}

fn has(triage: &Triage, reason: &str) -> bool {
    triage.reasons.iter().any(|r| r == reason)
}

// ---------------------------------------------------------------- payload

#[tokio::test]
async fn the_request_has_the_schema_shape_the_pinned_model_and_the_bearer_key() {
    let rig = Rig::new().await;
    let t = triage(&rig.engine(), &green_input()).await;
    assert_eq!((t.verdict, t.engine_id.as_str()), (Verdict::Green, "jev"));
    assert!(t.rubric_version.contains("jev-rubric/1"), "{t:?}");
    assert!(
        t.rubric_version.contains("jev-1.14.0"),
        "the answering model is logged"
    );

    let seen = rig.fake.seen();
    assert_eq!(seen.len(), 1, "3 windows fit one batched request");
    let s = &seen[0];
    assert_eq!(
        s.authorization.as_deref(),
        Some(&*format!("Bearer {API_KEY}"))
    );
    assert_eq!(s.content_type.as_deref(), Some("application/json"));
    assert_eq!(
        s.danger_zone_images, None,
        "no frames, no Danger-Zone header"
    );
    let body = &s.body;
    assert_eq!(body["model"], "jev-1.14.0");
    assert!(body.get("assets").is_none());
    let windows = body["state"]["windows"].as_object().unwrap();
    assert_eq!(
        windows.keys().collect::<Vec<_>>(),
        ["w01", "w02", "w03"],
        "one named object per 15-minute window"
    );
    assert_eq!(windows["w01"]["person_input_seconds"], 600);
    assert_eq!(windows["w02"]["minutes_from_first_window"], 15);
    assert_eq!(windows["w01"]["app_switches_per_minute"], 1.5);

    let questions = body["questions"].as_object().unwrap();
    for w in ["w01", "w02", "w03"] {
        assert_eq!(questions[&format!("scripted_input__{w}")]["type"], "noul");
        assert_eq!(questions[&format!("work_pattern__{w}")]["type"], "choice");
        assert_eq!(
            questions[&format!("work_pattern_fits__{w}")]["type"],
            "noul"
        );
        assert_eq!(questions[&format!("visible_effort__{w}")]["type"], "score");
        let instructions = questions[&format!("scripted_input__{w}")]["instructions"]
            .as_str()
            .unwrap();
        assert!(
            instructions.contains(&format!("`windows.{w}`")),
            "each question names its window by path: {instructions}"
        );
    }
    let noul_q = &questions["scripted_input__w01"];
    assert!(noul_q["criteria"]["true"].is_string() && noul_q["criteria"]["false"].is_string());
    assert_eq!(
        questions["visible_effort__w01"]["criteria"]
            .as_array()
            .unwrap()
            .len(),
        3
    );
}

#[tokio::test]
async fn base_url_and_model_are_configurable_for_a_self_hosted_server() {
    let rig = Rig::new().await;
    let mut config = rig.config();
    config.model = "jev-1.13.0".into();
    let t = triage(&engine_with(config), &green_input()).await;
    assert_eq!(t.engine_id, "jev");
    assert_eq!(rig.bodies()[0]["model"], "jev-1.13.0");
}

#[tokio::test]
async fn no_choice_has_a_catch_all_other_and_each_has_a_companion_fits_noul() {
    let rig = Rig::new().await;
    triage(&rig.engine(), &green_input()).await;
    let body = &rig.bodies()[0];
    let questions = body["questions"].as_object().unwrap();
    let mut choices = 0;
    for (name, q) in questions {
        if q["type"] == "choice" {
            choices += 1;
            for option in q["criteria"].as_object().unwrap().keys() {
                assert!(
                    !["other", "none", "none_of_the_above"].contains(&option.as_str()),
                    "{name} has a catch-all {option}"
                );
            }
            let (base, window) = name.split_once("__").unwrap();
            let fits = &questions[&format!("{base}_fits__{window}")];
            assert_eq!(fits["type"], "noul", "{name} has a companion noul");
        }
    }
    assert_eq!(choices, 3);
}

#[tokio::test]
async fn the_serialized_request_names_no_user_path_username_or_project() {
    let rig = Rig::new().await;
    triage(&rig.engine(), &green_input()).await;
    let text = serde_json::to_string(&rig.bodies()[0])
        .unwrap()
        .to_lowercase();
    for forbidden in ["user", "path", "project", "root", "home", "sref-7f3a9c"] {
        assert!(!text.contains(forbidden), "`{forbidden}` in {text}");
    }
}

// ---------------------------------------------------------------- routing

#[tokio::test]
async fn nothing_fits_routes_to_yellow_and_a_fitting_option_uses_its_colour() {
    let rig = Rig::new().await;
    rig.fake.respond_with(|name, q| {
        if name == "work_pattern_fits__w02" {
            Some(noul(0.1))
        } else {
            calm_answer(name, q)
        }
    });
    let t = triage(&rig.engine(), &green_input()).await;
    assert_eq!(t.verdict, Verdict::Yellow, "{t:?}");
    assert!(has(&t, "jev_work_pattern_nothing_fits"));

    let rig = Rig::new().await;
    rig.fake.respond_with(|name, q| {
        if name.starts_with("work_pattern__") {
            Some(choice(&[
                ("building", 0.1),
                ("debugging", 0.1),
                ("directing_ai", 0.1),
                ("presence_without_output", 0.7),
            ]))
        } else {
            calm_answer(name, q)
        }
    });
    let t = triage(&rig.engine(), &green_input()).await;
    assert_eq!(t.verdict, Verdict::Yellow);
    assert!(has(&t, "jev_work_pattern_presence_without_output"));
}

#[tokio::test]
async fn an_unsettled_fits_noul_is_yellow_too() {
    let rig = Rig::new().await;
    rig.fake.respond_with(|name, q| {
        if name.starts_with("work_pattern_fits__") {
            Some(noul(0.5))
        } else {
            calm_answer(name, q)
        }
    });
    let t = triage(&rig.engine(), &green_input()).await;
    assert_eq!(t.verdict, Verdict::Yellow);
    assert!(has(&t, "jev_work_pattern_fit_unsettled"));
}

async fn verdict_for_scripted(p: f64) -> Triage {
    let rig = Rig::new().await;
    rig.fake.respond_with(move |name, q| {
        if name == "scripted_input__w01" {
            Some(noul(p))
        } else {
            calm_answer(name, q)
        }
    });
    triage(&rig.engine(), &green_input()).await
}

#[tokio::test]
async fn noul_thresholds_start_at_0_7_and_0_3() {
    let yes = verdict_for_scripted(0.7).await;
    assert_eq!(yes.verdict, Verdict::Yellow);
    assert!(has(&yes, "jev_scripted_input"));
    assert_eq!(
        verdict_for_scripted(0.69).await.verdict,
        Verdict::Green,
        "unsettled"
    );
    assert_eq!(
        verdict_for_scripted(0.3).await.verdict,
        Verdict::Green,
        "no"
    );
}

#[tokio::test]
async fn the_noul_thresholds_and_colours_are_config() {
    let rig = Rig::new().await;
    let mut config = rig.config();
    config.noul_yes = 0.9;
    config.nouls[0].colour_unsettled = Verdict::Yellow;
    rig.fake.respond_with(|name, q| {
        if name == "scripted_input__w03" {
            Some(noul(0.8))
        } else {
            calm_answer(name, q)
        }
    });
    let t = triage(&engine_with(config), &green_input()).await;
    assert_eq!(t.verdict, Verdict::Yellow);
    assert!(has(&t, "jev_scripted_input_unsettled"), "{t:?}");
}

async fn verdict_for_effort(probs: [f64; 3]) -> Triage {
    let rig = Rig::new().await;
    rig.fake.respond_with(move |name, q| {
        if name == "visible_effort__w02" {
            Some(score(q, &probs))
        } else {
            calm_answer(name, q)
        }
    });
    triage(&rig.engine(), &green_input()).await
}

#[tokio::test]
async fn two_score_distributions_with_the_same_mean_route_differently() {
    // Both have an expected score of exactly 1.0.
    let split = verdict_for_effort([0.5, 0.0, 0.5]).await;
    let middle = verdict_for_effort([0.0, 1.0, 0.0]).await;
    assert_eq!(
        split.verdict,
        Verdict::Yellow,
        "half the mass on `no effort`"
    );
    assert!(has(&split, "jev_visible_effort_no_visible_effort"));
    assert_eq!(middle.verdict, Verdict::Green);
}

#[tokio::test]
async fn jev_escalates_heuristic_but_never_downgrades_it() {
    // Heuristic Yellow (an offline-buffered window), Jev all calm.
    let rig = Rig::new().await;
    let mut input = green_input();
    input.flags.offline_buffered_windows = 1;
    let t = triage(&rig.engine(), &input).await;
    assert_eq!((t.verdict, t.engine_id.as_str()), (Verdict::Yellow, "jev"));
    assert!(has(&t, "offline_buffered"), "Heuristic reasons are kept");
    assert_eq!(rig.fake.seen().len(), 1, "Jev was asked");

    // Heuristic Green, Jev Yellow.
    let t = verdict_for_scripted(0.95).await;
    assert_eq!((t.verdict, t.engine_id.as_str()), (Verdict::Yellow, "jev"));
}

#[tokio::test]
async fn jev_is_not_called_when_heuristic_or_a_deterministic_check_is_red() {
    let rig = Rig::new().await;
    let engine = rig.engine();

    let mut late = green_input();
    late.flags.late_anchor_windows = 5; // 25% of realtime windows: Red
    let t = triage(&engine, &late).await;
    assert_eq!(
        (t.verdict, t.engine_id.as_str()),
        (Verdict::Red, "heuristic")
    );

    let mut broken = green_input();
    broken.verification.failure = Some("tip_mismatch".into());
    let t = triage(&engine, &broken).await;
    assert_eq!(
        (t.verdict, t.engine_id.as_str()),
        (Verdict::Red, "deterministic")
    );

    assert!(rig.fake.seen().is_empty(), "no Jev request at all");
}

#[tokio::test]
async fn jev_cannot_upgrade_a_deterministic_red_even_through_the_engine_itself() {
    let rig = Rig::new().await;
    let mut broken = green_input();
    broken.verification.failure = Some("rows_mismatch".into());
    // Called directly (bypassing `decide`), the engine still stays Red.
    let t = rig.engine().triage(&broken).await.unwrap();
    assert_eq!(t.verdict, Verdict::Red);
    assert!(rig.fake.seen().is_empty());
}

#[tokio::test]
async fn the_router_never_bans_even_when_every_answer_is_alarming() {
    let rig = Rig::new().await;
    let mut config = rig.config();
    config.nouls[0].colour_yes = Verdict::Red;
    rig.fake.respond_with(|name, q| match q["type"].as_str() {
        Some("noul") if name.contains("_fits__") => Some(noul(0.0)),
        Some("noul") => Some(noul(1.0)),
        _ => calm_answer(name, q),
    });
    let t = triage(&engine_with(config), &green_input()).await;
    // Red is an anomaly pin for Fraud Squad, the worst an engine can say.
    assert_eq!(t.verdict, Verdict::Red);
    assert!(matches!(
        t.verdict,
        Verdict::Green | Verdict::Yellow | Verdict::Red
    ));
    assert!(t.reasons.iter().all(|r| !r.contains("ban")));
}

// ---------------------------------------------------------------- frames

fn with_frames(review: ReviewContext, frames: &[&[u8]]) -> TriageInput {
    let mut input = green_input();
    input.review = review;
    input.frames = frames.iter().map(|f| f.to_vec()).collect();
    input
}

#[tokio::test]
async fn frames_go_only_with_submitted_or_flagged_bundles_with_the_danger_zone_header() {
    for review in [ReviewContext::Submitted, ReviewContext::Flagged] {
        let rig = Rig::new().await;
        let t = triage(&rig.engine(), &with_frames(review, &[NORMAL, LARGE])).await;
        assert_eq!(t.verdict, Verdict::Green, "{t:?}");
        let seen = rig.fake.seen();
        let with_assets: Vec<_> = seen
            .iter()
            .filter(|s| s.body.get("assets").is_some())
            .collect();
        assert_eq!(with_assets.len(), 1, "{review:?}: one frame request");
        let frame_req = with_assets[0];
        assert_eq!(frame_req.danger_zone_images.as_deref(), Some("true"));
        let assets = frame_req.body["assets"].as_object().unwrap();
        assert_eq!(assets.len(), 2);
        let state_text = frame_req.body["state"].to_string();
        for (placeholder, asset) in assets {
            assert_eq!(placeholder.len(), 36, "a UUID placeholder: {placeholder}");
            assert_eq!(placeholder.matches('-').count(), 4);
            assert!(
                state_text.contains(placeholder.as_str()),
                "used in the state"
            );
            assert_eq!(
                (asset["type"].as_str(), asset["format"].as_str()),
                (Some("image"), Some("jpg"))
            );
        }
        for s in &seen {
            if s.body.get("assets").is_none() {
                assert_eq!(
                    s.danger_zone_images, None,
                    "only the frame request carries it"
                );
            }
        }
    }

    let rig = Rig::new().await;
    triage(
        &rig.engine(),
        &with_frames(ReviewContext::Routine, &[NORMAL]),
    )
    .await;
    let seen = rig.fake.seen();
    assert!(!seen.is_empty());
    for s in seen {
        assert!(
            s.body.get("assets").is_none(),
            "a routine bundle sends no image"
        );
        assert_eq!(s.danger_zone_images, None);
    }
}

#[tokio::test]
async fn frame_answers_are_reviewer_hints_that_never_change_the_colour() {
    let rig = Rig::new().await;
    rig.fake.respond_with(|name, q| {
        if name.starts_with("frame_shows_software_work__") {
            Some(noul(0.05))
        } else {
            calm_answer(name, q)
        }
    });
    let t = triage(
        &rig.engine(),
        &with_frames(ReviewContext::Submitted, &[NORMAL]),
    )
    .await;
    assert_eq!(t.verdict, Verdict::Green);
    assert!(has(&t, "jev_frame_hint_no_software_work"), "{t:?}");
}

#[tokio::test]
async fn frames_under_280_px_are_skipped_and_an_all_skipped_bundle_is_at_least_yellow() {
    // Some skipped: recorded, colour unchanged, the good frame is still sent.
    let rig = Rig::new().await;
    let t = triage(
        &rig.engine(),
        &with_frames(ReviewContext::Submitted, &[TINY, NORMAL]),
    )
    .await;
    assert_eq!(t.verdict, Verdict::Green, "{t:?}");
    assert!(has(&t, "frame_too_small_skipped"));
    assert!(!has(&t, "frame_skip_share"));
    let frame_reqs: Vec<_> = rig
        .fake
        .seen()
        .into_iter()
        .filter(|s| s.body.get("assets").is_some())
        .collect();
    assert_eq!(frame_reqs[0].body["assets"].as_object().unwrap().len(), 1);

    // All skipped: at least Yellow, and no frame request is made.
    let rig = Rig::new().await;
    let t = triage(
        &rig.engine(),
        &with_frames(ReviewContext::Submitted, &[TINY, TINY]),
    )
    .await;
    assert_eq!(t.verdict, Verdict::Yellow, "{t:?}");
    assert!(has(&t, "frame_too_small_skipped") && has(&t, "frame_skip_share"));
    assert!(
        rig.fake
            .seen()
            .iter()
            .all(|s| s.body.get("assets").is_none())
    );

    // The share threshold is config.
    let rig = Rig::new().await;
    let mut config = rig.config();
    config.frame_skip_yellow_share = 0.5;
    let t = triage(
        &engine_with(config),
        &with_frames(ReviewContext::Submitted, &[TINY, NORMAL]),
    )
    .await;
    assert_eq!(t.verdict, Verdict::Yellow);
}

#[tokio::test]
async fn the_all_skipped_guard_holds_even_when_jev_falls_back() {
    let rig = Rig::new().await;
    rig.fake.fail_with(500);
    let t = triage(
        &rig.engine(),
        &with_frames(ReviewContext::Submitted, &[TINY]),
    )
    .await;
    assert_eq!(
        (t.verdict, t.engine_id.as_str()),
        (Verdict::Yellow, "heuristic")
    );
    assert!(has(&t, "frame_skip_share") && has(&t, "jev_fallback_error"));
}

// ---------------------------------------------------------------- fallback

#[tokio::test]
async fn an_error_reply_falls_back_to_heuristic_and_is_recorded() {
    let rig = Rig::new().await;
    rig.fake.fail_with(500);
    let mut input = green_input();
    input.flags.clock_skew = true; // Heuristic Yellow
    let t = triage(&rig.engine(), &input).await;
    assert_eq!(
        (t.verdict, t.engine_id.as_str()),
        (Verdict::Yellow, "heuristic")
    );
    assert!(
        has(&t, "jev_fallback_error") && has(&t, "clock_skew"),
        "{t:?}"
    );
    assert!(t.rubric_version.starts_with("heuristic/"));
}

#[tokio::test]
async fn a_reply_missing_an_answer_is_an_error_never_a_no() {
    let rig = Rig::new().await;
    rig.fake.respond_with(|name, q| {
        if name == "scripted_input__w02" {
            None
        } else {
            calm_answer(name, q)
        }
    });
    let t = triage(&rig.engine(), &green_input()).await;
    assert_eq!(
        (t.verdict, t.engine_id.as_str()),
        (Verdict::Green, "heuristic")
    );
    assert!(has(&t, "jev_fallback_error"));
}

#[tokio::test]
async fn a_wrong_key_is_an_error_fallback() {
    let rig = Rig::new().await;
    let engine = JevEngine::new(
        rig.config(),
        Heuristic::default(),
        "wrong".into(),
        Arc::new(|| NOW_MS),
    )
    .unwrap();
    let t = triage(&engine, &green_input()).await;
    assert_eq!(t.engine_id, "heuristic");
    assert!(has(&t, "jev_fallback_error"));
}

#[tokio::test]
async fn a_timeout_falls_back_to_heuristic_and_is_recorded() {
    let rig = Rig::new().await;
    rig.fake.delay(Duration::from_millis(1_500));
    let mut config = rig.config();
    config.timeout_ms = 200;
    let t = triage(&engine_with(config), &green_input()).await;
    assert_eq!(
        (t.verdict, t.engine_id.as_str()),
        (Verdict::Green, "heuristic")
    );
    assert!(has(&t, "jev_fallback_timeout"), "{t:?}");
}

#[tokio::test]
async fn the_spend_limit_falls_back_before_any_request_is_sent() {
    let rig = Rig::new().await;
    let mut config = rig.config();
    config.monthly_spend_limit_usd = 0.000_000_1; // ~2 tokens
    let engine = engine_with(config);
    let t = triage(&engine, &green_input()).await;
    assert_eq!(
        (t.verdict, t.engine_id.as_str()),
        (Verdict::Green, "heuristic")
    );
    assert!(has(&t, "jev_fallback_spend_limit"));
    assert!(rig.fake.seen().is_empty(), "nothing is sent over the limit");
    assert_eq!(engine.spent_this_month_usd(), 0.0);
}

#[tokio::test]
async fn spend_never_crosses_the_monthly_limit_across_submissions_and_restarts() {
    let rig = Rig::new().await;
    // Bill the worst case: one input token per body byte (the estimate).
    rig.fake.tokens(|len| len as u64);
    triage(&rig.engine(), &green_input()).await;
    let one = rig.fake.seen()[0].raw_len as f64;
    let price = rig.config().price_usd_per_million_input_tokens / 1e6;

    let dir = tempfile::tempdir().unwrap();
    let mut config = jev_config(&rig.fake, &dir);
    let limit = 1.5 * one * price; // room for one submission, not two
    config.monthly_spend_limit_usd = limit;

    let engine = engine_with(config.clone());
    let first = triage(&engine, &green_input()).await;
    assert_eq!(first.engine_id, "jev");
    let second = triage(&engine, &green_input()).await;
    assert_eq!(second.engine_id, "heuristic");
    assert!(has(&second, "jev_fallback_spend_limit"));
    let spent = engine.spent_this_month_usd();
    assert!(spent > 0.0 && spent <= limit, "{spent} <= {limit}");
    assert!(
        (spent - one * price).abs() < 1e-12,
        "billed from usage.input_tokens"
    );

    // A restart reads the month's spend back.
    let restarted = engine_with(config.clone());
    assert!((restarted.spent_this_month_usd() - spent).abs() < 1e-12);
    let third = triage(&restarted, &green_input()).await;
    assert!(has(&third, "jev_fallback_spend_limit"));

    // A new month starts from zero.
    let next_month = JevEngine::new(
        config,
        Heuristic::default(),
        API_KEY.into(),
        Arc::new(|| NOW_MS + 5 * 86_400_000),
    )
    .unwrap();
    assert_eq!(next_month.spent_this_month_usd(), 0.0);
    assert_eq!(triage(&next_month, &green_input()).await.engine_id, "jev");
    assert_eq!(
        rig.fake.seen().len(),
        3,
        "exactly three requests were ever sent"
    );
}

#[tokio::test]
async fn the_price_per_token_is_config() {
    let rig = Rig::new().await;
    rig.fake.tokens(|_| 1_000_000);
    let mut config = rig.config();
    config.price_usd_per_million_input_tokens = 0.5;
    let engine = engine_with(config);
    triage(&engine, &green_input()).await;
    assert!((engine.spent_this_month_usd() - 0.5).abs() < 1e-12);
}

// ---------------------------------------------------------------- wiring

#[tokio::test]
async fn the_api_key_comes_from_the_configured_variable_or_file_only() {
    let rig = Rig::new().await;
    let mut config = rig.config();
    config.api_key_env = "TRACTION_TEST_JEV_KEY_UNSET_X9".into();
    let made = JevEngine::from_config(
        &config,
        &Heuristic::default(),
        |_| None,
        Arc::new(|| NOW_MS),
    );
    assert!(made.is_err(), "no key, no engine");

    let key_file = rig.dir.path().join("typesafe.key");
    std::fs::write(&key_file, format!("{API_KEY}\n")).unwrap();
    config.api_key_file = Some(key_file.to_string_lossy().into_owned());
    let engine = JevEngine::from_config(
        &config,
        &Heuristic::default(),
        |_| None,
        Arc::new(|| NOW_MS),
    )
    .unwrap();
    assert_eq!(engine.engine_id(), "jev");
    assert_eq!(triage(&engine, &green_input()).await.engine_id, "jev");
}

#[tokio::test]
async fn a_base_url_that_is_neither_https_nor_loopback_is_refused() {
    let rig = Rig::new().await;
    let mut config = rig.config();
    config.base_url = "http://api.typesafe.ai".into();
    assert!(
        JevEngine::new(
            config,
            Heuristic::default(),
            API_KEY.into(),
            Arc::new(|| NOW_MS)
        )
        .is_err()
    );
}

#[tokio::test]
async fn many_windows_are_batched_and_capped_at_the_busiest() {
    let rig = Rig::new().await;
    let mut config = rig.config();
    config.windows_per_request = 4;
    config.max_windows = 6;
    let mut input = green_input();
    input.slots = (0..10).map(honest_slot).collect();
    input.slots[9].human_active_sec = 10; // the quietest windows are left out
    input.slots[8].human_active_sec = 20;
    input.slots[7].human_active_sec = 30;
    input.slots[0].human_active_sec = 40;
    triage(&engine_with(config), &input).await;
    let bodies = rig.bodies();
    let counts: Vec<usize> = bodies
        .iter()
        .map(|b| b["state"]["windows"].as_object().unwrap().len())
        .collect();
    assert_eq!(counts, [4, 2]);
}
