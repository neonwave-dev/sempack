//! SemPack CLI -- wires the plugin registries and exposes named subcommands.
//!
//! stdout = machine-readable output only (pipe-friendly).
//! stderr = progress, warnings, errors, diagnostic reports.
//!
//! Exit codes: 0 = success, 1 = completed with warnings, 2 = error.

use std::path::PathBuf;

use clap::{Parser, Subcommand};
use sempack_core::{detect, run, ContentClass, Extractor, Input, OutputFormat, Profile, Registry};
use sempack_ir::Block;

#[derive(Parser)]
#[command(name = "sempack", version, about = "Semantic compression toolkit", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the full detection -> extract -> reduce -> emit pipeline.
    Compress {
        /// Input file to compress.
        input: PathBuf,
        /// Write output here (default: stdout).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Compression profile: human | llm | compact.
        #[arg(long, default_value = "human")]
        profile: String,
        /// Output format: markdown | jsonl | ndjson | text | html.
        #[arg(long, default_value = "markdown")]
        format: String,
        /// Fail with exit code 2 if any warnings are emitted.
        #[arg(long)]
        strict: bool,
        /// Print size/token metrics to stderr after completion.
        #[arg(long)]
        stats: bool,
        /// Emit a machine-readable JSON object with compression metrics.
        /// With -o: JSON -> stdout; without -o: JSON -> stderr (pipe-friendly).
        #[arg(long)]
        stats_json: bool,
        /// Dry-run: run the full pipeline but emit a human-readable report
        /// instead of the compressed output. Exits 0 if pipeline would succeed.
        #[arg(long)]
        explain: bool,
    },

    /// Run detection + extraction only; emit raw DocumentIr blocks as JSONL to stdout.
    Extract {
        /// Input file to extract.
        input: PathBuf,
        /// Write output here (default: stdout).
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Fail with exit code 2 if any warnings are emitted.
        #[arg(long)]
        strict: bool,
        /// Pretty-print each JSON block (breaks strict one-block-per-line JSONL).
        #[arg(long)]
        pretty: bool,
    },

    /// Detect format and report a block-type breakdown; no reduce or emit.
    Inspect {
        /// Input file to inspect.
        input: PathBuf,
        /// Fail with exit code 2 if any warnings are emitted.
        #[arg(long)]
        strict: bool,
    },

    /// List all registered extractors and the format keys they handle.
    Formats,

    /// Run the full pipeline and report token/size statistics to stdout.
    Stats {
        /// Input file to measure.
        input: PathBuf,
    },
}

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
    let cli = Cli::parse();
    let code = match cli.command {
        None => {
            use clap::CommandFactory;
            Cli::command().print_help().ok();
            eprintln!();
            0
        }
        Some(cmd) => match run_command(cmd) {
            Ok(exit_code) => exit_code,
            Err(e) => {
                eprintln!("sempack: error: {e}");
                2
            }
        },
    };
    std::process::exit(code);
}

fn run_command(cmd: Commands) -> Result<i32, Box<dyn std::error::Error>> {
    match cmd {
        Commands::Compress {
            input,
            output,
            profile,
            format,
            strict,
            stats,
            stats_json,
            explain,
        } => cmd_compress(input, output, profile, format, strict, stats, stats_json, explain),
        Commands::Extract {
            input,
            output,
            strict,
            pretty,
        } => cmd_extract(input, output, strict, pretty),
        Commands::Inspect { input, strict } => cmd_inspect(input, strict),
        Commands::Formats => cmd_formats(),
        Commands::Stats { input } => cmd_stats(input),
    }
}

