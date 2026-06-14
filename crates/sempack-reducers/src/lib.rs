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

// ---------------------------------------------------------------------------
// Log-aware reduction
// ---------------------------------------------------------------------------

/// Log level parsed from a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Trace,
    Unknown,
}

/// Decide if a single line (trimmed) is a stack-trace frame. These are always
/// kept regardless of their log level — they carry essential diagnostic context.
///
/// Patterns handled:
/// - `at ClassName.method(File.java:42)` — Java / JVM
/// - `at /path/to/file.rs:42` or `at crate::module::fn` — Rust / JS
/// - `File "path.py", line 42` — Python
/// - `caused by:` — Rust / Java chain prefixes
/// - `  0x...` or `  #0 ...` — C/GDB frame pointers
/// - `    panicked at 'msg', src/main.rs:10:5` — Rust panic
fn is_stack_trace_line(trimmed: &str) -> bool {
    let lower = trimmed.to_ascii_lowercase();

    // "at ..." — Java, JS, Rust backtraces
    if lower.starts_with("at ") {
        return true;
    }
    // Python traceback frames
    if lower.starts_with("file \"") && lower.contains(", line ") {
        return true;
    }
    // "caused by:" chain prefix (Rust / Java)
    if lower.starts_with("caused by:") || lower.starts_with("caused by ") {
        return true;
    }
    // "panicked at" Rust panic lines
    if lower.starts_with("panicked at") || lower.starts_with("thread '") {
        return true;
    }
    // C/GDB-style frame pointers: "0x00007f..." or "#0  0x..."
    if trimmed.starts_with("0x") || trimmed.starts_with('#') {
        return true;
    }
    // Rust/cargo backtrace: "   N: symbol" where N is a digit
    if trimmed
        .chars()
        .next()
        .map_or(false, |c| c.is_ascii_digit())
        && trimmed.contains(": ")
    {
        return true;
    }
    false
}

/// Classify the log level of a line, scanning for common level tags.
///
/// Formats handled (case-insensitive):
/// - `[LEVEL]` or `(LEVEL)` brackets
/// - `LEVEL:` prefix (syslog style)
/// - pytest / cargo embedded words: `FAILED`, `PASSED`, `ok`, `ERROR`, `WARN`
/// - `npm ERR!` / `npm WARN`
fn classify_level(line: &str) -> LogLevel {
    // Search within first 60 *characters* so we are not misled by levels deep
    // in content. Use char_indices to avoid splitting a multi-byte code point.
    let head_len = line
        .char_indices()
        .nth(60)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    let head = &line[..head_len];
    let head_up = head.to_ascii_uppercase();

    for (tag, level) in &[
        ("ERROR", LogLevel::Error),
        ("ERR!", LogLevel::Error),
        (" ERR ", LogLevel::Error),
        ("FAILED", LogLevel::Error),
        ("FAIL", LogLevel::Error),
        ("PANIC", LogLevel::Error),
        ("FATAL", LogLevel::Error),
        ("CRITICAL", LogLevel::Error),
        ("WARN", LogLevel::Warn),
        ("WARNING", LogLevel::Warn),
        ("INFO", LogLevel::Info),
        ("PASSED", LogLevel::Info),
        ("DEBUG", LogLevel::Debug),
        ("TRACE", LogLevel::Trace),
    ] {
        if head_up.contains(tag) {
            return *level;
        }
    }

    LogLevel::Unknown
}

