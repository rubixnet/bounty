//! Shared Traction types: the Traction Event wire schema, canonical JSON and
//! hash-chain math, used by the daemon now and by the server from Step 4.
//!
//! This crate must stay free of OS, I/O and async dependencies.

mod event;
mod validate;

pub use event::{
    EndReason, EventType, HarnessId, MetricsOnly, NetworkClass, SCHEMA_VERSION, TractionEvent,
};
pub use validate::{
    EventError, MAX_CWD_LEN, MAX_IDENTIFIER_LEN, MAX_SESSION_ID_LEN, ValidationError,
};
