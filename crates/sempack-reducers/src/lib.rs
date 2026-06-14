//! Reducer plugins — the compression profiles.
//!
//! P1 ships two. Both clean whitespace and drop empty blocks; `llm` additionally
//! squeezes structure for token economy. The profile set widens later (`compact`
//! soon, `debug` dev-flag, `rag` deferred) — each is just another [`Reducer`].

use sempack_core::{Profile, Reducer, Result};
use sempack_ir::{Block, DocumentIr};

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

/// `llm` — human cleanup plus token-economy moves: merge adjacent paragraphs and
/// fold block quotes into paragraphs (the prose matters, the decoration does not).
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

        // Quotes → paragraphs (drop the citation framing for the model).
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
        Ok(())
    }
}

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
}
