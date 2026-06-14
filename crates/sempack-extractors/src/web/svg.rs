//! SVG extractor -- extracts text content (title, desc, text, aria-label) only.
//!
//! Geometry (path, rect, circle, etc.) is explicitly skipped. Produces
//! `Block::Paragraph` from each text element found.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use sempack_core::{Extractor, Input, Result};
use sempack_ir::{Block, DocumentIr};

use super::super::{doc_id, source};

/// Tags we actively extract text from.
const TEXT_TAGS: &[&str] = &["title", "desc", "text", "tspan"];

/// Extracts human-readable text from SVG documents.
pub struct SvgExtractor;

impl Extractor for SvgExtractor {
    fn name(&self) -> &'static str {
        "svg"
    }
    fn formats(&self) -> &'static [&'static str] {
        &["svg"]
    }
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        let text = input.text();
        let mut doc = DocumentIr::new(doc_id(input), source(input));
        doc.metadata.extra.insert("format".into(), "svg".into());

        let mut reader = Reader::from_str(&text);
        reader.config_mut().trim_text(true);

        // Track whether we are inside a text-bearing element.
        let mut in_text_tag: Option<String> = None;
        let mut current_text = String::new();
        let mut found_any = false;

        loop {
            match reader.read_event() {
                Ok(Event::Start(ref e)) => {
                    let tag = String::from_utf8_lossy(e.name().local_name().as_ref())
                        .to_ascii_lowercase();

                    // Collect aria-label attributes from any element.
                    for attr in e.attributes().filter_map(|a| a.ok()) {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                        if key == "aria-label" {
                            let val = attr.unescape_value().unwrap_or_default().to_string();
                            let val = val.trim().to_string();
                            if !val.is_empty() {
                                doc.push(Block::Paragraph { text: val });
                                found_any = true;
                            }
                        }
                    }

                    if TEXT_TAGS.contains(&tag.as_str()) {
                        in_text_tag = Some(tag);
                        current_text.clear();
                    }
                }
                Ok(Event::Text(ref e)) => {
                    if in_text_tag.is_some() {
                        let txt = e.unescape().unwrap_or_default();
                        let trimmed = txt.trim().to_string();
                        if !trimmed.is_empty() {
                            if !current_text.is_empty() {
                                current_text.push(' ');
                            }
                            current_text.push_str(&trimmed);
                        }
                    }
                }
                Ok(Event::End(_)) => {
                    if in_text_tag.is_some() {
                        let t = current_text.trim().to_string();
                        if !t.is_empty() {
                            doc.push(Block::Paragraph { text: t });
                            found_any = true;
                        }
                        in_text_tag = None;
                        current_text.clear();
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    // Self-closing elements: only check aria-label.
                    for attr in e.attributes().filter_map(|a| a.ok()) {
                        let key = String::from_utf8_lossy(attr.key.as_ref()).to_string();
                        if key == "aria-label" {
                            let val = attr.unescape_value().unwrap_or_default().to_string();
                            let val = val.trim().to_string();
                            if !val.is_empty() {
                                doc.push(Block::Paragraph { text: val });
                                found_any = true;
                            }
                        }
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    doc.warn(
                        "svg.malformed",
                        format!(
                            "SVG parse error at position {}: {e}",
                            reader.error_position()
                        ),
                    );
                    break;
                }
                _ => {}
            }
        }

        if !found_any {
            doc.warn("svg.no_text_content", "no text content found in SVG");
        }

        Ok(doc)
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

    #[test]
    fn svg_extracts_title_and_desc() {
        let svg = concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">\n",
            "  <title>My Chart</title>\n",
            "  <desc>A bar chart showing sales data</desc>\n",
            "  <rect x=\"0\" y=\"0\" width=\"100\" height=\"50\" />\n",
            "</svg>",
        );
        let doc = SvgExtractor.extract(&input("chart.svg", svg)).unwrap();
        assert_eq!(
            doc.metadata.extra.get("format").map(|s| s.as_str()),
            Some("svg")
        );
        let text = doc.plain_text();
        assert!(text.contains("My Chart"));
        assert!(text.contains("A bar chart"));
        // Geometry must NOT appear.
        assert!(!text.contains("rect"));
        assert!(!text.contains("0"));
    }

    #[test]
    fn svg_extracts_text_element() {
        let svg = concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">\n",
            "  <text x=\"10\" y=\"20\">Hello SVG</text>\n",
            "  <path d=\"M 0 0 L 100 100\" />\n",
            "</svg>",
        );
        let doc = SvgExtractor.extract(&input("icon.svg", svg)).unwrap();
        let text = doc.plain_text();
        assert!(text.contains("Hello SVG"));
        assert!(!text.contains("M 0 0"));
    }

    #[test]
    fn svg_extracts_aria_label() {
        let svg = concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\" aria-label=\"Logo icon\">\n",
            "  <circle cx=\"50\" cy=\"50\" r=\"40\" aria-label=\"Circle\" />\n",
            "</svg>",
        );
        let doc = SvgExtractor.extract(&input("logo.svg", svg)).unwrap();
        let text = doc.plain_text();
        assert!(text.contains("Logo icon"));
        assert!(text.contains("Circle"));
    }

    #[test]
    fn svg_no_text_emits_warning() {
        let svg = concat!(
            "<svg xmlns=\"http://www.w3.org/2000/svg\">\n",
            "  <rect x=\"0\" y=\"0\" width=\"100\" height=\"100\" />\n",
            "</svg>",
        );
        let doc = SvgExtractor.extract(&input("blank.svg", svg)).unwrap();
        assert!(
            doc.warnings.iter().any(|w| w.code == "svg.no_text_content"),
            "expected svg.no_text_content warning"
        );
    }

    #[test]
    fn svg_malformed_emits_warning() {
        // A truncated element triggers a quick-xml parse error.
        let svg = "<svg><bad";
        let doc = SvgExtractor.extract(&input("bad.svg", svg)).unwrap();
        assert!(
            doc.warnings.iter().any(|w| w.code == "svg.malformed"),
            "expected svg.malformed warning, got {:?}",
            doc.warnings
        );
    }

    #[test]
    fn svg_empty_file() {
        let doc = SvgExtractor.extract(&input("empty.svg", "")).unwrap();
        assert!(doc.warnings.iter().any(|w| w.code == "svg.no_text_content"));
    }
}
