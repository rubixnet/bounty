//! `JevEngine`: TypeSafe System One behind the [`DecisionEngine`] seam
//! (spec S4, ticket 05).
//!
//! **Combination order.** Deterministic checks (S3a) are Red before any
//! engine runs. `HeuristicEngine` always runs next; if it says Red, Jev is not
//! called. Otherwise Jev is asked qualitative per-window questions and the
//! colour is `max_severity(heuristic, jev)`: Jev escalates, never downgrades,
//! and there is no ban variant. On the spend limit, an error, a malformed
//! reply or a timeout, `HeuristicEngine`'s decision stands, recorded with
//! `engine_id = heuristic` and a `jev_fallback_*` reason.
//!
//! **What is sent.** Only the anonymized [`TriageInput`]: at most
//! `max_windows` 15-minute windows (the busiest), `windows_per_request` per
//! request, each a named object under `state.windows` (`w01`, `w02`, ...)
//! with no user, path, username, Project name or submission reference. Every
//! question, criterion and colour comes from `[jev]` (`jev.default.toml`).
//! Each `choice` is paired with a `<name>_fits` `noul` instead of an
//! `"other"` option; `score` answers are read as their full level
//! distribution, never the expected score.
//!
//! **Frames.** Only for `Submitted` or `Flagged` bundles: sampled WebP frames
//! are transcoded in memory to baseline JPEG ([`crate::transcode`]), sent in
//! a separate request under UUID placeholders with
//! `TypeSafe-Danger-Zone-Images: true`, and their answers are informational
//! reviewer hints only. Frames under 280 px are skipped
//! (`frame_too_small_skipped`); if the skipped share reaches
//! `frame_skip_yellow_share` the decision is at least Yellow, even when Jev
//! falls back.

pub mod spend;
pub mod wire;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};

use crate::config::{ChoiceRubric, ConfigError, Heuristic, Jev, NoulRubric};
use crate::decision::{
    BoxFuture, DecisionEngine, EngineError, Slot, Triage, TriageInput, Verdict, reasons,
};
use crate::heuristic::HeuristicEngine;
use crate::transcode::{self, FrameSkip};
use spend::{SpendError, SpendLedger};
use wire::{Answer, Asset, NoulCriteria, Question, SystemOneRequest, SystemOneResponse};

pub const ENGINE_ID: &str = "jev";
/// Largest accepted reply body.
const MAX_REPLY_BYTES: u64 = 4 * 1024 * 1024;

/// Milliseconds since the Unix epoch.
pub type ClockMs = Arc<dyn Fn() -> u64 + Send + Sync>;

pub struct JevEngine {
    inner: Arc<Inner>,
}

struct Inner {
    config: Jev,
    heuristic: HeuristicEngine,
    api_key: String,
    url: String,
    agent: ureq::Agent,
    ledger: SpendLedger,
    clock: ClockMs,
}

/// Why Jev's answer was not used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fallback {
    SpendLimit,
    Error,
    Timeout,
}

impl Fallback {
    fn reason(self) -> &'static str {
        match self {
            Fallback::SpendLimit => reasons::JEV_FALLBACK_SPEND_LIMIT,
            Fallback::Error => reasons::JEV_FALLBACK_ERROR,
            Fallback::Timeout => reasons::JEV_FALLBACK_TIMEOUT,
        }
    }
}

impl JevEngine {
    /// Builds the engine from `[jev]`, reading the API key only from
    /// `api_key_file` or the `api_key_env` variable (through `env`).
    pub fn from_config(
        config: &Jev,
        heuristic: &Heuristic,
        env: impl Fn(&str) -> Option<String>,
        clock: ClockMs,
    ) -> Result<Self, String> {
        let key = config.api_key(env).map_err(|e| match e {
            ConfigError::MissingSecret(what) => {
                format!("[jev] is enabled but its API key is not available: {what}")
            }
            other => other.to_string(),
        })?;
        Self::new(config.clone(), heuristic.clone(), key, clock)
    }

