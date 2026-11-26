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
//! - `user` record with prompt text (not a tool result, not a slash-command
//!   wrapper, not a task notification) → `PROMPT_SUBMIT`. With no `origin`,
//!   text starting like injected output ([`INJECTED_PREFIXES`]) is not a
//!   prompt either;
//! - `user` record marked `isMeta`, `isCompactSummary`,
//!   `isVisibleInTranscriptOnly` or `isVirtual`, or whose `origin.kind` is
//!   `task-notification` → nothing;
//! - `user` record that is exactly the wrapper of `/stop-track` or
//!   `/resume-track` → no event, but a Session Pause command
//!   ([`Line::Pause`]): Claude Code writes one only when no hook blocked
//!   the command (daemon down, no token);
//! - `tool_use` block of an `assistant` record → `TOOL_PRE` (`tool_name`);
//! - `tool_result` block of a `user` record → `TOOL_POST`, or `TOOL_FAIL`
//!   when `is_error` is true (`tool_name` from the matching `tool_use`).
//!   A record with `toolDenialKind` (the call was refused or interrupted)
//!   maps to nothing, as the hooks report no result for it either;
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

use super::hook::{capped_identifier, is_task_notification};
use super::pause::Command;
use crate::content_metrics::{self, TextMetrics};
use crate::event_id::derived_event_id;
use crate::router::Pause;
use crate::symbol_index::SymbolIndexes;

/// `tool_use` IDs remembered per transcript (for the `tool_name` of their
/// results) before the table is cleared: bounded memory.
const MAX_OPEN_TOOL_USES: usize = 1024;

/// Text that starts a slash-command wrapper record, not a typed prompt.
const COMMAND_MARKERS: [&str; 3] = ["<command-name>", "<command-message>", "<local-command-"];

/// Text Claude Code injects as a `user` record. Checked only on records
/// without an `origin`, which cannot be told apart otherwise.
const INJECTED_PREFIXES: [&str; 6] = [
    "This session is being continued",
    "<local-command-stdout>",
    "<local-command-stderr>",
    "<bash-stdout>",
    "<bash-stderr>",
    "Caveat:",
];

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

/// What one record asks of the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line {
    Events(Mapped),
    /// A recorded `/stop-track` or `/resume-track`.
    Pause(Pause),
}

/// Maps one transcript line (without its `\n`). `Ok(None)` for a record
/// that maps to nothing.
pub fn map_line(
    line: &[u8],
    tools: &mut ToolNames,
    symbols: &SymbolIndexes,
) -> Result<Option<Line>, LineError> {
    let record: Record = serde_json::from_slice(line).map_err(|error| {
        if error.is_data() {
            LineError::UnexpectedShape
        } else {
            LineError::InvalidJson
        }
    })?;
    map_record(record, tools, symbols)
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
    #[serde(rename = "isCompactSummary", default)]
    is_compact_summary: Option<bool>,
    #[serde(rename = "isVisibleInTranscriptOnly", default)]
    is_transcript_only: Option<bool>,
    #[serde(rename = "isVirtual", default)]
    is_virtual: Option<bool>,
    /// Who produced a `user` record; absent on many.
    #[serde(default)]
    origin: Option<Lenient<Origin>>,
    /// Set on the `user` record of a refused or interrupted tool call. Only
    /// its presence matters.
    #[serde(rename = "toolDenialKind", default)]
    tool_denial: Option<IgnoredAny>,
    /// Any non-object `message` (other record types) is skipped.
    #[serde(default)]
    message: Lenient<Message>,
}

#[derive(Deserialize)]
struct Origin {
    #[serde(default)]
    kind: Option<String>,
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
#[derive(Clone)]
struct PromptText {
    metrics: TextMetrics,
    /// Starts with a slash-command wrapper tag.
    is_command: bool,
    /// Starts like one of [`INJECTED_PREFIXES`].
    looks_injected: bool,
    is_task_notification: bool,
    /// The Session Pause command this text is the wrapper of.
    pause: Option<Command>,
}

impl PromptText {
    fn of(text: &str) -> Self {
        let trimmed = text.trim_start();
        Self {
            metrics: content_metrics::measure(text),
            is_command: COMMAND_MARKERS.iter().any(|m| text.starts_with(m)),
            looks_injected: INJECTED_PREFIXES.iter().any(|p| trimmed.starts_with(p)),
            is_task_notification: is_task_notification(text),
            pause: Command::matching_wrapper(text),
        }
    }

