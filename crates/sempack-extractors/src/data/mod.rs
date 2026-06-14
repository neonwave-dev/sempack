//! Data-format extractors: JSON, JSONL, CSV, TSV, PSV.
pub mod json;
pub mod jsonl;
pub mod delimited;

pub use json::JsonExtractor;
pub use jsonl::JsonlExtractor;
pub use delimited::{CsvExtractor, PsvExtractor, TsvExtractor};
