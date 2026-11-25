//! Claude Code transcript records, mapped to content-free Traction Events.
//!
//! One transcript line (a JSON record of
//! `<claude config dir>/projects/<slug>/<session-id>.jsonl`) maps to zero or
//! more events, all `harness_id: claude-code`, `attribution_source:
//! harness_log`, `network_class: cloud_api`. A line is parsed into an
//! allowlisted struct: tool input and output, assistant text, thinking and
//! attachments are skipped by the parser, and prompt text is reduced to
//! [`TextMetrics`] inside the deserializer, so no raw text outlives parsing.
//!
//! Mapping, decided from the real fixtures in `tests/data/claude-code/`
//! (Claude Code 2.1.332):
//! - `user` record with prompt text (not a tool result, not `isMeta`, not a
//!   slash-command wrapper) → `PROMPT_SUBMIT`;
//! - `tool_use` block of an `assistant` record → `TOOL_PRE` (`tool_name`);
//! - `tool_result` block of a `user` record → `TOOL_POST`, or `TOOL_FAIL`
//!   when `is_error` is true (`tool_name` from the matching `tool_use`);
//! - `assistant` record with `isApiErrorMessage: true` → `STOP_FAILURE`;
//! - nothing maps to `TURN_END`: the only per-turn record,
//!   `system`/`stop_hook_summary`, exists only when a `Stop` hook is
//!   configured, so a tailed turn ends at the next prompt or by the
//!   stale-turn timeout;
//! - every other record (`system`, including the `informational`
//!   `blocked_by_hook` record a `/stop-track` block writes, `attachment`,
//!   `api-request*`, bookkeeping) and every sidechain record → nothing.
//!
//! The first event of a session is preceded by a synthesized
//! `SESSION_START` ([`session_start`]); the tailer decides when.
//!
//! `timestamp_ms` is the record's ISO 8601 `timestamp`. `event_id` is
//! [`derived_event_id`] of `("claude-code", session_id, record uuid, block
//! index, event type)`, so a reread line maps to the same IDs.

use std::collections::HashMap;
use std::fmt;
use std::marker::PhantomData;

use serde::de::value::MapAccessDeserializer;
use serde::de::{self, IgnoredAny, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use traction_core::{
    AttributionSource, EventType, HarnessId, MetricsOnly, NetworkClass, SCHEMA_VERSION,
    TractionEvent,
};

use super::hook::capped_identifier;
use crate::content_metrics::{self, TextMetrics};
use crate::event_id::derived_event_id;

/// `tool_use` IDs remembered per transcript (for the `tool_name` of their
/// results) before the table is cleared: bounded memory.
const MAX_OPEN_TOOL_USES: usize = 1024;

/// Text that starts a slash-command wrapper record, not a typed prompt.
const COMMAND_MARKERS: [&str; 3] = ["<command-name>", "<command-message>", "<local-command-"];

/// Why a transcript line was skipped. Logged by code only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineError {
    /// Not JSON (or truncated JSON).
    InvalidJson,
    /// JSON, but a field the mapping reads has the wrong type.
    UnexpectedShape,
    /// A record that would map has a missing or unreadable `timestamp`.
    BadTimestamp,
}

/// The `tool_name` of each open `tool_use` in one transcript, so its
/// `tool_result` (which carries only the ID) gets the same name.
#[derive(Debug, Default)]
pub struct ToolNames(HashMap<String, String>);

impl ToolNames {
    fn open(&mut self, id: String, name: String) {
        if self.0.len() >= MAX_OPEN_TOOL_USES {
            self.0.clear();
        }
        self.0.insert(id, name);
    }

    fn close(&mut self, id: Option<&str>) -> Option<String> {
        self.0.remove(id?)
    }
}

/// The events one record maps to (never empty), with the record's `uuid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mapped {
    pub record_uuid: String,
    pub events: Vec<TractionEvent>,
}

/// Maps one transcript line (without its `\n`). `Ok(None)` for a record
/// that maps to nothing.
pub fn map_line(line: &[u8], tools: &mut ToolNames) -> Result<Option<Mapped>, LineError> {
    let record: Record = serde_json::from_slice(line).map_err(|error| {
        if error.is_data() {
            LineError::UnexpectedShape
        } else {
            LineError::InvalidJson
        }
    })?;
    map_record(record, tools)
}

