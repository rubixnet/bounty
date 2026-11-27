//! A local `v1/systemone` server for the `JevEngine` contract tests (spec
//! S4). Its validation and replies follow TypeSafe's published contract
//! (`https://api.typesafe.ai/openapi.json` v0.2.0 and the images page, read
//! 2026-11-27), not the engine's code:
//!
//! - `SystemOneRequest`: `model` (string), `state` (string | object | array)
//!   and `questions` (object, at least one) are required; `assets` is
//!   optional (object, at least one). Any other top-level key is a `422`.
//! - `Question` is discriminated by `type`: `noul` (`instructions`,
//!   `criteria {true,false}`), `choice` (`criteria` object of 1-255 names),
//!   `score` (`criteria` array of 1-10 levels) or `box` (images only).
//! - `Asset`: `{type: "image", format: "png" | "jpg" | "jpeg", data_base64}`.
//!   A request with assets must send `TypeSafe-Danger-Zone-Images: true`;
//!   each side must be 280-1568 px; every placeholder must be used in the
//!   state, an instruction or a criterion. Violations are `400` or `413`.
//! - Replies are a `SystemOneResponse`: `model`, `answers` keyed by question
//!   name (`NoulAnswer {type, noul}`, `ChoiceAnswer {type, choice,
//!   confidence, probabilities}`, `ScoreAnswer {type, score, confidence,
//!   legend, probabilities}`) and `usage {input_tokens, output_tokens}`.
#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Map, Value, json};
use tokio::task::JoinHandle;

pub const API_KEY: &str = "ts_test_key_not_real";

/// Answers one question: `(name, question) -> answer` (the full answer
/// object). `None` leaves the answer out (a malformed reply).
pub type Responder = Arc<dyn Fn(&str, &Value) -> Option<Value> + Send + Sync>;

#[derive(Debug, Clone)]
pub struct Seen {
    pub authorization: Option<String>,
    pub danger_zone_images: Option<String>,
    pub content_type: Option<String>,
    pub body: Value,
    pub raw_len: usize,
}

pub struct TsState {
    pub seen: Mutex<Vec<Seen>>,
    pub responder: Mutex<Responder>,
    /// Model name to reply with (the versioned ID that answered).
    pub reply_model: Mutex<String>,
    /// Reply with this status and an error body instead of answering.
    pub fail_status: Mutex<Option<u16>>,
    /// Wait this long before replying.
    pub delay: Mutex<Duration>,
    /// `usage.input_tokens` as a function of the raw body length.
    pub tokens_for_len: Mutex<Arc<dyn Fn(usize) -> u64 + Send + Sync>>,
}

pub struct FakeTypeSafe {
    pub base: String,
    pub state: Arc<TsState>,
    task: JoinHandle<()>,
}

