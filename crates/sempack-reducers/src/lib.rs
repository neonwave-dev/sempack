//! Reducer plugins -- the compression profiles.
//!
//! P1 ships two. Both clean whitespace and drop empty blocks; `llm` additionally
//! squeezes structure for token economy. The profile set widens later (`compact`
//! soon, `debug` dev-flag, `rag` deferred) -- each is just another [`Reducer`].
//!
//! ## Diff-aware reduction
//!
//! [`DiffReducer`] is a sub-pass (not a top-level [`Reducer`] impl) invoked by
//! both `llm` and `compact` profiles. It compresses `git diff` output inside
//! [`Block::Code`] blocks without discarding any `+`/`-` lines.

use sempack_core::{Profile, Reducer, Result};
use sempack_ir::{Block, DocumentIr};

// ---------------------------------------------------------------------------
// Whitespace helpers (shared by all profiles)
// ---------------------------------------------------------------------------

/// Collapse all runs of whitespace in `s` into single spaces.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

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
                items.retain(|i| !i.is_empty());
            }
            _ => {}
        }
    }
}

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
// Diff-aware sub-pass
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct DiffConfig {
    pub max_context: usize,
    pub max_hunks: usize,
    pub max_files: usize,
}

impl Default for DiffConfig {
    fn default() -> Self {
        Self {
            max_context: 2,
            max_hunks: 10,
            max_files: 20,
        }
    }
}

pub struct DiffReducer {
    config: DiffConfig,
}

impl DiffReducer {
    pub fn new(config: DiffConfig) -> Self {
        Self { config }
    }

    pub fn apply(&self, doc: &mut DocumentIr) {
        for block in &mut doc.blocks {
            if let Block::Code { lang, text } = block {
                if is_diff_block(lang.as_deref(), text) {
                    *text = self.reduce_diff(text);
                }
            }
        }
    }

    fn reduce_diff(&self, text: &str) -> String {
        let trailing_newline = text.ends_with('\n');
        let lines: Vec<&str> = text.lines().collect();
        let mut file_sections: Vec<Vec<&str>> = Vec::new();
        let mut current: Vec<&str> = Vec::new();

        // Use `diff --git` as the file boundary for git diffs; fall back to
        // `--- ` for plain unified diffs.  Splitting on `--- ` alone is wrong
        // for git diffs because each file starts with `diff --git ...` / `index ...`
        // lines before the `---` header, so those prelude lines would land in the
        // *previous* section and skew the file count.
        let is_git_diff = lines.iter().any(|l| l.starts_with("diff --git "));
        let is_section_start = |l: &str| {
            if is_git_diff {
                l.starts_with("diff --git ")
            } else {
                l.starts_with("--- ")
            }
        };

        for &line in &lines {
            if is_section_start(line) && !current.is_empty() {
                file_sections.push(current);
                current = Vec::new();
            }
            current.push(line);
        }
        if !current.is_empty() {
            file_sections.push(current);
        }
        if file_sections.is_empty() {
            file_sections.push(lines.clone());
        }

        let total_files = file_sections.len();
        let omitted_files = total_files.saturating_sub(self.config.max_files);
        file_sections.truncate(self.config.max_files);

        let mut out = String::new();
        for section in &file_sections {
            let reduced = self.reduce_file_section(section);
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&reduced);
        }

        if omitted_files > 0 {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("[{omitted_files} files omitted]"));
        }

        // Preserve trailing newline from original text (text.lines() strips it).
        if trailing_newline {
            out.push('\n');
        }

        out
    }

    fn reduce_file_section(&self, lines: &[&str]) -> String {
        let hunk_starts: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter_map(|(i, l)| if l.starts_with("@@ ") { Some(i) } else { None })
            .collect();

        let total_hunks = hunk_starts.len();

        if total_hunks == 0 {
            return lines.join("\n");
        }

        let kept_hunks = total_hunks.min(self.config.max_hunks);
        let omitted_hunks = total_hunks - kept_hunks;

        let mut out = String::new();

        let first_hunk = hunk_starts[0];
        if first_hunk > 0 {
            out.push_str(&lines[..first_hunk].join("\n"));
        }

        for h in 0..kept_hunks {
            let start = hunk_starts[h];
            let end = if h + 1 < hunk_starts.len() {
                hunk_starts[h + 1]
            } else {
                lines.len()
            };
            let hunk_lines = &lines[start..end];

            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&self.reduce_hunk(hunk_lines));
        }

        if omitted_hunks > 0 {
            out.push('\n');
            out.push_str(&format!("[{omitted_hunks} hunks omitted]"));
        }

        out
    }

    fn reduce_hunk<'a>(&self, lines: &[&'a str]) -> String {
        if lines.is_empty() {
            return String::new();
        }

        let mut out = String::new();
        out.push_str(lines[0]);

        let mut context_run: Vec<&'a str> = Vec::new();
        let max_ctx = self.config.max_context;

        let flush_context = |run: &mut Vec<&str>, out: &mut String| {
            if run.is_empty() {
                return;
            }
            if run.len() <= max_ctx {
                for &l in run.iter() {
                    out.push('\n');
                    out.push_str(l);
                }
            } else {
                for &l in run.iter().take(max_ctx) {
                    out.push('\n');
                    out.push_str(l);
                }
                let omitted = run.len() - max_ctx;
                out.push('\n');
                out.push_str(&format!("[{omitted} context lines omitted]"));
            }
            run.clear();
        };

        for &line in &lines[1..] {
            if line.starts_with('+') || line.starts_with('-') {
                flush_context(&mut context_run, &mut out);
                out.push('\n');
                out.push_str(line);
            } else {
                context_run.push(line);
            }
        }

        flush_context(&mut context_run, &mut out);
        out
    }
}

