//! Reducer plugins — the compression profiles.
//!
//! P1 ships two. Both clean whitespace and drop empty blocks; `llm` additionally
//! squeezes structure for token economy. The profile set widens later (`compact`
//! soon, `debug` dev-flag, `rag` deferred) — each is just another [`Reducer`].

use sempack_core::{Profile, Reducer, Result};
use sempack_ir::{Block, DocumentIr, Field};

/// Collapse all runs of whitespace in `s` into single spaces.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapse runs of whitespace in the prose blocks (paragraph / heading / quote /
/// list items). Code blocks and structured blocks (table / record / unsupported) are
/// left as-is, and container blocks are not recursed into.
fn collapse_ws(doc: &mut DocumentIr) {
    for b in &mut doc.blocks {
        match b {
            Block::Paragraph { text } | Block::Heading { text, .. } | Block::Quote { text } => {
                *text = collapse(text);
            }
            Block::List { items, .. } => {
                for i in items.iter_mut() {
                    *i = collapse(i);
                }
                // An item that was only whitespace collapses to "" — drop it so we
                // never emit a blank bullet (`- `). A list emptied this way is then
                // discarded by `drop_empty`.
                items.retain(|i| !i.is_empty());
            }
            _ => {}
        }
    }
}

/// Drop blocks that carry no content after cleanup.
fn drop_empty(doc: &mut DocumentIr) {
    doc.blocks.retain(|b| match b {
        Block::Paragraph { text } | Block::Heading { text, .. } | Block::Quote { text } => {
            !text.trim().is_empty()
        }
        Block::List { items, .. } => !items.is_empty(),
        _ => true,
    });
}

// ---------------------------------------------------------------------------
// JSON array trimming (llm + compact profiles only)
// ---------------------------------------------------------------------------

/// Returns `true` when the document originated from a JSON or JSONL extractor.
/// This is the gate that prevents CSV/TSV/Markdown tables from being trimmed.
fn is_json_format(doc: &DocumentIr) -> bool {
    matches!(
        doc.metadata.extra.get("format").map(|s| s.as_str()),
        Some("json") | Some("jsonl")
    )
}

/// Keywords (lowercase) that, when found anywhere in a cell value, flag the
/// row as an outlier/error row that should always be preserved.
const OUTLIER_KEYWORDS: &[&str] = &["error", "fail", "exception"];

/// Return `true` if any cell in a table row is an outlier indicator.
fn is_table_row_outlier(row: &[String]) -> bool {
    row.iter().any(|cell| {
        let lower = cell.to_ascii_lowercase();
        // Keyword match.
        if OUTLIER_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
            return true;
        }
        // Explicit null value.
        if lower == "null" {
            return true;
        }
        false
    })
}

/// Return `true` if any field value in a Record block is an outlier indicator.
fn is_record_outlier(fields: &[Field]) -> bool {
    fields.iter().any(|f| {
        let lower = f.value.to_ascii_lowercase();
        if OUTLIER_KEYWORDS.iter().any(|kw| lower.contains(kw)) {
            return true;
        }
        if lower == "null" {
            return true;
        }
        false
    })
}

/// Build the sentinel paragraph text for a trim operation.
///
/// `total_rows` — original row count before trim
/// `kept` — rows kept in output (first-N + outliers rescued beyond N)
/// `outlier_count` — rescued outliers beyond the first-N window
fn sentinel_text(total_rows: usize, kept: usize, outlier_count: usize) -> String {
    let omitted = total_rows.saturating_sub(kept);
    let outlier_label = if outlier_count == 1 { "row" } else { "rows" };
    format!("[{omitted} rows omitted — {kept} shown including {outlier_count} outlier {outlier_label}]")
}

