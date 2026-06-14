//! JSON extractor — turns a JSON file into [`Block::Record`] / [`Block::Table`] blocks.
//!
//! Rules:
//! - JSON array where **all** elements are flat objects with the **same** key set
//!   → single [`Block::Table`] (headers = sorted keys, rows = values).
//! - JSON array of any other mix → one [`Block::Record`] per element.
//! - JSON object → single [`Block::Record`].
//! - JSON scalar (string/number/bool/null) → single [`Block::Paragraph`].
//! - Nested values are serialized back to compact JSON text.
//! - Invalid JSON emits `"json.invalid"` warning; returns empty doc.

use sempack_core::{Extractor, Input, Result};
use sempack_ir::{Block, DocumentIr, Field};
use serde_json::Value;

use crate::{doc_id, source};

pub struct JsonExtractor;

impl Extractor for JsonExtractor {
    fn name(&self) -> &'static str {
        "json"
    }

    fn formats(&self) -> &'static [&'static str] {
        &["json"]
    }

    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        let mut doc = DocumentIr::new(doc_id(input), source(input));

        // Populate filename metadata.
        if let Some(path) = &input.path {
            let filename = path.rsplit(['/', '\\']).next().unwrap_or(path);
            doc.metadata
                .extra
                .insert("filename".into(), filename.to_string());
        }

        let text = input.text();
        let value: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(e) => {
                doc.warn("json.invalid", format!("failed to parse JSON: {e}"));
                return Ok(doc);
            }
        };

        match &value {
            Value::Array(arr) => {
                let record_count = arr.len();
                if let Some(table) = try_table(arr) {
                    doc.push(table);
                } else {
                    for item in arr {
                        doc.push(value_to_block(item));
                    }
                }
                doc.metadata.extra.insert("format".into(), "json".into());
                doc.metadata
                    .extra
                    .insert("record_count".into(), record_count.to_string());
            }
            Value::Object(_) => {
                doc.push(value_to_block(&value));
                doc.metadata.extra.insert("format".into(), "json".into());
                doc.metadata.extra.insert("record_count".into(), "1".into());
            }
            scalar => {
                let text = scalar_to_string(scalar);
                doc.push(Block::Paragraph { text });
                doc.metadata.extra.insert("format".into(), "json".into());
                doc.metadata.extra.insert("record_count".into(), "1".into());
            }
        }

        Ok(doc)
    }
}

/// Try to build a single `Block::Table` from an array.
/// Returns `None` if elements are not all flat objects sharing the same key set.
fn try_table(arr: &[Value]) -> Option<Block> {
    if arr.is_empty() {
        return None;
    }
    let mut key_sets: Vec<Vec<String>> = Vec::new();
    for item in arr {
        match item {
            Value::Object(map) => {
                // Reject if any value is itself an object or array (not flat).
                for v in map.values() {
                    if matches!(v, Value::Object(_) | Value::Array(_)) {
                        return None;
                    }
                }
                let mut keys: Vec<String> = map.keys().cloned().collect();
                keys.sort();
                key_sets.push(keys);
            }
            _ => return None,
        }
    }
    let first = &key_sets[0];
    if key_sets.iter().any(|ks| ks != first) {
        return None;
    }
    let headers: Vec<String> = first.clone();
    let rows: Vec<Vec<String>> = arr
        .iter()
        .map(|item| {
            let map = item.as_object().unwrap();
            headers
                .iter()
                .map(|k| scalar_to_string(map.get(k).unwrap_or(&Value::Null)))
                .collect()
        })
        .collect();
    Some(Block::Table { headers, rows })
}

/// Convert any JSON value into a `Block`. Objects → Record; arrays → List; scalars → Paragraph.
fn value_to_block(v: &Value) -> Block {
    match v {
        Value::Object(map) => {
            let mut fields: Vec<Field> = map
                .iter()
                .map(|(k, val)| Field {
                    key: k.clone(),
                    value: value_to_string(val),
                })
                .collect();
            // Sort so field order is deterministic.
            fields.sort_by(|a, b| a.key.cmp(&b.key));
            Block::Record { fields }
        }
        Value::Array(arr) => Block::List {
            ordered: false,
            items: arr.iter().map(value_to_string).collect(),
        },
        scalar => Block::Paragraph {
            text: scalar_to_string(scalar),
        },
    }
}