    /// Whether the user typed this as a prompt.
    fn is_typed(&self, has_origin: bool) -> bool {
        !(self.is_command || self.is_task_notification || (!has_origin && self.looks_injected))
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

fn map_record(
    record: Record,
    tools: &mut ToolNames,
    symbols: &SymbolIndexes,
) -> Result<Option<Line>, LineError> {
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
    let mut pause = None;
    if is_user {
        let synthetic = [
            record.is_meta,
            record.is_compact_summary,
            record.is_transcript_only,
            record.is_virtual,
        ]
        .contains(&Some(true));
        let has_origin = record.origin.is_some();
        let from_task = record
            .origin
            .and_then(|origin| origin.0?.kind)
            .is_some_and(|kind| kind == "task-notification");
        if synthetic || from_task {
            return Ok(None);
        }
        match content {
            Content::None => {}
            Content::Text(text) => {
                if let Some(command) = text.pause {
                    pause = Some(command);
                } else if text.is_typed(has_origin) {
                    planned.push((
                        0,
                        EventType::PromptSubmit,
                        prompt_metrics(&[text], record.cwd.as_deref(), symbols),
                    ));
                }
            }
            Content::Blocks(blocks) => {
                let blocks: Vec<Block> = blocks.into_iter().filter_map(|b| b.0).collect();
                let denied = record.tool_denial.is_some();
                let mut texts = Vec::new();
                let mut has_image = false;
                let mut has_result = false;
                for (index, block) in blocks.iter().enumerate() {
                    match block.kind.as_deref() {
                        Some("tool_result") => {
                            has_result = true;
                            let tool_name = tools.close(block.tool_use_id.as_deref());
                            if denied {
                                continue;
                            }
                            let event_type = if block.is_error == Some(true) {
                                EventType::ToolFail
                            } else {
                                EventType::ToolPost
                            };
                            let metrics = MetricsOnly {
                                tool_name,
                                ..MetricsOnly::default()
                            };
                            planned.push((index, event_type, metrics));
                        }
                        Some("text") => texts.extend(block.text.clone()),
                        Some("image") => has_image = true,
                        _ => {}
                    }
                }
                // Text beside a tool result (such as an interrupt notice)
                // is never a prompt.
                if !has_result && !denied {
                    pause = texts.iter().find_map(|t| t.pause);
                    let typed = texts.iter().any(|t| t.is_typed(has_origin));
                    if typed || (has_image && texts.is_empty()) {
                        planned.push((
                            0,
                            EventType::PromptSubmit,
                            prompt_metrics(&texts, record.cwd.as_deref(), symbols),
                        ));
                    }
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
    if planned.is_empty() && pause.is_none() {
        return Ok(None);
    }
    let timestamp_ms = record
        .timestamp
        .as_deref()
        .and_then(parse_timestamp_ms)
        .ok_or(LineError::BadTimestamp)?;
    if let Some(command) = pause {
        // Validated as the prompt's event would be, so a pause is keyed by
        // a safe session ID; the event itself is dropped.
        let probe = event(
            &session_id,
            &uuid,
            0,
            EventType::PromptSubmit,
            timestamp_ms,
            None,
            MetricsOnly::default(),
        );
        return Ok(probe.validate().is_ok().then_some(Line::Pause(Pause {
            action: command.action,
            harness: HarnessId::ClaudeCode,
            session_id,
            at_ms: timestamp_ms,
        })));
    }
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
    Ok((!events.is_empty()).then_some(Line::Events(Mapped {
        record_uuid: uuid,
        events,
    })))
}

/// `PROMPT_SUBMIT` metrics of one prompt made of `texts`.
///
/// `repo_symbol_hits` counts the distinct words of every text found in the
/// file-name index of the Project holding `cwd`; the words are dropped here.
fn prompt_metrics(texts: &[PromptText], cwd: Option<&str>, symbols: &SymbolIndexes) -> MetricsOnly {
    let hits = cwd.and_then(|cwd| {
        let mut total = 0u64;
        let mut known = false;
        for t in texts {
            if let Some(n) = symbols.hits_at(Some(cwd), &t.metrics.candidates) {
                known = true;
                total = total.saturating_add(n);
            }
        }
        known.then_some(total)
    });
    let len = texts
        .iter()
        .map(|t| t.metrics.prompt_char_len)
        .fold(0u64, u64::saturating_add);
    MetricsOnly {
        prompt_char_len: Some(len),
        repo_symbol_hits: hits,
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
    use serde_json::{Value, json};

    use super::*;
    use crate::claude_code::pause::PauseAction;

    #[test]
    fn rfc3339_timestamps_parse_to_unix_millis() {
        assert_eq!(
            parse_timestamp_ms("2026-11-25T16:36:30.382Z"),
            Some(1_795_624_590_382)
        );
        assert_eq!(parse_timestamp_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_timestamp_ms("2000-02-29T12:00:00.5Z"),
            Some(951_825_600_500)
        );
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

    /// A synthetic `user` record with `content`, plus `extra` fields.
    fn user_record(content: Value, extra: Value) -> String {
        let mut record = json!({
            "type": "user",
            "sessionId": "s",
            "uuid": "00000000-0000-4000-8000-000000000001",
            "timestamp": "2026-11-25T16:36:30.382Z",
            "cwd": "/code/app",
            "message": { "role": "user", "content": content },
        });
        for (key, value) in extra.as_object().expect("object") {
            record[key] = value.clone();
        }
        record.to_string()
    }

    /// The event types `line` maps to.
    fn types_of(line: &str, tools: &mut ToolNames) -> Vec<EventType> {
        match map_line(line.as_bytes(), tools, &SymbolIndexes::default()).expect("a readable line")
        {
            Some(Line::Events(mapped)) => mapped.events.iter().map(|e| e.event_type).collect(),
            Some(Line::Pause(pause)) => panic!("unexpected pause: {pause:?}"),
            None => vec![],
        }
    }

    #[test]
    fn a_transcript_prompt_counts_the_repo_files_it_names() {
        let symbols = SymbolIndexes::default();
        let root = std::path::Path::new("/code/app");
        symbols.note(root, &root.join("src/quasarfoxtrot_widget.rs"));
        let hits_of = |text: &str| {
            let line = user_record(json!(text), json!({}));
            match map_line(line.as_bytes(), &mut ToolNames::default(), &symbols) {
                Ok(Some(Line::Events(mapped))) => mapped.events[0].metrics_only.repo_symbol_hits,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(hits_of("the panic is in quasarfoxtrot_widget.rs"), Some(1));
        assert_eq!(hits_of("continue"), Some(0));
        // A prompt from outside every indexed Project is unknown, not zero.
        let line = user_record(json!("fix it"), json!({"cwd": "/elsewhere"}));
        let out = map_line(line.as_bytes(), &mut ToolNames::default(), &symbols);
        let Ok(Some(Line::Events(mapped))) = out else {
            panic!("{out:?}")
        };
        assert_eq!(mapped.events[0].metrics_only.repo_symbol_hits, None);
    }

    // No events means no turn window: windows open only on events.
    #[test]
    fn compact_summaries_and_other_synthetic_user_records_map_to_nothing() {
        let summary = "This session is being continued from a previous conversation \
                       that ran out of context. Summary: fix the build.";
        let flags = [
            json!({"isCompactSummary": true, "isVisibleInTranscriptOnly": true}),
            json!({"isCompactSummary": true}),
            json!({"isVisibleInTranscriptOnly": true}),
            json!({"isVirtual": true}),
        ];
        for flag in flags {
            for content in [json!(summary), json!([{"type": "text", "text": "plain"}])] {
                let line = user_record(content, flag.clone());
                assert_eq!(types_of(&line, &mut ToolNames::default()), vec![], "{line}");
            }
        }
    }

    #[test]
    fn injected_text_without_an_origin_is_not_a_prompt() {
        for prefix in [
            "This session is being continued",
            "<local-command-stdout>",
            "<local-command-stderr>",
            "<bash-stdout>",
            "<bash-stderr>",
            "Caveat:",
        ] {
            let text = format!("{prefix} and more");
            for content in [json!(text), json!([{"type": "text", "text": text}])] {
                let line = user_record(content, json!({}));
                assert_eq!(types_of(&line, &mut ToolNames::default()), vec![], "{line}");
            }
        }
        // With a human origin the same words were typed.
        let line = user_record(json!("Caveat: typed"), json!({"origin": {"kind": "human"}}));
        assert_eq!(
            types_of(&line, &mut ToolNames::default()),
            vec![EventType::PromptSubmit]
        );
    }

    #[test]
    fn task_notifications_are_not_prompts() {
        for line in [
            user_record(
                json!("a background task finished"),
                json!({"origin": {"kind": "task-notification"}}),
            ),
            user_record(
                json!("<task-notification>\n<task-id>a</task-id>"),
                json!({}),
            ),
            user_record(
                json!([{"type": "text", "text": "<task-notification>x"}]),
                json!({}),
            ),
        ] {
            assert_eq!(types_of(&line, &mut ToolNames::default()), vec![], "{line}");
        }
    }

    #[test]
    fn a_denied_tool_result_is_neither_a_failure_nor_a_prompt() {
        for kind in [
            "user-rejected",
            "permission-rule",
            "automode-unavailable",
            "interrupted",
        ] {
            let mut tools = ToolNames::default();
            tools.open("toolu_1".to_owned(), "Bash".to_owned());
            let line = user_record(
                json!([
                    {"type": "tool_result", "tool_use_id": "toolu_1", "is_error": true,
                     "content": "The user doesn't want to proceed with this tool use."},
                    {"type": "text", "text": "[Request interrupted by user for tool use]"},
                ]),
                json!({"toolDenialKind": kind}),
            );
            assert_eq!(types_of(&line, &mut tools), vec![], "{kind}");
            assert_eq!(tools.close(Some("toolu_1")), None, "the call is closed");
        }
    }

    #[test]
    fn only_an_exact_command_wrapper_is_a_pause() {
        let pause_of = |text: &str| {
            let line = user_record(json!(text), json!({}));
            match map_line(
                line.as_bytes(),
                &mut ToolNames::default(),
                &SymbolIndexes::default(),
            )
            .expect("readable")
            {
                Some(Line::Pause(pause)) => Some((pause.action, pause.session_id, pause.at_ms)),
                Some(Line::Events(mapped)) => panic!("stored as events: {mapped:?}"),
                None => None,
            }
        };
        let at = |action| Some((action, "s".to_owned(), 1_795_624_590_382));
        for text in [
            "<command-name>/stop-track</command-name>",
            "<command-message>stop-track</command-message>\n<command-name>/stop-track</command-name>",
            "<command-name>/stop-track</command-name>\n            <command-message>stop-track</command-message>\n            <command-args></command-args>",
            "  <command-name> /stop-track </command-name>\n",
        ] {
            assert_eq!(pause_of(text), at(PauseAction::Stop), "{text:?}");
        }
        assert_eq!(
            pause_of(
                "<command-message>resume-track</command-message>\n<command-name>/resume-track</command-name>"
            ),
            at(PauseAction::Resume)
        );
        for text in [
            "<command-name>/stop-tracking</command-name>",
            "<command-name>stop-track</command-name>",
            "<command-name>/clear</command-name>",
            "<command-name>/stop-track</command-name><command-args>please</command-args>",
            "<command-name>/stop-track</command-name> and then some",
            "<command-message>/stop-track</command-message>",
        ] {
            assert_eq!(pause_of(text), None, "{text:?}");
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
                    map_line(line.as_bytes(), &mut tools, &SymbolIndexes::default()).is_ok(),
                    "{}: {line:.120}",
                    path.display()
                );
            }
        }
        assert!(lines > 100);
    }
}
