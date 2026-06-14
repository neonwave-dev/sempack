//! Built-in extractor plugins.
//!
//! P1 ships two: [`TextExtractor`] (plain text -> paragraphs) and [`MarkdownExtractor`]
//! (a pulldown-cmark-powered CommonMark parser covering headings, paragraphs, lists,
//! block quotes, fenced code with language tags, tables, and YAML/TOML front-matter
//! stripping).

use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use sempack_core::{Extractor, Input, Result};
use sempack_ir::{Block, DocumentIr, SourceInfo};

/// Build the `SourceInfo` for an input.
fn source(input: &Input) -> SourceInfo {
    SourceInfo {
        path: input.path.clone(),
        media_type: input.detected.media_type.clone(),
        detected_format: input.detected.format.clone(),
        bytes: input.bytes.len() as u64,
    }
}

/// Derive a stable document id from the file name (or `"document"`).
fn doc_id(input: &Input) -> String {
    input
        .path
        .as_deref()
        .and_then(|p| p.rsplit(['/', '\\']).next())
        .unwrap_or("document")
        .to_string()
}

// ---------------------------------------------------------------------------
// Plain text
// ---------------------------------------------------------------------------

/// Splits plain text into paragraphs on blank lines.
pub struct TextExtractor;

impl Extractor for TextExtractor {
    fn name(&self) -> &'static str {
        "text"
    }
    fn formats(&self) -> &'static [&'static str] {
        &["text"]
    }
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        let text = input.text();
        let mut doc = DocumentIr::new(doc_id(input), source(input));
        // Split paragraphs on blank lines via `lines()`, which strips both `\n`
        // and `\r\n` -- so Windows (CRLF) files aren't collapsed into one paragraph.
        let mut para: Vec<&str> = Vec::new();
        for line in text.lines() {
            if line.trim().is_empty() {
                push_paragraph(&mut doc, &mut para);
            } else {
                para.push(line);
            }
        }
        push_paragraph(&mut doc, &mut para);
        Ok(doc)
    }
}

/// Join the accumulated lines into one paragraph (internal whitespace collapsed)
/// and push it, unless empty. Clears `lines`.
fn push_paragraph(doc: &mut DocumentIr, lines: &mut Vec<&str>) {
    if lines.is_empty() {
        return;
    }
    let collapsed = lines
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    lines.clear();
    if !collapsed.is_empty() {
        doc.push(Block::Paragraph { text: collapsed });
    }
}

// ---------------------------------------------------------------------------
// Front-matter stripping
// ---------------------------------------------------------------------------

/// Strip YAML (`---` ... `---`) or TOML (`+++` ... `+++`) front-matter from
/// the start of the text. Returns (front_matter_lines, rest_of_text).
///
/// If the closing fence is never found the whole text is returned unchanged
/// (no front-matter stripped). This prevents treating an accidental `---` at
/// the top of a document as front-matter.
fn strip_front_matter(text: &str) -> (Vec<&str>, &str) {
    let mut lines = text.lines();
    let first = match lines.next() {
        Some(l) => l.trim_end(),
        None => return (Vec::new(), text),
    };

    let fence = match first {
        "---" => "---",
        "+++" => "+++",
        _ => return (Vec::new(), text),
    };

    let mut fm_lines: Vec<&str> = Vec::new();
    let text_bytes = text.as_bytes();

    // Skip past the opening fence line (including its newline).
    let mut byte_offset = first.len();
    byte_offset = skip_newline(text_bytes, byte_offset);

    loop {
        if byte_offset >= text_bytes.len() {
            // EOF without closing fence: bail out, treat as no front-matter.
            return (Vec::new(), text);
        }

        let line_start = byte_offset;
        let line_end = text_bytes[byte_offset..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| byte_offset + p)
            .unwrap_or(text_bytes.len());

        let raw_line = &text[line_start..line_end];
        let trimmed = raw_line.trim_end();

        if trimmed == fence {
            // Closing fence found -- advance past it and return rest.
            byte_offset = line_end;
            byte_offset = skip_newline(text_bytes, byte_offset);
            return (fm_lines, &text[byte_offset..]);
        }

        fm_lines.push(raw_line);
        byte_offset = line_end;
        byte_offset = skip_newline(text_bytes, byte_offset);
    }
}

