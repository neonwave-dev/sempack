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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrontMatterKind {
    Yaml,
    Toml,
}

/// Strip YAML (`---` ... `---`) or TOML (`+++` ... `+++`) front-matter from
/// the start of the text. Returns (kind, front_matter_lines, rest_of_text).
///
/// Returns `(None, [], text)` if no front-matter is found or if the closing
/// fence is missing (treats an accidental opening fence as plain content).
fn strip_front_matter(text: &str) -> (Option<FrontMatterKind>, Vec<&str>, &str) {
    let mut lines = text.lines();
    let first = match lines.next() {
        Some(l) => l.trim_end(),
        None => return (None, Vec::new(), text),
    };

    let (fence, kind) = match first {
        "---" => ("---", FrontMatterKind::Yaml),
        "+++" => ("+++", FrontMatterKind::Toml),
        _ => return (None, Vec::new(), text),
    };

    let mut fm_lines: Vec<&str> = Vec::new();
    let text_bytes = text.as_bytes();

    // Skip past the opening fence line (including its newline).
    let mut byte_offset = first.len();
    byte_offset = skip_newline(text_bytes, byte_offset);

    loop {
        if byte_offset >= text_bytes.len() {
            // EOF without closing fence: bail out, treat as no front-matter.
            return (None, Vec::new(), text);
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
            return (Some(kind), fm_lines, &text[byte_offset..]);
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

/// Parse front-matter lines using format-specific rules.
///
/// YAML uses `key: value`; TOML uses `key = "value"`. Keeping them separate
/// avoids splitting TOML values that contain colons (e.g. `url = "https://…"`).
fn parse_front_matter_kv(lines: &[&str], kind: FrontMatterKind) -> Vec<(String, String)> {
    lines
        .iter()
        .filter_map(|line| match kind {
            FrontMatterKind::Yaml => {
                let (k, v) = line.split_once(':')?;
                let key = k.trim();
                let val = v.trim().trim_matches('"').trim_matches('\'');
                (!key.is_empty() && !key.contains(' ')).then(|| (key.to_string(), val.to_string()))
            }
            FrontMatterKind::Toml => {
                let (k, v) = line.split_once('=')?;
                let key = k.trim();
                let val = v.trim().trim_matches('"');
                (!key.is_empty() && !key.contains(' ')).then(|| (key.to_string(), val.to_string()))
            }
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
    Paragraph { buf: String },
    Heading { level: u8, buf: String },
    BlockQuote { buf: String },
    ListItem { buf: String },
    List { ordered: bool, items: Vec<String> },
    CodeBlock { lang: Option<String>, buf: String },
    Table,
    TableHead,
    TableRow,
    TableCell { buf: String },
}

/// Return a mutable reference to the inline text buffer of the topmost frame
/// that holds one, or `None` if the top frame has no inline buffer.
fn top_buf_mut(stack: &mut Vec<Frame>) -> Option<&mut String> {
    stack.last_mut().and_then(|f| match f {
        Frame::Paragraph { buf }
        | Frame::Heading { buf, .. }
        | Frame::BlockQuote { buf }
        | Frame::ListItem { buf }
        | Frame::CodeBlock { buf, .. }
        | Frame::TableCell { buf } => Some(buf),
        Frame::List { .. } | Frame::Table | Frame::TableHead | Frame::TableRow => None,
    })
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
        let (fm_kind, fm_lines, md_text) = strip_front_matter(&raw);
        if let Some(kind) = fm_kind {
            for (k, v) in parse_front_matter_kv(&fm_lines, kind) {
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
        // Each frame carries its own inline buffer so that nested containers
        // (e.g. a paragraph inside a blockquote, or a nested list) do not
        // corrupt each other's accumulated text.
        let mut stack: Vec<Frame> = Vec::new();

        // Table accumulation (tables don't nest, so one set of globals is fine).
        let mut table_headers: Vec<String> = Vec::new();
        let mut table_rows: Vec<Vec<String>> = Vec::new();
        let mut current_row: Vec<String> = Vec::new();
        let mut in_table_head = false;

        for event in events {
            match event {
                // ---- Headings -----------------------------------------------
                Event::Start(Tag::Heading { level, .. }) => {
                    stack.push(Frame::Heading {
                        level: heading_level(level),
                        buf: String::new(),
                    });
                }
                Event::End(TagEnd::Heading(_)) => {
                    if let Some(Frame::Heading { level, buf }) = stack.pop() {
                        if doc.title.is_none() && level == 1 {
                            doc.title = Some(buf.clone());
                        }
                        if !buf.is_empty() {
                            doc.push(Block::Heading { level, text: buf });
                        }
                    }
                }

                // ---- Paragraphs ---------------------------------------------
                Event::Start(Tag::Paragraph) => {
                    stack.push(Frame::Paragraph { buf: String::new() });
                }
                Event::End(TagEnd::Paragraph) => {
                    if let Some(Frame::Paragraph { buf }) = stack.pop() {
                        if !buf.is_empty() {
                            // Paragraphs inside a blockquote or list item
                            // contribute their text to the parent frame rather
                            // than becoming a top-level block.
                            match stack.last_mut() {
                                Some(
                                    Frame::BlockQuote { buf: parent }
                                    | Frame::ListItem { buf: parent },
                                ) => {
                                    if !parent.is_empty() {
                                        parent.push(' ');
                                    }
                                    parent.push_str(&buf);
                                }
                                _ => {
                                    doc.push(Block::Paragraph { text: buf });
                                }
                            }
                        }
                    }
                }

                // ---- Block quotes -------------------------------------------
                Event::Start(Tag::BlockQuote(_)) => {
                    stack.push(Frame::BlockQuote { buf: String::new() });
                }
                Event::End(TagEnd::BlockQuote(_)) => {
                    if let Some(Frame::BlockQuote { buf }) = stack.pop() {
                        if !buf.is_empty() {
                            doc.push(Block::Quote { text: buf });
                        }
                    }
                }

                // ---- Lists --------------------------------------------------
                Event::Start(Tag::List(start_num)) => {
                    stack.push(Frame::List {
                        ordered: start_num.is_some(),
                        items: Vec::new(),
                    });
                }
                Event::End(TagEnd::List(_)) => {
                    if let Some(Frame::List { ordered, items }) = stack.pop() {
                        if !items.is_empty() {
                            doc.push(Block::List { ordered, items });
                        }
                    }
                }
                Event::Start(Tag::Item) => {
                    stack.push(Frame::ListItem { buf: String::new() });
                }
                Event::End(TagEnd::Item) => {
                    if let Some(Frame::ListItem { buf }) = stack.pop() {
                        if let Some(Frame::List { items, .. }) = stack.last_mut() {
                            items.push(buf);
                        }
                    }
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
                    stack.push(Frame::CodeBlock {
                        lang,
                        buf: String::new(),
                    });
                }
                Event::End(TagEnd::CodeBlock) => {
                    if let Some(Frame::CodeBlock { lang, buf }) = stack.pop() {
                        doc.push(Block::Code { lang, text: buf });
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
                    stack.push(Frame::TableCell { buf: String::new() });
                }
                Event::End(TagEnd::TableCell) => {
                    if let Some(Frame::TableCell { buf }) = stack.pop() {
                        current_row.push(buf);
                    }
                }

                // ---- Inline text --------------------------------------------
                Event::Text(t) | Event::Code(t) => {
                    if let Some(buf) = top_buf_mut(&mut stack) {
                        buf.push_str(&t);
                    }
                }
                Event::SoftBreak => {
                    if let Some(buf) = top_buf_mut(&mut stack) {
                        buf.push(' ');
                    }
                }
                Event::HardBreak => {
                    if let Some(buf) = top_buf_mut(&mut stack) {
                        buf.push('\n');
                    }
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
                    if let Some(buf) = top_buf_mut(&mut stack) {
                        buf.insert_str(0, marker);
                    }
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
        assert!(doc.blocks.iter().any(|b| matches!(b, Block::Table { .. })));
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

    // --- Regression tests for frame-local state & front-matter fixes --------

    #[test]
    fn toml_front_matter_url_colon_not_split() {
        let md = "+++\nurl = \"https://example.com\"\ntitle = \"TOML URL\"\n+++\n\n# Body\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        assert_eq!(
            doc.metadata.extra.get("url").map(|s| s.as_str()),
            Some("https://example.com"),
            "TOML front-matter: colon inside value must not split the key"
        );
        assert_eq!(doc.title.as_deref(), Some("TOML URL"));
    }

    #[test]
    fn blockquote_contains_paragraph_text() {
        let doc = MarkdownExtractor
            .extract(&input("a.md", "> blockquote text\n"))
            .unwrap();
        let quote = doc.blocks.iter().find_map(|b| match b {
            Block::Quote { text } => Some(text.as_str()),
            _ => None,
        });
        assert_eq!(
            quote,
            Some("blockquote text"),
            "blockquote block must carry its inner paragraph text, not be empty"
        );
    }

    #[test]
    fn nested_mixed_lists_produce_both_list_blocks() {
        let md = "- outer\n  1. inner\n";
        let doc = MarkdownExtractor.extract(&input("a.md", md)).unwrap();
        let lists: Vec<_> = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::List { .. }))
            .collect();
        assert!(!lists.is_empty(), "no list blocks: {:?}", doc.blocks);
        assert!(
            lists
                .iter()
                .any(|b| matches!(b, Block::List { ordered: true, .. })),
            "expected an ordered inner list"
        );
        assert!(
            lists
                .iter()
                .any(|b| matches!(b, Block::List { ordered: false, .. })),
            "expected an unordered outer list"
        );
    }
}
