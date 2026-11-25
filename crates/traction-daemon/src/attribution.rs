//! Attribution windows: in-memory, never persisted, fed by every accepted
//! (routed) Traction Event on the Router thread, and asked once per held
//! `FILE_MUTATION` when its hold ends.
//!
//! - A **hook window** (`harness_hook` / `harness_plugin`) opens on a
//!   `TOOL_PRE` while the session's turn is open, and closes on the next
//!   `TOOL_POST` / `TOOL_FAIL` / `TOOL_PRE` or on turn end (`TURN_END`,
//!   `STOP_FAILURE`, `SESSION_END`).
//! - A **log-tailed turn window** (`harness_log` / `harness_db_poll`) opens
//!   on a `PROMPT_SUBMIT` and closes on the next turn end or prompt.
//! - A **trailing fallback window** covers a log-tailed session that has
//!   never reported a turn boundary: [`LOG_TAIL_TRAILING_WINDOW`] after any
//!   of its events.
//!
//! Windows are intervals of event timestamps (the hold lets late records
//! arrive), compared with the mutation's own timestamp, and scoped to the
//! session's current Project: when a session's events move to another
//! Project, its open windows close there and reopen on the new one.
//! Precedence is hook, then log-tailed turn, then trailing; among windows
//! of one kind the most recently opened wins.
//!
//! [`LOG_TAIL_TRAILING_WINDOW`]: crate::config::LOG_TAIL_TRAILING_WINDOW

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use traction_core::{AttributionSource, EventType, HarnessId, NetworkClass, TractionEvent};

/// How long after it closed (or after a session's last event) window state
/// is kept beyond the hold, for mutations decided late (a busy Router).
const RETENTION_MARGIN: Duration = Duration::from_secs(60);
/// Closed windows and trailing marks kept per session at most.
const MAX_HISTORY: usize = 64;
/// Sessions are swept for expired state every this many events.
const SWEEP_EVERY: u32 = 256;

/// Who a claimed mutation belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub harness_id: HarnessId,
    pub session_id: String,
    pub network_class: NetworkClass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Trailing,
    LogTurn,
    Hook,
}

#[derive(Debug, Clone)]
struct Window {
    kind: Kind,
    project: Option<String>,
    opened_ms: u64,
    closed_ms: Option<u64>,
}

impl Window {
    fn covers(&self, ts: u64) -> bool {
        self.opened_ms <= ts && self.closed_ms.is_none_or(|closed| ts <= closed)
    }
}

#[derive(Debug)]
struct SessionWindows {
    network_class: NetworkClass,
    project: Option<String>,
    /// A hook-reported turn is open (`PROMPT_SUBMIT`, no turn end yet).
    turn_open: bool,
    log_tailed: bool,
    /// Ever reported `PROMPT_SUBMIT` or `TURN_END` (log-tailed: turn
    /// windows instead of the trailing fallback).
    has_boundaries: bool,
    /// Hook and log-tailed turn windows, oldest first.
    windows: VecDeque<Window>,
    /// Trailing fallback marks: (event timestamp, Project), oldest first.
    marks: VecDeque<(u64, Option<String>)>,
    /// Latest event timestamp seen.
    last_ms: u64,
}

type SessionKey = (HarnessId, String);
/// A covering window's kind, then when it opened (higher wins).
type Rank = (Kind, u64);

pub struct Attribution {
    sessions: HashMap<SessionKey, SessionWindows>,
    trailing_ms: u64,
    retention_ms: u64,
    since_sweep: u32,
}

impl Attribution {
    /// No open windows (a daemon start). `hold` bounds how late a mutation
    /// can be decided, so how long closed windows must be kept.
    pub fn new(trailing_window: Duration, hold: Duration) -> Self {
        Self {
            sessions: HashMap::new(),
            trailing_ms: millis(trailing_window),
            retention_ms: millis(hold + RETENTION_MARGIN),
            since_sweep: 0,
        }
    }