/// The `SESSION_START` synthesized before `first`, the first event of a
/// session, mapped from the record `record_uuid`.
pub fn session_start(first: &TractionEvent, record_uuid: &str) -> TractionEvent {
    let session_id = first.session_id.clone().unwrap_or_default();
    event(
        &session_id,
        record_uuid,
        0,
        EventType::SessionStart,
        first.timestamp_ms,
        first.cwd.clone(),
        MetricsOnly::default(),
    )
}

/// The only record fields the mapping reads. Everything else is skipped.
#[derive(Deserialize)]
struct Record {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    #[serde(rename = "sessionId", default)]
    session_id: Option<String>,
    #[serde(default)]
    uuid: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(rename = "isMeta", default)]
    is_meta: Option<bool>,
    #[serde(rename = "isSidechain", default)]
    is_sidechain: Option<bool>,
    #[serde(rename = "isApiErrorMessage", default)]
    is_api_error: Option<bool>,
    /// Any non-object `message` (other record types) is skipped.
    #[serde(default)]
    message: Lenient<Message>,
}

#[derive(Deserialize)]
struct Message {
    #[serde(default)]
    content: Content,
}

/// A message's `content`: prompt text (measured, then dropped) or blocks.
#[derive(Default)]
enum Content {
    #[default]
    None,
    Text(PromptText),
    Blocks(Vec<Lenient<Block>>),
}

/// What prompt text is reduced to.
#[derive(Clone, Copy)]
struct PromptText {
    metrics: TextMetrics,
    /// Starts with a slash-command wrapper tag.
    is_command: bool,
}

impl PromptText {
    fn of(text: &str) -> Self {
        Self {
            metrics: content_metrics::measure(text),
            is_command: COMMAND_MARKERS.iter().any(|m| text.starts_with(m)),
        }
    }
}

#[derive(Deserialize)]
struct Block {
    #[serde(rename = "type", default)]
    kind: Option<String>,
    /// `tool_use`
    #[serde(default)]
    id: Option<String>,
    /// `tool_use`
    #[serde(default)]
    name: Option<String>,
    /// `tool_result`
    #[serde(default)]
    tool_use_id: Option<String>,
    /// `tool_result`
    #[serde(default)]
    is_error: Option<bool>,
    /// `text`: measured while parsing, never kept.
    #[serde(default, deserialize_with = "measured")]
    text: Option<PromptText>,
}

