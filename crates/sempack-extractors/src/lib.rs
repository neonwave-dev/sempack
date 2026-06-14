//! Built-in extractor plugins.
//!
//! P1 ships two: [`TextExtractor`] (plain text -> paragraphs) and [`MarkdownExtractor`]
//! (a small, dependency-free line parser covering headings, paragraphs, lists, block
//! quotes and fenced code). The markdown parser is deliberately minimal -- the planned
//! upgrade is `pulldown-cmark` for full CommonMark fidelity (TODO P1).
//!
//! The `web` feature (on by default) adds [`HtmlExtractor`], [`XmlExtractor`], and
//! [`SvgExtractor`] using `scraper` and `quick-xml`.

use sempack_core::{Extractor, Input, Result};
use sempack_ir::{Block, DocumentIr, SourceInfo};

#[cfg(feature = "web")]
pub mod web;

#[cfg(feature = "web")]
pub use web::{HtmlExtractor, SvgExtractor, XmlExtractor};

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
// Markdown (minimal line parser)
// ---------------------------------------------------------------------------

/// A small CommonMark-subset parser: headings, paragraphs, lists, quotes, fenced code.
pub struct MarkdownExtractor;

impl Extractor for MarkdownExtractor {
    fn name(&self) -> &'static str {
        "markdown"
    }
    fn formats(&self) -> &'static [&'static str] {
        &["markdown"]
    }
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        let text = input.text();
        let mut doc = DocumentIr::new(doc_id(input), source(input));

        let mut para: Vec<String> = Vec::new();
        let mut list: Vec<String> = Vec::new();
        let mut list_ordered = false;
        let mut in_code = false;
        let mut code = String::new();
        let mut code_lang: Option<String> = None;
        let mut title: Option<String> = None;

        for line in text.lines() {
            // fenced code toggles
            if let Some(rest) = line.trim_start().strip_prefix("```") {
                if in_code {
                    doc.push(Block::Code {
                        lang: code_lang.take(),
                        text: std::mem::take(&mut code),
                    });
                    in_code = false;
                } else {
                    flush_para(&mut doc, &mut para);
                    flush_list(&mut doc, &mut list, list_ordered);
                    in_code = true;
                    let l = rest.trim();
                    code_lang = (!l.is_empty()).then(|| l.to_string());
                }
                continue;
            }
            if in_code {
                code.push_str(line);
                code.push('\n');
                continue;
            }

            let trimmed = line.trim_end();
            if trimmed.trim().is_empty() {
                flush_para(&mut doc, &mut para);
                flush_list(&mut doc, &mut list, list_ordered);
                continue;
            }

            if let Some((level, htext)) = heading(trimmed) {
                flush_para(&mut doc, &mut para);
                flush_list(&mut doc, &mut list, list_ordered);
                if title.is_none() && level == 1 {
                    title = Some(htext.clone());
                }
                doc.push(Block::Heading { level, text: htext });
                continue;
            }

            if let Some((ordered, item)) = list_item(trimmed) {
                flush_para(&mut doc, &mut para);
                // If the marker style flips mid-list (e.g. `- ` then `1. ` with no
                // blank line between), flush the current list first so ordered and
                // unordered items don't merge into one block with the wrong type.
                if !list.is_empty() && ordered != list_ordered {
                    flush_list(&mut doc, &mut list, list_ordered);
                }
                list_ordered = ordered;
                list.push(item);
                continue;
            }

            if let Some(q) = trimmed.trim_start().strip_prefix('>') {
                flush_para(&mut doc, &mut para);
                flush_list(&mut doc, &mut list, list_ordered);
                doc.push(Block::Quote {
                    text: q.trim().to_string(),
                });
                continue;
            }

            // default: paragraph text (lists end at a non-list line)
            flush_list(&mut doc, &mut list, list_ordered);
            para.push(trimmed.trim().to_string());
        }

        // trailing flush -- emit even an empty unterminated fence so a lone opening
        // ``` at EOF still becomes a (possibly empty) Block::Code rather than vanishing.
        if in_code {
            doc.push(Block::Code {
                lang: code_lang.take(),
                text: code,
            });
        }
        flush_para(&mut doc, &mut para);
        flush_list(&mut doc, &mut list, list_ordered);
        doc.title = title;
        Ok(doc)
    }
}

fn flush_para(doc: &mut DocumentIr, para: &mut Vec<String>) {
    if !para.is_empty() {
        doc.push(Block::Paragraph {
            text: para.join(" "),
        });
        para.clear();
    }
}

fn flush_list(doc: &mut DocumentIr, list: &mut Vec<String>, ordered: bool) {
    if !list.is_empty() {
        doc.push(Block::List {
            ordered,
            items: std::mem::take(list),
        });
    }
}

/// `## Title` -> `(2, "Title")`.
fn heading(line: &str) -> Option<(u8, String)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && line[hashes..].starts_with(' ') {
        Some((hashes as u8, line[hashes..].trim().to_string()))
    } else {
        None
    }
}

/// `- item` / `* item` / `+ item` / `1. item` / `1) item`.
fn list_item(line: &str) -> Option<(bool, String)> {
    let t = line.trim_start();
    if let Some(r) = t
        .strip_prefix("- ")
        .or_else(|| t.strip_prefix("* "))
        .or_else(|| t.strip_prefix("+ "))
    {
        return Some((false, r.trim().to_string()));
    }
    let digits: String = t.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() {
        let rest = &t[digits.len()..];
        if let Some(r) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((true, r.trim().to_string()));
        }
    }
    None
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
    fn unterminated_empty_fence_still_emits_code_block() {
        let doc = MarkdownExtractor
            .extract(&input("a.md", "text\n\n```"))
            .unwrap();
        assert!(doc.blocks.iter().any(|b| matches!(b, Block::Code { .. })));
    }

    #[test]
    fn markdown_marker_flip_does_not_merge_lists() {
        // `- ` then `1. ` with no blank line must yield two separate lists.
        let doc = MarkdownExtractor
            .extract(&input("a.md", "- bullet\n1. number\n"))
            .unwrap();
        let lists: Vec<_> = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::List { .. }))
            .collect();
        assert_eq!(lists.len(), 2);
        assert!(matches!(lists[0], Block::List { ordered: false, .. }));
        assert!(matches!(lists[1], Block::List { ordered: true, .. }));
    }
}
