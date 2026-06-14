//! Emitter plugins — serialize a [`DocumentIr`] to a string.
//!
//! P1 ships five: Markdown, JSONL (one JSON object per block, header first),
//! NDJSON (one JSON object = the whole document), plain Text, and minimal HTML.

use sempack_core::{Emitter, Error, OutputFormat, Result};
use sempack_ir::{Block, DocumentIr};

fn json_err(e: serde_json::Error) -> Error {
    Error::Other(format!("json serialization failed: {e}"))
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------

pub struct MarkdownEmitter;

impl Emitter for MarkdownEmitter {
    fn name(&self) -> &'static str {
        "markdown"
    }
    fn format(&self) -> OutputFormat {
        OutputFormat::Markdown
    }
    fn emit(&self, doc: &DocumentIr) -> Result<String> {
        let mut o = String::new();
        for b in &doc.blocks {
            emit_md(b, &mut o);
        }
        Ok(o.trim_end().to_string() + "\n")
    }
}

fn emit_md(b: &Block, o: &mut String) {
    match b {
        Block::Document { children } | Block::Section { children } => {
            for c in children {
                emit_md(c, o);
            }
        }
        Block::Heading { level, text } => {
            for _ in 0..*level {
                o.push('#');
            }
            o.push(' ');
            o.push_str(text);
            o.push_str("\n\n");
        }
        Block::Paragraph { text } => {
            o.push_str(text);
            o.push_str("\n\n");
        }
        Block::Quote { text } => {
            o.push_str("> ");
            o.push_str(text);
            o.push_str("\n\n");
        }
        Block::List { ordered, items } => {
            for (i, it) in items.iter().enumerate() {
                if *ordered {
                    o.push_str(&format!("{}. ", i + 1));
                } else {
                    o.push_str("- ");
                }
                o.push_str(it);
                o.push('\n');
            }
            o.push('\n');
        }
        Block::Code { lang, text } => {
            o.push_str("```");
            if let Some(l) = lang {
                o.push_str(l);
            }
            o.push('\n');
            o.push_str(text);
            if !text.ends_with('\n') {
                o.push('\n');
            }
            o.push_str("```\n\n");
        }
        Block::Table { headers, rows } => {
            o.push_str("| ");
            o.push_str(&headers.join(" | "));
            o.push_str(" |\n|");
            for _ in headers {
                o.push_str(" --- |");
            }
            o.push('\n');
            for r in rows {
                o.push_str("| ");
                o.push_str(&r.join(" | "));
                o.push_str(" |\n");
            }
            o.push('\n');
        }
        Block::Record { fields } => {
            for f in fields {
                o.push_str("- **");
                o.push_str(&f.key);
                o.push_str("**: ");
                o.push_str(&f.value);
                o.push('\n');
            }
            o.push('\n');
        }
        Block::Unsupported { note } => {
            o.push_str("> [!warning] ");
            o.push_str(note);
            o.push_str("\n\n");
        }
    }
}

// ---------------------------------------------------------------------------
// JSONL — header line + one line per top-level block
// ---------------------------------------------------------------------------

pub struct JsonlEmitter;

impl Emitter for JsonlEmitter {
    fn name(&self) -> &'static str {
        "jsonl"
    }
    fn format(&self) -> OutputFormat {
        OutputFormat::Jsonl
    }
    fn emit(&self, doc: &DocumentIr) -> Result<String> {
        let mut o = String::new();
        let header = serde_json::json!({
            "type": "document",
            "id": doc.id,
            "title": doc.title,
            "source": serde_json::to_value(&doc.source).map_err(json_err)?,
        });
        o.push_str(&serde_json::to_string(&header).map_err(json_err)?);
        o.push('\n');
        for b in &doc.blocks {
            o.push_str(&serde_json::to_string(b).map_err(json_err)?);
            o.push('\n');
        }
        Ok(o)
    }
}

// ---------------------------------------------------------------------------
// NDJSON — the whole document as one line
// ---------------------------------------------------------------------------

pub struct NdjsonEmitter;

impl Emitter for NdjsonEmitter {
    fn name(&self) -> &'static str {
        "ndjson"
    }
    fn format(&self) -> OutputFormat {
        OutputFormat::Ndjson
    }
    fn emit(&self, doc: &DocumentIr) -> Result<String> {
        let line = serde_json::to_string(doc).map_err(json_err)?;
        Ok(format!("{line}\n"))
    }
}

// ---------------------------------------------------------------------------
// Plain text
// ---------------------------------------------------------------------------

pub struct TextEmitter;

impl Emitter for TextEmitter {
    fn name(&self) -> &'static str {
        "text"
    }
    fn format(&self) -> OutputFormat {
        OutputFormat::Text
    }
    fn emit(&self, doc: &DocumentIr) -> Result<String> {
        let mut o = String::new();
        for b in &doc.blocks {
            emit_text(b, &mut o);
        }
        Ok(o.trim_end().to_string() + "\n")
    }
}

