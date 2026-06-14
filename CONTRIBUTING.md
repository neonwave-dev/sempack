# Contributing to sempack

Thanks for your interest in contributing!

## Workflow

1. Fork (external) or branch (collaborator). Branch names follow
   `<type>/<short-description>` (e.g. `feat/add-csv-extractor`, `fix/nfc-edge-case`).
2. Make your change with focused, [Conventional Commits](https://www.conventionalcommits.org/).
3. Run the checks locally before opening a PR:
   ```sh
   cargo fmt --all -- --check
   cargo clippy --all-targets --all-features -- -D warnings
   cargo test --all
   cargo build --all
   ```
4. Open your PR **as a draft** while iterating. Mark it **Ready for review** to trigger
   CI — the workflow is gated to skip draft PRs.
5. CodeRabbit and Copilot review automatically; address or reply to their threads.
6. A maintainer merges once CI is green and review is resolved.

## Architecture

sempack is a Cargo workspace. The three pipeline stages — **extractors**, **reducers**,
**emitters** — are plugin registries wired together in `sempack-cli`. New file-format
support is usually a new `Extractor` in `sempack-extractors` (feature-gated) that emits the
universal `DocumentIr` from `sempack-ir`; nothing downstream needs to change. See the
`README.md` workspace table.

## Code style

- `rustfmt` and `clippy` are the source of truth — keep both clean (CI denies warnings).
- Keep PRs small and single-purpose where possible.
- Extractors emit IR only; reducers mutate IR only; emitters serialize only. Don't blur
  the stage boundaries.

## Reporting bugs / requesting features

Use the issue templates under **Issues → New issue**.
