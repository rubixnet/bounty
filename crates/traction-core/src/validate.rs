//! The single gate every Traction Event passes: deserialize, then validate.
//!
//! Errors never carry input values, so callers can log or return them
//! without echoing raw content.

use std::fmt;

use crate::event::TractionEvent;

/// Why a payload was refused as a Traction Event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventError {
    /// Not JSON, or JSON that does not fit the Traction Event shape (unknown
    /// field, wrong enum spelling, float where an integer is required, ...).
    Malformed,
    /// Well-formed, but breaks a Traction Event rule.
    Invalid(ValidationError),
}

impl EventError {
    /// A short, stable, machine-readable reason code.
    pub fn code(&self) -> &'static str {
        match self {
            EventError::Malformed => "malformed_event",
            EventError::Invalid(error) => error.code(),
        }
    }
}

impl fmt::Display for EventError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for EventError {}

impl From<ValidationError> for EventError {
    fn from(error: ValidationError) -> Self {
        EventError::Invalid(error)
    }
}

/// A rule a deserialized Traction Event breaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationError {}

impl ValidationError {
    /// A short, stable, machine-readable reason code.
    pub fn code(&self) -> &'static str {
        match *self {}
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for ValidationError {}

impl TractionEvent {
    /// Parse and validate a Traction Event from JSON bytes.
    ///
    /// This is the one entry point Adapters' payloads go through.
    pub fn from_json(bytes: &[u8]) -> Result<Self, EventError> {
        let event: TractionEvent =
            serde_json::from_slice(bytes).map_err(|_| EventError::Malformed)?;
        event.validate()?;
        Ok(event)
    }

    /// Check every Traction Event rule that the type alone cannot express.
    pub fn validate(&self) -> Result<(), ValidationError> {
        Ok(())
    }
}