impl Drop for FakeTypeSafe {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl FakeTypeSafe {
    pub fn seen(&self) -> Vec<Seen> {
        self.state.seen.lock().unwrap().clone()
    }
    pub fn respond_with(
        &self,
        responder: impl Fn(&str, &Value) -> Option<Value> + Send + Sync + 'static,
    ) {
        *self.state.responder.lock().unwrap() = Arc::new(responder);
    }
    pub fn fail_with(&self, status: u16) {
        *self.state.fail_status.lock().unwrap() = Some(status);
    }
    pub fn delay(&self, delay: Duration) {
        *self.state.delay.lock().unwrap() = delay;
    }
    pub fn tokens(&self, f: impl Fn(usize) -> u64 + Send + Sync + 'static) {
        *self.state.tokens_for_len.lock().unwrap() = Arc::new(f);
    }
}

pub fn noul(p: f64) -> Value {
    json!({"type": "noul", "noul": p})
}

/// A choice answer from `(option, probability)` pairs.
pub fn choice(probs: &[(&str, f64)]) -> Value {
    let (top, top_p) =
        probs.iter().copied().fold(
            ("", -1.0),
            |best, (o, p)| if p > best.1 { (o, p) } else { best },
        );
    let probabilities: Map<String, Value> = probs
        .iter()
        .map(|(o, p)| ((*o).to_owned(), json!(p)))
        .collect();
    json!({"type": "choice", "choice": top, "confidence": top_p, "probabilities": probabilities})
}

/// A score answer from per-level probabilities; `score` is the expected
/// level, as the contract defines it.
pub fn score(question: &Value, probs: &[f64]) -> Value {
    let levels = question["criteria"].as_array().unwrap();
    assert_eq!(levels.len(), probs.len(), "one probability per level");
    let legend: Map<String, Value> = levels
        .iter()
        .enumerate()
        .map(|(i, l)| (i.to_string(), l.clone()))
        .collect();
    let probabilities: Map<String, Value> = probs
        .iter()
        .enumerate()
        .map(|(i, p)| (i.to_string(), json!(p)))
        .collect();
    let expected: f64 = probs.iter().enumerate().map(|(i, p)| i as f64 * p).sum();
    let confidence = probs.iter().copied().fold(0.0, f64::max);
    json!({"type": "score", "score": expected, "confidence": confidence,
           "legend": legend, "probabilities": probabilities})
}

/// The calm default: every noul "no" (0.1), every choice its first option
/// with 0.9, every score all on its top level.
pub fn calm_answer(name: &str, question: &Value) -> Option<Value> {
    Some(match question["type"].as_str()? {
        "noul" if name.contains("_fits__") => noul(0.95),
        "noul" => noul(0.1),
        "choice" => {
            let options: Vec<&String> = question["criteria"].as_object()?.keys().collect();
            let mut probs: Vec<(&str, f64)> = vec![(options[0].as_str(), 0.9)];
            let rest = 0.1 / (options.len().max(2) - 1) as f64;
            probs.extend(options[1..].iter().map(|o| (o.as_str(), rest)));
            choice(&probs)
        }
        "score" => {
            let n = question["criteria"].as_array()?.len();
            let mut probs = vec![0.0; n];
            probs[n - 1] = 1.0;
            score(question, &probs)
        }
        _ => return None,
    })
}

pub async fn start() -> FakeTypeSafe {
    let state = Arc::new(TsState {
        seen: Mutex::new(Vec::new()),
        responder: Mutex::new(Arc::new(calm_answer)),
        reply_model: Mutex::new("jev-1.14.0".into()),
        fail_status: Mutex::new(None),
        delay: Mutex::new(Duration::ZERO),
        tokens_for_len: Mutex::new(Arc::new(|len| (len / 4) as u64)),
    });
    let app = Router::new()
        .route("/v1/systemone", post(systemone))
        .with_state(Arc::clone(&state));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    FakeTypeSafe { base, state, task }
}

fn bad(status: StatusCode, detail: &str) -> Response {
    (status, axum::Json(json!({"detail": detail}))).into_response()
}

fn invalid(loc: &[&str], msg: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        axum::Json(json!({"detail": [{"loc": loc, "msg": msg, "type": "value_error"}]})),
    )
        .into_response()
}

fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned)
}

/// Every string anywhere in `value`.
fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(items) => items.iter().for_each(|v| strings(v, out)),
        Value::Object(map) => map.values().for_each(|v| strings(v, out)),
        _ => {}
    }
}

/// Width and height from a JPEG's first start-of-frame marker.
fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if bytes.get(..2)? != [0xFF, 0xD8] {
        return None;
    }
    let mut at = 2;
    while at + 9 < bytes.len() {
        if bytes[at] != 0xFF {
            return None;
        }
        let marker = bytes[at + 1];
        let len = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        if (0xC0..=0xCF).contains(&marker) && ![0xC4, 0xC8, 0xCC].contains(&marker) {
            let h = u16::from_be_bytes([bytes[at + 5], bytes[at + 6]]);
            let w = u16::from_be_bytes([bytes[at + 7], bytes[at + 8]]);
            return Some((u32::from(w), u32::from(h)));
        }
        at += 2 + len;
    }
    None
}