    pub fn new(
        config: Jev,
        heuristic: Heuristic,
        api_key: String,
        clock: ClockMs,
    ) -> Result<Self, String> {
        config.validate()?;
        let base = config.base_url.trim_end_matches('/').to_owned();
        if !traction_core::secure_http_url(&base) {
            return Err("`jev.base_url` must be https (plain http only on loopback)".into());
        }
        let ledger = SpendLedger::open(
            Path::new(&config.spend_ledger_file),
            config.monthly_spend_limit_usd * (1.0 - config.spend_safety_margin),
            config.price_usd_per_million_input_tokens,
            clock(),
        )?;
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(Duration::from_millis(config.timeout_ms.max(1))))
            .build()
            .into();
        Ok(Self {
            inner: Arc::new(Inner {
                url: format!("{base}/v1/systemone"),
                heuristic: HeuristicEngine::new(heuristic),
                config,
                api_key,
                agent,
                ledger,
                clock,
            }),
        })
    }

    /// What Jev has cost this UTC month, in USD.
    pub fn spent_this_month_usd(&self) -> f64 {
        self.inner.ledger.spent_usd((self.inner.clock)())
    }
}

impl DecisionEngine for JevEngine {
    fn engine_id(&self) -> &'static str {
        ENGINE_ID
    }

    fn triage<'a>(&'a self, input: &'a TriageInput) -> BoxFuture<'a, Result<Triage, EngineError>> {
        let inner = Arc::clone(&self.inner);
        let owned = input.clone();
        Box::pin(async move {
            let fallback_input = owned.clone();
            match tokio::task::spawn_blocking(move || inner.run(&owned)).await {
                Ok(triage) => Ok(triage),
                // The blocking task panicked: Heuristic decides, recorded.
                Err(_) => {
                    let mut triage = self.inner.heuristic.evaluate(&fallback_input);
                    triage.reasons.push(Fallback::Error.reason().into());
                    Ok(triage)
                }
            }
        })
    }
}

/// What the frames of a bundle came to before anything is sent.
#[derive(Default)]
struct Frames {
    /// Transcoded frames to send.
    jpegs: Vec<transcode::Jpeg>,
    /// Informational reason codes (`frame_too_small_skipped`, ...).
    reasons: Vec<String>,
    /// The skipped share reached the threshold.
    floor_yellow: bool,
}

/// One planned request and how to read it back.
struct Planned {
    body: Vec<u8>,
    images: bool,
    estimate: u64,
    kind: Kind,
}

enum Kind {
    /// Window labels in this request.
    Windows(Vec<String>),
    /// Frame labels in this request.
    Frames(Vec<String>),
}

/// The colours and reasons read from Jev's answers.
#[derive(Default)]
struct Reading {
    verdict: Option<Verdict>,
    reasons: Vec<String>,
    models: Vec<String>,
}

impl Reading {
    fn colour(&mut self, colour: Verdict, reason: String) {
        if colour != Verdict::Green {
            self.verdict = Some(self.verdict.map_or(colour, |v| v.max(colour)));
            push_unique(&mut self.reasons, reason);
        }
    }
}

fn push_unique(list: &mut Vec<String>, item: String) {
    if !list.contains(&item) {
        list.push(item);
    }
}

impl Inner {
    fn run(&self, input: &TriageInput) -> Triage {
        let heuristic = self.heuristic.evaluate(input);
        if heuristic.verdict == Verdict::Red {
            return heuristic;
        }
        let frames = self.prepare_frames(input);
        let plan = self.plan(input, &frames);
        if plan.is_empty() {
            return self.fallback_shape(heuristic, &frames, None);
        }
        match self.send_all(&plan) {
            Ok(reading) => self.combine(heuristic, &frames, reading),
            Err(why) => self.fallback_shape(heuristic, &frames, Some(why)),
        }
    }

