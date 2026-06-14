//! Delimited-value extractors: CSV, TSV, and PSV (pipe-delimited).
//!
//! All three share the same logic via [`DelimitedExtractor`]. The first row is
//! treated as a header row; subsequent rows become the table data.
//!
//! Ragged rows (wrong column count) emit a `"csv.ragged_row"` warning and are
//! still included (padded with empty strings or truncated to match the header count).

use csv::ReaderBuilder;
use sempack_core::{Extractor, Input, Result};
use sempack_ir::{Block, DocumentIr};

use crate::{doc_id, source};

// ---------------------------------------------------------------------------
// Public extractor types
// ---------------------------------------------------------------------------

pub struct CsvExtractor;
pub struct TsvExtractor;
pub struct PsvExtractor;

impl Extractor for CsvExtractor {
    fn name(&self) -> &'static str {
        "csv"
    }
    fn formats(&self) -> &'static [&'static str] {
        &["csv"]
    }
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        DelimitedExtractor {
            delimiter: b',',
            format_name: "csv",
        }
        .extract(input)
    }
}

impl Extractor for TsvExtractor {
    fn name(&self) -> &'static str {
        "tsv"
    }
    fn formats(&self) -> &'static [&'static str] {
        &["tsv"]
    }
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        DelimitedExtractor {
            delimiter: b'\t',
            format_name: "tsv",
        }
        .extract(input)
    }
}

impl Extractor for PsvExtractor {
    fn name(&self) -> &'static str {
        "psv"
    }
    fn formats(&self) -> &'static [&'static str] {
        &["psv"]
    }
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        DelimitedExtractor {
            delimiter: b'|',
            format_name: "psv",
        }
        .extract(input)
    }
}

// ---------------------------------------------------------------------------
// Shared implementation
// ---------------------------------------------------------------------------

struct DelimitedExtractor {
    delimiter: u8,
    format_name: &'static str,
}

impl DelimitedExtractor {
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        let mut doc = DocumentIr::new(doc_id(input), source(input));

        // Populate filename metadata.
        if let Some(path) = &input.path {
            let filename = path.rsplit(['/', '\\']).next().unwrap_or(path);
            doc.metadata
                .extra
                .insert("filename".into(), filename.to_string());
        }

        let mut rdr = ReaderBuilder::new()
            .delimiter(self.delimiter)
            .flexible(true) // allow ragged rows; we warn below
            .has_headers(true)
            .from_reader(input.bytes.as_slice());

        // Extract headers.
        let headers: Vec<String> = match rdr.headers() {
            Ok(h) => h.iter().map(|s| s.to_string()).collect(),
            Err(e) => {
                doc.warn("csv.read_error", format!("failed to read headers: {e}"));
                return Ok(doc);
            }
        };

        let col_count = headers.len();
        let mut rows: Vec<Vec<String>> = Vec::new();

        for (row_idx, result) in rdr.records().enumerate() {
            match result {
                Ok(record) => {
                    let n = record.len();
                    if n != col_count {
                        doc.warn(
                            "csv.ragged_row",
                            format!("row {} has {n} field(s), expected {col_count}", row_idx + 2),
                        );
                    }
                    // Pad short rows with empty strings; truncate long ones.
                    let mut row: Vec<String> = record.iter().map(|s| s.to_string()).collect();
                    row.resize(col_count, String::new());
                    rows.push(row);
                }
                Err(e) => {
                    doc.warn("csv.read_error", format!("row {}: {e}", row_idx + 2));
                }
            }
        }

        let row_count = rows.len();

        if col_count > 0 || row_count > 0 {
            doc.push(Block::Table { headers, rows });
        }

        doc.metadata
            .extra
            .insert("format".into(), self.format_name.into());
        doc.metadata
            .extra
            .insert("row_count".into(), row_count.to_string());
        doc.metadata
            .extra
            .insert("col_count".into(), col_count.to_string());

        Ok(doc)
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

    // -----------------------------------------------------------------------
    // CSV tests
    // -----------------------------------------------------------------------

