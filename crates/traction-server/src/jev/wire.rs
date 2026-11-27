//! `POST /v1/systemone` request and response types, written from TypeSafe's
//! published OpenAPI document (`https://api.typesafe.ai/openapi.json`,
//! v0.2.0, read 2026-11-27). Only the fields `JevEngine` uses are modelled;
//! unknown response fields are ignored so a newer server still parses.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `SystemOneRequest`: `model`, `state` and `questions` are required;
/// `assets` is optional and, when present, has at least one entry.
#[derive(Debug, Clone, Serialize)]
pub struct SystemOneRequest {
    pub model: String,
    pub state: Value,
    pub questions: BTreeMap<String, Question>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub assets: BTreeMap<String, Asset>,
}

/// `Question`, discriminated by `type`. `box` is never asked.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// `NoulQuestion`: `criteria` is `NoulCriteria {true, false}`.
    Noul {
        instructions: String,
        criteria: NoulCriteria,
    },
    /// `ChoiceQuestion`: `criteria` maps 1-255 choice names to descriptions.
    Choice {
        instructions: String,
        criteria: BTreeMap<String, String>,
    },
    /// `ScoreQuestion`: `criteria` is 1-10 ordered level descriptions.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize)]
pub struct NoulCriteria {
    #[serde(rename = "true")]
    pub yes: String,
    #[serde(rename = "false")]
    pub no: String,
}

/// `Asset`: an image whose bytes replace its placeholder. Requests with
/// assets must send `TypeSafe-Danger-Zone-Images: true`.
#[derive(Debug, Clone, Serialize)]
pub struct Asset {
    #[serde(rename = "type")]
    pub kind: &'static str,
    pub format: &'static str,
    pub data_base64: String,
}

/// `SystemOneResponse`.
#[derive(Debug, Clone, Deserialize)]
pub struct SystemOneResponse {
    /// The versioned model that answered (logged with the decision).
    pub model: String,
    pub answers: BTreeMap<String, Answer>,
    pub usage: Usage,
}

/// `Answer`, discriminated by `type`.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    /// `NoulAnswer.noul`: probability of yes, 0 to 1.
    Noul { noul: f64 },
    /// `ChoiceAnswer`: `probabilities` keyed by choice name.
    Choice {
        choice: String,
        probabilities: BTreeMap<String, f64>,
    },
    /// `ScoreAnswer`: `probabilities` keyed by level (`"0"`, `"1"`, ...).
    /// `score` (the expected level) is deliberately not read.
    Score {
        probabilities: BTreeMap<String, f64>,
    },
    /// `BoxAnswer`; never asked, so receiving one is a malformed reply.
    Box {
        #[serde(rename = "box")]
        _box: Value,
    },
}

/// `Usage`: billable input tokens (output tokens are free).
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}