/// Trim large JSON arrays in a document down to a representative sample.
///
/// Only fires when:
/// - `is_json_format(doc)` returns `true` (guards CSV/TSV/Markdown tables), AND
/// - the row/record count exceeds `keep_n`.
///
/// Handles two IR shapes produced by JSON/JSONL extraction:
/// - `Block::Table` (uniform JSON arrays via `try_table`) — rebuilt with a trimmed row set and sentinel.
/// - Contiguous runs of `Block::Record` blocks (mixed JSON arrays + JSONL) — the
///   run is sliced, and a sentinel paragraph appended after the run.
///
/// Sentinel: `[N rows omitted — N_kept shown including N_outliers outlier rows]`
/// Always appended directly after the trimmed block.
fn trim_data_rows(doc: &mut DocumentIr, keep_n: usize) {
    if !is_json_format(doc) {
        return;
    }

    // Rebuild the block list so we can splice in sentinels after runs.
    let mut output: Vec<Block> = Vec::with_capacity(doc.blocks.len() + 2);
    let mut i = 0;
    let blocks = std::mem::take(&mut doc.blocks);

    while i < blocks.len() {
        let b = &blocks[i];
        match b {
            Block::Table { headers, rows } => {
                let total = rows.len();
                if total <= keep_n {
                    // Small table — pass through unchanged.
                    output.push(blocks[i].clone());
                } else {
                    // Partition: first-N rows + outliers beyond N.
                    let first_n: Vec<Vec<String>> = rows[..keep_n].to_vec();
                    let rescued: Vec<Vec<String>> = rows[keep_n..]
                        .iter()
                        .filter(|row| is_table_row_outlier(row))
                        .cloned()
                        .collect();
                    let outlier_count = rescued.len();
                    let mut kept_rows = first_n;
                    kept_rows.extend(rescued);
                    let kept = kept_rows.len();

                    output.push(Block::Table {
                        headers: headers.clone(),
                        rows: kept_rows,
                    });
                    output.push(Block::Paragraph {
                        text: sentinel_text(total, kept, outlier_count),
                    });
                }
                i += 1;
            }

            Block::Record { .. } => {
                // Collect the whole contiguous run of Record blocks starting at i.
                let run_start = i;
                while i < blocks.len() && matches!(blocks[i], Block::Record { .. }) {
                    i += 1;
                }
                let run = &blocks[run_start..i];
                let total = run.len();

                if total <= keep_n {
                    output.extend_from_slice(run);
                } else {
                    // Keep first-N + outliers from the tail.
                    let first_n = &run[..keep_n];
                    let rescued: Vec<&Block> = run[keep_n..]
                        .iter()
                        .filter(|blk| {
                            if let Block::Record { fields } = blk {
                                is_record_outlier(fields)
                            } else {
                                false
                            }
                        })
                        .collect();
                    let outlier_count = rescued.len();
                    let kept = keep_n + outlier_count;

                    output.extend_from_slice(first_n);
                    for blk in &rescued {
                        output.push((*blk).clone());
                    }
                    output.push(Block::Paragraph {
                        text: sentinel_text(total, kept, outlier_count),
                    });
                }
            }

            _ => {
                output.push(blocks[i].clone());
                i += 1;
            }
        }
    }

    doc.blocks = output;
}

// ---------------------------------------------------------------------------
// Reducer implementations
// ---------------------------------------------------------------------------

/// `human` — light touch: tidy whitespace, drop empties, keep all structure.
pub struct HumanReducer;

impl Reducer for HumanReducer {
    fn name(&self) -> &'static str {
        "human"
    }
    fn profile(&self) -> Profile {
        Profile::Human
    }
    fn reduce(&self, doc: &mut DocumentIr) -> Result<()> {
        collapse_ws(doc);
        drop_empty(doc);
        Ok(())
    }
}

/// Keep at most this many rows in `llm` profile JSON array trimming.
const LLM_JSON_KEEP_ROWS: usize = 20;

