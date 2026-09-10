# Template provenance

Codex Retain was initialized using the template's own `scripts/init.py` identity
workflow, from [Dankosik/rust-cli-template at
89e91d903455022f6ad6a10cc33027a36adcd6b1](https://github.com/Dankosik/rust-cli-template/tree/89e91d903455022f6ad6a10cc33027a36adcd6b1).

Retained and adapted: Clap parsing/completions, Rust 1.98.1/edition 2024, typed
JSON handling, native packaging/identity/checksum checks, CI gate aggregation,
MIT license, library guidance and the immutable Rust skill snapshot.

Replaced: the demonstration statistics command and TOML formatting configuration,
its tests, template-consumer setup checks, product docs, and unused runtime
dependencies. Windows releases were removed; local destructive validation is
macOS-first. Linux CI checks portable core logic and non-mutating CLI paths.

The initial publication at [Dankosik/codex-retain](https://github.com/Dankosik/codex-retain)
was source-only. [Release 0.1.0](https://github.com/Dankosik/codex-retain/releases/tag/0.1.0)
adds macOS binaries, an installer and a Homebrew tap; Cargo registry publication
remains disabled. Original Git history is retained for provenance; the local upstream remote is named `template` to avoid
an accidental push to the template repository.