/// Advance past a `\n` or `\r\n` at position `pos` in `bytes`.
fn skip_newline(bytes: &[u8], pos: usize) -> usize {
    if pos >= bytes.len() {
        return pos;
    }
    if bytes[pos] == b'\r' && pos + 1 < bytes.len() && bytes[pos + 1] == b'\n' {
        pos + 2
    } else if bytes[pos] == b'\n' {
        pos + 1
    } else {
        pos
    }
}

/// Parse front-matter lines as simple `key: value` (YAML) or `key = "value"` (TOML)
/// and return them as a flat list of pairs.
fn parse_front_matter_kv(lines: &[&str]) -> Vec<(String, String)> {
    lines
        .iter()
        .filter_map(|line| {
            // Try YAML-style: "key: value"
            if let Some((k, v)) = line.split_once(':') {
                let key = k.trim();
                let val = v.trim().trim_matches('"').trim_matches('\'');
                if !key.is_empty() && !key.contains(' ') {
                    return Some((key.to_string(), val.to_string()));
                }
            }
            // Try TOML-style: `key = "value"` (only if no colon branch matched)
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = v.trim().trim_matches('"');
                if !key.is_empty() && !key.contains(' ') {
                    return Some((key.to_string(), val.to_string()));
                }
            }
            None
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Markdown (pulldown-cmark powered)
// ---------------------------------------------------------------------------

/// CommonMark extractor powered by pulldown-cmark.
///
/// Handles headings, paragraphs, lists (ordered/unordered), block quotes,
/// fenced code blocks with language tags, tables, and YAML/TOML front-matter
/// (stripped and promoted to `DocumentIr.metadata.extra`).
pub struct MarkdownExtractor;

#[derive(Debug, Clone, PartialEq)]
enum Frame {
    Paragraph,
    Heading(u8),
    BlockQuote,
    ListItem,
    List { ordered: bool },
    CodeBlock { lang: Option<String> },
    Table,
    TableHead,
    TableRow,
    TableCell,
}

impl Extractor for MarkdownExtractor {
    fn name(&self) -> &'static str {
        "markdown"
    }
    fn formats(&self) -> &'static [&'static str] {
        &["markdown"]
    }
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        let raw = input.text();
        let mut doc = DocumentIr::new(doc_id(input), source(input));

        // --- Front-matter --------------------------------------------------
        let (fm_lines, md_text) = strip_front_matter(&raw);
        if !fm_lines.is_empty() {
            for (k, v) in parse_front_matter_kv(&fm_lines) {
                // Promote "title" to DocumentIr.title; everything else to extra.
                if k == "title" && doc.title.is_none() {
                    doc.title = Some(v.clone());
                }
                doc.metadata.extra.insert(k, v);
            }
        }

        // --- pulldown-cmark parse -------------------------------------------
        let mut opts = Options::empty();
        opts.insert(Options::ENABLE_TABLES);
        // ENABLE_SMART_PUNCTUATION is intentionally NOT set: SemPack is a faithful
        // extraction tool and must not rewrite source text (straight quotes -> curly,
        // -- -> en-dash, etc.) before downstream reducers/emitters see it.
        opts.insert(Options::ENABLE_STRIKETHROUGH);

        let events: Vec<Event<'_>> = Parser::new_ext(md_text, opts).collect();

        // State machine driven by a flat event stream.
        let mut stack: Vec<Frame> = Vec::new();
        // Inline text accumulator (paragraph / heading / quote / list-item text).
        let mut inline_buf = String::new();

        // Accumulated list items for one list level.
        let mut list_items: Vec<String> = Vec::new();
        let mut list_ordered = false;

        // Table accumulation.
        let mut table_headers: Vec<String> = Vec::new();
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut current_row: Vec<String> = Vec::new();
        let mut in_table_head = false;

        for event in events {
            match event {
                // ---- Headings -----------------------------------------------
                Event::Start(Tag::Heading { level, .. }) => {
                    inline_buf.clear();
                    stack.push(Frame::Heading(heading_level(level)));
                }
                Event::End(TagEnd::Heading(_)) => {
                    let text = std::mem::take(&mut inline_buf);
                    if let Some(Frame::Heading(level)) = stack.pop() {
                        if doc.title.is_none() && level == 1 {
                            doc.title = Some(text.clone());
                        }
                        doc.push(Block::Heading { level, text });
                    }
                }

                // ---- Paragraphs ---------------------------------------------
                Event::Start(Tag::Paragraph) => {
                    inline_buf.clear();
                    stack.push(Frame::Paragraph);
                }
                Event::End(TagEnd::Paragraph) => {
                    let text = std::mem::take(&mut inline_buf);
                    stack.pop();
                    if !text.is_empty() {
                        doc.push(Block::Paragraph { text });
                    }
                }

                // ---- Block quotes -------------------------------------------
                Event::Start(Tag::BlockQuote(_)) => {
                    inline_buf.clear();
                    stack.push(Frame::BlockQuote);
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    let text = std::mem::take(&mut inline_buf);
                    stack.pop();
                    if !text.is_empty() {
                        doc.push(Block::Quote { text });
                    }
                }

                // ---- Lists --------------------------------------------------
                Event::Start(Tag::List(start_num)) => {
                    list_ordered = start_num.is_some();
                    list_items.clear();
                    stack.push(Frame::List { ordered: list_ordered });
                }
                Event::End(TagEnd::List(_)) => {
                    let ordered = list_ordered;
                    let items = std::mem::take(&mut list_items);
                    stack.pop();
                    if !items.is_empty() {
                        doc.push(Block::List { ordered, items });
                    }
                }
                Event::Start(Tag::Item) => {
                    inline_buf.clear();
                    stack.push(Frame::ListItem);
                }
                Event::End(TagEnd::Item) => {
                    let text = std::mem::take(&mut inline_buf);
                    stack.pop();
                    list_items.push(text);
                }

                // ---- Code blocks --------------------------------------------
                Event::Start(Tag::CodeBlock(kind)) => {
                    let lang = match kind {
                        pulldown_cmark::CodeBlockKind::Fenced(ref tag) => {
                            let s = tag.as_ref().trim();
                            (!s.is_empty()).then(|| s.to_string())
                        }
                        pulldown_cmark::CodeBlockKind::Indented => None,
                    };
                    inline_buf.clear();
                    stack.push(Frame::CodeBlock { lang });
                }
                Event::End(TagEnd::CodeBlock) => {
                    let text = std::mem::take(&mut inline_buf);
                    if let Some(Frame::CodeBlock { lang }) = stack.pop() {
                        doc.push(Block::Code { lang, text });
                    }
                }

                // ---- Tables -------------------------------------------------
                Event::Start(Tag::Table(_)) => {
                    table_headers.clear();
                    table_rows.clear();
                    current_row.clear();
                    in_table_head = false;
                    stack.push(Frame::Table);
                }
                Event::End(TagEnd::Table) => {
                    stack.pop();
                    doc.push(Block::Table {
                        headers: std::mem::take(&mut table_headers),
                        rows: std::mem::take(&mut table_rows),
                    });
                }
                Event::Start(Tag::TableHead) => {
                    in_table_head = true;
                    current_row.clear();
                    stack.push(Frame::TableHead);
                }
                Event::End(TagEnd::TableHead) => {
                    in_table_head = false;
                    stack.pop();
                    table_headers = std::mem::take(&mut current_row);
                }
                Event::Start(Tag::TableRow) => {
                    current_row.clear();
                    stack.push(Frame::TableRow);
                }
                Event::End(TagEnd::TableRow) => {
                    stack.pop();
                    if !in_table_head {
                        table_rows.push(std::mem::take(&mut current_row));
                    }
                }
                Event::Start(Tag::TableCell) => {
                    inline_buf.clear();
                    stack.push(Frame::TableCell);
                }
                Event::End(TagEnd::TableCell) => {
                    let cell_text = std::mem::take(&mut inline_buf);
                    stack.pop();
                    current_row.push(cell_text);
                }

                // ---- Inline text --------------------------------------------
                Event::Text(t) | Event::Code(t) => {
                    inline_buf.push_str(&t);
                }
                Event::SoftBreak => {
                    inline_buf.push(' ');
                }
                Event::HardBreak => {
                    inline_buf.push('\n');
                }

                // ---- Inline formatting (open/close: text arrives as Event::Text)
                Event::Start(Tag::Emphasis)
                | Event::End(TagEnd::Emphasis)
                | Event::Start(Tag::Strong)
                | Event::End(TagEnd::Strong)
                | Event::Start(Tag::Strikethrough)
                | Event::End(TagEnd::Strikethrough)
                | Event::Start(Tag::Link { .. })
                | Event::End(TagEnd::Link)
                | Event::Start(Tag::Image { .. })
                | Event::End(TagEnd::Image) => {
                    // No action: child text is emitted as Event::Text.
                }

                // ---- Horizontal rule ----------------------------------------
                Event::Rule => {
                    // No semantic content in SemPack IR.
                }

                // ---- Raw HTML -----------------------------------------------
                Event::Html(html) | Event::InlineHtml(html) => {
                    let trimmed = html.trim();
                    if !trimmed.is_empty() {
                        doc.warn("unhandled:html", format!("Raw HTML skipped: {}", trimmed));
                    }
                }
                Event::Start(Tag::HtmlBlock) | Event::End(TagEnd::HtmlBlock) => {
                    // HTML block delimiters -- content arrives as Event::Html.
                }

                // ---- Footnotes ----------------------------------------------
                Event::FootnoteReference(label) => {
                    doc.warn(
                        "unhandled:footnote_reference",
                        format!("Footnote reference '[^{}]' skipped", label),
                    );
                }
                Event::Start(Tag::FootnoteDefinition(label)) => {
                    doc.warn(
                        "unhandled:footnote_definition",
                        format!("Footnote definition '[^{}]' skipped", label),
                    );
                }
                Event::End(TagEnd::FootnoteDefinition) => {}

                // ---- Task list markers --------------------------------------
                Event::TaskListMarker(checked) => {
                    let marker = if checked { "[x] " } else { "[ ] " };
                    inline_buf.insert_str(0, marker);
                    doc.warn(
                        "unhandled:task_list",
                        "Task list marker mapped to text prefix; GFM task-lists are not natively modelled in IR"
                            .to_string(),
                    );
                }

                // ---- Metadata blocks (not enabled; guard for completeness) --
                Event::Start(Tag::MetadataBlock(_)) | Event::End(TagEnd::MetadataBlock(_)) => {}

                // ---- Math (not enabled; guard for completeness) -------------
                Event::InlineMath(m) | Event::DisplayMath(m) => {
                    doc.warn(
                        "unhandled:math",
                        format!("Math expression skipped: {}", m.trim()),
                    );
                }

                // ---- Catch-all for new/unhandled variants -------------------
                #[allow(unreachable_patterns)]
                _ => {
                    doc.warn(
                        "unhandled:unknown",
                        "Unrecognised pulldown-cmark event skipped".to_string(),
                    );
                }
            }
        }

        Ok(doc)
    }
}

