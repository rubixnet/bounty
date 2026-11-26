//! The Presence Minute wire type (`docs/protocol.md` section 2b).
//!
//! Counts, ratios and closed enums only: no key code, character, pointer
//! position, window title, URL or raw application identifier has a slot here.

use serde::{Deserialize, Serialize};

/// Length of one Presence Minute's sub-slot, in seconds.
pub const SUBSLOT_SEC: u32 = 10;
/// Sub-slots per Presence Minute.
pub const SUBSLOTS_PER_MINUTE: u8 = 6;

/// What the foreground application is, resolved in memory from a compiled-in
/// table. The raw identifier is never kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppCategory {
    Ide,
    Terminal,
    Browser,
    AiDesktop,
    Other,
}

impl AppCategory {
    pub const ALL: [Self; 5] = [
        Self::Ide,
        Self::Terminal,
        Self::Browser,
        Self::AiDesktop,
        Self::Other,
    ];
}

/// How much a platform backend could observe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TelemetryTier {
    /// Idle, foreground category and real-vs-injected input counts.
    Full,
    /// Idle and foreground category; no input counts.
    Reduced,
    /// Idle only.
    IdleOnly,
    /// Nothing observable.
    None,
}

/// Foreground seconds per [`AppCategory`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CategorySec {
    pub ide: u32,
    pub terminal: u32,
    pub browser: u32,
    pub ai_desktop: u32,
    pub other: u32,
}

impl CategorySec {
    pub fn add(&mut self, category: AppCategory, sec: u32) {
        let slot = match category {
            AppCategory::Ide => &mut self.ide,
            AppCategory::Terminal => &mut self.terminal,
            AppCategory::Browser => &mut self.browser,
            AppCategory::AiDesktop => &mut self.ai_desktop,
            AppCategory::Other => &mut self.other,
        };
        *slot = slot.saturating_add(sec);
    }

    pub fn get(&self, category: AppCategory) -> u32 {
        match category {
            AppCategory::Ide => self.ide,
            AppCategory::Terminal => self.terminal,
            AppCategory::Browser => self.browser,
            AppCategory::AiDesktop => self.ai_desktop,
            AppCategory::Other => self.other,
        }
    }

    pub fn total(&self) -> u32 {
        AppCategory::ALL.iter().map(|c| self.get(*c)).sum()
    }
}

/// One wall-clock minute of device-wide presence. A field the backend's
/// [`TelemetryTier`] cannot fill is `None` (`null`), never `0`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PresenceMinute {
    pub minute_start_ms: u64,
    pub key_events: Option<u32>,
    pub pointer_events: Option<u32>,
    pub injected_events: Option<u32>,
    /// 0 to 6: sub-slots that saw real input.
    pub active_subslots: Option<u8>,
    /// Shannon entropy (bits x 1000) of log-binned real inter-event intervals.
    pub cadence_entropy_milli: Option<u32>,
    pub category_sec: Option<CategorySec>,
    pub category_switches: Option<u32>,
    pub idle_sec: Option<u32>,
    pub telemetry_tier: TelemetryTier,
}

/// Why a [`PresenceMinute`] is invalid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceError {
    ActiveSubslotsOutOfRange,
}

impl PresenceMinute {
    pub fn validate(&self) -> Result<(), PresenceError> {
        match self.active_subslots {
            Some(n) if n > SUBSLOTS_PER_MINUTE => Err(PresenceError::ActiveSubslotsOutOfRange),
            _ => Ok(()),
        }
    }
}