    /// Heuristic's decision with the frame guard and an optional fallback
    /// reason.
    fn fallback_shape(&self, mut triage: Triage, frames: &Frames, why: Option<Fallback>) -> Triage {
        if frames.floor_yellow {
            triage.verdict = triage.verdict.max(Verdict::Yellow);
        }
        for r in &frames.reasons {
            push_unique(&mut triage.reasons, r.clone());
        }
        if let Some(why) = why {
            push_unique(&mut triage.reasons, why.reason().into());
        }
        triage
    }

    fn combine(&self, heuristic: Triage, frames: &Frames, reading: Reading) -> Triage {
        let mut verdict = heuristic.verdict;
        if let Some(jev) = reading.verdict {
            verdict = verdict.max(jev);
        }
        if frames.floor_yellow {
            verdict = verdict.max(Verdict::Yellow);
        }
        let mut all = heuristic.reasons;
        for r in frames.reasons.iter().chain(&reading.reasons) {
            push_unique(&mut all, r.clone());
        }
        Triage {
            verdict,
            reasons: all,
            engine_id: ENGINE_ID.into(),
            rubric_version: format!(
                "{}@{}+{}",
                self.config.rubric_version,
                reading.models.join(","),
                heuristic.rubric_version
            ),
        }
    }

    fn prepare_frames(&self, input: &TriageInput) -> Frames {
        let mut frames = Frames::default();
        if !input.review.allows_images() || input.frames.is_empty() {
            return frames;
        }
        let mut usable = Vec::new();
        let mut skipped = 0usize;
        let mut skip = |code: FrameSkip, frames: &mut Frames| {
            skipped += 1;
            push_unique(&mut frames.reasons, code.code().into());
        };
        for frame in &input.frames {
            match transcode::probe(frame) {
                Ok(_) => usable.push(frame),
                Err(code) => skip(code, &mut frames),
            }
        }
        let take = usable.len().min(self.config.max_frames);
        for i in 0..take {
            let frame = usable[i * usable.len() / take];
            match transcode::webp_to_baseline_jpeg(frame) {
                Ok(jpeg) => frames.jpegs.push(jpeg),
                Err(code) => skip(code, &mut frames),
            }
        }
        let share = skipped as f64 / input.frames.len() as f64;
        frames.floor_yellow = share >= self.config.frame_skip_yellow_share;
        if frames.floor_yellow {
            frames.reasons.push(reasons::FRAME_SKIP_SHARE.into());
        }
        frames
    }