/// Strip a leading timestamp / counter token from a line so that consecutive
/// lines that differ only in timestamp compare as similar. Returns a substring
/// of `line` (no allocation).
///
/// Removed prefixes (heuristic):
/// - ISO-8601 / RFC-3339 datetime: `2024-01-15T12:34:56Z` or `2024-01-15 12:34:56`
/// - `[HH:MM:SS]` or `[1234]`
/// - Leading numeric counters / PID: `1234 `
fn strip_timestamp(line: &str) -> &str {
    let s = line.trim_start();

    // ISO date prefix: YYYY-MM-DD (must be exactly 4-digit-dash-2-digit-dash-2-digit)
    if s.len() >= 10 {
        let b = s.as_bytes();
        if b[4] == b'-' && b[7] == b'-' {
            // Skip the date part (10 chars: YYYY-MM-DD).
            let after_date = &s[10..];
            // Skip optional separator between date and time ('T' or ' ').
            let after_sep = after_date.trim_start_matches(|c: char| c == 'T' || c == ' ');
            // Skip HH:MM:SS and optional fractional seconds.
            // Time pattern: digit digit ':' digit digit ':' digit digit [. digits]
            let after_time = skip_time_component(after_sep);
            // Skip trailing timezone: Z / +HH:MM / -HH:MM.
            let after_tz = skip_tz_offset(after_time);
            let rest = after_tz.trim_start();
            if !rest.is_empty() {
                return rest;
            }
        }
    }

    // Bracketed token [HH:MM:SS] or [1234] or [DEBUG] (the last case is fine —
    // stripping [DEBUG] still leaves `cache miss …` as a stable prefix).
    if s.starts_with('[') {
        if let Some(end) = s.find(']') {
            let after = s[end + 1..].trim_start();
            if !after.is_empty() {
                return after;
            }
        }
    }

    // Bare numeric counter at start: "12345 rest"
    let digits_end = s
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(s.len());
    if digits_end > 0 && digits_end < s.len() && s.as_bytes()[digits_end] == b' ' {
        return &s[digits_end + 1..];
    }

    s
}

/// Skip an HH:MM:SS[.fraction] time substring from the start of `s`.
/// Returns whatever comes after the time (or `s` unchanged if no match).
fn skip_time_component(s: &str) -> &str {
    // Minimum: "HH:MM:SS" = 8 chars.
    if s.len() < 8 {
        return s;
    }
    let b = s.as_bytes();
    // Require digits at positions 0,1,3,4,6,7 and ':' at 2,5.
    if b[2] == b':' && b[5] == b':' {
        let all_digits = b[0].is_ascii_digit()
            && b[1].is_ascii_digit()
            && b[3].is_ascii_digit()
            && b[4].is_ascii_digit()
            && b[6].is_ascii_digit()
            && b[7].is_ascii_digit();
        if all_digits {
            let after = &s[8..];
            // Optional fractional seconds: .NNN or ,NNN
            if after.starts_with('.') || after.starts_with(',') {
                let frac_end = after[1..]
                    .find(|c: char| !c.is_ascii_digit())
                    .map_or(after.len(), |i| i + 1);
                return &after[frac_end..];
            }
            return after;
        }
    }
    s
}

/// Skip a timezone offset like `Z`, `+05:30`, or `-08:00` from the start of `s`.
fn skip_tz_offset(s: &str) -> &str {
    if s.starts_with('Z') {
        return &s[1..];
    }
    if s.starts_with(['+', '-']) && s.len() >= 5 {
        // +HH:MM or +HHMM
        let offset_len = if s.as_bytes().get(3) == Some(&b':') { 6 } else { 5 };
        return &s[offset_len.min(s.len())..];
    }
    s
}

/// Compute a similarity signature for a log line: strip leading timestamp,
/// then replace all digit runs with `N` so that lines differing only in
/// counter values (e.g. `tick=0` vs `tick=1`) compare as equal.
fn line_sig(trimmed: &str) -> String {
    let after_ts = strip_timestamp(trimmed);
    // Replace digit sequences with 'N' to normalise counter/id fields.
    let mut sig = String::with_capacity(after_ts.len());
    let mut in_digits = false;
    for c in after_ts.chars() {
        if c.is_ascii_digit() {
            if !in_digits {
                sig.push('N');
                in_digits = true;
            }
        } else {
            in_digits = false;
            sig.push(c);
        }
    }
    sig
}