    #[test]
    fn csv_happy_path() {
        let body = "name,age\nAlice,30\nBob,25\n";
        let doc = CsvExtractor.extract(&make_input("data.csv", body)).unwrap();
        assert!(
            doc.warnings.is_empty(),
            "unexpected warnings: {:?}",
            doc.warnings
        );
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Table { headers, rows } => {
                assert_eq!(headers, &["name", "age"]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0], vec!["Alice", "30"]);
                assert_eq!(rows[1], vec!["Bob", "25"]);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        assert_eq!(
            doc.metadata.extra.get("format").map(|s| s.as_str()),
            Some("csv")
        );
        assert_eq!(
            doc.metadata.extra.get("row_count").map(|s| s.as_str()),
            Some("2")
        );
        assert_eq!(
            doc.metadata.extra.get("col_count").map(|s| s.as_str()),
            Some("2")
        );
    }

    #[test]
    fn csv_ragged_row_warns_and_pads() {
        let body = "a,b,c\n1,2\n3,4,5\n";
        let doc = CsvExtractor.extract(&make_input("data.csv", body)).unwrap();
        assert_eq!(doc.warnings.len(), 1);
        assert_eq!(doc.warnings[0].code, "csv.ragged_row");
        match &doc.blocks[0] {
            Block::Table { rows, .. } => {
                assert_eq!(rows[0], vec!["1", "2", ""]);
                assert_eq!(rows[1], vec!["3", "4", "5"]);
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn csv_quoted_fields() {
        let body = "a,b\n\"hello, world\",2\n";
        let doc = CsvExtractor.extract(&make_input("data.csv", body)).unwrap();
        assert!(doc.warnings.is_empty());
        match &doc.blocks[0] {
            Block::Table { rows, .. } => {
                assert_eq!(rows[0][0], "hello, world");
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    #[test]
    fn csv_empty_file_produces_no_blocks() {
        let doc = CsvExtractor.extract(&make_input("empty.csv", "")).unwrap();
        assert!(doc.warnings.is_empty());
        assert!(doc.blocks.is_empty());
    }

    #[test]
    fn csv_headers_only_produces_empty_table() {
        let body = "name,age\n";
        let doc = CsvExtractor.extract(&make_input("data.csv", body)).unwrap();
        assert!(doc.warnings.is_empty());
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Table { headers, rows } => {
                assert_eq!(headers, &["name", "age"]);
                assert!(rows.is_empty());
            }
            other => panic!("expected Table, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // TSV tests
    // -----------------------------------------------------------------------

    #[test]
    fn tsv_happy_path() {
        let body = "name\tage\nAlice\t30\n";
        let doc = TsvExtractor.extract(&make_input("data.tsv", body)).unwrap();
        assert!(doc.warnings.is_empty());
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Table { headers, rows } => {
                assert_eq!(headers, &["name", "age"]);
                assert_eq!(rows[0], vec!["Alice", "30"]);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        assert_eq!(
            doc.metadata.extra.get("format").map(|s| s.as_str()),
            Some("tsv")
        );
    }

    #[test]
    fn tsv_empty_file() {
        let doc = TsvExtractor.extract(&make_input("empty.tsv", "")).unwrap();
        assert!(doc.blocks.is_empty());
    }

    // -----------------------------------------------------------------------
    // PSV tests
    // -----------------------------------------------------------------------

    #[test]
    fn psv_happy_path() {
        let body = "name|age\nAlice|30\nBob|25\n";
        let doc = PsvExtractor.extract(&make_input("data.psv", body)).unwrap();
        assert!(doc.warnings.is_empty());
        assert_eq!(doc.blocks.len(), 1);
        match &doc.blocks[0] {
            Block::Table { headers, rows } => {
                assert_eq!(headers, &["name", "age"]);
                assert_eq!(rows.len(), 2);
            }
            other => panic!("expected Table, got {other:?}"),
        }
        assert_eq!(
            doc.metadata.extra.get("format").map(|s| s.as_str()),
            Some("psv")
        );
    }

    #[test]
    fn psv_ragged_row_warns() {
        let body = "a|b\n1\n";
        let doc = PsvExtractor.extract(&make_input("data.psv", body)).unwrap();
        assert_eq!(doc.warnings.len(), 1);
        assert_eq!(doc.warnings[0].code, "csv.ragged_row");
    }

    #[test]
    fn psv_empty_file() {
        let doc = PsvExtractor.extract(&make_input("empty.psv", "")).unwrap();
        assert!(doc.blocks.is_empty());
    }
}