fn is_diff_block(lang: Option<&str>, text: &str) -> bool {
    if let Some(l) = lang {
        if l.eq_ignore_ascii_case("diff") || l.eq_ignore_ascii_case("patch") {
            return true;
        }
    }
    let mut has_minus_header = false;
    let mut has_plus_header = false;
    let mut has_hunk = false;
    for line in text.lines().take(30) {
        if line.starts_with("--- ") {
            has_minus_header = true;
        }
        if line.starts_with("+++ ") {
            has_plus_header = true;
        }
        if line.starts_with("@@ ") && line.contains(" @@") {
            has_hunk = true;
        }
    }
    (has_minus_header && has_plus_header) || has_hunk
}

// ---------------------------------------------------------------------------
// Registered reducers
// ---------------------------------------------------------------------------

/// `human` -- light touch: tidy whitespace, drop empties, keep all structure.
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

/// `llm` -- human cleanup plus token-economy moves: merge adjacent paragraphs and
/// fold block quotes into paragraphs (the prose matters, the decoration does not).
/// Also applies diff-aware reduction to any `Code` blocks containing diffs.
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

        for b in &mut doc.blocks {
            if let Block::Quote { text } = b {
                *b = Block::Paragraph {
                    text: std::mem::take(text),
                };
            }
        }

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

        DiffReducer::new(DiffConfig::default()).apply(doc);

        Ok(())
    }
}

/// `compact` -- aggressive token-economy: whitespace cleanup, drop quotes entirely,
/// deduplicate identical paragraphs, merge consecutive short paragraphs
/// (combined length < 200 chars). Also applies diff-aware reduction.
pub struct CompactReducer;

