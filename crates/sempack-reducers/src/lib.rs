//! Reducer plugins â€” the compression profiles.
//!
//! P1 ships two. Both clean whitespace and drop empty blocks; `llm` additionally
//! squeezes structure for token economy. The profile set widens later (`compact`
//! soon, `debug` dev-flag, `rag` deferred) â€” each is just another [`Reducer`].

use sempack_core::{Profile, ReductionEvent, Reducer, Result};
use sempack_ir::{Block, DocumentIr};

/// Collapse all runs of whitespace in `s` into single spaces.
fn collapse(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Collapse runs of whitespace in the prose blocks (paragraph / heading / quote /
/// list items). Code blocks and structured blocks (table / record / unsupported) are
/// left as-is, and container blocks are not recursed into.
/// Returns the number of blocks that were modified.
fn collapse_ws(doc: &mut DocumentIr) -> usize {
    let mut changed = 0usize;
    for b in &mut doc.blocks {
        match b {
            Block::Paragraph { text } | Block::Heading { text, .. } | Block::Quote { text } => {
                let new = collapse(text);
                if new != *text {
                    *text = new;
                    changed += 1;
                }
            }
            Block::List { items, .. } => {
                let before = items.len();
                for i in items.iter_mut() {
                    *i = collapse(i);
                }
                items.retain(|i| !i.is_empty());
                if items.len() < before {
                    changed += 1;
                }
            }
            _ => {}
        }
    }
    changed
}

/// Drop blocks that carry no content after cleanup.
/// Returns the number of blocks dropped.
fn drop_empty(doc: &mut DocumentIr) -> usize {
    let before = doc.blocks.len();
    doc.blocks.retain(|b| match b {
        Block::Paragraph { text } | Block::Heading { text, .. } | Block::Quote { text } => {
            !text.trim().is_empty()
        }
        Block::List { items, .. } => !items.is_empty(),
        _ => true,
    });
    before - doc.blocks.len()
}

/// `human` â€” light touch: tidy whitespace, drop empties, keep all structure.
pub struct HumanReducer;

impl Reducer for HumanReducer {
    fn name(&self) -> &'static str {
        "human"
    }
    fn profile(&self) -> Profile {
        Profile::Human
    }
    fn reduce(&self, doc: &mut DocumentIr) -> Result<Vec<ReductionEvent>> {
        let mut events = Vec::new();
        let ws_changed = collapse_ws(doc);
        if ws_changed > 0 {
            events.push(ReductionEvent {
                reducer: "human",
                detail: format!("collapsed whitespace in {ws_changed} block(s)"),
            });
        }
        let dropped = drop_empty(doc);
        if dropped > 0 {
            events.push(ReductionEvent {
                reducer: "human",
                detail: format!("dropped {dropped} empty block(s)"),
            });
        }
        Ok(events)
    }
}

/// `llm` â€” human cleanup plus token-economy moves: merge adjacent paragraphs and
/// fold block quotes into paragraphs (the prose matters, the decoration does not).
pub struct LlmReducer;

impl Reducer for LlmReducer {
    fn name(&self) -> &'static str {
        "llm"
    }
    fn profile(&self) -> Profile {
        Profile::Llm
    }
    fn reduce(&self, doc: &mut DocumentIr) -> Result<Vec<ReductionEvent>> {
        let mut events = Vec::new();

        let ws_changed = collapse_ws(doc);
        if ws_changed > 0 {
            events.push(ReductionEvent {
                reducer: "llm",
                detail: format!("collapsed whitespace in {ws_changed} block(s)"),
            });
        }

        let dropped = drop_empty(doc);
        if dropped > 0 {
            events.push(ReductionEvent {
                reducer: "llm",
                detail: format!("dropped {dropped} empty block(s)"),
            });
        }

        // Quotes -> paragraphs (drop the citation framing for the model).
        let mut quotes_converted = 0usize;
        for b in &mut doc.blocks {
            if let Block::Quote { text } = b {
                *b = Block::Paragraph {
                    text: std::mem::take(text),
                };
                quotes_converted += 1;
            }
        }
        if quotes_converted > 0 {
            events.push(ReductionEvent {
                reducer: "llm",
                detail: format!("converted {quotes_converted} quote(s) to paragraph(s)"),
            });
        }

        // Merge consecutive paragraphs into one.
        let before_merge = doc.blocks.len();
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
        let pairs_merged = before_merge - doc.blocks.len();
        if pairs_merged > 0 {
            events.push(ReductionEvent {
                reducer: "llm",
                detail: format!("merged {pairs_merged} consecutive paragraph pair(s)"),
            });
        }

        Ok(events)
    }
}