/// `llm` — human cleanup plus token-economy moves: merge adjacent paragraphs and
/// fold block quotes into paragraphs (the prose matters, the decoration does not).
/// Also trims large JSON arrays to at most [`LLM_JSON_KEEP_ROWS`] rows, preserving
/// outlier/error rows and appending a sentinel.
pub struct LlmReducer;

impl Reducer for LlmReducer {
    fn name(&self) -> &'static str {
        "llm"
    }
    fn profile(&self) -> Profile {
        Profile::Llm
    }
    fn reduce(&self, doc: &mut DocumentIr) -> Result<()> {
        collapse_ws(doc);
        drop_empty(doc);

        // Quotes — paragraphs (drop the citation framing for the model).
        for b in &mut doc.blocks {
            if let Block::Quote { text } = b {
                *b = Block::Paragraph {
                    text: std::mem::take(text),
                };
            }
        }

        // Merge consecutive paragraphs into one.
        let mut merged: Vec<Block> = Vec::with_capacity(doc.blocks.len());
        for b in doc.blocks.drain(..) {
            if let (Some(Block::Paragraph { text: prev }), Block::Paragraph { text }) =
                (merged.last_mut(), &b)
            {
                prev.push(' ');
                prev.push_str(text);
            } else {
                merged.push(b);
            }
        }
        doc.blocks = merged;

        // Trim large JSON arrays last so the sentinel paragraph is not merged
        // into adjacent paragraphs by the merge pass above.
        trim_data_rows(doc, LLM_JSON_KEEP_ROWS);

        Ok(())
    }
}

/// Keep at most this many rows in `compact` profile JSON array trimming.
const COMPACT_JSON_KEEP_ROWS: usize = 10;

/// `compact` -- aggressive token-economy: whitespace cleanup, drop quotes entirely,
/// deduplicate identical paragraphs, merge consecutive short paragraphs
/// (combined length < 200 chars). Targets >=40% block-count reduction vs `human`
/// on typical prose. Also trims large JSON arrays to at most [`COMPACT_JSON_KEEP_ROWS`]
/// rows, preserving outlier/error rows and appending a sentinel.
pub struct CompactReducer;

