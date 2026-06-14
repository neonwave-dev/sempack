//! HTML extractor -- maps semantic tags to IR blocks with in-order DOM traversal.

use scraper::{ElementRef, Html, Node, Selector};
use sempack_core::{Extractor, Input, Result};
use sempack_ir::{Block, DocumentIr};

use super::super::{doc_id, source};

const STRIP_TAGS: &[&str] = &["script", "style", "nav", "header", "footer", "aside"];
const BLOCK_TAGS: &[&str] = &[
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "p",
    "pre",
    "table",
    "ul",
    "ol",
    "blockquote",
];

pub struct HtmlExtractor;

impl Extractor for HtmlExtractor {
    fn name(&self) -> &'static str {
        "html"
    }
    fn formats(&self) -> &'static [&'static str] {
        &["html"]
    }
    fn extract(&self, input: &Input) -> Result<DocumentIr> {
        let text = input.text();
        let mut doc = DocumentIr::new(doc_id(input), source(input));
        doc.metadata.extra.insert("format".into(), "html".into());
        let document = Html::parse_document(&text);
        if !document.errors.is_empty() {
            doc.warn(
                "html.malformed",
                format!(
                    "HTML parse errors ({}): {}",
                    document.errors.len(),
                    document.errors[0]
                ),
            );
        }
        let title_sel = Selector::parse("title").unwrap();
        if let Some(title_el) = document.select(&title_sel).next() {
            let title_text: String = title_el.text().collect::<Vec<_>>().join("");
            let title_text = title_text.trim().to_string();
            if !title_text.is_empty() {
                doc.title = Some(title_text.clone());
                doc.metadata.extra.insert("title".into(), title_text);
            }
        }
        let body_sel = Selector::parse("body").unwrap();
        if let Some(body) = document.select(&body_sel).next() {
            walk_element(body, &mut doc);
        } else {
            walk_element(document.root_element(), &mut doc);
        }
        Ok(doc)
    }
}

fn walk_element(el: ElementRef, doc: &mut DocumentIr) {
    for child in el.children() {
        match child.value() {
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    let tag = child_el.value().name().to_ascii_lowercase();
                    if STRIP_TAGS.contains(&tag.as_str()) {
                        continue;
                    }
                    if BLOCK_TAGS.contains(&tag.as_str()) {
                        emit_block(&tag, child_el, doc);
                    } else {
                        walk_element(child_el, doc);
                    }
                }
            }
            Node::Text(t) => {
                let text = t.trim().to_string();
                if !text.is_empty() {
                    doc.push(Block::Paragraph { text });
                }
            }
            _ => {}
        }
    }
}

fn emit_block(tag: &str, el: ElementRef, doc: &mut DocumentIr) {
    match tag {
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
            let level = tag[1..].parse::<u8>().unwrap_or(1);
            let text = collect_text(el);
            if !text.is_empty() {
                doc.push(Block::Heading { level, text });
            }
        }
        "p" => {
            let text = collect_text(el);
            if !text.is_empty() {
                doc.push(Block::Paragraph { text });
            }
        }
        "pre" => {
            let code_sel = Selector::parse("code").unwrap();
            let (lang, text) = if let Some(code_el) = el.select(&code_sel).next() {
                let lang = code_el.value().attr("class").and_then(|c| {
                    c.split_whitespace()
                        .find(|cls| cls.starts_with("language-"))
                        .map(|cls| cls["language-".len()..].to_string())
                });
                (lang, collect_text(code_el))
            } else {
                (None, collect_text(el))
            };
            if !text.is_empty() {
                doc.push(Block::Code { lang, text });
            }
        }
        "table" => emit_table(el, doc),
        "ul" | "ol" => {
            let ordered = tag == "ol";
            let items = collect_list_items(el);
            if !items.is_empty() {
                doc.push(Block::List { ordered, items });
            }
        }
        "blockquote" => {
            let text = collect_text(el);
            if !text.is_empty() {
                doc.push(Block::Quote { text });
            }
        }
        _ => {}
    }
}