async fn systemone(State(state): State<Arc<TsState>>, headers: HeaderMap, raw: Bytes) -> Response {
    let delay = *state.delay.lock().unwrap();
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    let body: Value = match serde_json::from_slice(&raw) {
        Ok(v) => v,
        Err(_) => return invalid(&["body"], "JSON decode error"),
    };
    state.seen.lock().unwrap().push(Seen {
        authorization: header(&headers, "authorization"),
        danger_zone_images: header(&headers, "typesafe-danger-zone-images"),
        content_type: header(&headers, "content-type"),
        body: body.clone(),
        raw_len: raw.len(),
    });
    if header(&headers, "authorization").as_deref() != Some(&format!("Bearer {API_KEY}")) {
        return bad(StatusCode::UNAUTHORIZED, "invalid API key");
    }
    if let Some(status) = *state.fail_status.lock().unwrap() {
        return bad(StatusCode::from_u16(status).unwrap(), "upstream failure");
    }

    let Some(obj) = body.as_object() else {
        return invalid(&["body"], "Input should be an object");
    };
    for key in obj.keys() {
        if !["model", "state", "questions", "assets"].contains(&key.as_str()) {
            return invalid(&["body", key], "Extra inputs are not permitted");
        }
    }
    if !obj.get("model").is_some_and(Value::is_string) {
        return invalid(&["body", "model"], "Field required");
    }
    if !obj
        .get("state")
        .is_some_and(|s| s.is_string() || s.is_object() || s.is_array())
    {
        return invalid(&["body", "state"], "Field required");
    }
    let Some(questions) = obj.get("questions").and_then(Value::as_object) else {
        return invalid(&["body", "questions"], "Field required");
    };
    if questions.is_empty() {
        return invalid(&["body", "questions"], "should have at least 1 item");
    }
    for (name, q) in questions {
        let ok = match q["type"].as_str() {
            Some("noul") => {
                q.as_object()
                    .unwrap()
                    .keys()
                    .all(|k| ["type", "instructions", "criteria"].contains(&k.as_str()))
                    && (q.get("criteria").is_none_or(|c| {
                        c.is_null()
                            || c.as_object()
                                .is_some_and(|c| c.keys().all(|k| k == "true" || k == "false"))
                    }))
            }
            Some("choice") => q["criteria"]
                .as_object()
                .is_some_and(|c| (1..=255).contains(&c.len())),
            Some("score") => q["criteria"]
                .as_array()
                .is_some_and(|c| (1..=10).contains(&c.len())),
            Some("box") => {
                header(&headers, "typesafe-danger-zone-images").as_deref() == Some("true")
            }
            _ => false,
        };
        if !ok {
            return invalid(&["body", "questions", name], "invalid question");
        }
    }

    if let Some(assets) = obj.get("assets") {
        if header(&headers, "typesafe-danger-zone-images").as_deref() != Some("true") {
            return bad(
                StatusCode::BAD_REQUEST,
                "assets need TypeSafe-Danger-Zone-Images: true",
            );
        }
        let Some(assets) = assets.as_object().filter(|a| !a.is_empty()) else {
            return invalid(&["body", "assets"], "should have at least 1 item");
        };
        let mut used = Vec::new();
        strings(&obj["state"], &mut used);
        for q in questions.values() {
            strings(&q["instructions"], &mut used);
            strings(&q["criteria"], &mut used);
        }
        for (placeholder, asset) in assets {
            if asset["type"] != "image"
                || !["png", "jpg", "jpeg"].contains(&asset["format"].as_str().unwrap_or(""))
            {
                return bad(StatusCode::BAD_REQUEST, "invalid asset");
            }
            let Some(bytes) = asset["data_base64"]
                .as_str()
                .and_then(|b| STANDARD.decode(b).ok())
            else {
                return bad(StatusCode::BAD_REQUEST, "invalid base64");
            };
            let Some((w, h)) = jpeg_size(&bytes) else {
                return bad(StatusCode::BAD_REQUEST, "damaged image");
            };
            if w.min(h) < 280 {
                return bad(StatusCode::BAD_REQUEST, "image below the minimum size");
            }
            if w.max(h) > 1568 {
                return bad(StatusCode::PAYLOAD_TOO_LARGE, "image over the maximum size");
            }
            if !used.iter().any(|s| s.contains(placeholder.as_str())) {
                return bad(StatusCode::BAD_REQUEST, "asset not referenced");
            }
        }
    }

    let responder = Arc::clone(&state.responder.lock().unwrap());
    let mut answers = Map::new();
    for (name, q) in questions {
        if let Some(answer) = responder(name, q) {
            answers.insert(name.clone(), answer);
        }
    }
    let tokens = (state.tokens_for_len.lock().unwrap())(raw.len());
    let model = state.reply_model.lock().unwrap().clone();
    axum::Json(json!({
        "model": model,
        "answers": answers,
        "usage": {"input_tokens": tokens, "output_tokens": questions.len()},
    }))
    .into_response()
}
