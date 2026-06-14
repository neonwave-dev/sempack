# SemPack

Open-source, Rust-first **semantic compression toolkit**: extract text from common
file types and emit compact, structured, human- and LLM-friendly representations —
cutting file size and AI tokens.

> Status: **P1 skeleton** (single-file extraction → reduce → emit). See the planning
> vault for the full P1→P8 roadmap. Repo: `neonwave-dev/sempack` (core, 100% OSS).

## Workspace

| Crate | Role |
|---|---|
| `sempack-ir` | The universal IR — `DocumentIr`, `Block`/`BlockKind`, source/metadata/warnings |
| `sempack-core` | Detect → **filetype router** → 3 plugin registries → pipeline (NFC normalize) |
| `sempack-extractors` | Built-in extractors (P1: text, markdown) |
| `sempack-reducers` | Compression profiles (P1: `human`, `llm`) |
| `sempack-emitters` | Output formats (P1: markdown, jsonl, ndjson, text, html) |
| `sempack-tokenizers` | Approximate token counting for before/after metrics |
| `sempack-cli` | `clap` CLI front end |

## Try it

```sh
cargo run -p sempack-cli -- examples/sample.md --profile llm --format markdown --stats
cargo run -p sempack-cli -- examples/sample.md --format jsonl
```

## Architecture (P1)

```
input ─▶ detect ─▶ ROUTER ─┬─ text  ─▶ extract ─▶ normalize(NFC) ─▶ reduce(profile) ─▶ emit(format) ─▶ output
                           └─ image ─▶ (P6 — not built yet)
```

All three stages (extract / reduce / emit) are plugin registries, wired in `sempack-cli`.

## Contributing

See [CONTRIBUTING.md](./CONTRIBUTING.md). By participating you agree to the
[Code of Conduct](./CODE_OF_CONDUCT.md).

Pull requests run a draft-gated CI workflow (`cargo fmt`/`clippy`/`test`/`build`) — open
as a draft while iterating, then mark **Ready for review** to trigger CI. CodeRabbit and
Copilot review PRs automatically.

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](./LICENSE-APACHE))
- MIT License ([LICENSE-MIT](./LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally
submitted for inclusion in this work, as defined in the Apache-2.0 license, shall be
dual-licensed as above, without any additional terms or conditions.