    /// The busiest `max_windows` slots, in time order.
    fn windows<'a>(&self, input: &'a TriageInput) -> Vec<&'a Slot> {
        let mut slots: Vec<&Slot> = input.slots.iter().collect();
        slots.sort_by(|a, b| {
            b.human_active_sec
                .cmp(&a.human_active_sec)
                .then(a.slot_start_ms.cmp(&b.slot_start_ms))
        });
        slots.truncate(self.config.max_windows);
        slots.sort_by_key(|s| s.slot_start_ms);
        slots
    }

    fn plan(&self, input: &TriageInput, frames: &Frames) -> Vec<Planned> {
        let mut plan = Vec::new();
        let windows = self.windows(input);
        let first = windows.first().map_or(0, |s| s.slot_start_ms);
        let labelled: Vec<(String, &Slot)> = windows
            .into_iter()
            .enumerate()
            .map(|(i, s)| (format!("w{:02}", i + 1), s))
            .collect();
        for chunk in labelled.chunks(self.config.windows_per_request) {
            let mut state = Map::new();
            let mut questions = BTreeMap::new();
            for (label, slot) in chunk {
                state.insert(label.clone(), window_state(slot, first));
                self.window_questions(label, &mut questions);
            }
            let request = SystemOneRequest {
                model: self.config.model.clone(),
                state: json!({ "windows": state }),
                questions,
                assets: BTreeMap::new(),
            };
            let labels = chunk.iter().map(|(l, _)| l.clone()).collect();
            plan.push(self.planned(request, Kind::Windows(labels)));
        }
        if !frames.jpegs.is_empty() && !self.config.frame_nouls.is_empty() {
            let mut state = Map::new();
            let mut questions = BTreeMap::new();
            let mut assets = BTreeMap::new();
            let mut labels = Vec::new();
            for (i, jpeg) in frames.jpegs.iter().enumerate() {
                let label = format!("f{}", i + 1);
                let placeholder = uuid_v4();
                state.insert(label.clone(), Value::String(placeholder.clone()));
                assets.insert(
                    placeholder,
                    Asset {
                        kind: "image",
                        format: "jpg",
                        data_base64: STANDARD.encode(&jpeg.bytes),
                    },
                );
                for rubric in &self.config.frame_nouls {
                    let at = format!("`frames.{label}`");
                    questions.insert(
                        format!("{}__{label}", rubric.name),
                        Question::Noul {
                            instructions: rubric.instructions.replace("{frame}", &at),
                            criteria: NoulCriteria {
                                yes: rubric.criteria_true.replace("{frame}", &at),
                                no: rubric.criteria_false.replace("{frame}", &at),
                            },
                        },
                    );
                }
                labels.push(label);
            }
            let request = SystemOneRequest {
                model: self.config.model.clone(),
                state: json!({ "frames": state }),
                questions,
                assets,
            };
            plan.push(self.planned(request, Kind::Frames(labels)));
        }
        plan
    }

    fn planned(&self, request: SystemOneRequest, kind: Kind) -> Planned {
        let body = serde_json::to_vec(&request).expect("the request serializes");
        // Upper bound: one token per UTF-8 byte of text, plus the image
        // ceiling per image instead of its base64 text.
        let base64_len: usize = request.assets.values().map(|a| a.data_base64.len()).sum();
        let text_tokens = (body.len() - base64_len) as u64;
        let image_tokens = request.assets.len() as u64 * self.config.image_token_ceiling;
        Planned {
            images: !request.assets.is_empty(),
            estimate: self.ledger.cost(text_tokens + image_tokens),
            body,
            kind,
        }
    }

    fn window_questions(&self, label: &str, out: &mut BTreeMap<String, Question>) {
        let at = format!("`windows.{label}`");
        let fill = |text: &str| text.replace("{window}", &at);
        for rubric in &self.config.nouls {
            out.insert(
                format!("{}__{label}", rubric.name),
                Question::Noul {
                    instructions: fill(&rubric.instructions),
                    criteria: NoulCriteria {
                        yes: fill(&rubric.criteria_true),
                        no: fill(&rubric.criteria_false),
                    },
                },
            );
        }
        for rubric in &self.config.choices {
            out.insert(
                format!("{}__{label}", rubric.name),
                Question::Choice {
                    instructions: fill(&rubric.instructions),
                    criteria: rubric
                        .options
                        .iter()
                        .map(|(k, v)| (k.clone(), fill(v)))
                        .collect(),
                },
            );
            out.insert(
                format!("{}_fits__{label}", rubric.name),
                Question::Noul {
                    instructions: fill(&rubric.fits_instructions),
                    criteria: NoulCriteria {
                        yes: fill(&rubric.fits_true),
                        no: fill(&rubric.fits_false),
                    },
                },
            );
        }
        for rubric in &self.config.scores {
            out.insert(
                format!("{}__{label}", rubric.name),
                Question::Score {
                    instructions: fill(&rubric.instructions),
                    criteria: rubric.levels.iter().map(|l| fill(l)).collect(),
                },
            );
        }
    }

    /// Reserves the whole plan, then sends it request by request.
    fn send_all(&self, plan: &[Planned]) -> Result<Reading, Fallback> {
        let total: u64 = plan.iter().map(|p| p.estimate).sum();
        let now = (self.clock)();
        self.ledger.reserve(now, total).map_err(|e| match e {
            SpendError::LimitReached => Fallback::SpendLimit,
            SpendError::Ledger => Fallback::Error,
        })?;
        let mut reading = Reading::default();
        let mut outstanding = total;
        for planned in plan {
            let reply = self.send(planned);
            let reply = match reply {
                Ok(reply) => reply,
                Err(why) => {
                    self.ledger.release((self.clock)(), outstanding);
                    return Err(why);
                }
            };
            outstanding -= planned.estimate;
            let billed = self.ledger.cost(reply.usage.input_tokens);
            if self
                .ledger
                .settle((self.clock)(), planned.estimate, billed)
                .is_err()
            {
                self.ledger.release((self.clock)(), outstanding);
                return Err(Fallback::Error);
            }
            push_unique(&mut reading.models, reply.model.clone());
            if let Err(why) = self.read(planned, &reply, &mut reading) {
                self.ledger.release((self.clock)(), outstanding);
                return Err(why);
            }
        }
        Ok(reading)
    }

    fn send(&self, planned: &Planned) -> Result<SystemOneResponse, Fallback> {
        let mut request = self
            .agent
            .post(&self.url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json");
        if planned.images {
            request = request.header("TypeSafe-Danger-Zone-Images", "true");
        }
        let mut response = request.send(&planned.body[..]).map_err(classify)?;
        if response.status().as_u16() != 200 {
            return Err(Fallback::Error);
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_REPLY_BYTES)
            .read_to_vec()
            .map_err(classify)?;
        serde_json::from_slice(&bytes).map_err(|_| Fallback::Error)
    }

    /// Reads every asked question's answer; a missing or mistyped answer is
    /// an error, never a "no".
    fn read(
        &self,
        planned: &Planned,
        reply: &SystemOneResponse,
        reading: &mut Reading,
    ) -> Result<(), Fallback> {
        let answer = |name: &str| reply.answers.get(name).ok_or(Fallback::Error);
        match &planned.kind {
            Kind::Windows(labels) => {
                for label in labels {
                    for rubric in &self.config.nouls {
                        let p = noul_of(answer(&format!("{}__{label}", rubric.name))?)?;
                        let (colour, reason) = self.noul_band(rubric, p);
                        reading.colour(colour, reason);
                    }
                    for rubric in &self.config.choices {
                        let fits = noul_of(answer(&format!("{}_fits__{label}", rubric.name))?)?;
                        let picked =
                            choice_of(answer(&format!("{}__{label}", rubric.name))?, rubric)?;
                        let (colour, reason) = self.choice_colour(rubric, fits, &picked);
                        reading.colour(colour, reason);
                    }
                    for rubric in &self.config.scores {
                        let probs = score_of(
                            answer(&format!("{}__{label}", rubric.name))?,
                            rubric.levels.len(),
                        )?;
                        for rule in &rubric.rules {
                            let mass: f64 = rule.levels.iter().map(|&l| probs[l]).sum();
                            if mass >= rule.min_mass {
                                reading.colour(
                                    rule.colour,
                                    format!("jev_{}_{}", rubric.name, rule.reason),
                                );
                            }
                        }
                    }
                }
            }
            Kind::Frames(labels) => {
                for label in labels {
                    for rubric in &self.config.frame_nouls {
                        let p = noul_of(answer(&format!("{}__{label}", rubric.name))?)?;
                        let hint = if p >= self.config.noul_yes {
                            &rubric.hint_if_yes
                        } else if p <= self.config.noul_no {
                            &rubric.hint_if_no
                        } else {
                            &rubric.hint_if_unsettled
                        };
                        if let Some(hint) = hint {
                            push_unique(&mut reading.reasons, hint.clone());
                        }
                    }
                }
            }
        }
        Ok(())
    }

    fn noul_band(&self, rubric: &NoulRubric, p: f64) -> (Verdict, String) {
        if p >= self.config.noul_yes {
            (rubric.colour_yes, format!("jev_{}", rubric.name))
        } else if p <= self.config.noul_no {
            (rubric.colour_no, format!("jev_{}_no", rubric.name))
        } else {
            (
                rubric.colour_unsettled,
                format!("jev_{}_unsettled", rubric.name),
            )
        }
    }

    fn choice_colour(&self, rubric: &ChoiceRubric, fits: f64, picked: &str) -> (Verdict, String) {
        if fits <= self.config.noul_no {
            (
                rubric.colour_nothing_fits,
                format!("jev_{}_nothing_fits", rubric.name),
            )
        } else if fits < self.config.noul_yes {
            (
                rubric.colour_fit_unsettled,
                format!("jev_{}_fit_unsettled", rubric.name),
            )
        } else {
            let colour = rubric.option_colours[picked];
            (colour, format!("jev_{}_{picked}", rubric.name))
        }
    }
}