fn collect_text(el: ElementRef) -> String {
    let mut parts: Vec<String> = Vec::new();
    collect_text_inner(el, &mut parts);
    parts
        .join(" ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn collect_text_inner(el: ElementRef, out: &mut Vec<String>) {
    for child in el.children() {
        match child.value() {
            Node::Text(t) => {
                let s = t.trim().to_string();
                if !s.is_empty() {
                    out.push(s);
                }
            }
            Node::Element(_) => {
                if let Some(child_el) = ElementRef::wrap(child) {
                    let tag = child_el.value().name().to_ascii_lowercase();
                    if tag == "script" || tag == "style" {
                        continue;
                    }
                    if tag == "img" {
                        if let Some(alt) = child_el.value().attr("alt") {
                            let alt = alt.trim();
                            if !alt.is_empty() {
                                out.push(alt.to_string());
                            }
                        }
                    } else {
                        collect_text_inner(child_el, out);
                    }
                }
            }
            _ => {}
        }
    }
}

fn emit_table(el: ElementRef, doc: &mut DocumentIr) {
    let th_sel = Selector::parse("th").unwrap();
    let tr_sel = Selector::parse("tr").unwrap();
    let td_sel = Selector::parse("td").unwrap();
    let headers: Vec<String> = el
        .select(&th_sel)
        .map(|th| collect_text(th))
        .filter(|s| !s.is_empty())
        .collect();
    let rows: Vec<Vec<String>> = el
        .select(&tr_sel)
        .map(|tr| {
            tr.select(&td_sel)
                .map(|td| collect_text(td))
                .collect::<Vec<_>>()
        })
        .filter(|row| !row.is_empty())
        .collect();
    if !headers.is_empty() || !rows.is_empty() {
        doc.push(Block::Table { headers, rows });
    }
}

fn collect_list_items(el: ElementRef) -> Vec<String> {
    let li_sel = Selector::parse("li").unwrap();
    el.select(&li_sel)
        .map(|li| collect_text(li))
        .filter(|s| !s.is_empty())
        .collect()
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
    fn html_headings_and_paragraphs() {
        let html = concat!(
            "<!DOCTYPE html>\n",
            "<html><head><title>My Page</title></head>\n",
            "<body>\n",
            "<h1>Welcome</h1>\n",
            "<p>Hello world.</p>\n",
            "<h2>Section</h2>\n",
            "<p>Another paragraph.</p>\n",
            "</body></html>",
        );
        let doc = HtmlExtractor.extract(&input("page.html", html)).unwrap();
        assert_eq!(doc.title.as_deref(), Some("My Page"));
        assert_eq!(
            doc.metadata.extra.get("title").map(|s| s.as_str()),
            Some("My Page")
        );
        assert_eq!(
            doc.metadata.extra.get("format").map(|s| s.as_str()),
            Some("html")
        );
        assert!(matches!(
            doc.blocks[0],
            Block::Heading { level: 1, ref text } if text == "Welcome"
        ));
        assert!(matches!(
            doc.blocks[1],
            Block::Paragraph { ref text } if text == "Hello world."
        ));
        assert!(matches!(
            doc.blocks[2],
            Block::Heading { level: 2, ref text } if text == "Section"
        ));
    }

    #[test]
    fn html_strips_nav_and_script() {
        let html = concat!(
            "<html><body>\n",
            "<nav><a href=\"/\">Home</a></nav>\n",
            "<script>alert(\"evil\")</script>\n",
            "<style>body { color: red; }</style>\n",
            "<header>Site Header</header>\n",
            "<footer>Footer content</footer>\n",
            "<aside>Sidebar</aside>\n",
            "<main><p>Real content</p></main>\n",
            "</body></html>",
        );
        let doc = HtmlExtractor.extract(&input("page.html", html)).unwrap();
        let text = doc.plain_text();
        assert!(text.contains("Real content"));
        assert!(!text.contains("Home"));
        assert!(!text.contains("evil"));
        assert!(!text.contains("Site Header"));
        assert!(!text.contains("Footer content"));
        assert!(!text.contains("Sidebar"));
        assert!(!text.contains("color: red"));
    }

    #[test]
    fn html_lists() {
        let html = concat!(
            "<html><body>\n",
            "<ul><li>Alpha</li><li>Beta</li></ul>\n",
            "<ol><li>One</li><li>Two</li></ol>\n",
            "</body></html>",
        );
        let doc = HtmlExtractor.extract(&input("page.html", html)).unwrap();
        let lists: Vec<_> = doc
            .blocks
            .iter()
            .filter(|b| matches!(b, Block::List { .. }))
            .collect();
        assert_eq!(lists.len(), 2);
        assert!(matches!(
            lists[0],
            Block::List { ordered: false, ref items } if items.len() == 2
        ));
        assert!(matches!(
            lists[1],
            Block::List { ordered: true, ref items } if items.len() == 2
        ));
    }

    #[test]
    fn html_table() {
        let html = concat!(
            "<html><body>\n",
            "<table>\n",
            "  <tr><th>Name</th><th>Age</th></tr>\n",
            "  <tr><td>Alice</td><td>30</td></tr>\n",
            "  <tr><td>Bob</td><td>25</td></tr>\n",
            "</table>\n",
            "</body></html>",
        );
        let doc = HtmlExtractor.extract(&input("page.html", html)).unwrap();
        assert!(doc.blocks.iter().any(|b| matches!(
            b,
            Block::Table { ref headers, ref rows } if headers.len() == 2 && rows.len() == 2
        )));
    }

    #[test]
    fn html_code_block() {
        let html = concat!(
            "<html><body>\n",
            "<pre><code class=\"language-rust\">fn main() {}</code></pre>\n",
            "</body></html>",
        );
        let doc = HtmlExtractor.extract(&input("page.html", html)).unwrap();
        assert!(doc.blocks.iter().any(|b| matches!(
            b,
            Block::Code { lang: Some(ref l), .. } if l == "rust"
        )));
    }

    #[test]
    fn html_blockquote() {
        let html = "<html><body><blockquote>Famous words</blockquote></body></html>";
        let doc = HtmlExtractor.extract(&input("page.html", html)).unwrap();
        assert!(doc.blocks.iter().any(|b| matches!(
            b,
            Block::Quote { ref text } if text == "Famous words"
        )));
    }

    #[test]
    fn html_image_alt_text() {
        let html =
            "<html><body><p>See: <img src=\"x.png\" alt=\"A cat photo\" /></p></body></html>";
        let doc = HtmlExtractor.extract(&input("page.html", html)).unwrap();
        let text = doc.plain_text();
        assert!(text.contains("A cat photo"));
    }

    #[test]
    fn html_malformed_emits_warning() {
        let html = "<html><body><p class=oops unclosed";
        let doc = HtmlExtractor.extract(&input("bad.html", html)).unwrap();
        assert!(
            doc.warnings.iter().any(|w| w.code == "html.malformed"),
            "expected html.malformed warning, got: {:?}",
            doc.warnings
        );
    }

    #[test]
    fn html_empty_file() {
        let doc = HtmlExtractor.extract(&input("empty.html", "")).unwrap();
        assert!(doc.blocks.is_empty());
    }
}
