//! sempack-core — the engine.
//!
//! Responsibilities: model the [`Input`], [`detect`] its format, **route** it to the
//! text or image pipeline, hold the three plugin [`Registry`]s (extractors, reducers,
//! emitters), and [`run`] the pipeline:
//!
//! ```text
//! detect ─▶ ROUTER ─┬─ text  ─▶ extract ─▶ normalize(NFC) ─▶ reduce ─▶ emit
//!                   └─ image ─▶ (P6 — not built yet)
//! ```
//!
//! Concrete plugins live in the sibling crates and are wired together by the CLI.

use std::borrow::Cow;
use std::str::FromStr;

use sempack_ir::{Block, DocumentIr, Warning};
use unicode_normalization::UnicodeNormalization;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("no extractor registered for format `{0}`")]
    NoExtractor(String),
    #[error("no reducer registered for profile `{0:?}`")]
    NoReducer(Profile),
    #[error("no emitter registered for format `{0:?}`")]
    NoEmitter(OutputFormat),
    #[error("image pipeline is not available yet (planned for P6)")]
    ImagePipelineUnavailable,
    #[error("unrecognized input: {0}")]
    UnsupportedInput(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------
// Detection & routing
// ---------------------------------------------------------------------------

/// Top-level content class the router dispatches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentClass {
    Text,
    Image,
    Unknown,
}

/// The result of detection: a format key plus the routing class.
#[derive(Debug, Clone)]
pub struct Detected {
    pub format: String,
    pub media_type: Option<String>,
    pub class: ContentClass,
}

/// Detect a file's format from its extension, falling back to a UTF-8 content sniff.
pub fn detect(path: Option<&str>, bytes: &[u8]) -> Detected {
    let ext = path.and_then(|p| {
        let name = p.rsplit(['/', '\\']).next().unwrap_or(p);
        name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())
    });

    let (format, class, media): (&str, ContentClass, &str) = match ext.as_deref() {
        Some("md") | Some("markdown") => ("markdown", ContentClass::Text, "text/markdown"),
        Some("txt") | Some("text") | Some("log") => ("text", ContentClass::Text, "text/plain"),
        Some("json") => ("json", ContentClass::Text, "application/json"),
        Some("jsonl") | Some("ndjson") => ("jsonl", ContentClass::Text, "application/x-ndjson"),
        Some("csv") => ("csv", ContentClass::Text, "text/csv"),
        Some("tsv") => ("tsv", ContentClass::Text, "text/tab-separated-values"),
        Some("html") | Some("htm") => ("html", ContentClass::Text, "text/html"),
        Some("xml") => ("xml", ContentClass::Text, "application/xml"),
        Some("svg") => ("svg", ContentClass::Text, "image/svg+xml"),
        Some("png") => ("png", ContentClass::Image, "image/png"),
        Some("jpg") | Some("jpeg") => ("jpeg", ContentClass::Image, "image/jpeg"),
        Some("webp") => ("webp", ContentClass::Image, "image/webp"),
        Some("gif") => ("gif", ContentClass::Image, "image/gif"),
        _ => {
            if std::str::from_utf8(bytes).is_ok() {
                ("text", ContentClass::Text, "text/plain")
            } else {
                ("unknown", ContentClass::Unknown, "application/octet-stream")
            }
        }
    };

    Detected {
        format: format.to_string(),
        media_type: Some(media.to_string()),
        class,
    }
}

// ---------------------------------------------------------------------------
// Profiles & output formats
// ---------------------------------------------------------------------------

/// Compression profile (which reducer to run). P1 ships `human` + `llm`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Human,
    Llm,
    Compact,
    Debug,
}

impl FromStr for Profile {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "human" => Ok(Profile::Human),
            "llm" => Ok(Profile::Llm),
            "compact" => Ok(Profile::Compact),
            "debug" => Err("debug profile is not yet available".to_string()),
            other => Err(format!(
                "unknown profile `{other}` (try: human, llm, compact)"
            )),
        }
    }
}

/// Serialization target (which emitter to run).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    Markdown,
    Jsonl,
    Ndjson,
    Text,
    Html,
}