impl Reducer for CompactReducer {
    fn name(&self) -> &'static str {
        "compact"
    }
    fn profile(&self) -> Profile {
        Profile::Compact
    }
    fn reduce(&self, doc: &mut DocumentIr) -> Result<()> {
        // 1. Collapse whitespace runs in prose blocks.
        collapse_ws(doc);

        // 2. Drop Block::Quote entirely (aggressive -- the citation framing and
        //    the quoted prose both go; compact cares only about token savings).
        doc.blocks.retain(|b| !matches!(b, Block::Quote { .. }));

        // 3. Drop empty blocks.
        drop_empty(doc);

        // 4. Deduplicate paragraphs with identical text (keep first occurrence).
        // collapse_ws already trimmed all prose blocks, so no extra trim needed.
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(doc.blocks.len());
        doc.blocks.retain(|b| match b {
            Block::Paragraph { text } => seen.insert(text.clone()),
            _ => true,
        });

        // 5. Merge consecutive paragraphs whose combined length is < 200 chars.
        let mut merged: Vec<Block> = Vec::with_capacity(doc.blocks.len());
        for b in doc.blocks.drain(..) {
            if let (Some(Block::Paragraph { text: prev }), Block::Paragraph { text: next }) =
                (merged.last_mut(), &b)
            {
                if prev.chars().count() + 1 + next.chars().count() < 200 {
                    prev.push(' ');
                    prev.push_str(next);
                    continue;
                }
            }
            merged.push(b);
        }
        doc.blocks = merged;

        // 6. Trim large JSON arrays last so the sentinel paragraph is not merged
        //    into adjacent paragraphs by the merge pass above.
        trim_data_rows(doc, COMPACT_JSON_KEEP_ROWS);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sempack_ir::{Field, SourceInfo};

    fn doc(blocks: Vec<Block>) -> DocumentIr {
        let mut d = DocumentIr::new(
            "t",
            SourceInfo {
                path: None,
                media_type: None,
                detected_format: "markdown".into(),
                bytes: 0,
            },
        );
        d.blocks = blocks;
        d
    }

    /// Build a DocumentIr with `format` metadata set to `"json"` (as the JSON extractor
    /// would produce), plus the given blocks.
    fn json_doc(blocks: Vec<Block>) -> DocumentIr {
        let mut d = doc(blocks);
        d.metadata.extra.insert("format".into(), "json".into());
        d
    }

    /// Build a DocumentIr with `format` metadata set to `"jsonl"`.
    fn jsonl_doc(blocks: Vec<Block>) -> DocumentIr {
        let mut d = doc(blocks);
        d.metadata.extra.insert("format".into(), "jsonl".into());
        d
    }

    /// Build a Block::Table with the given number of uniform rows (no outliers).
    fn uniform_table(row_count: usize) -> Block {
        let headers = vec!["id".into(), "name".into(), "value".into()];
        let rows = (0..row_count)
            .map(|i| vec![i.to_string(), format!("item_{i}"), (i * 10).to_string()])
            .collect();
        Block::Table { headers, rows }
    }

    /// Build a Vec<Block::Record> with the given number of uniform records.
    fn uniform_records(count: usize) -> Vec<Block> {
        (0..count)
            .map(|i| Block::Record {
                fields: vec![
                    Field {
                        key: "id".into(),
                        value: i.to_string(),
                    },
                    Field {
                        key: "name".into(),
                        value: format!("item_{i}"),
                    },
                ],
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Pre-existing tests (unchanged)
    // -----------------------------------------------------------------------

    #[test]
    fn human_collapses_and_drops() {
        let mut d = doc(vec![
            Block::Paragraph {
                text: "a    b
 c"
                .into(),
            },
            Block::Paragraph { text: "   ".into() },
        ]);
        HumanReducer.reduce(&mut d).unwrap();
        assert_eq!(d.blocks.len(), 1);
        assert_eq!(
            match &d.blocks[0] {
                Block::Paragraph { text } => text.as_str(),
                _ => "",
            },
            "a b c"
        );
    }

    #[test]
    fn llm_merges_paragraphs() {
        let mut d = doc(vec![
            Block::Paragraph { text: "one".into() },
            Block::Paragraph { text: "two".into() },
        ]);
        LlmReducer.reduce(&mut d).unwrap();
        assert_eq!(d.blocks.len(), 1);
    }

    #[test]
    fn whitespace_only_list_items_are_pruned() {
        // A list whose items are all whitespace must be dropped, not emitted as `- `.
        let mut d = doc(vec![Block::List {
            ordered: false,
            items: vec!["  ".into(), "real".into(), "\t".into()],
        }]);
        HumanReducer.reduce(&mut d).unwrap();
        match d.blocks.as_slice() {
            [Block::List { items, .. }] => assert_eq!(items, &["real".to_string()]),
            other => panic!("expected one list with one item, got {other:?}"),
        }
    }

    #[test]
    fn compact_drops_quotes() {
        let mut d = doc(vec![
            Block::Paragraph {
                text: "intro".into(),
            },
            Block::Quote {
                text: "someone said something".into(),
            },
            Block::Paragraph {
                text: "outro".into(),
            },
        ]);
        CompactReducer.reduce(&mut d).unwrap();
        assert!(
            d.blocks.iter().all(|b| !matches!(b, Block::Quote { .. })),
            "compact must drop all quote blocks"
        );
    }

    #[test]
    fn compact_deduplicates_paragraphs() {
        // Use long texts so the merge step does not coalesce them.
        let para = "x".repeat(150);
        let other = "y".repeat(150);
        let mut d = doc(vec![
            Block::Paragraph { text: para.clone() },
            Block::Paragraph {
                text: other.clone(),
            },
            Block::Paragraph { text: para.clone() },
        ]);
        CompactReducer.reduce(&mut d).unwrap();
        // The duplicate third block should be gone; para + other should not merge
        // (combined 150 + 1 + 150 = 301 >= 200).
        assert_eq!(d.blocks.len(), 2, "duplicate paragraph should be dropped");
    }

    #[test]
    fn compact_merges_short_consecutive_paragraphs() {
        // Two short paragraphs (well under 200 combined) must be merged.
        let mut d = doc(vec![
            Block::Paragraph {
                text: "short one".into(),
            },
            Block::Paragraph {
                text: "short two".into(),
            },
        ]);
        CompactReducer.reduce(&mut d).unwrap();
        assert_eq!(d.blocks.len(), 1, "short paragraphs should be merged");
    }

    #[test]
    fn compact_does_not_merge_long_paragraphs() {
        // Two paragraphs with different long texts (>=200 combined) must stay separate.
        // Different texts ensure dedup does not collapse them first.
        let long_a = "x".repeat(150);
        let long_b = "y".repeat(150);
        let mut d = doc(vec![
            Block::Paragraph {
                text: long_a.clone(),
            },
            Block::Paragraph {
                text: long_b.clone(),
            },
        ]);
        CompactReducer.reduce(&mut d).unwrap();
        assert_eq!(d.blocks.len(), 2, "long paragraphs must not be merged");
    }

    #[test]
    fn compact_achieves_block_count_reduction_vs_human() {
        // Fixture: quotes, duplicate paragraphs, short consecutive paragraphs.
        // Compact must produce <=60% of human block count (i.e., >=40% reduction).
        let fixture = vec![
            Block::Paragraph {
                text: "Introduction paragraph that sets the scene.".into(),
            },
            Block::Quote {
                text: "A famous quote about something interesting.".into(),
            },
            Block::Paragraph {
                text: "Short note.".into(),
            },
            Block::Paragraph {
                text: "Another short note.".into(),
            },
            Block::Paragraph {
                text: "Yet another short note.".into(),
            },
            Block::Quote {
                text: "Another quotation that adds bulk.".into(),
            },
            Block::Paragraph {
                text: "Duplicate paragraph appears here.".into(),
            },
            Block::Paragraph {
                text: "Non-duplicate content.".into(),
            },
            Block::Paragraph {
                text: "Duplicate paragraph appears here.".into(),
            },
            Block::Paragraph {
                text: "Concluding thoughts.".into(),
            },
        ];

        let mut human_doc = doc(fixture.clone());
        HumanReducer.reduce(&mut human_doc).unwrap();
        let human_count = human_doc.blocks.len();

        let mut compact_doc = doc(fixture);
        CompactReducer.reduce(&mut compact_doc).unwrap();
        let compact_count = compact_doc.blocks.len();

        assert!(
            compact_count * 100 <= human_count * 60,
            "compact ({compact_count} blocks) must be <=60% of human ({human_count} blocks)"
        );
    }

    // -----------------------------------------------------------------------
    // JSON array trimming -- Table path (uniform arrays -> Block::Table)
    // -----------------------------------------------------------------------

    #[test]
    fn trim_table_small_array_no_trim() {
        // Small array (5 rows < llm keep=20): no trimming, no sentinel.
        let mut d = json_doc(vec![uniform_table(5)]);
        LlmReducer.reduce(&mut d).unwrap();
        assert_eq!(d.blocks.len(), 1, "small table should not be trimmed");
        assert!(matches!(d.blocks[0], Block::Table { .. }));
    }

    #[test]
    fn trim_table_llm_large_uniform_array() {
        // 200-row uniform JSON array -> llm profile keeps 20 rows + sentinel.
        let mut d = json_doc(vec![uniform_table(200)]);
        LlmReducer.reduce(&mut d).unwrap();

        // 2 blocks: trimmed Table + sentinel Paragraph.
        assert_eq!(d.blocks.len(), 2, "expected table + sentinel");
        match &d.blocks[0] {
            Block::Table { headers, rows } => {
                assert_eq!(headers, &["id", "name", "value"]);
                assert_eq!(
                    rows.len(),
                    LLM_JSON_KEEP_ROWS,
                    "should keep exactly 20 rows"
                );
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match &d.blocks[1] {
            Block::Paragraph { text } => {
                // 200 total, 20 kept, 0 outliers rescued beyond the first 20.
                assert_eq!(
                    text,
                    "[180 rows omitted — 20 shown including 0 outlier rows]"
                );
            }
            other => panic!("expected sentinel Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn trim_table_compact_large_uniform_array() {
        // 200-row uniform JSON array -> compact profile keeps 10 rows + sentinel.
        let mut d = json_doc(vec![uniform_table(200)]);
        CompactReducer.reduce(&mut d).unwrap();

        assert_eq!(d.blocks.len(), 2, "expected table + sentinel");
        match &d.blocks[0] {
            Block::Table { rows, .. } => {
                assert_eq!(
                    rows.len(),
                    COMPACT_JSON_KEEP_ROWS,
                    "should keep exactly 10 rows"
                );
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match &d.blocks[1] {
            Block::Paragraph { text } => {
                assert_eq!(
                    text,
                    "[190 rows omitted — 10 shown including 0 outlier rows]"
                );
            }
            other => panic!("expected sentinel Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn trim_table_outlier_rows_preserved() {
        // 30-row table where row index 25 contains an "error" value.
        // LLM profile (keep=20): rows 0-19 kept; row 25 (outlier) rescued from tail.
        let headers = vec!["id".into(), "status".into()];
        let rows: Vec<Vec<String>> = (0..30_usize)
            .map(|i| {
                let status = if i == 25 {
                    "error: timeout".into()
                } else {
                    "ok".into()
                };
                vec![i.to_string(), status]
            })
            .collect();
        let mut d = json_doc(vec![Block::Table { headers, rows }]);
        LlmReducer.reduce(&mut d).unwrap();

        assert_eq!(d.blocks.len(), 2, "expected trimmed table + sentinel");
        match &d.blocks[0] {
            Block::Table { rows, .. } => {
                // 20 first-N + 1 outlier (row 25).
                assert_eq!(rows.len(), 21, "20 normal + 1 outlier row");
                // The outlier row is appended at the end.
                assert_eq!(rows[20][1], "error: timeout");
            }
            other => panic!("expected Table, got {other:?}"),
        }
        match &d.blocks[1] {
            Block::Paragraph { text } => {
                // 30 total, 21 kept (20 first-N + 1 outlier), 1 outlier.
                assert_eq!(text, "[9 rows omitted — 21 shown including 1 outlier row]");
            }
            other => panic!("expected sentinel Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn trim_table_human_profile_unaffected() {
        // Human profile must not trim, even for large JSON arrays.
        let mut d = json_doc(vec![uniform_table(200)]);
        HumanReducer.reduce(&mut d).unwrap();
        match &d.blocks[0] {
            Block::Table { rows, .. } => {
                assert_eq!(rows.len(), 200, "human profile must not trim");
            }
            other => panic!("expected Table, got {other:?}"),
        }
        assert_eq!(d.blocks.len(), 1, "human profile must not add sentinel");
    }

    #[test]
    fn trim_table_csv_unaffected() {
        // CSV tables (format=csv) must never be trimmed.
        let mut d = doc(vec![Block::Table {
            headers: vec!["a".into(), "b".into()],
            rows: (0..200).map(|i| vec![i.to_string(), "x".into()]).collect(),
        }]);
        d.metadata.extra.insert("format".into(), "csv".into());
        LlmReducer.reduce(&mut d).unwrap();
        match &d.blocks[0] {
            Block::Table { rows, .. } => {
                assert_eq!(rows.len(), 200, "CSV tables must not be trimmed");
            }
            other => panic!("expected Table, got {other:?}"),
        }
        assert_eq!(d.blocks.len(), 1, "no sentinel for CSV");
    }

    // -----------------------------------------------------------------------
    // JSON array trimming -- Record sequence path (mixed arrays + JSONL)
    // -----------------------------------------------------------------------

    #[test]
    fn trim_records_small_sequence_no_trim() {
        // 5 records < llm keep=20: no trimming, no sentinel.
        let mut d = jsonl_doc(uniform_records(5));
        LlmReducer.reduce(&mut d).unwrap();
        // No sentinel, just the records (Records are left alone by the merge pass).
        assert_eq!(d.blocks.len(), 5);
        assert!(d.blocks.iter().all(|b| matches!(b, Block::Record { .. })));
    }

    #[test]
    fn trim_records_llm_large_sequence() {
        // 50 JSONL records -> llm keeps 20 + sentinel.
        let mut d = jsonl_doc(uniform_records(50));
        LlmReducer.reduce(&mut d).unwrap();

        // 20 records + 1 sentinel paragraph.
        assert_eq!(d.blocks.len(), 21, "expected 20 records + sentinel");
        assert!(d.blocks[..20]
            .iter()
            .all(|b| matches!(b, Block::Record { .. })));
        match &d.blocks[20] {
            Block::Paragraph { text } => {
                assert_eq!(
                    text,
                    "[30 rows omitted — 20 shown including 0 outlier rows]"
                );
            }
            other => panic!("expected sentinel Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn trim_records_outlier_preserved() {
        // 30 JSONL records; record at index 25 has a "fail" field value.
        let records: Vec<Block> = (0..30_usize)
            .map(|i| {
                let status = if i == 25 { "fail" } else { "ok" };
                Block::Record {
                    fields: vec![
                        Field {
                            key: "id".into(),
                            value: i.to_string(),
                        },
                        Field {
                            key: "status".into(),
                            value: status.into(),
                        },
                    ],
                }
            })
            .collect();
        let mut d = jsonl_doc(records);
        LlmReducer.reduce(&mut d).unwrap();

        // 20 first-N + 1 outlier + 1 sentinel.
        assert_eq!(
            d.blocks.len(),
            22,
            "expected 20 records + outlier + sentinel"
        );
        // Record at position 20 should be the outlier (index 25 in original).
        match &d.blocks[20] {
            Block::Record { fields } => {
                let status = fields.iter().find(|f| f.key == "status").unwrap();
                assert_eq!(status.value, "fail");
            }
            other => panic!("expected Record outlier, got {other:?}"),
        }
        match &d.blocks[21] {
            Block::Paragraph { text } => {
                assert_eq!(text, "[9 rows omitted — 21 shown including 1 outlier row]");
            }
            other => panic!("expected sentinel Paragraph, got {other:?}"),
        }
    }

    #[test]
    fn trim_records_null_value_is_outlier() {
        // Records where a field value is "null" are flagged as outliers.
        let records: Vec<Block> = (0..25_usize)
            .map(|i| Block::Record {
                fields: vec![
                    Field {
                        key: "id".into(),
                        value: i.to_string(),
                    },
                    Field {
                        key: "data".into(),
                        // Record 22 has a null value -- should be rescued.
                        value: if i == 22 {
                            "null".into()
                        } else {
                            "good".into()
                        },
                    },
                ],
            })
            .collect();
        let mut d = jsonl_doc(records);
        LlmReducer.reduce(&mut d).unwrap();

        // 20 first-N + 1 null-outlier (index 22) + sentinel.
        assert_eq!(d.blocks.len(), 22, "expected 21 records + sentinel");
        match &d.blocks[20] {
            Block::Record { fields } => {
                let data = fields.iter().find(|f| f.key == "data").unwrap();
                assert_eq!(data.value, "null", "null-field record must be rescued");
            }
            other => panic!("expected null-outlier Record, got {other:?}"),
        }
    }
}