/// Decide whether a `Block::Code` block looks like log output.
///
/// A block is considered a log when **both** conditions hold:
/// 1. The language hint (if present) is one of: `None`, `"text"`, `"log"`,
///    `"console"`, `"sh"`, `"bash"`, `"shell"`, `"output"`, `"plaintext"`.
///    Language hints that name real programming languages (e.g. `"rust"`,
///    `"python"`, `"javascript"`) disqualify the block immediately.
/// 2. At least 20% of lines (minimum 2 lines) match a log-line pattern.
fn is_log_block(lang: Option<&str>, text: &str) -> bool {
    // Check language hint first — real source code languages are disqualified.
    match lang {
        Some(l) => {
            let l = l.to_ascii_lowercase();
            let log_langs: &[&str] = &[
                "text", "log", "console", "sh", "bash", "shell", "output", "plaintext", "plain",
                "txt",
            ];
            if !log_langs.contains(&l.as_str()) {
                return false; // e.g. "rust", "python", "javascript" — not a log
            }
        }
        None => {} // no hint — proceed to content scan
    }

    let lines: Vec<&str> = text.lines().collect();
    if lines.len() < 2 {
        return false; // too short to bother
    }

    let matching = lines
        .iter()
        .filter(|l| {
            let up = l.to_ascii_uppercase();
            up.contains("ERROR")
                || up.contains("WARN")
                || up.contains("INFO")
                || up.contains("DEBUG")
                || up.contains("TRACE")
                || up.contains("FATAL")
                || up.contains("CRITICAL")
                || up.contains("FAILED")
                || up.contains("PASSED")
                || up.contains("ERR!")
                // cargo test: "test foo ... ok" / "test foo ... FAILED"
                || (up.contains("TEST ") && (up.ends_with("OK") || up.ends_with("FAILED")))
        })
        .count();

    matching * 5 >= lines.len() // >=20% of lines look like log lines
}

/// Process a single Code block's text through log-aware reduction.
/// Returns the reduced text (may be identical to input if no changes needed).
fn reduce_log_text(text: &str) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let mut output: Vec<String> = Vec::with_capacity(lines.len());

    // For dedup: track the similarity signature of the last line in the current
    // run (timestamp-stripped, digits normalised to 'N').
    let mut prev_sig: Option<String> = None;
    let mut run_count: usize = 0; // consecutive similar-line count

    let flush_run = |output: &mut Vec<String>, count: usize| {
        if count > 0 {
            let noun = if count == 1 { "line" } else { "lines" };
            output.push(format!("[{count} similar {noun} omitted]"));
        }
    };

    for line in &lines {
        let trimmed = line.trim();

        // (a) Stack trace lines — always keep, regardless of level.
        if is_stack_trace_line(trimmed) {
            if run_count > 0 {
                flush_run(&mut output, run_count);
                run_count = 0;
                prev_sig = None;
            }
            output.push((*line).to_owned());
            continue;
        }

        // (b) Level scoring.
        let level = classify_level(line);
        match level {
            LogLevel::Debug | LogLevel::Trace => {
                // Drop DEBUG/TRACE — count toward similar-line dedup if same prefix.
                let sig = line_sig(trimmed);
                match &prev_sig {
                    Some(ps) if *ps == sig => {
                        // Similar line (modulo timestamp/counter) — count silently.
                        run_count += 1;
                    }
                    _ => {
                        if run_count > 0 {
                            flush_run(&mut output, run_count);
                        }
                        run_count = 1;
                        prev_sig = Some(sig);
                    }
                }
                continue;
            }
            _ => {}
        }

        // (c) Deduplication of kept lines (ERROR/WARN/INFO/Unknown).
        let sig = line_sig(trimmed);
        if let Some(ps) = &prev_sig {
            if *ps == sig {
                // Similar content as previous kept line — suppress.
                run_count += 1;
                continue;
            }
        }
        // Different line: flush any accumulated run, then emit this line.
        if run_count > 0 {
            flush_run(&mut output, run_count);
            run_count = 0;
        }
        prev_sig = Some(sig);
        output.push((*line).to_owned());
    }

    // Flush final run.
    if run_count > 0 {
        flush_run(&mut output, run_count);
    }

    output.join("\n")
}