fn classify(error: ureq::Error) -> Fallback {
    match error {
        ureq::Error::Timeout(_) => Fallback::Timeout,
        ureq::Error::Io(e) if e.kind() == std::io::ErrorKind::TimedOut => Fallback::Timeout,
        _ => Fallback::Error,
    }
}

fn probability(p: f64) -> Result<f64, Fallback> {
    if p.is_finite() && (0.0..=1.0).contains(&p) {
        Ok(p)
    } else {
        Err(Fallback::Error)
    }
}

fn noul_of(answer: &Answer) -> Result<f64, Fallback> {
    match answer {
        Answer::Noul { noul } => probability(*noul),
        _ => Err(Fallback::Error),
    }
}

/// The most probable option, from the full `probabilities` map (which must
/// cover only the asked options).
fn choice_of(answer: &Answer, rubric: &ChoiceRubric) -> Result<String, Fallback> {
    let Answer::Choice {
        choice,
        probabilities,
    } = answer
    else {
        return Err(Fallback::Error);
    };
    if !rubric.options.contains_key(choice) {
        return Err(Fallback::Error);
    }
    let mut best: Option<(&String, f64)> = None;
    for (option, &p) in probabilities {
        if !rubric.options.contains_key(option) {
            return Err(Fallback::Error);
        }
        let p = probability(p)?;
        if best.is_none_or(|(_, b)| p > b) {
            best = Some((option, p));
        }
    }
    best.map(|(o, _)| o.clone()).ok_or(Fallback::Error)
}

