//! Shared JSON-value-to-string helpers used by both the JSON and JSONL extractors.

use serde_json::Value;

/// Render a scalar JSON value to a human-readable string (no quotes around strings).
pub(crate) fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => value_to_string(other),
    }
}

/// Render any JSON value to a string; nested containers become compact JSON text.
pub(crate) fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "?".into()),
    }
}
