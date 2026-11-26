//! Session Pause state: per `(harness_id, session_id)` spans of time during
//! which a session's events are not stored.
//!
//! The Router owns the only copy, seeded from the `session_pause` table at
//! startup and changed only by [`Pauses::apply`], which returns the write
//! to queue (through the single writer) when the state changed. Repeating
//! a command is a no-op. Spans are compared with event timestamps, so an
//! event stamped before a `/stop-track` but routed after it is still kept.

use std::collections::HashMap;

use traction_core::HarnessId;

use crate::claude_code::pause::PauseAction;
use crate::storage::{PauseSpanRow, WriteOp};

type SessionKey = (HarnessId, String);

/// One pause span: from `from_ms`, until `to_ms` (open while `None`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Span {
    from_ms: i64,
    to_ms: Option<i64>,
}

impl Span {
    fn covers(self, ts: i64) -> bool {
        self.from_ms <= ts && self.to_ms.is_none_or(|to| ts < to)
    }
}

#[derive(Debug, Default)]
pub struct Pauses {
    /// Each session's spans, oldest first; at most one is open.
    spans: HashMap<SessionKey, Vec<Span>>,
}

impl Pauses {
    /// Seeded with every stored span, oldest first per session.
    pub fn new(rows: Vec<PauseSpanRow>) -> Self {
        let mut pauses = Self::default();
        for row in rows {
            pauses
                .spans
                .entry((row.harness_id, row.session_id))
                .or_default()
                .push(Span {
                    from_ms: row.from_ms,
                    to_ms: row.to_ms,
                });
        }
        for spans in pauses.spans.values_mut() {
            spans.sort_by_key(|span| span.from_ms);
        }
        pauses
    }

    /// Whether the session was paused at event timestamp `ts`.
    pub fn is_paused_at(&self, harness: HarnessId, session_id: &str, ts: i64) -> bool {
        // Spans are few (one per `/stop-track`), so a scan is enough.
        self.spans
            .get(&(harness, session_id.to_owned()))
            .is_some_and(|spans| spans.iter().rev().any(|span| span.covers(ts)))
    }

    /// Applies a command read from a transcript, stamped `at_ms`, which
    /// may be older than the spans already known (a backfill, or a reread
    /// of a file). Replaying a command changes nothing, and a resume never
    /// closes a pause that began after it. A stop older than a later span
    /// pauses up to where that span begins, so spans never overlap.
    pub fn apply_recorded(
        &mut self,
        action: PauseAction,
        harness: HarnessId,
        session_id: &str,
        at_ms: i64,
    ) -> Option<WriteOp> {
        let key = (harness, session_id.to_owned());
        let spans = self.spans.get(&key).map(Vec::as_slice).unwrap_or_default();
        let next = spans.iter().map(|s| s.from_ms).find(|&from| from > at_ms);
        match action {
            PauseAction::Stop => {
                if spans.iter().any(|s| s.from_ms == at_ms || s.covers(at_ms)) {
                    return None;
                }
                let Some(to_ms) = next else {
                    return self.apply(action, harness, session_id, at_ms);
                };
                let spans = self.spans.entry(key).or_default();
                let index = spans.partition_point(|s| s.from_ms < at_ms);
                spans.insert(
                    index,
                    Span {
                        from_ms: at_ms,
                        to_ms: Some(to_ms),
                    },
                );
                Some(WriteOp::PauseStart(PauseSpanRow {
                    harness_id: harness,
                    session_id: session_id.to_owned(),
                    from_ms: at_ms,
                    to_ms: Some(to_ms),
                }))
            }
            PauseAction::Resume => {
                let open_before = spans
                    .iter()
                    .any(|s| s.to_ms.is_none() && s.from_ms <= at_ms);
                if !open_before {
                    return None;
                }
                self.apply(action, harness, session_id, at_ms)
            }
        }
    }

    /// Pauses or resumes the session at `at_ms`. Returns the write to
    /// queue, or `None` when the session already was in that state.
    pub fn apply(
        &mut self,
        action: PauseAction,
        harness: HarnessId,
        session_id: &str,
        at_ms: i64,
    ) -> Option<WriteOp> {
        let key = (harness, session_id.to_owned());
        let spans = self.spans.entry(key).or_default();
        let open = spans.iter_mut().find(|span| span.to_ms.is_none());
        let row = |from_ms, to_ms| PauseSpanRow {
            harness_id: harness,
            session_id: session_id.to_owned(),
            from_ms,
            to_ms,
        };
        match (action, open) {
            (PauseAction::Stop, None) => {
                spans.push(Span {
                    from_ms: at_ms,
                    to_ms: None,
                });
                Some(WriteOp::PauseStart(row(at_ms, None)))
            }
            (PauseAction::Resume, Some(span)) => {
                // A clock step backwards never makes a span end before it
                // began.
                let to_ms = at_ms.max(span.from_ms);
                span.to_ms = Some(to_ms);
                Some(WriteOp::PauseEnd(row(span.from_ms, Some(to_ms))))
            }
            (PauseAction::Stop, Some(_)) | (PauseAction::Resume, None) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: HarnessId = HarnessId::ClaudeCode;

    fn paused(pauses: &Pauses, ts: i64) -> bool {
        pauses.is_paused_at(CLAUDE, "s", ts)
    }

    #[test]
    fn replaying_recorded_commands_changes_nothing() {
        let mut pauses = Pauses::default();
        let stop = |p: &mut Pauses, at| p.apply_recorded(PauseAction::Stop, CLAUDE, "s", at);
        let resume = |p: &mut Pauses, at| p.apply_recorded(PauseAction::Resume, CLAUDE, "s", at);
        assert!(stop(&mut pauses, 10).is_some());
        assert!(resume(&mut pauses, 20).is_some());
        // A live pause opened later must survive a reread of the file.
        assert!(pauses.apply(PauseAction::Stop, CLAUDE, "s", 50).is_some());
        assert_eq!(stop(&mut pauses, 10), None);
        assert_eq!(resume(&mut pauses, 20), None);
        assert!(paused(&pauses, 15) && !paused(&pauses, 30) && paused(&pauses, 60));
    }

    #[test]
    fn an_older_recorded_pause_ends_where_a_later_span_begins() {
        let mut pauses = Pauses::default();
        assert!(pauses.apply(PauseAction::Stop, CLAUDE, "s", 50).is_some());
        let op = pauses.apply_recorded(PauseAction::Stop, CLAUDE, "s", 10);
        assert!(matches!(op, Some(WriteOp::PauseStart(row)) if row.to_ms == Some(50)));
        assert!(paused(&pauses, 10) && paused(&pauses, 60) && !paused(&pauses, 9));
        // The live span is still the one a live resume closes.
        assert!(pauses.apply(PauseAction::Resume, CLAUDE, "s", 70).is_some());
        assert!(!paused(&pauses, 70));
    }
}