fn cmd_compress(
    input: PathBuf,
    output: Option<PathBuf>,
    profile: String,
    format: String,
    strict: bool,
    show_stats: bool,
    stats_json: bool,
    explain: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    let profile: Profile = profile.parse().map_err(string_err)?;
    let format: OutputFormat = format.parse().map_err(string_err)?;

    let bytes = std::fs::read(&input)?;
    let path = input.to_string_lossy().to_string();
    let detected = detect(Some(&path), &bytes);
    let source_format = detected.format.clone();

    let inp = Input {
        path: Some(path),
        bytes,
        detected,
    };
    let out = run(&registry(), inp, profile, format)?;

    if explain {
        // --explain: dry-run mode — print a report, no output file written.
        for ev in &out.events {
            println!("[{}] {}", ev.reducer, ev.detail);
        }
        let savings = if out.tokens_in > 0 {
            100.0 * (1.0 - out.tokens_out as f64 / out.tokens_in as f64)
        } else {
            0.0
        };
        println!(
            "Total: {} -> {} tokens ({:.0}% reduction)",
            out.tokens_in, out.tokens_out, savings
        );
        for w in &out.warnings {
            eprintln!("sempack: warning [{}]: {}", w.code, w.message);
        }
        return Ok(warnings_exit_code(&out.warnings, strict));
    }

    // Normal output: write compressed content.
    match &output {
        Some(p) => std::fs::write(p, &out.content)?,
        None => print!("{}", out.content),
    }

    for w in &out.warnings {
        eprintln!("sempack: warning [{}]: {}", w.code, w.message);
    }

    if show_stats {
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

    if stats_json {
        let savings_pct = if out.bytes_in > 0 {
            let saved = out.bytes_in.saturating_sub(out.bytes_out);
            (saved as f64 / out.bytes_in as f64) * 100.0
        } else {
            0.0
        };
        // Round to 2 decimal places.
        let savings_pct = (savings_pct * 100.0).round() / 100.0;
        let json = serde_json::json!({
            "tokens_in": out.tokens_in,
            "tokens_out": out.tokens_out,
            "bytes_in": out.bytes_in,
            "bytes_out": out.bytes_out,
            "savings_pct": savings_pct,
            "blocks_in": out.blocks_in,
            "blocks_out": out.blocks_out,
            "profile": profile.as_str(),
            "format": format.as_str(),
            "source_format": source_format,
        });
        let json_str = serde_json::to_string(&json)?;
        match &output {
            // -o supplied: compressed content -> file, JSON -> stdout.
            Some(_) => println!("{json_str}"),
            // No -o: compressed content -> stdout (already printed), JSON -> stderr.
            None => eprintln!("{json_str}"),
        }
    }

    Ok(warnings_exit_code(&out.warnings, strict))
}

fn cmd_extract(
    input: PathBuf,
    output: Option<PathBuf>,
    strict: bool,
    pretty: bool,
) -> Result<i32, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(&input)?;
    let path = input.to_string_lossy().to_string();
    let detected = detect(Some(&path), &bytes);

    let inp = Input {
        path: Some(path),
        bytes,
        detected,
    };

    if inp.detected.class != ContentClass::Text {
        return Err(format!(
            "cannot extract: content class is {:?} (only Text is supported)",
            inp.detected.class
        )
        .into());
    }

    let reg = registry();
    let fmt = inp.detected.format.clone();
    let extractor = reg
        .find_extractor(&fmt)
        .or_else(|| reg.find_extractor("text"))
        .ok_or_else(|| format!("no extractor registered for format `{fmt}`"))?;

    let doc = extractor.extract(&inp)?;

    match &output {
        Some(p) => {
            let mut content = String::new();
            for block in &doc.blocks {
                let line = if pretty {
                    serde_json::to_string_pretty(block)?
                } else {
                    serde_json::to_string(block)?
                };
                content.push_str(&line);
                content.push('\n');
            }
            std::fs::write(p, &content)?;
        }
        None => {
            for block in &doc.blocks {
                let line = if pretty {
                    serde_json::to_string_pretty(block)?
                } else {
                    serde_json::to_string(block)?
                };
                println!("{line}");
            }
        }
    }

    for w in &doc.warnings {
        eprintln!("sempack: warning [{}]: {}", w.code, w.message);
    }

    Ok(warnings_exit_code(&doc.warnings, strict))
}