impl FromStr for OutputFormat {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "markdown" | "md" => Ok(OutputFormat::Markdown),
            "jsonl" => Ok(OutputFormat::Jsonl),
            "ndjson" => Ok(OutputFormat::Ndjson),
            "text" | "txt" => Ok(OutputFormat::Text),
            "html" => Ok(OutputFormat::Html),
            other => Err(format!(
                "unknown format `{other}` (try: markdown, jsonl, ndjson, text, html)"
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Plugin traits
// ---------------------------------------------------------------------------

/// One unit of work handed to an [`Extractor`].
pub struct Input {
    pub path: Option<String>,
    pub bytes: Vec<u8>,
    pub detected: Detected,
}

impl Input {
    /// The raw bytes as text (lossy — invalid UTF-8 becomes U+FFFD).
    pub fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }
}

/// Turns raw input into a [`DocumentIr`]. Must not reduce or serialize.
pub trait Extractor: Send + Sync {
    fn name(&self) -> &'static str;
    /// Format keys (see [`detect`]) this extractor handles.
    fn formats(&self) -> &'static [&'static str];
    fn extract(&self, input: &Input) -> Result<DocumentIr>;
}

/// Mutates a [`DocumentIr`] in place according to a [`Profile`]. Must not serialize.
pub trait Reducer: Send + Sync {
    fn name(&self) -> &'static str;
    fn profile(&self) -> Profile;
    fn reduce(&self, doc: &mut DocumentIr) -> Result<()>;
}

/// Serializes a [`DocumentIr`] to a string in one [`OutputFormat`]. Must not extract.
pub trait Emitter: Send + Sync {
    fn name(&self) -> &'static str;
    fn format(&self) -> OutputFormat;
    fn emit(&self, doc: &DocumentIr) -> Result<String>;
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Holds the registered plugins for all three stages. Built with a fluent API and
/// queried by the pipeline. (Feature-gated built-ins and third-party dynamic plugins
/// register here later.)
#[derive(Default)]
pub struct Registry {
    extractors: Vec<Box<dyn Extractor>>,
    reducers: Vec<Box<dyn Reducer>>,
    emitters: Vec<Box<dyn Emitter>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn extractor(mut self, e: impl Extractor + 'static) -> Self {
        self.extractors.push(Box::new(e));
        self
    }

    pub fn reducer(mut self, r: impl Reducer + 'static) -> Self {
        self.reducers.push(Box::new(r));
        self
    }

    pub fn emitter(mut self, e: impl Emitter + 'static) -> Self {
        self.emitters.push(Box::new(e));
        self
    }

    pub fn find_extractor(&self, format: &str) -> Option<&dyn Extractor> {
        self.extractors
            .iter()
            .find(|e| e.formats().contains(&format))
            .map(|b| b.as_ref())
    }

    pub fn find_reducer(&self, profile: Profile) -> Option<&dyn Reducer> {
        self.reducers
            .iter()
            .find(|r| r.profile() == profile)
            .map(|b| b.as_ref())
    }