    /// Updates window state from one routed event (its `project_id` is the
    /// Router's). `now_ms` is the wall clock, for expiring old state.
    pub fn observe(&mut self, event: &TractionEvent, now_ms: u64) {
        let (Some(harness), Some(session_id)) = (event.harness_id, &event.session_id) else {
            return;
        };
        let hook = matches!(
            event.attribution_source,
            AttributionSource::HarnessHook | AttributionSource::HarnessPlugin
        );
        let log = matches!(
            event.attribution_source,
            AttributionSource::HarnessLog | AttributionSource::HarnessDbPoll
        );
        let ts = event.timestamp_ms;
        let state = self
            .sessions
            .entry((harness, session_id.clone()))
            .or_insert_with(|| SessionWindows {
                network_class: event.network_class,
                project: event.project_id.clone(),
                turn_open: false,
                log_tailed: false,
                has_boundaries: false,
                windows: VecDeque::new(),
                marks: VecDeque::new(),
                last_ms: ts,
            });
        state.network_class = event.network_class;
        state.last_ms = state.last_ms.max(ts);
        if state.project != event.project_id {
            state.move_to(event.project_id.clone(), ts);
        }
        match event.event_type {
            EventType::PromptSubmit if hook => state.turn_open = true,
            EventType::PromptSubmit if log => {
                state.has_boundaries = true;
                state.close(Kind::LogTurn, ts);
                state.open(Kind::LogTurn, ts);
            }
            EventType::TurnEnd | EventType::StopFailure | EventType::SessionEnd => {
                if log && event.event_type == EventType::TurnEnd {
                    state.has_boundaries = true;
                }
                state.turn_open = false;
                state.close(Kind::Hook, ts);
                state.close(Kind::LogTurn, ts);
            }
            EventType::ToolPre if hook => {
                state.close(Kind::Hook, ts);
                if state.turn_open {
                    state.open(Kind::Hook, ts);
                }
            }
            EventType::ToolPost | EventType::ToolFail if hook => state.close(Kind::Hook, ts),
            _ => {}
        }
        if log {
            state.log_tailed = true;
            if state.has_boundaries {
                state.marks.clear();
            } else {
                state.marks.push_back((ts, event.project_id.clone()));
                if state.marks.len() > MAX_HISTORY {
                    state.marks.pop_front();
                }
            }
        }
        self.since_sweep += 1;
        if self.since_sweep >= SWEEP_EVERY {
            self.since_sweep = 0;
            self.sweep(now_ms);
        }
    }

    /// The session whose window on a Project accepted by `on_project`
    /// covers `ts`, by precedence; `None` means manual work.
    pub fn claim(&self, ts: u64, on_project: impl Fn(&str) -> bool) -> Option<Claim> {
        let on = |project: &Option<String>| project.as_deref().is_some_and(&on_project);
        let mut best: Option<(Rank, &SessionKey, &SessionWindows)> = None;
        for (key, state) in &self.sessions {
            let windows = state
                .windows
                .iter()
                .filter(|w| on(&w.project) && w.covers(ts))
                .map(|w| (w.kind, w.opened_ms));
            let trailing = state
                .marks
                .iter()
                .filter(|(mark, project)| {
                    state.log_tailed
                        && !state.has_boundaries
                        && on(project)
                        && *mark <= ts
                        && ts <= mark.saturating_add(self.trailing_ms)
                })
                .map(|(mark, _)| (Kind::Trailing, *mark));
            for rank in windows.chain(trailing) {
                let better = best.as_ref().is_none_or(|(top, top_key, _)| {
                    // Ties: the lowest session key, so the answer never
                    // depends on hash order.
                    rank > *top
                        || (rank == *top
                            && (key.0.as_str(), &key.1) < (top_key.0.as_str(), &top_key.1))
                });
                if better {
                    best = Some((rank, key, state));
                }
            }
        }
        best.map(|(_, (harness_id, session_id), state)| Claim {
            harness_id: *harness_id,
            session_id: session_id.clone(),
            network_class: state.network_class,
        })
    }

    /// Drops closed windows, marks and idle sessions too old to claim any
    /// mutation still to be decided.
    fn sweep(&mut self, now_ms: u64) {
        let horizon = now_ms.saturating_sub(self.retention_ms);
        let trailing = self.trailing_ms;
        self.sessions.retain(|_, state| {
            state
                .windows
                .retain(|w| w.closed_ms.is_none_or(|closed| closed >= horizon));
            state
                .marks
                .retain(|(mark, _)| mark.saturating_add(trailing) >= horizon);
            state.turn_open
                || !state.windows.is_empty()
                || state.last_ms.saturating_add(trailing) >= horizon
        });
    }
}

impl SessionWindows {
    fn open(&mut self, kind: Kind, ts: u64) {
        self.windows.push_back(Window {
            kind,
            project: self.project.clone(),
            opened_ms: ts,
            closed_ms: None,
        });
        if self.windows.len() > MAX_HISTORY
            && let Some(at) = self.windows.iter().position(|w| w.closed_ms.is_some())
        {
            self.windows.remove(at);
        }
    }

    fn close(&mut self, kind: Kind, ts: u64) {
        for window in &mut self.windows {
            if window.kind == kind && window.closed_ms.is_none() {
                window.closed_ms = Some(ts);
            }
        }
    }

    /// The session's events moved to `project`: its open windows end on the
    /// old Project at `ts` and continue on the new one.
    fn move_to(&mut self, project: Option<String>, ts: u64) {
        self.project = project;
        let open: Vec<Kind> = self
            .windows
            .iter()
            .filter(|w| w.closed_ms.is_none())
            .map(|w| w.kind)
            .collect();
        for kind in open {
            self.close(kind, ts);
            self.open(kind, ts);
        }
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