impl Reducer for CompactReducer {
    fn name(&self) -> &'static str {
        "compact"
    }
    fn profile(&self) -> Profile {
        Profile::Compact
    }
    fn reduce(&self, doc: &mut DocumentIr) -> Result<()> {
        collapse_ws(doc);

        doc.blocks.retain(|b| !matches!(b, Block::Quote { .. }));

        drop_empty(doc);

        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(doc.blocks.len());
        doc.blocks.retain(|b| match b {
            Block::Paragraph { text } => seen.insert(text.clone()),
            _ => true,
        });

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

        DiffReducer::new(DiffConfig::default()).apply(doc);

        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use sempack_ir::SourceInfo;

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

    fn code_block(lang: Option<&str>, text: &str) -> Block {
        Block::Code {
            lang: lang.map(String::from),
            text: text.to_string(),
        }
    }

    #[test]
    fn human_collapses_and_drops() {
        let mut d = doc(vec![
            Block::Paragraph {
                text: "a    b\n c".into(),
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
        assert_eq!(d.blocks.len(), 2, "duplicate paragraph should be dropped");
    }

    #[test]
    fn compact_merges_short_consecutive_paragraphs() {
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
    // DiffReducer tests
    // -----------------------------------------------------------------------

    #[test]
    fn diff_small_no_change() {
        // 1 context line before + 1 after -- both under the cap of 2.
        let diff = concat!(
            "--- a/foo.rs\n",
            "+++ b/foo.rs\n",
            "@@ -1,5 +1,5 @@\n",
            " fn main() {\n",
            "-    let x = 1;\n",
            "+    let x = 2;\n",
            " }",
        );
        let mut d = doc(vec![code_block(Some("diff"), diff)]);
        LlmReducer.reduce(&mut d).unwrap();
        match &d.blocks[0] {
            Block::Code { text, .. } => assert_eq!(text, diff),
            other => panic!("expected Code block, got {other:?}"),
        }
    }

    #[test]
    fn diff_context_cap_fires() {
        let config = DiffConfig {
            max_context: 2,
            max_hunks: 10,
            max_files: 20,
        };
        // 5 context lines before change (cap=2 -> 3 omitted), 3 after (cap=2 -> 1 omitted).
        let diff = concat!(
            "--- a/foo.rs\n",
            "+++ b/foo.rs\n",
            "@@ -1,10 +1,10 @@\n",
            " ctx1\n",
            " ctx2\n",
            " ctx3\n",
            " ctx4\n",
            " ctx5\n",
            "-old line\n",
            "+new line\n",
            " trail1\n",
            " trail2\n",
            " trail3",
        );
        let result = DiffReducer::new(config).reduce_diff(diff);
        assert!(result.contains("-old line"), "must keep deletion");
        assert!(result.contains("+new line"), "must keep addition");
        assert!(
            result.contains("[3 context lines omitted]"),
            "must emit leading context sentinel: {result}"
        );
        assert!(
            result.contains("[1 context lines omitted]"),
            "must emit trailing context sentinel: {result}"
        );
        assert!(result.contains("--- a/foo.rs"), "must keep --- header");
        assert!(result.contains("+++ b/foo.rs"), "must keep +++ header");
    }

    #[test]
    fn diff_hunk_cap_fires() {
        let config = DiffConfig {
            max_context: 2,
            max_hunks: 3,
            max_files: 20,
        };
        let mut diff = String::from("--- a/big.rs\n+++ b/big.rs\n");
        for i in 0..5usize {
            let n = i * 10 + 1;
            diff.push_str(&format!(
                "@@ -{n},{n} +{n},{n} @@\n ctx\n-old{i}\n+new{i}\n"
            ));
        }
        let result = DiffReducer::new(config).reduce_diff(&diff);
        for i in 0..3usize {
            assert!(
                result.contains(&format!("-old{i}")),
                "kept hunk {i} deletion must be present"
            );
            assert!(
                result.contains(&format!("+new{i}")),
                "kept hunk {i} addition must be present"
            );
        }
        for i in 3..5usize {
            assert!(
                !result.contains(&format!("-old{i}")),
                "omitted hunk {i} must be absent"
            );
            assert!(
                !result.contains(&format!("+new{i}")),
                "omitted hunk {i} must be absent"
            );
        }
        assert!(
            result.contains("[2 hunks omitted]"),
            "must emit hunk sentinel: {result}"
        );
    }

    #[test]
    fn diff_file_cap_fires() {
        let config = DiffConfig {
            max_context: 2,
            max_hunks: 10,
            max_files: 3,
        };
        let mut diff = String::new();
        for i in 0..5usize {
            diff.push_str(&format!(
                "--- a/file{i}.rs\n+++ b/file{i}.rs\n@@ -1,3 +1,3 @@\n ctx\n-old\n+new\n"
            ));
        }
        let result = DiffReducer::new(config).reduce_diff(&diff);
        for i in 0..3usize {
            assert!(
                result.contains(&format!("--- a/file{i}.rs")),
                "file {i} must be present"
            );
        }
        for i in 3..5usize {
            assert!(
                !result.contains(&format!("--- a/file{i}.rs")),
                "file {i} must be omitted"
            );
        }
        assert!(
            result.contains("[2 files omitted]"),
            "must emit file sentinel: {result}"
        );
    }

    #[test]
    fn diff_human_profile_unaffected() {
        let diff = concat!(
            "--- a/foo.rs\n",
            "+++ b/foo.rs\n",
            "@@ -1,10 +1,10 @@\n",
            " ctx1\n",
            " ctx2\n",
            " ctx3\n",
            " ctx4\n",
            " ctx5\n",
            "-old line\n",
            "+new line",
        );
        let mut d = doc(vec![code_block(Some("diff"), diff)]);
        HumanReducer.reduce(&mut d).unwrap();
        match &d.blocks[0] {
            Block::Code { text, .. } => assert_eq!(text, diff, "human must not alter diff blocks"),
            other => panic!("expected Code block, got {other:?}"),
        }
    }

    #[test]
    fn diff_detected_by_content_sniff() {
        let diff = concat!(
            "--- a/foo.rs\n",
            "+++ b/foo.rs\n",
            "@@ -1,3 +1,3 @@\n",
            " ctx1\n",
            " ctx2\n",
            "-old line\n",
            "+new line",
        );
        let mut d = doc(vec![code_block(None, diff)]);
        LlmReducer.reduce(&mut d).unwrap();
        match &d.blocks[0] {
            Block::Code { text, .. } => assert_eq!(text, diff),
            other => panic!("expected Code block, got {other:?}"),
        }
    }

    #[test]
    fn diff_1000_line_trimmed_preserves_changes() {
        let mut diff = String::from("--- a/large.rs\n+++ b/large.rs\n@@ -1,900 +1,900 @@\n");
        for i in 0..100usize {
            diff.push_str(&format!(" context line {i}\n"));
        }
        for i in 0..400usize {
            diff.push_str(&format!("-removed line {i}\n"));
            diff.push_str(&format!("+added line {i}\n"));
        }
        let diff = diff.trim_end_matches('\n').to_string();

        let mut d = doc(vec![code_block(Some("diff"), &diff)]);
        LlmReducer.reduce(&mut d).unwrap();

        let result = match &d.blocks[0] {
            Block::Code { text, .. } => text.clone(),
            other => panic!("expected Code block, got {other:?}"),
        };

        assert!(
            result.len() < diff.len(),
            "llm must trim a large diff (before={}, after={})",
            diff.len(),
            result.len()
        );

        for i in 0..400usize {
            assert!(
                result.contains(&format!("-removed line {i}")),
                "deletion {i} must be preserved"
            );
            assert!(
                result.contains(&format!("+added line {i}")),
                "addition {i} must be preserved"
            );
        }

        assert!(
            result.contains("context lines omitted"),
            "context sentinel must be present"
        );
    }
}
