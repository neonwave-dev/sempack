//! SemPack IR — the single universal intermediate representation.
//!
//! Every extractor normalizes its input into a [`DocumentIr`]: an ordered list of
//! [`Block`]s plus source provenance, metadata and warnings. Reducers mutate the IR;
//! emitters serialize it. The IR is the contract between the three plugin stages.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A fully extracted document in SemPack's universal form.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocumentIr {
    pub id: String,
    pub source: SourceInfo,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub metadata: Metadata,
    pub blocks: Vec<Block>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<Warning>,
}

impl DocumentIr {
    pub fn new(id: impl Into<String>, source: SourceInfo) -> Self {
        Self {
            id: id.into(),
            source,
            title: None,
            metadata: Metadata::default(),
            blocks: Vec::new(),
            warnings: Vec::new(),
        }
    }

    pub fn push(&mut self, block: Block) {
        self.blocks.push(block);
    }

    pub fn warn(&mut self, code: impl Into<String>, message: impl Into<String>) {
        self.warnings.push(Warning {
            code: code.into(),
            message: message.into(),
        });
    }

    /// Concatenate all human-visible text — used for token/size metrics.
    pub fn plain_text(&self) -> String {
        let mut s = String::new();
        for b in &self.blocks {
            b.write_text(&mut s);
        }
        s
    }
}

/// Where a document came from and how it was detected.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceInfo {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub detected_format: String,
    pub bytes: u64,
}

/// Free-form key/value metadata (front-matter, doc properties, …).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metadata {
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub extra: BTreeMap<String, String>,
}

/// A non-fatal issue encountered during extraction or reduction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Warning {
    pub code: String,
    pub message: String,
}

/// A key/value pair inside a [`Block::Record`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Field {
    pub key: String,
    pub value: String,
}

/// The universal block set.
///
/// P1 implements the text-document variants. The reserved variants
/// (`Email`, `Event`, `Contact`, `FeedItem`, `AttachmentRef`, `ImageRef`,
/// `ArchiveEntry`) land with the fast-follow extractors; they are intentionally
/// not yet defined to keep the surface small while the pipeline stabilizes.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Block {
    Document {
        children: Vec<Block>,
    },
    Section {
        children: Vec<Block>,
    },
    Heading {
        level: u8,
        text: String,
    },
    Paragraph {
        text: String,
    },
    List {
        ordered: bool,
        items: Vec<String>,
    },
    Table {
        headers: Vec<String>,
        rows: Vec<Vec<String>>,
    },
    Record {
        fields: Vec<Field>,
    },
    Code {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        lang: Option<String>,
        text: String,
    },
    Quote {
        text: String,
    },
    Unsupported {
        note: String,
    },
}

impl Block {
    /// Append this block's human-visible text to `out` (recursing into containers).
    pub fn write_text(&self, out: &mut String) {
        match self {
            Block::Document { children } | Block::Section { children } => {
                for c in children {
                    c.write_text(out);
                }
            }
            Block::Heading { text, .. }
            | Block::Paragraph { text }
            | Block::Quote { text }
            | Block::Code { text, .. } => {
                out.push_str(text);
                out.push('\n');
            }
            Block::List { items, .. } => {
                for i in items {
                    out.push_str(i);
                    out.push('\n');
                }
            }
            Block::Table { headers, rows } => {
                out.push_str(&headers.join(" "));
                out.push('\n');
                for r in rows {
                    out.push_str(&r.join(" "));
                    out.push('\n');
                }
            }
            Block::Record { fields } => {
                for f in fields {
                    out.push_str(&f.key);
                    out.push_str(": ");
                    out.push_str(&f.value);
                    out.push('\n');
                }
            }
            Block::Unsupported { note } => {
                out.push_str(note);
                out.push('\n');
            }
        }
    }
}
