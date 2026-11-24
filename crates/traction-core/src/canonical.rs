//! Canonical JSON: the one byte-exact serializer the Step 3 hash-chain and the
//! Step 4 server hash.
//!
//! Rules (see `.scratch/step-1a/spec.md`):
//! - object keys sorted by UTF-8 bytes, recursively;
//! - no insignificant whitespace;
//! - integers written without exponent or fraction;
//! - floats are rejected;
//! - `None` is written as an explicit `null`.
//!
//! Key order is enforced here explicitly rather than relying on
//! `serde_json::Map` being a `BTreeMap`, because any crate in the graph
//! enabling serde_json's `preserve_order` feature would silently change it.

use serde::Serialize;
use serde_json::Value;

use crate::TractionEvent;

/// Why a value cannot be written as canonical JSON. Carries no input data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalJsonError {
    /// The value contains a floating-point number.
    Float,
    /// The value cannot be represented as JSON (e.g. a map with non-string keys).
    Unrepresentable,
}

impl CanonicalJsonError {
    /// A stable reason code.
    pub fn code(self) -> &'static str {
        match self {
            CanonicalJsonError::Float => "canonical_json_float",
            CanonicalJsonError::Unrepresentable => "canonical_json_unrepresentable",
        }
    }
}

impl std::fmt::Display for CanonicalJsonError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for CanonicalJsonError {}

/// Serialize `value` to canonical JSON bytes.
pub fn to_canonical_json<T: Serialize + ?Sized>(value: &T) -> Result<Vec<u8>, CanonicalJsonError> {
    let value = serde_json::to_value(value).map_err(|_| CanonicalJsonError::Unrepresentable)?;
    let mut out = Vec::new();
    write_value(&value, &mut out)?;
    Ok(out)
}

impl TractionEvent {
    /// This Traction Event's canonical JSON bytes.
    pub fn to_canonical_json(&self) -> Vec<u8> {
        // A Traction Event has only string keys, integers, booleans, strings
        // and nulls, so canonical serialization cannot fail.
        to_canonical_json(self).expect("a Traction Event is always canonical-JSON representable")
    }
}

fn write_value(value: &Value, out: &mut Vec<u8>) -> Result<(), CanonicalJsonError> {
    match value {
        Value::Null => out.extend_from_slice(b"null"),
        Value::Bool(true) => out.extend_from_slice(b"true"),
        Value::Bool(false) => out.extend_from_slice(b"false"),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                out.extend_from_slice(i.to_string().as_bytes());
            } else if let Some(u) = n.as_u64() {
                out.extend_from_slice(u.to_string().as_bytes());
            } else {
                return Err(CanonicalJsonError::Float);
            }
        }
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_value(item, out)?;
            }
            out.push(b']');
        }
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_unstable_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push(b'{');
            for (i, (key, item)) in entries.into_iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                write_string(key, out);
                out.push(b':');
                write_value(item, out)?;
            }
            out.push(b'}');
        }
    }
    Ok(())
}

/// serde_json's string escaping is deterministic: it escapes `"`, `\` and
/// control characters only, and writes all other characters as raw UTF-8.
fn write_string(s: &str, out: &mut Vec<u8>) {
    serde_json::to_writer(&mut *out, s).expect("writing a string to a Vec cannot fail");
}