/// Render a scalar JSON value to a human-readable string (no quotes around strings).
fn scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => value_to_string(other),
    }
}

/// Render any JSON value to a string; nested containers become compact JSON text.
fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "?".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sempack_core::{detect, Input};

    fn make_input(name: &str, body: &str) -> Input {
        let bytes = body.as_bytes().to_vec();
        let detected = detect(Some(name), &bytes);
        Input {
            path: Some(name.to_string()),
            bytes,
            detected,
        }
    }

    #[test]
    fn json_array_flat_objects_becomes_table() {
        let body = r#"[{"b":2,"a":1},{"a":3,"b":4}]"#;
        let doc = JsonExtractor
            .extract(&make_input("data.json", body))
            .unwrap();
        assert!(
            doc.warnings.is_empty(),
            "unexpected warnings: {:?}",
            doc.warnings
        );
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Table { headers, rows } => {
                assert_eq!(headers, &["a", "b"]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0], vec!["1", "2"]);
                assert_eq!(rows[1], vec!["3", "4"]);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        assert_eq!(
            doc.metadata.extra.get("format").map(|s| s.as_str()),
            Some("json")
        );
        assert_eq!(
            doc.metadata.extra.get("record_count").map(|s| s.as_str()),
            Some("2")
        );
    }

    #[test]
    fn json_array_mixed_becomes_records() {
        let body = r#"[{"a":1},{"a":2,"b":3}]"#;
        let doc = JsonExtractor
            .extract(&make_input("data.json", body))
            .unwrap();
        assert!(doc.warnings.is_empty());
        assert_eq!(doc.blocks.len(), 2);
        assert!(doc.blocks.iter().all(|b| matches!(b, Block::Record { .. })));
    }

    #[test]
    fn json_object_becomes_single_record() {
        let body = r#"{"name":"Alice","age":30}"#;
        let doc = JsonExtractor
            .extract(&make_input("data.json", body))
            .unwrap();
        assert!(doc.warnings.is_empty());
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Record { fields } => {
                // Sorted alphabetically: age before name.
                assert_eq!(fields[0].key, "age");
                assert_eq!(fields[0].value, "30");
                assert_eq!(fields[1].key, "name");
                assert_eq!(fields[1].value, "Alice");
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn json_nested_values_flatten_to_json_string() {
        let body = r#"{"outer":{"inner":"x"}}"#;
        let doc = JsonExtractor
            .extract(&make_input("data.json", body))
            .unwrap();
        assert!(doc.warnings.is_empty());
        match &doc.blocks[0] {
            Block::Record { fields } => {
                assert_eq!(fields[0].key, "outer");
                assert_eq!(fields[0].value, r#"{"inner":"x"}"#);
            }
            other => panic!("expected Record, got {other:?}"),
        }
    }

    #[test]
    fn json_invalid_emits_warning_and_empty_doc() {
        let doc = JsonExtractor
            .extract(&make_input("bad.json", "not json {{{"))
            .unwrap();
        assert!(!doc.warnings.is_empty());
        assert_eq!(doc.warnings[0].code, "json.invalid");
        assert!(doc.blocks.is_empty());
    }

    #[test]
    fn json_empty_file_emits_warning() {
        let doc = JsonExtractor
            .extract(&make_input("empty.json", ""))
            .unwrap();
        assert!(!doc.warnings.is_empty());
        assert_eq!(doc.warnings[0].code, "json.invalid");
    }

    #[test]
    fn json_array_with_nested_object_falls_back_to_records() {
        let body = r#"[{"a":{"nested":true}},{"a":1}]"#;
        let doc = JsonExtractor
            .extract(&make_input("data.json", body))
            .unwrap();
        assert!(doc.warnings.is_empty());
        assert_eq!(doc.blocks.len(), 2);
        assert!(matches!(doc.blocks[0], Block::Record { .. }));
    }

    #[test]
    fn json_empty_array_produces_no_blocks() {
        let doc = JsonExtractor
            .extract(&make_input("empty.json", "[]"))
            .unwrap();
        assert!(doc.warnings.is_empty());
        assert!(doc.blocks.is_empty());
        assert_eq!(
            doc.metadata.extra.get("record_count").map(|s| s.as_str()),
            Some("0")
        );
    }
}