/// Apply log-aware reduction to every `Code` block that heuristically looks like
/// log output. Modifies `doc` in place. Called by `llm` and `compact` profiles.
fn reduce_logs(doc: &mut DocumentIr) {
    for block in &mut doc.blocks {
        if let Block::Code { lang, text } = block {
            if is_log_block(lang.as_deref(), text) {
                let reduced = reduce_log_text(text);
                if reduced != *text {
                    *text = reduced;
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Reducers
// ---------------------------------------------------------------------------

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
/// Also applies log-aware reduction to Code blocks that look like log output.
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
        reduce_logs(doc);

        // Quotes — paragraphs (drop the citation framing for the model).
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

/// `compact` -- aggressive token-economy: whitespace cleanup, drop quotes entirely,
/// deduplicate identical paragraphs, merge consecutive short paragraphs
/// (combined length < 200 chars). Also applies log-aware reduction to Code blocks
/// that look like log output. Targets >=40% block-count reduction vs `human`
/// on typical prose.
pub struct CompactReducer;

impl Reducer for CompactReducer {
    fn name(&self) -> &'static str {
        "compact"
    }
    fn profile(&self) -> Profile {
        Profile::Compact
    }
    fn reduce(&self, doc: &mut DocumentIr) -> Result<()> {
        // 1. Collapse whitespace runs in prose blocks.
        collapse_ws(doc);

        // 2. Drop Block::Quote entirely (aggressive -- the citation framing and
        //    the quoted prose both go; compact cares only about token savings).
        doc.blocks.retain(|b| !matches!(b, Block::Quote { .. }));

        // 3. Drop empty blocks.
        drop_empty(doc);

        // 4. Apply log-aware reduction to matching Code blocks.
        reduce_logs(doc);

        // 5. Deduplicate paragraphs with identical text (keep first occurrence).
        // collapse_ws already trimmed all prose blocks, so no extra trim needed.
        let mut seen: std::collections::HashSet<String> =
            std::collections::HashSet::with_capacity(doc.blocks.len());
        doc.blocks.retain(|b| match b {
            Block::Paragraph { text } => seen.insert(text.clone()),
            _ => true,
        });

        // 6. Merge consecutive paragraphs whose combined length is < 200 chars.
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

    fn code_block(lang: Option<&str>, text: &str) -> Block {
        Block::Code {
            lang: lang.map(str::to_owned),
            text: text.to_owned(),
        }
    }

    // --- Existing tests (unchanged) ------------------------------------------

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

    // --- Log reducer tests ---------------------------------------------------

    #[test]
    fn human_does_not_reduce_logs() {
        // HumanReducer must leave log Code blocks untouched.
        let log_text = "2024-01-15T12:00:00Z INFO service started\n\
                        2024-01-15T12:00:01Z DEBUG polling interval=100ms\n\
                        2024-01-15T12:00:02Z DEBUG cache miss key=foo\n\
                        2024-01-15T12:00:03Z ERROR connection refused";
        let mut d = doc(vec![code_block(None, log_text)]);
        HumanReducer.reduce(&mut d).unwrap();
        match &d.blocks[0] {
            Block::Code { text, .. } => assert_eq!(text, log_text),
            _ => panic!("expected Code block"),
        }
    }

    #[test]
    fn log_detection_requires_enough_log_lines() {
        // A code block with a named programming language must not be treated as a log.
        assert!(
            !is_log_block(Some("rust"), "fn main() { println!(\"ERROR: oops\"); }"),
            "rust code should not be a log block even with ERROR keyword"
        );
        assert!(
            !is_log_block(Some("python"), "print('DEBUG: value')"),
            "python code should not be a log block"
        );
        // A plain block with no log lines should not be a log.
        assert!(
            !is_log_block(None, "hello world\nfoo bar"),
            "generic prose should not be a log block"
        );
    }

    #[test]
    fn log_detection_accepts_log_lang_hint() {
        let text = "INFO  server started\nINFO  listening on :8080\n\
                    DEBUG poll\nDEBUG tick\nERROR oops";
        assert!(
            is_log_block(Some("log"), text),
            "block with lang=log should be detected"
        );
        assert!(
            is_log_block(Some("console"), text),
            "block with lang=console should be detected"
        );
    }

    #[test]
    fn debug_trace_lines_are_dropped() {
        let log_text = "INFO  starting up\n\
                        DEBUG connecting to db\n\
                        TRACE sql=SELECT 1\n\
                        INFO  ready";
        let reduced = reduce_log_text(log_text);
        assert!(
            !reduced.contains("DEBUG"),
            "DEBUG lines must be removed: {reduced:?}"
        );
        assert!(
            !reduced.contains("TRACE"),
            "TRACE lines must be removed: {reduced:?}"
        );
        assert!(
            reduced.contains("INFO  starting up"),
            "INFO lines must be kept: {reduced:?}"
        );
        assert!(
            reduced.contains("INFO  ready"),
            "INFO lines must be kept: {reduced:?}"
        );
    }

    #[test]
    fn error_and_warn_lines_are_kept() {
        let log_text = "ERROR disk full\nWARN  approaching limit\nDEBUG irrelevant";
        let reduced = reduce_log_text(log_text);
        assert!(
            reduced.contains("ERROR disk full"),
            "ERROR must be kept: {reduced:?}"
        );
        assert!(
            reduced.contains("WARN  approaching limit"),
            "WARN must be kept: {reduced:?}"
        );
        assert!(
            !reduced.contains("DEBUG"),
            "DEBUG must be dropped: {reduced:?}"
        );
    }

    #[test]
    fn consecutive_similar_lines_are_deduped() {
        // Lines with the same structure but different timestamps/counters collapse.
        let log_text = "2024-01-15 12:00:01 DEBUG heartbeat ok\n\
                        2024-01-15 12:00:02 DEBUG heartbeat ok\n\
                        2024-01-15 12:00:03 DEBUG heartbeat ok\n\
                        2024-01-15 12:00:04 DEBUG heartbeat ok\n\
                        2024-01-15 12:00:05 DEBUG heartbeat ok";
        let reduced = reduce_log_text(log_text);
        assert!(
            !reduced.contains("heartbeat ok"),
            "all debug heartbeats should be dropped/collapsed: {reduced:?}"
        );
        // The sentinel should appear since they are similar.
        assert!(
            reduced.contains("similar lines omitted"),
            "dedup sentinel expected: {reduced:?}"
        );
    }

    #[test]
    fn stack_trace_lines_are_always_preserved() {
        // Stack trace lines survive even if they would be classified as DEBUG.
        let log_text = "ERROR unhandled exception\n\
                        at com.example.App.main(App.java:42)\n\
                        at sun.reflect.NativeMethodAccessorImpl.invoke0\n\
                        caused by: java.lang.NullPointerException\n\
                        DEBUG after trace";
        let reduced = reduce_log_text(log_text);
        assert!(
            reduced.contains("at com.example.App"),
            "Java stack frame must be kept: {reduced:?}"
        );
        assert!(
            reduced.contains("caused by:"),
            "caused by: must be kept: {reduced:?}"
        );
        assert!(
            !reduced.contains("DEBUG after trace"),
            "trailing DEBUG must be dropped: {reduced:?}"
        );
    }

    /// Snapshot test: pytest-style output.
    #[test]
    fn snapshot_pytest_output() {
        let input = "\
============================= test session starts ==============================
platform linux -- Python 3.11.0, pytest-7.4.0
collected 5 items

tests/test_api.py::test_login PASSED
tests/test_api.py::test_logout PASSED
tests/test_api.py::test_login_bad_creds FAILED
tests/test_api.py::test_health PASSED
tests/test_api.py::test_rate_limit FAILED

=================================== FAILURES ===================================
_________________ test_login_bad_creds _________________

    def test_login_bad_creds():
>       resp = client.post('/login', json={'user':'x','pass':'wrong'})
E       AssertionError: expected 401, got 200

tests/test_api.py:42: AssertionError
=========================== short test summary info ============================
FAILED tests/test_api.py::test_login_bad_creds - AssertionError
FAILED tests/test_api.py::test_rate_limit - AssertionError
2 failed, 3 passed in 0.42s";

        let reduced = reduce_log_text(input);

        // PASSED lines should be kept (INFO-equivalent).
        assert!(
            reduced.contains("PASSED"),
            "PASSED lines must be kept: {reduced:?}"
        );
        // FAILED lines must be kept (ERROR-equivalent).
        assert!(
            reduced.contains("FAILED"),
            "FAILED lines must be kept: {reduced:?}"
        );
        // The AssertionError detail must survive.
        assert!(
            reduced.contains("AssertionError"),
            "error detail must be kept: {reduced:?}"
        );
    }

    /// Snapshot test: cargo test output (500-line-scale, mixed ERROR/DEBUG).
    #[test]
    fn snapshot_cargo_test_output() {
        // Build a representative cargo test output with heavy DEBUG spam (~500 lines).
        let mut lines: Vec<String> = Vec::new();
        lines.push("   Compiling sempack-reducers v0.1.0".into());
        lines.push("    Finished test [unoptimized + debuginfo] target(s)".into());
        lines.push("     Running unittests src/lib.rs".into());
        lines.push("".into());
        lines.push("running 20 tests".into());

        // 18 tests pass (INFO equivalent).
        for i in 0..18 {
            lines.push(format!("test tests::case_{i} ... ok"));
        }
        // Heavy DEBUG noise (simulates log capture from test) — 234 scheduler ticks
        // plus 234 cache-miss lines, totalling 468 debug lines of noise.
        for tick in 0..234 {
            lines.push(format!("[DEBUG] scheduler tick={tick} queue=0"));
        }
        for _ in 0..234 {
            lines.push("[DEBUG] cache miss key=abc123".into());
        }
        // 2 failures.
        lines.push("test tests::case_error ... FAILED".into());
        lines.push("test tests::case_panic ... FAILED".into());
        lines.push("".into());
        lines.push("failures:".into());
        lines.push("".into());
        lines.push("---- tests::case_error stdout ----".into());
        lines.push(
            "thread 'tests::case_error' panicked at 'assertion failed', src/lib.rs:99:5".into(),
        );
        lines.push("note: run with RUST_BACKTRACE=1".into());
        lines.push("".into());
        lines.push("test result: FAILED. 18 passed; 2 failed".into());
        let input = lines.join("\n");
        let input_lines = input.lines().count();
        assert!(
            input_lines >= 500,
            "fixture must be at least 500 lines, got {input_lines}"
        );

        // Wrap in a Code block and run through llm reducer.
        let mut d = doc(vec![code_block(None, &input)]);
        LlmReducer.reduce(&mut d).unwrap();

        let output_text = match &d.blocks[0] {
            Block::Code { text, .. } => text.clone(),
            _ => panic!("expected Code block"),
        };
        let output_lines = output_text.lines().count();

        // Visibly trimmed: output must be meaningfully shorter.
        assert!(
            output_lines < input_lines / 2,
            "log-reduced cargo output ({output_lines} lines) should be <50% of input ({input_lines} lines)"
        );

        // ERROR/FAIL content must survive.
        assert!(
            output_text.contains("FAILED"),
            "FAILED must survive: {output_text:?}"
        );
        assert!(
            output_text.contains("panicked at"),
            "panic line (stack trace) must survive: {output_text:?}"
        );

        // DEBUG noise must be gone (replaced by sentinel).
        assert!(
            !output_text.contains("[DEBUG] scheduler tick=0"),
            "debug tick spam must be reduced: {output_text:?}"
        );
        assert!(
            output_text.contains("similar lines omitted"),
            "dedup sentinel must appear for repeated debug lines: {output_text:?}"
        );
    }

    /// Snapshot test: generic syslog format.
    #[test]
    fn snapshot_syslog_format() {
        let input = "\
Jan 15 12:00:00 host sshd[1234]: INFO Accepted publickey for alice
Jan 15 12:00:01 host sshd[1234]: DEBUG kex: client->server cipher: chacha20-poly1305
Jan 15 12:00:01 host sshd[1234]: DEBUG kex: server->client cipher: chacha20-poly1305
Jan 15 12:00:01 host sshd[1234]: DEBUG kex: client->server MAC: <implicit>
Jan 15 12:00:01 host sshd[1234]: DEBUG kex: server->client MAC: <implicit>
Jan 15 12:00:02 host sshd[1234]: INFO session opened for user alice
Jan 15 12:00:10 host sshd[1234]: WARN  failed to resolve hostname 10.0.0.1
Jan 15 12:00:11 host sshd[1234]: DEBUG kex: client->server cipher: chacha20-poly1305
Jan 15 12:00:11 host sshd[1234]: DEBUG kex: server->client cipher: chacha20-poly1305
Jan 15 12:00:20 host sshd[1234]: ERROR Connection reset by peer";

        let reduced = reduce_log_text(input);

        // INFO and above must be kept.
        assert!(
            reduced.contains("INFO Accepted publickey"),
            "INFO must be kept: {reduced:?}"
        );
        assert!(
            reduced.contains("WARN  failed to resolve"),
            "WARN must be kept: {reduced:?}"
        );
        assert!(
            reduced.contains("ERROR Connection reset"),
            "ERROR must be kept: {reduced:?}"
        );

        // DEBUG lines must be collapsed.
        let debug_count = reduced.lines().filter(|l| l.contains("DEBUG")).count();
        assert_eq!(
            debug_count, 0,
            "no raw DEBUG lines should remain in syslog output: {reduced:?}"
        );
        assert!(
            reduced.contains("similar line omitted"),
            "dedup sentinel expected for repeated DEBUG kex lines: {reduced:?}"
        );
    }

    #[test]
    fn compact_profile_applies_log_reduction() {
        // The compact profile must also apply log reduction.
        let log_text = "INFO  service start\n\
                        DEBUG poll tick=1\n\
                        DEBUG poll tick=2\n\
                        DEBUG poll tick=3\n\
                        ERROR service crashed";
        let mut d = doc(vec![code_block(None, log_text)]);
        CompactReducer.reduce(&mut d).unwrap();
        let text = match &d.blocks[0] {
            Block::Code { text, .. } => text.clone(),
            _ => panic!("expected Code block"),
        };
        assert!(
            !text.contains("DEBUG poll tick=1"),
            "compact must drop debug lines: {text:?}"
        );
        assert!(
            text.contains("ERROR service crashed"),
            "compact must keep error lines: {text:?}"
        );
    }

    #[test]
    fn python_source_code_is_not_log_reduced() {
        // A Python source file containing log-like strings must not be mangled.
        let py_code = "import logging\n\
                       logger.debug('DEBUG: processing item')\n\
                       logger.info('INFO: done')\n\
                       logger.error('ERROR: failed')";
        let mut d = doc(vec![code_block(Some("python"), py_code)]);
        LlmReducer.reduce(&mut d).unwrap();
        match &d.blocks[0] {
            Block::Code { text, .. } => assert_eq!(
                text, py_code,
                "python source must not be log-reduced"
            ),
            _ => panic!("expected Code block"),
        }
    }
}
