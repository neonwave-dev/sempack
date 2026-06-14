# SemPack Sample

SemPack turns bulky files into compact, structured text that is cheap for both
humans and language models to read.

This second paragraph exists so the `llm` profile has adjacent paragraphs to
merge, demonstrating token savings.

## Why it helps

- Cuts file size before an agent ever reads it
- Keeps document structure (headings, lists, tables)
- Emits to several formats from one intermediate representation

> Compression should be lossless for structure, lossy only for noise.

```rust
fn main() {
    println!("hello from a fenced code block");
}
```