fn heading_level(level: HeadingLevel) -> u8 {
    match level {
        HeadingLevel::H1 => 1,
        HeadingLevel::H2 => 2,
        HeadingLevel::H3 => 3,
        HeadingLevel::H4 => 4,
        HeadingLevel::H5 => 5,
        HeadingLevel::H6 => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sempack_core::{detect, Input};

    fn input(name: &str, body: &str) -> Input {
        let bytes = body.as_bytes().to_vec();
        let detected = detect(Some(name), &bytes);
        Input {
            path: Some(name.to_string()),
            bytes,
            detected,
        }
    }

    // --- Existing tests (ported / adapted) ----------------------------------

    #[test]
    fn markdown_heading_and_list() {
        let doc = MarkdownExtractor
            .extract(&input("a.md", "# Title\n\nbody text\n\n- one\n- two\n"))
            .unwrap();
        assert_eq!(doc.title.as_deref(), Some("Title"));
        assert!(matches!(doc.blocks[0], Block::Heading { level: 1, .. }));
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::List { items, .. } if items.len() == 2)));
    }

    #[test]
    fn text_splits_paragraphs() {
        let doc = TextExtractor
            .extract(&input("a.txt", "one\nline\n\nsecond para"))
            .unwrap();
        assert_eq!(doc.blocks.len(), 2);
    }

    #[test]
    fn text_splits_paragraphs_with_crlf() {
        // Windows blank lines (\r\n\r\n) must still separate paragraphs.
        let doc = TextExtractor
            .extract(&input("a.txt", "first\r\n\r\nsecond\r\n\r\nthird"))
            .unwrap();
        assert_eq!(doc.blocks.len(), 3);
    }

    #[test]
    fn unterminated_empty_fence_no_panic() {
        // pulldown-cmark treats an unterminated fence as regular paragraph text.
        // We just verify no panic and at least one block is produced.
        let doc = MarkdownExtractor
            .extract(&input("a.md", "text\n\n```"))
            .unwrap();
        assert!(!doc.blocks.is_empty());
    }

    #[test]
    fn markdown_ordered_and_unordered_lists() {
        // Two list types separated by a blank line produce two Block::List entries.
        let md = "- bullet\n\n1. number\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        let lists: Vec<_> = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::List { .. }))
            .collect();
        assert_eq!(lists.len(), 2);
        assert!(matches!(lists[0], Block::List { ordered: false, .. }));
        assert!(matches!(lists[1], Block::List { ordered: true, .. }));
    }

    // --- New tests ----------------------------------------------------------

    #[test]
    fn table_produces_table_block() {
        let md = "| Name | Age |\n|------|-----|\n| Alice | 30 |\n| Bob | 25 |\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        let table = doc
            .blocks
            .iter()
            .find(|b| matches!(b, Block::Table { .. }))
            .expect("expected a Table block");
        match table {
            Block::Table { headers, rows } => {
                assert_eq!(headers, &["Name", "Age"]);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0], vec!["Alice", "30"]);
                assert_eq!(rows[1], vec!["Bob", "25"]);
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn yaml_front_matter_not_emitted_as_paragraph() {
        let md = "---\ntitle: My Doc\nauthor: Alice\n---\n\n# Hello\n\nBody text.\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        // No block should contain raw front-matter text.
        for block in &doc.blocks {
            if let Block::Paragraph { text } = block {
                assert!(
                    !text.contains("title:") && !text.contains("author:"),
                    "Front-matter leaked into paragraph: {text}"
                );
            }
        }
        // Title should be promoted.
        assert_eq!(doc.title.as_deref(), Some("My Doc"));
        assert_eq!(
            doc.metadata.extra.get("author").map(|s| s.as_str()),
            Some("Alice")
        );
    }

    #[test]
    fn toml_front_matter_stripped() {
        let md = "+++\ntitle = \"TOML Doc\"\nversion = \"1\"\n+++\n\n# Section\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        for block in &doc.blocks {
            if let Block::Paragraph { text } = block {
                assert!(
                    !text.contains("+++") && !text.contains("version ="),
                    "TOML front-matter leaked into paragraph: {text}"
                );
            }
        }
        assert_eq!(
            doc.metadata.extra.get("version").map(|s| s.as_str()),
            Some("1")
        );
    }

    #[test]
    fn fenced_code_block_with_language() {
        let md = "```rust\nfn main() {}\n```\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        let code = doc
            .blocks
            .iter()
            .find(|b| matches!(b, Block::Code { .. }))
            .expect("expected a Code block");
        match code {
            Block::Code { lang, text } => {
                assert_eq!(lang.as_deref(), Some("rust"));
                assert!(text.contains("fn main()"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn fenced_code_block_without_language() {
        let md = "```\nsome code\n```\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        let code = doc
            .blocks
            .iter()
            .find(|b| matches!(b, Block::Code { .. }))
            .expect("expected a Code block");
        match code {
            Block::Code { lang, text } => {
                assert!(lang.is_none());
                assert!(text.contains("some code"));
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn mixed_content_headings_table_code_paragraph() {
        let md = concat!(
            "# Title\n\n",
            "Some intro text.\n\n",
            "| A | B |\n|---|---|\n| 1 | 2 |\n\n",
            "```python\nprint('hello')\n```\n\n",
            "Closing paragraph.\n",
        );
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Heading { level: 1, .. })));
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Paragraph { .. })));
        assert!(doc
            .blocks
            .iter()
            .any(|b| matches!(b, Block::Table { .. })));
        assert!(doc.blocks.iter().any(|b| matches!(
            b,
            Block::Code { lang, .. } if lang.as_deref() == Some("python")
        )));
    }

    #[test]
    fn empty_file_no_panic() {
        let doc = MarkdownExtractor.extract(&input("a.md", "")).unwrap();
        assert!(doc.blocks.is_empty());
    }

    #[test]
    fn malformed_front_matter_no_panic() {
        // Opening fence but no closing fence -- treated as regular MD text, no panic.
        let md = "---\ntitle: Oops\n\n# Heading\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        assert!(!doc.blocks.is_empty());
    }

    #[test]
    fn front_matter_with_no_body_no_panic() {
        let md = "---\ntitle: Solo\n---\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        assert_eq!(doc.title.as_deref(), Some("Solo"));
        assert!(doc.blocks.is_empty());
    }
}
