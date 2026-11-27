//! Timeline Marks (spec S2, Q20/Q25, `docs/adr/0005`): per Project and
//! minute, content-free counts the Audit Strip can show next to the Work
//! Buckets. They are **unsigned**: not part of the hash chain, so they are
//! context, never proof. A mark carries counts and the `harness_id` set only;
//! no `cwd`, `model_id`, session ID, path or text can be expressed in it.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::sync::IngestError;
use crate::validate::is_project_id;
use crate::{EventType, HarnessId, MINUTE_MS, TractionEvent};

/// The largest count a mark may claim: far above any real minute, small
/// enough to refuse nonsense.
pub const MAX_MARK_COUNT: u32 = 100_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimelineMark {
    pub project_id: String,
    pub minute_start_ms: u64,
    /// `FILE_MUTATION`s outside every Harness window.
    pub mutations_manual: u32,
    /// `FILE_MUTATION`s attributed to a Harness.
    pub mutations_harness: u32,
    /// `PROMPT_SUBMIT`s.
    pub turns_started: u32,
    /// `TOOL_FAIL`s.
    pub tool_failures: u32,
    /// Harnesses that produced any event in the minute, sorted by wire name.
    pub harnesses: Vec<HarnessId>,
}

/// Why a mark is refused: reuses the value-free ingest codes.
pub fn validate_timeline_mark(mark: &TimelineMark) -> Result<(), IngestError> {
    if !is_project_id(&mark.project_id) {
        return Err(IngestError::InvalidProjectId);
    }
    if !mark.minute_start_ms.is_multiple_of(MINUTE_MS) {
        return Err(IngestError::MisalignedMinute);
    }
    let counts = [
        mark.mutations_manual,
        mark.mutations_harness,
        mark.turns_started,
        mark.tool_failures,
    ];
    let mut seen = mark.harnesses.clone();
    seen.sort_by_key(|h| h.as_str());
    seen.dedup();
    if counts.iter().any(|c| *c > MAX_MARK_COUNT) || seen.len() != mark.harnesses.len() {
        return Err(IngestError::InvalidTimelineMark);
    }
    Ok(())
}

#[derive(Default)]
struct Counts {
    manual: u32,
    harness: u32,
    turns: u32,
    failures: u32,
    harnesses: Vec<HarnessId>,
}