/// `compact` -- aggressive token-economy: whitespace cleanup, drop quotes entirely,
/// deduplicate identical paragraphs, merge consecutive short paragraphs
/// (combined length < 200 chars). Targets >=40% block-count reduction vs `human`
/// on typical prose.
pub struct CompactReducer;

impl Reducer for CompactReducer {
    fn name(&self) -> &'static str {
        "compact"
    }
    fn profile(&self) -> Profile {
        Profile::Compact
    }
    fn reduce(&self, doc: &mut DocumentIr) -> Result<Vec<ReductionEvent>> {
        let mut events = Vec::new();

        // 1. Collapse whitespace runs in prose blocks.
        let ws_changed = collapse_ws(doc);
        if ws_changed > 0 {
            events.push(ReductionEvent {
                reducer: "compact",
                detail: format!("collapsed whitespace in {ws_changed} block(s)"),
            });
        }

        // 2. Drop Block::Quote entirely (aggressive -- the citation framing and
        //    the quoted prose both go; compact cares only about token savings).
        let before_quotes = doc.blocks.len();
        doc.blocks.retain(|b| !matches!(b, Block::Quote { .. }));
        let quotes_dropped = before_quotes - doc.blocks.len();
        if quotes_dropped > 0 {
            events.push(ReductionEvent {
                reducer: "compact",
                detail: format!("dropped {quotes_dropped} quote block(s)"),
            });
        }

        // 3. Drop empty blocks.
        let dropped = drop_empty(doc);
        if dropped > 0 {
            events.push(ReductionEvent {
                reducer: "compact",
                detail: format!("dropped {dropped} empty block(s)"),
            });
        }

        // 4. Deduplicate paragraphs with identical text (keep first occurrence).
        // collapse_ws already trimmed all prose blocks, so no extra trim needed.
        let before_dedup = doc.blocks.len();
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(doc.blocks.len());
        doc.blocks.retain(|b| match b {
            Block::Paragraph { text } => seen.insert(text.clone()),
            _ => true,
        });
        let deduped = before_dedup - doc.blocks.len();
        if deduped > 0 {
            events.push(ReductionEvent {
                reducer: "compact",
                detail: format!("removed {deduped} duplicate paragraph(s)"),
            });
        }

        // 5. Merge consecutive paragraphs whose combined length is < 200 chars.
        let before_merge = doc.blocks.len();
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
        let pairs_merged = before_merge - doc.blocks.len();
        if pairs_merged > 0 {
            events.push(ReductionEvent {
                reducer: "compact",
                detail: format!("merged {pairs_merged} short consecutive paragraph(s)"),
            });
        }

        Ok(events)
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
    #[test]
    fn human_reducer_returns_events_on_change() {
        let mut d = doc(vec![
            Block::Paragraph { text: "a  b".into() },
            Block::Paragraph { text: "   ".into() },
        ]);
        let events = HumanReducer.reduce(&mut d).unwrap();
        assert!(!events.is_empty(), "expected at least one reduction event");
    }

    #[test]
    fn no_events_when_nothing_changes() {
        let mut d = doc(vec![Block::Paragraph {
            text: "already clean".into(),
        }]);
        let events = HumanReducer.reduce(&mut d).unwrap();
        assert!(events.is_empty(), "expected no events when nothing changed");
    }

}