fn emit_text(b: &Block, o: &mut String) {
    match b {
        Block::Document { children } | Block::Section { children } => {
            for c in children {
                emit_text(c, o);
            }
        }
        Block::Heading { text, .. } | Block::Paragraph { text } | Block::Quote { text } => {
            o.push_str(text);
            o.push_str("\n\n");
        }
        Block::List { items, .. } => {
            for it in items {
                o.push_str("- ");
                o.push_str(it);
                o.push('\n');
            }
            o.push('\n');
        }
        Block::Code { text, .. } => {
            o.push_str(text);
            if !text.ends_with('\n') {
                o.push('\n');
            }
            o.push('\n');
        }
        Block::Table { headers, rows } => {
            o.push_str(&headers.join("\t"));
            o.push('\n');
            for r in rows {
                o.push_str(&r.join("\t"));
                o.push('\n');
            }
            o.push('\n');
        }
        Block::Record { fields } => {
            for f in fields {
                o.push_str(&f.key);
                o.push_str(": ");
                o.push_str(&f.value);
                o.push('\n');
            }
            o.push('\n');
        }
        Block::Unsupported { note } => {
            o.push_str(note);
            o.push_str("\n\n");
        }
    }
}

// ---------------------------------------------------------------------------
// HTML (minimal)
// ---------------------------------------------------------------------------

pub struct HtmlEmitter;

impl Emitter for HtmlEmitter {
    fn name(&self) -> &'static str {
        "html"
    }
    fn format(&self) -> OutputFormat {
        OutputFormat::Html
    }
    fn emit(&self, doc: &DocumentIr) -> Result<String> {
        let mut o = String::from("<!doctype html>\n<html>\n<head><meta charset=\"utf-8\">");
        if let Some(t) = &doc.title {
            o.push_str("<title>");
            o.push_str(&esc(t));
            o.push_str("</title>");
        }
        o.push_str("</head>\n<body>\n");
        for b in &doc.blocks {
            emit_html(b, &mut o);
        }
        o.push_str("</body>\n</html>\n");
        Ok(o)
    }
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn emit_html(b: &Block, o: &mut String) {
    match b {
        Block::Document { children } | Block::Section { children } => {
            for c in children {
                emit_html(c, o);
            }
        }
        Block::Heading { level, text } => {
            let l = (*level).clamp(1, 6);
            o.push_str(&format!("<h{l}>{}</h{l}>\n", esc(text)));
        }
        Block::Paragraph { text } => {
            o.push_str(&format!("<p>{}</p>\n", esc(text)));
        }
        Block::Quote { text } => {
            o.push_str(&format!("<blockquote>{}</blockquote>\n", esc(text)));
        }
        Block::List { ordered, items } => {
            let tag = if *ordered { "ol" } else { "ul" };
            o.push_str(&format!("<{tag}>\n"));
            for it in items {
                o.push_str(&format!("  <li>{}</li>\n", esc(it)));
            }
            o.push_str(&format!("</{tag}>\n"));
        }
        Block::Code { text, .. } => {
            o.push_str(&format!("<pre><code>{}</code></pre>\n", esc(text)));
        }
        Block::Table { headers, rows } => {
            o.push_str("<table>\n<thead><tr>");
            for h in headers {
                o.push_str(&format!("<th>{}</th>", esc(h)));
            }
            o.push_str("</tr></thead>\n<tbody>\n");
            for r in rows {
                o.push_str("<tr>");
                for c in r {
                    o.push_str(&format!("<td>{}</td>", esc(c)));
                }
                o.push_str("</tr>\n");
            }
            o.push_str("</tbody>\n</table>\n");
        }
        Block::Record { fields } => {
            o.push_str("<dl>\n");
            for f in fields {
                o.push_str(&format!(
                    "  <dt>{}</dt><dd>{}</dd>\n",
                    esc(&f.key),
                    esc(&f.value)
                ));
            }
            o.push_str("</dl>\n");
        }
        Block::Unsupported { note } => {
            o.push_str(&format!("<!-- unsupported: {} -->\n", esc(note)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sempack_ir::SourceInfo;

    fn sample() -> DocumentIr {
        let mut d = DocumentIr::new(
            "t",
            SourceInfo {
                path: None,
                media_type: None,
                detected_format: "markdown".into(),
                bytes: 0,
            },
        );
        d.title = Some("Hello".into());
        d.blocks = vec![
            Block::Heading {
                level: 1,
                text: "Hello".into(),
            },
            Block::Paragraph {
                text: "world & friends".into(),
            },
            Block::List {
                ordered: false,
                items: vec!["a".into(), "b".into()],
            },
        ];
        d
    }

    #[test]
    fn markdown_roundtrips_structure() {
        let out = MarkdownEmitter.emit(&sample()).unwrap();
        assert!(out.contains("# Hello"));
        assert!(out.contains("- a"));
    }

    #[test]
    fn jsonl_has_header_plus_blocks() {
        let out = JsonlEmitter.emit(&sample()).unwrap();
        let lines: Vec<_> = out.lines().collect();
        assert_eq!(lines.len(), 4); // header + 3 blocks
        assert!(lines[0].contains("\"type\":\"document\""));
    }

    #[test]
    fn html_escapes() {
        let out = HtmlEmitter.emit(&sample()).unwrap();
        assert!(out.contains("world &amp; friends"));
    }
}