fn cmd_inspect(input: PathBuf, strict: bool) -> Result<i32, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(&input)?;
    let file_size = bytes.len();
    let path = input.to_string_lossy().to_string();
    let detected = detect(Some(&path), &bytes);

    println!("File        : {path}");
    println!("Size        : {file_size} bytes");
    println!("Format      : {}", detected.format);
    println!("Content     : {:?}", detected.class);
    if let Some(ref mt) = detected.media_type {
        println!("Media-type  : {mt}");
    }

    if detected.class != ContentClass::Text {
        println!("Blocks      : (extraction skipped -- not a text document)");
        println!("Warnings    : 0");
        return Ok(0);
    }

    let inp = Input {
        path: Some(path),
        bytes,
        detected,
    };
    let reg = registry();
    let fmt = inp.detected.format.clone();
    let extractor = reg
        .find_extractor(&fmt)
        .or_else(|| reg.find_extractor("text"))
        .ok_or_else(|| format!("no extractor for format `{fmt}`"))?;

    let doc = extractor.extract(&inp)?;

    let mut counts: std::collections::BTreeMap<&'static str, usize> =
        std::collections::BTreeMap::new();
    for b in &doc.blocks {
        count_block(b, &mut counts);
    }

    println!("Blocks      :");
    for (kind, n) in &counts {
        println!("  {kind:<12} {n}");
    }
    println!("Warnings    : {}", doc.warnings.len());
    for w in &doc.warnings {
        eprintln!("sempack: warning [{}]: {}", w.code, w.message);
    }

    Ok(warnings_exit_code(&doc.warnings, strict))
}

fn count_block(b: &Block, counts: &mut std::collections::BTreeMap<&'static str, usize>) {
    let kind: &'static str = match b {
        Block::Document { children } => {
            for c in children {
                count_block(c, counts);
            }
            "document"
        }
        Block::Section { children } => {
            for c in children {
                count_block(c, counts);
            }
            "section"
        }
        Block::Heading { .. } => "heading",
        Block::Paragraph { .. } => "paragraph",
        Block::List { .. } => "list",
        Block::Table { .. } => "table",
        Block::Record { .. } => "record",
        Block::Code { .. } => "code",
        Block::Quote { .. } => "quote",
        Block::Unsupported { .. } => "unsupported",
    };
    *counts.entry(kind).or_insert(0) += 1;
}

fn cmd_formats() -> Result<i32, Box<dyn std::error::Error>> {
    let extractors: Vec<Box<dyn Extractor>> = vec![
        Box::new(sempack_extractors::MarkdownExtractor),
        Box::new(sempack_extractors::TextExtractor),
    ];
    println!("{:<20} {:<30} FEATURE", "EXTRACTOR", "FORMATS");
    println!("{}", "-".repeat(60));
    for e in &extractors {
        println!("{:<20} {:<30} built-in", e.name(), e.formats().join(", "));
    }
    Ok(0)
}

fn cmd_stats(input: PathBuf) -> Result<i32, Box<dyn std::error::Error>> {
    let bytes = std::fs::read(&input)?;
    let path = input.to_string_lossy().to_string();
    let detected = detect(Some(&path), &bytes);

    let inp = Input {
        path: Some(path),
        bytes,
        detected,
    };
    let out = run(&registry(), inp, Profile::Human, OutputFormat::Markdown)?;

    for w in &out.warnings {
        eprintln!("sempack: warning [{}]: {}", w.code, w.message);
    }

    println!("Input  : {} bytes, ~{} tokens", out.bytes_in, out.tokens_in);
    println!(
        "Output : {} bytes, ~{} tokens",
        out.bytes_out, out.tokens_out
    );
    println!(
        "Ratio  : {:.1}% bytes  {:.1}% tokens",
        ratio(out.bytes_out as f64, out.bytes_in as f64),
        ratio(out.tokens_out as f64, out.tokens_in as f64),
    );
    println!("Warnings: {}", out.warnings.len());

    Ok(if out.warnings.is_empty() { 0 } else { 1 })
}

fn string_err(e: String) -> Box<dyn std::error::Error> {
    e.into()
}

fn ratio(a: f64, b: f64) -> f64 {
    if b == 0.0 {
        0.0
    } else {
        100.0 * a / b
    }
}

fn warnings_exit_code(warnings: &[sempack_ir::Warning], strict: bool) -> i32 {
    if warnings.is_empty() {
        0
    } else if strict {
        2
    } else {
        1
    }
}