/// Computes the marks of the minutes in `[from_ms, to_ms)` from stored
/// events: one mark per Project and minute that has any event. Events
/// without a Project are skipped. Only counts and harness IDs are read; no
/// other field of an event can reach a mark. Sorted by `(minute, project)`.
pub fn timeline_marks(events: &[TractionEvent], from_ms: u64, to_ms: u64) -> Vec<TimelineMark> {
    let mut minutes: BTreeMap<(u64, &str), Counts> = BTreeMap::new();
    for event in events {
        let Some(project_id) = event.project_id.as_deref() else {
            continue;
        };
        if event.timestamp_ms < from_ms || event.timestamp_ms >= to_ms {
            continue;
        }
        let minute = event.timestamp_ms / MINUTE_MS * MINUTE_MS;
        let counts = minutes.entry((minute, project_id)).or_default();
        match event.event_type {
            EventType::FileMutation if event.harness_id.is_some() => counts.harness += 1,
            EventType::FileMutation => counts.manual += 1,
            EventType::PromptSubmit => counts.turns += 1,
            EventType::ToolFail => counts.failures += 1,
            _ => {}
        }
        if let Some(harness) = event.harness_id
            && !counts.harnesses.contains(&harness)
        {
            counts.harnesses.push(harness);
        }
    }
    minutes
        .into_iter()
        .map(|((minute_start_ms, project_id), mut counts)| {
            counts.harnesses.sort_by_key(|h| h.as_str());
            TimelineMark {
                project_id: project_id.to_owned(),
                minute_start_ms,
                mutations_manual: counts.manual,
                mutations_harness: counts.harness,
                turns_started: counts.turns,
                tool_failures: counts.failures,
                harnesses: counts.harnesses,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AttributionSource, MetricsOnly, NetworkClass};

    const T0: u64 = 1_800_000_000_000 / 60_000 * 60_000;
    fn project() -> String {
        format!("dir:{}", "a".repeat(64))
    }

    fn event(
        n: u32,
        offset_ms: u64,
        kind: EventType,
        harness: Option<HarnessId>,
        project_id: Option<&str>,
    ) -> TractionEvent {
        TractionEvent {
            schema_version: 1,
            event_id: format!("e{n}"),
            timestamp_ms: T0 + offset_ms,
            harness_id: harness,
            session_id: Some("SECRET-SESSION".into()),
            parent_orchestrator_id: None,
            event_type: kind,
            end_reason: None,
            cwd: Some("/home/secret/cwd".into()),
            project_id: project_id.map(str::to_owned),
            model_id: Some("secret-model".into()),
            network_class: NetworkClass::CloudApi,
            attribution_source: AttributionSource::HarnessHook,
            metrics_only: MetricsOnly::default(),
        }
    }

    #[test]
    fn counts_per_project_and_minute_with_a_sorted_harness_set() {
        let p = project();
        let claude = Some(HarnessId::ClaudeCode);
        let codex = Some(HarnessId::CodexCli);
        let events = vec![
            event(1, 1_000, EventType::PromptSubmit, claude, Some(&p)),
            event(2, 2_000, EventType::FileMutation, claude, Some(&p)),
            event(3, 3_000, EventType::FileMutation, None, Some(&p)),
            event(4, 4_000, EventType::FileMutation, None, Some(&p)),
            event(5, 5_000, EventType::ToolFail, codex, Some(&p)),
            event(6, 61_000, EventType::PromptSubmit, claude, Some(&p)),
            event(7, 62_000, EventType::ToolPre, claude, None),
        ];
        let marks = timeline_marks(&events, T0, T0 + 120_000);
        assert_eq!(marks.len(), 2);
        assert_eq!(
            marks[0],
            TimelineMark {
                project_id: p.clone(),
                minute_start_ms: T0,
                mutations_manual: 2,
                mutations_harness: 1,
                turns_started: 1,
                tool_failures: 1,
                harnesses: vec![HarnessId::ClaudeCode, HarnessId::CodexCli],
            }
        );
        assert_eq!(marks[1].minute_start_ms, T0 + 60_000);
        assert_eq!(marks[1].turns_started, 1);
    }

    #[test]
    fn events_outside_the_range_are_not_marked() {
        let p = project();
        let events = vec![event(1, 0, EventType::PromptSubmit, None, Some(&p))];
        assert!(timeline_marks(&events, T0 + 60_000, T0 + 120_000).is_empty());
        assert!(timeline_marks(&events, T0 - 60_000, T0).is_empty());
    }

    #[test]
    fn a_mark_serializes_without_any_content_field() {
        let p = project();
        let events = vec![event(
            1,
            0,
            EventType::PromptSubmit,
            Some(HarnessId::OpenCode),
            Some(&p),
        )];
        let json = serde_json::to_string(&timeline_marks(&events, T0, T0 + 60_000)).unwrap();
        for secret in ["SECRET-SESSION", "/home/secret", "secret-model", "e1"] {
            assert!(!json.contains(secret), "{secret} leaked: {json}");
        }
        assert!(json.contains("\"opencode\""));
    }

    #[test]
    fn unknown_fields_are_refused() {
        let text = format!(
            "{{\"project_id\":\"{}\",\"minute_start_ms\":0,\"mutations_manual\":0,\
             \"mutations_harness\":0,\"turns_started\":0,\"tool_failures\":0,\
             \"harnesses\":[],\"cwd\":\"/x\"}}",
            project()
        );
        assert!(serde_json::from_str::<TimelineMark>(&text).is_err());
    }

    #[test]
    fn validation_has_stable_value_free_codes() {
        let good = TimelineMark {
            project_id: project(),
            minute_start_ms: T0,
            mutations_manual: 1,
            mutations_harness: 0,
            turns_started: 0,
            tool_failures: 0,
            harnesses: vec![HarnessId::ClaudeCode],
        };
        assert_eq!(validate_timeline_mark(&good), Ok(()));
        let code = |m: &TimelineMark| validate_timeline_mark(m).unwrap_err().code();
        let mut bad = good.clone();
        bad.project_id = "nope".into();
        assert_eq!(code(&bad), "invalid_project_id");
        let mut bad = good.clone();
        bad.minute_start_ms += 1;
        assert_eq!(code(&bad), "misaligned_minute");
        let mut bad = good.clone();
        bad.turns_started = MAX_MARK_COUNT + 1;
        assert_eq!(code(&bad), "invalid_timeline_mark");
        let mut bad = good;
        bad.harnesses = vec![HarnessId::ClaudeCode, HarnessId::ClaudeCode];
        assert_eq!(code(&bad), "invalid_timeline_mark");
    }
}