fn measured<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<PromptText>, D::Error> {
    let text: Option<std::borrow::Cow<'de, str>> = Option::deserialize(deserializer)?;
    Ok(text.map(|text| PromptText::of(&text)))
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ContentVisitor;
        impl<'de> Visitor<'de> for ContentVisitor {
            type Value = Content;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("message content")
            }
            fn visit_str<E: de::Error>(self, text: &str) -> Result<Content, E> {
                Ok(Content::Text(PromptText::of(text)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Content, A::Error> {
                let mut blocks = Vec::new();
                while let Some(block) = seq.next_element()? {
                    blocks.push(block);
                }
                Ok(Content::Blocks(blocks))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Content, A::Error> {
                while map.next_entry::<IgnoredAny, IgnoredAny>()?.is_some() {}
                Ok(Content::None)
            }
            fn visit_unit<E: de::Error>(self) -> Result<Content, E> {
                Ok(Content::None)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Content, E> {
                Ok(Content::None)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Content, E> {
                Ok(Content::None)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Content, E> {
                Ok(Content::None)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Content, E> {
                Ok(Content::None)
            }
        }
        deserializer.deserialize_any(ContentVisitor)
    }
}

/// `T` when the value is a JSON object, else skipped (`None`), so record
/// types the mapping ignores never fail on their own shapes.
struct Lenient<T>(Option<T>);

impl<T> Default for Lenient<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Lenient<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LenientVisitor<T>(PhantomData<T>);
        impl<'de, T: Deserialize<'de>> Visitor<'de> for LenientVisitor<T> {
            type Value = Lenient<T>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("any JSON value")
            }
            fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<Lenient<T>, A::Error> {
                T::deserialize(MapAccessDeserializer::new(map)).map(|value| Lenient(Some(value)))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Lenient<T>, A::Error> {
                while seq.next_element::<IgnoredAny>()?.is_some() {}
                Ok(Lenient(None))
            }
            fn visit_str<E: de::Error>(self, _: &str) -> Result<Lenient<T>, E> {
                Ok(Lenient(None))
            }
            fn visit_unit<E: de::Error>(self) -> Result<Lenient<T>, E> {
                Ok(Lenient(None))
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> Result<Lenient<T>, E> {
                Ok(Lenient(None))
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> Result<Lenient<T>, E> {
                Ok(Lenient(None))
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> Result<Lenient<T>, E> {
                Ok(Lenient(None))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> Result<Lenient<T>, E> {
                Ok(Lenient(None))
            }
        }
        deserializer.deserialize_any(LenientVisitor(PhantomData))
    }
}

fn map_record(record: Record, tools: &mut ToolNames) -> Result<Option<Mapped>, LineError> {
    let is_user = match record.kind.as_deref() {
        Some("user") => true,
        Some("assistant") => false,
        _ => return Ok(None),
    };
    if record.is_sidechain == Some(true) {
        return Ok(None);
    }
    let (Some(session_id), Some(uuid)) = (record.session_id, record.uuid) else {
        return Ok(None);
    };
    let content = record.message.0.map(|m| m.content).unwrap_or_default();
    // (block index, event type, metrics) before the timestamp is needed.
    let mut planned: Vec<(usize, EventType, MetricsOnly)> = Vec::new();
    if is_user {
        if record.is_meta == Some(true) {
            return Ok(None);
        }
        match content {
            Content::None => {}
            Content::Text(text) => {
                if !text.is_command {
                    planned.push((0, EventType::PromptSubmit, prompt_metrics(&[text])));
                }
            }
            Content::Blocks(blocks) => {
                let blocks: Vec<Block> = blocks.into_iter().filter_map(|b| b.0).collect();
                let mut texts = Vec::new();
                let mut has_image = false;
                for (index, block) in blocks.iter().enumerate() {
                    match block.kind.as_deref() {
                        Some("tool_result") => {
                            let event_type = if block.is_error == Some(true) {
                                EventType::ToolFail
                            } else {
                                EventType::ToolPost
                            };
                            let metrics = MetricsOnly {
                                tool_name: tools.close(block.tool_use_id.as_deref()),
                                ..MetricsOnly::default()
                            };
                            planned.push((index, event_type, metrics));
                        }
                        Some("text") => texts.extend(block.text),
                        Some("image") => has_image = true,
                        _ => {}
                    }
                }
                let typed = texts.iter().any(|t| !t.is_command);
                if planned.is_empty() && (typed || (has_image && texts.is_empty())) {
                    planned.push((0, EventType::PromptSubmit, prompt_metrics(&texts)));
                }
            }
        }
    } else if record.is_api_error == Some(true) {
        planned.push((0, EventType::StopFailure, MetricsOnly::default()));
    } else if let Content::Blocks(blocks) = content {
        for (index, block) in blocks.into_iter().enumerate() {
            let Some(block) = block.0 else { continue };
            if block.kind.as_deref() != Some("tool_use") {
                continue;
            }
            let name = block.name.and_then(capped_identifier);
            if let (Some(id), Some(name)) = (block.id, name.clone()) {
                tools.open(id, name);
            }
            let metrics = MetricsOnly {
                tool_name: name,
                ..MetricsOnly::default()
            };
            planned.push((index, EventType::ToolPre, metrics));
        }
    }
    if planned.is_empty() {
        return Ok(None);
    }
    let timestamp_ms = record
        .timestamp
        .as_deref()
        .and_then(parse_timestamp_ms)
        .ok_or(LineError::BadTimestamp)?;
    let events: Vec<TractionEvent> = planned
        .into_iter()
        .map(|(index, event_type, metrics)| {
            event(
                &session_id,
                &uuid,
                index,
                event_type,
                timestamp_ms,
                record.cwd.clone(),
                metrics,
            )
        })
        .filter(|event| event.validate().is_ok())
        .collect();
    Ok((!events.is_empty()).then_some(Mapped {
        record_uuid: uuid,
        events,
    }))
}

/// `PROMPT_SUBMIT` metrics of one prompt made of `texts`.
fn prompt_metrics(texts: &[PromptText]) -> MetricsOnly {
    let len = texts
        .iter()
        .map(|t| t.metrics.prompt_char_len)
        .fold(0u64, u64::saturating_add);
    MetricsOnly {
        prompt_char_len: Some(len),
        has_stacktrace: Some(texts.iter().any(|t| t.metrics.has_stacktrace)),
        ..MetricsOnly::default()
    }
}

fn event(
    session_id: &str,
    record_uuid: &str,
    block_index: usize,
    event_type: EventType,
    timestamp_ms: u64,
    cwd: Option<String>,
    metrics_only: MetricsOnly,
) -> TractionEvent {
    let harness = HarnessId::ClaudeCode;
    let index = block_index.to_string();
    let event_id = derived_event_id(&[
        harness.as_str().as_bytes(),
        session_id.as_bytes(),
        record_uuid.as_bytes(),
        index.as_bytes(),
        event_type.as_str().as_bytes(),
    ]);
    TractionEvent {
        schema_version: SCHEMA_VERSION,
        event_id,
        timestamp_ms,
        harness_id: Some(harness),
        session_id: Some(session_id.to_owned()),
        parent_orchestrator_id: None,
        event_type,
        end_reason: None,
        cwd,
        project_id: None,
        model_id: None,
        network_class: NetworkClass::CloudApi,
        attribution_source: AttributionSource::HarnessLog,
        metrics_only,
    }
}

/// Unix milliseconds of an RFC 3339 timestamp
/// (`2026-11-25T16:36:30.382Z`, or with a `±HH:MM` offset). `None` for
/// anything else or a time before 1970.
pub fn parse_timestamp_ms(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let part = bytes.get(range)?;
        if part.is_empty() || !part.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(part).ok()?.parse().ok()
    };
    if bytes.len() < 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't' | b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let (year, month, day) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (hour, minute, second) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=days_in_month(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = &text[19..];
    let mut millis = 0i64;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let mut scaled = 0i64;
        for (i, b) in fraction.bytes().take(3).enumerate().take(digits) {
            scaled += i64::from(b - b'0') * [100, 10, 1][i];
        }
        millis = scaled;
        rest = &fraction[digits..];
    }
    let offset_minutes = match rest.as_bytes() {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), h1, h2, b':', m1, m2] => {
            let digits = [*h1, *h2, *m1, *m2];
            if !digits.iter().all(u8::is_ascii_digit) {
                return None;
            }
            let value = |a: u8, b: u8| i64::from(a - b'0') * 10 + i64::from(b - b'0');
            let minutes = value(*h1, *h2) * 60 + value(*m1, *m2);
            if *sign == b'+' { minutes } else { -minutes }
        }
        _ => return None,
    };
    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    u64::try_from(seconds * 1_000 + millis).ok()
}

fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        2 if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days since 1970-01-01 of a proleptic Gregorian date (H. Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let yoe = year - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_timestamps_parse_to_unix_millis() {
        assert_eq!(
            parse_timestamp_ms("2026-11-25T16:36:30.382Z"),
            Some(1_795_624_590_382)
        );
        assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp_ms("2000-02-29T12:00:00.5Z"), Some(951_825_600_500));
        assert_eq!(
            parse_timestamp_ms("2026-11-25T17:36:30.382123+01:00"),
            Some(1_795_624_590_382)
        );
        for bad in [
            "",
            "2026-11-25",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-11-25T16:36:30",
            "2026-11-25T16:36:30.Z",
            "1969-12-31T23:59:59Z",
            "2026-11-25T16:36:30.382Zjunk",
        ] {
            assert_eq!(parse_timestamp_ms(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn every_captured_transcript_line_parses() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/claude-code");
        let mut lines = 0;
        for scenario in std::fs::read_dir(root).expect("fixtures") {
            let path = scenario.expect("entry").path().join("transcript.jsonl");
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let mut tools = ToolNames::default();
            for line in text.lines() {
                lines += 1;
                assert!(
                    map_line(line.as_bytes(), &mut tools).is_ok(),
                    "{}: {line:.120}",
                    path.display()
                );
            }
        }
        assert!(lines > 100);
    }
}
