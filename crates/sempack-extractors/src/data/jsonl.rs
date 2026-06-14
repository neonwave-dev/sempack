//! JSONL / NDJSON extractor — one [`Block::Record`] per valid JSON line.
//!
//! Bad lines emit `"jsonl.bad_line"` warnings; the rest of the file is still processed.

use sempack_core::{Extractor, Input, Result};
use sempack_ir::{Block, DocumentIr, Field};
use serde_json::Value;

use crate::{doc_id, source};

pub struct JsonlExtractor;

impl Extractor for JsonlExtractor {
    fn name(&self) -> &'static str {
        "jsonl"
    }

    fn formats(&self) -> &'static [&'static str] {
        &["jsonl", "ndjson"]
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
        let mut line_count = 0usize;
        let mut error_count = 0usize;

        for (line_idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            line_count += 1;
            match serde_json::from_str::<Value>(trimmed) {
                Ok(v) => {
                    doc.push(json_value_to_block(&v));
                }
                Err(e) => {
                    error_count += 1;
                    doc.warn(
                        "jsonl.bad_line",
                        format!("line {}: failed to parse JSON: {e}", line_idx + 1),
                    );
                }
            }
        }

        doc.metadata.extra.insert("format".into(), "jsonl".into());
        doc.metadata
            .extra
            .insert("line_count".into(), line_count.to_string());
        doc.metadata
            .extra
            .insert("error_count".into(), error_count.to_string());

        Ok(doc)
    }
}

/// Convert a JSON value to a Block. Objects → Record; others → Paragraph.
fn json_value_to_block(v: &Value) -> Block {
    match v {
        Value::Object(map) => {
            let mut fields: Vec<Field> = map
                .iter()
                .map(|(k, val)| Field {
                    key: k.clone(),
                    value: json_value_to_string(val),
                })
                .collect();
            fields.sort_by(|a, b| a.key.cmp(&b.key));
            Block::Record { fields }
        }
        Value::Array(arr) => Block::List {
            ordered: false,
            items: arr.iter().map(json_value_to_string).collect(),
        },
        scalar => Block::Paragraph {
            text: json_scalar_to_string(scalar),
        },
    }
}

fn json_scalar_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => json_value_to_string(other),
    }
}

fn json_value_to_string(v: &Value) -> String {
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
    fn jsonl_happy_path() {
        let body = "{\"a\":1}\n{\"b\":2}\n";
        let doc = JsonlExtractor
            .extract(&make_input("data.jsonl", body))
            .unwrap();
        assert!(
            doc.warnings.is_empty(),
            "unexpected warnings: {:?}",
            doc.warnings
        );
        assert_eq!(doc.blocks.len(), 2);
        assert!(matches!(doc.blocks[0], Block::Record { .. }));
        assert_eq!(
            doc.metadata.extra.get("line_count").map(|s| s.as_str()),
            Some("2")
        );
        assert_eq!(
            doc.metadata.extra.get("error_count").map(|s| s.as_str()),
            Some("0")
        );
    }

    #[test]
    fn jsonl_bad_line_warns_and_continues() {
        let body = "{\"a\":1}\nnot json\n{\"c\":3}\n";
        let doc = JsonlExtractor
            .extract(&make_input("data.jsonl", body))
            .unwrap();
        assert_eq!(doc.warnings.len(), 1);
        assert_eq!(doc.warnings[0].code, "jsonl.bad_line");
        // Two valid records should still be there.
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(
            doc.metadata.extra.get("error_count").map(|s| s.as_str()),
            Some("1")
        );
    }

    #[test]
    fn jsonl_empty_file_produces_no_blocks() {
        let doc = JsonlExtractor
            .extract(&make_input("empty.jsonl", ""))
            .unwrap();
        assert!(doc.warnings.is_empty());
        assert!(doc.blocks.is_empty());
        assert_eq!(
            doc.metadata.extra.get("line_count").map(|s| s.as_str()),
            Some("0")
        );
    }

    #[test]
    fn jsonl_blank_lines_skipped() {
        let body = "{\"x\":1}\n\n{\"y\":2}\n";
        let doc = JsonlExtractor
            .extract(&make_input("data.jsonl", body))
            .unwrap();
        assert!(doc.warnings.is_empty());
        assert_eq!(doc.blocks.len(), 2);
        assert_eq!(
            doc.metadata.extra.get("line_count").map(|s| s.as_str()),
            Some("2")
        );
    }

    #[test]
    fn jsonl_ndjson_format_recognized() {
        let body = "{\"a\":1}\n";
        let doc = JsonlExtractor
            .extract(&make_input("data.ndjson", body))
            .unwrap();
        assert_eq!(doc.blocks.len(), 1);
        assert_eq!(
            doc.metadata.extra.get("format").map(|s| s.as_str()),
            Some("jsonl")
        );
    }

    #[test]
    fn jsonl_all_bad_lines_emits_all_warnings() {
        let body = "bad\nalso bad\n";
        let doc = JsonlExtractor
            .extract(&make_input("data.jsonl", body))
            .unwrap();
        assert_eq!(doc.warnings.len(), 2);
        assert!(doc.blocks.is_empty());
        assert_eq!(
            doc.metadata.extra.get("error_count").map(|s| s.as_str()),
            Some("2")
        );
    }
}