    pub fn find_emitter(&self, format: OutputFormat) -> Option<&dyn Emitter> {
        self.emitters
            .iter()
            .find(|e| e.format() == format)
            .map(|b| b.as_ref())
    }
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// The product of a pipeline run: serialized output plus before/after metrics.
#[derive(Debug, Clone)]
pub struct Output {
    pub content: String,
    pub format: OutputFormat,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub tokens_in: usize,
    pub tokens_out: usize,
    pub warnings: Vec<Warning>,
}

/// Run the full pipeline: route → extract → normalize → reduce → emit.
pub fn run(reg: &Registry, input: Input, profile: Profile, format: OutputFormat) -> Result<Output> {
    let bytes_in = input.bytes.len() as u64;
    // Baseline metrics reflect what an agent would consume if it read the raw file —
    // so the before/after compares the original input against the compressed output.
    let tokens_in = sempack_tokenizers::approx_tokens(&input.text());

    // --- Route -------------------------------------------------------------
    match input.detected.class {
        ContentClass::Image => return Err(Error::ImagePipelineUnavailable),
        // Unknown = not valid UTF-8 and not a recognized image. Reject it rather
        // than lossy-decoding binary into replacement characters and reporting
        // misleading metrics for it.
        ContentClass::Unknown => {
            return Err(Error::UnsupportedInput(
                "input is neither valid UTF-8 text nor a recognized image format".into(),
            ))
        }
        ContentClass::Text => {}
    }

    // --- Extract (with graceful fall-back to the plain-text extractor) ------
    let fmt = input.detected.format.clone();
    let extractor = reg
        .find_extractor(&fmt)
        .or_else(|| reg.find_extractor("text"))
        .ok_or(Error::NoExtractor(fmt))?;
    let mut doc = extractor.extract(&input)?;

    // --- Normalize (Unicode Safe / NFC, on by default) ---------------------
    normalize_nfc(&mut doc);

    // --- Reduce ------------------------------------------------------------
    let reducer = reg.find_reducer(profile).ok_or(Error::NoReducer(profile))?;
    reducer.reduce(&mut doc)?;

    // --- Emit --------------------------------------------------------------
    let emitter = reg.find_emitter(format).ok_or(Error::NoEmitter(format))?;
    let content = emitter.emit(&doc)?;

    let bytes_out = content.len() as u64;
    let tokens_out = sempack_tokenizers::approx_tokens(&content);

    Ok(Output {
        content,
        format,
        bytes_in,
        bytes_out,
        tokens_in,
        tokens_out,
        warnings: doc.warnings.clone(),
    })
}

/// Apply Unicode NFC normalization to every text-bearing field.
fn normalize_nfc(doc: &mut DocumentIr) {
    if let Some(t) = &mut doc.title {
        *t = t.nfc().collect();
    }
    for b in &mut doc.blocks {
        normalize_block(b);
    }
}

fn normalize_block(b: &mut Block) {
    match b {
        Block::Document { children } | Block::Section { children } => {
            for c in children {
                normalize_block(c);
            }
        }
        Block::Heading { text, .. }
        | Block::Paragraph { text }
        | Block::Quote { text }
        | Block::Code { text, .. } => {
            *text = text.nfc().collect();
        }
        Block::List { items, .. } => {
            for i in items {
                *i = i.nfc().collect();
            }
        }
        Block::Table { headers, rows } => {
            for h in headers {
                *h = h.nfc().collect();
            }
            for r in rows {
                for c in r {
                    *c = c.nfc().collect();
                }
            }
        }
        Block::Record { fields } => {
            for f in fields {
                f.key = f.key.nfc().collect();
                f.value = f.value.nfc().collect();
            }
        }
        Block::Unsupported { note } => {
            *note = note.nfc().collect();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_markdown_by_extension() {
        let d = detect(Some("notes/readme.md"), b"# hi");
        assert_eq!(d.format, "markdown");
        assert_eq!(d.class, ContentClass::Text);
    }

    #[test]
    fn detects_image_by_extension() {
        let d = detect(Some("photo.PNG"), &[]);
        assert_eq!(d.class, ContentClass::Image);
    }

    #[test]
    fn unknown_binary_sniffs_to_unknown() {
        let d = detect(None, &[0xff, 0xfe, 0x00]);
        assert_eq!(d.class, ContentClass::Unknown);
    }

    #[test]
    fn unknown_input_is_rejected_by_pipeline() {
        // Non-UTF8 binary with no known extension must error, not flow through
        // the text path as replacement characters.
        let bytes = vec![0xff, 0xfe, 0x00];
        let detected = detect(None, &bytes);
        let input = Input {
            path: None,
            bytes,
            detected,
        };
        let err = run(&Registry::new(), input, Profile::Human, OutputFormat::Text).unwrap_err();
        assert!(matches!(err, Error::UnsupportedInput(_)));
    }

    #[test]
    fn profile_and_format_parse() {
        assert_eq!("llm".parse::<Profile>().unwrap(), Profile::Llm);
        assert_eq!("compact".parse::<Profile>().unwrap(), Profile::Compact);
        assert_eq!(
            "md".parse::<OutputFormat>().unwrap(),
            OutputFormat::Markdown
        );
        assert!("nope".parse::<Profile>().is_err());
        assert!("debug".parse::<Profile>().is_err());
    }
}
