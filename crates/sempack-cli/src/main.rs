//! SemPack CLI -- wires the plugin registries and runs the pipeline on one file.
//!
//! ```text
//! sempack <INPUT> [--profile human|llm] [--format markdown|jsonl|ndjson|text|html]
//!                 [-o OUTPUT] [--stats]
//! ```
//!
//! P1 is single-file only; batch/folder processing arrives with the P4 history phase.

use std::path::PathBuf;

use clap::Parser;
use sempack_core::{detect, run, Input, OutputFormat, Profile, Registry};

#[derive(Parser)]
#[command(name = "sempack", version, about = "Semantic compression toolkit")]
struct Cli {
    /// Input file to compress.
    input: PathBuf,

    /// Write output here (default: stdout).
    #[arg(short, long)]
    output: Option<PathBuf>,

    /// Compression profile: human | llm | compact (debug not yet available).
    #[arg(long, default_value = "human")]
    profile: String,

    /// Output format: markdown | jsonl | ndjson | text | html.
    #[arg(long, default_value = "markdown")]
    format: String,

    /// Print size/token metrics to stderr.
    #[arg(long)]
    stats: bool,
}

/// Build the plugin registry. (This is the single wiring point for all three stages.)
fn registry() -> Registry {
    Registry::new()
        .extractor(sempack_extractors::MarkdownExtractor)
        .extractor(sempack_extractors::TextExtractor)
        .extractor(sempack_extractors::HtmlExtractor)
        .extractor(sempack_extractors::XmlExtractor)
        .extractor(sempack_extractors::SvgExtractor)
        .extractor(sempack_extractors::JsonExtractor)
        .extractor(sempack_extractors::JsonlExtractor)
        .extractor(sempack_extractors::CsvExtractor)
        .extractor(sempack_extractors::TsvExtractor)
        .extractor(sempack_extractors::PsvExtractor)
        .reducer(sempack_reducers::HumanReducer)
        .reducer(sempack_reducers::LlmReducer)
        .reducer(sempack_reducers::CompactReducer)
        .emitter(sempack_emitters::MarkdownEmitter)
        .emitter(sempack_emitters::JsonlEmitter)
        .emitter(sempack_emitters::NdjsonEmitter)
        .emitter(sempack_emitters::TextEmitter)
        .emitter(sempack_emitters::HtmlEmitter)
}

fn main() {
    if let Err(e) = real_main(Cli::parse()) {
        eprintln!("sempack: error: {e}");
        std::process::exit(1);
    }
}

fn real_main(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let profile: Profile = cli.profile.parse().map_err(string_err)?;
    let format: OutputFormat = cli.format.parse().map_err(string_err)?;

    let bytes = std::fs::read(&cli.input)?;
    let path = cli.input.to_string_lossy().to_string();
    let detected = detect(Some(&path), &bytes);

    let input = Input {
        path: Some(path),
        bytes,
        detected,
    };

    let out = run(&registry(), input, profile, format)?;

    match &cli.output {
        Some(p) => std::fs::write(p, &out.content)?,
        None => print!("{}", out.content),
    }

    for w in &out.warnings {
        eprintln!("sempack: warning [{}]: {}", w.code, w.message);
    }

    if cli.stats {
        eprintln!(
            "bytes {} -> {} ({:.0}% of original) | tokens ~{} -> ~{} ({:.0}%) | warnings {}",
            out.bytes_in,
            out.bytes_out,
            ratio(out.bytes_out as f64, out.bytes_in as f64),
            out.tokens_in,
            out.tokens_out,
            ratio(out.tokens_out as f64, out.tokens_in as f64),
            out.warnings.len(),
        );
    }

    Ok(())
}

fn ratio(a: f64, b: f64) -> f64 {
    if b == 0.0 {
        0.0
    } else {
        100.0 * a / b
    }
}

/// Lift a `String` error into a boxed `std::error::Error`.
fn string_err(e: String) -> Box<dyn std::error::Error> {
    e.into()
}
