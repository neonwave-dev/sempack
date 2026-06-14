//! XML extractor -- conservative: elements + text content to flat Record blocks.

use quick_xml::events::Event;
use quick_xml::reader::Reader;
use sempack_core::{Extractor, Input, Result};
use sempack_ir::{Block, DocumentIr, Field};

use super::super::{doc_id, source};

/// Extracts flat `Block::Record` entries from XML documents.
pub struct XmlExtractor;

impl Extractor for XmlExtractor {
    fn name(&self) -> &'static str {
        "xml"
    }
    fn formats(&self) -> &'static [&'static str] {
        &["xml"]
    }
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        let text = input.text();
        let mut doc = DocumentIr::new(doc_id(input), source(input));
        doc.metadata.extra.insert("format".into(), "xml".into());

        let mut reader = Reader::from_str(&text);
        reader.config_mut().trim_text(true);

        let mut root_element: Option<String> = None;

        // Stack of (tag_name, attrs_string) for open elements.
        let mut stack: Vec<(String, String)> = Vec::new();
        // Accumulated text within the current element.
        let mut current_text = String::new();

        loop {
            match reader.read_event() {
                Ok(Event::Start(ref e)) => {
                    let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if root_element.is_none() {
                        root_element = Some(tag.clone());
                    }
                    let attrs = e
                        .attributes()
                        .filter_map(|a| a.ok())
                        .map(|a| {
                            let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(a.value.as_ref()).to_string();
                            format!("{key}={val}")
                        })
                        .collect::<Vec<_>>()
                        .join(",");
                    stack.push((tag, attrs));
                    current_text.clear();
                }
                Ok(Event::Text(ref e)) => {
                    let txt = e.unescape().unwrap_or_default();
                    let trimmed = txt.trim().to_string();
                    if !trimmed.is_empty() {
                        if !current_text.is_empty() {
                            current_text.push(' ');
                        }
                        current_text.push_str(&trimmed);
                    }
                }
                Ok(Event::End(_)) => {
                    if let Some((tag, attrs)) = stack.pop() {
                        let text = current_text.trim().to_string();
                        current_text.clear();
                        if !text.is_empty() {
                            let mut fields = vec![
                                Field {
                                    key: "tag".into(),
                                    value: tag,
                                },
                                Field {
                                    key: "text".into(),
                                    value: text,
                                },
                            ];
                            if !attrs.is_empty() {
                                fields.push(Field {
                                    key: "attrs".into(),
                                    value: attrs,
                                });
                            }
                            doc.push(Block::Record { fields });
                        }
                    }
                }
                Ok(Event::Empty(ref e)) => {
                    // Self-closing tags: record if they have attributes.
                    let tag = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    if root_element.is_none() {
                        root_element = Some(tag.clone());
                    }
                    let attrs: Vec<String> = e
                        .attributes()
                        .filter_map(|a| a.ok())
                        .map(|a| {
                            let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
                            let val = String::from_utf8_lossy(a.value.as_ref()).to_string();
                            format!("{key}={val}")
                        })
                        .collect();
                    if !attrs.is_empty() {
                        doc.push(Block::Record {
                            fields: vec![
                                Field {
                                    key: "tag".into(),
                                    value: tag,
                                },
                                Field {
                                    key: "attrs".into(),
                                    value: attrs.join(","),
                                },
                            ],
                        });
                    }
                }
                Ok(Event::Eof) => break,
                Err(e) => {
                    doc.warn(
                        "xml.malformed",
                        format!(
                            "XML parse error at position {}: {e}",
                            reader.error_position()
                        ),
                    );
                    break;
                }
                _ => {}
            }
        }

        if let Some(root) = root_element {
            doc.metadata.extra.insert("root_element".into(), root);
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
    fn xml_basic_records() {
        let xml = concat!(
            "<?xml version=\"1.0\"?>\n",
            "<root>\n",
            "  <item id=\"1\">First</item>\n",
            "  <item id=\"2\">Second</item>\n",
            "</root>",
        );
        let doc = XmlExtractor.extract(&input("data.xml", xml)).unwrap();
        assert_eq!(
            doc.metadata.extra.get("format").map(|s| s.as_str()),
            Some("xml")
        );
        assert_eq!(
            doc.metadata.extra.get("root_element").map(|s| s.as_str()),
            Some("root")
        );
        let records: Vec<_> = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::Record { .. }))
            .collect();
        assert_eq!(records.len(), 2);
        if let Block::Record { fields } = &records[0] {
            let tag = fields
                .iter()
                .find(|f| f.key == "tag")
                .map(|f| f.value.as_str());
            assert_eq!(tag, Some("item"));
            let text = fields
                .iter()
                .find(|f| f.key == "text")
                .map(|f| f.value.as_str());
            assert_eq!(text, Some("First"));
        }
    }

    #[test]
    fn xml_malformed_emits_warning() {
        // A truncated element start triggers a quick-xml parse error.
        let xml = "<root><bad";
        let doc = XmlExtractor.extract(&input("bad.xml", xml)).unwrap();
        // Partial results are allowed; warning must be present.
        assert!(
            doc.warnings.iter().any(|w| w.code == "xml.malformed"),
            "expected xml.malformed, got {:?}",
            doc.warnings
        );
    }

    #[test]
    fn xml_empty_file() {
        let doc = XmlExtractor.extract(&input("empty.xml", "")).unwrap();
        assert!(doc.blocks.is_empty());
    }

    #[test]
    fn xml_root_element_captured() {
        let xml = "<catalog><book>Rust Programming</book></catalog>";
        let doc = XmlExtractor.extract(&input("books.xml", xml)).unwrap();
        assert_eq!(
            doc.metadata.extra.get("root_element").map(|s| s.as_str()),
            Some("catalog")
        );
    }
}