/// The probability of each level `0..levels`; every level must be present.
fn score_of(answer: &Answer, levels: usize) -> Result<Vec<f64>, Fallback> {
    let Answer::Score { probabilities } = answer else {
        return Err(Fallback::Error);
    };
    if probabilities.len() != levels {
        return Err(Fallback::Error);
    }
    (0..levels)
        .map(|l| {
            probabilities
                .get(&l.to_string())
                .ok_or(Fallback::Error)
                .and_then(|&p| probability(p))
        })
        .collect()
}

/// One window as a person would read it: named counts, no identities.
/// Telemetry Tier is left out on purpose (it never changes the colour).
fn window_state(slot: &Slot, first_start_ms: u64) -> Value {
    let mut w = Map::new();
    let mut put = |k: &str, v: Value| {
        w.insert(k.into(), v);
    };
    put(
        "minutes_from_first_window",
        json!(slot.slot_start_ms.saturating_sub(first_start_ms) / 60_000),
    );
    put("minutes_with_activity", json!(slot.minutes));
    put("person_input_seconds", json!(slot.human_active_sec));
    put("ai_agent_working_seconds", json!(slot.agent_compute_sec));
    put("manual_file_edits", json!(slot.mutations_manual));
    put("ai_file_edits", json!(slot.mutations_harness));
    put("ai_turns_started", json!(slot.turns_started));
    put("ai_tool_failures", json!(slot.tool_failures));
    put("commits_by_the_person", json!(slot.commits_human));
    put("commits_by_an_ai_agent", json!(slot.commits_agent));
    if slot.rollup_windows > 0 {
        let per_minute =
            slot.switch_rate_milli_sum as f64 / f64::from(slot.rollup_windows) / 1000.0;
        put(
            "app_switches_per_minute",
            json!((per_minute * 10.0).round() / 10.0),
        );
    }
    Value::Object(w)
}

/// A random RFC 4122 version-4 UUID, used as an image placeholder.
fn uuid_v4() -> String {
    let mut b = [0u8; 16];
    getrandom::fill(&mut b).expect("the OS random source is available");
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}
