//! Approximate token counting.
//!
//! P1 ships a cheap, dependency-free heuristic so the CLI can report before/after
//! token deltas. Real tokenizer backends (tiktoken-style BPE, model-specific
//! vocabularies) plug in later behind the same function surface.

/// Rough GPT-style estimate: ~4 chars per token, floored at the word count so very
/// short, word-dense text is never under-counted.
pub fn approx_tokens(text: &str) -> usize {
    let chars = text.chars().count();
    let words = text.split_whitespace().count();
    let by_chars = chars.div_ceil(4);
    by_chars.max(words)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_is_zero() {
        assert_eq!(approx_tokens(""), 0);
    }

    #[test]
    fn scales_with_length() {
        assert!(approx_tokens("the quick brown fox") >= 4);
    }
}
