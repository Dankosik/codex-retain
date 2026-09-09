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

The initialized repository URL is an intended project identity, not evidence of
a created remote repository or published release. Original Git history remains
available locally for provenance; the upstream remote is named `template` to
avoid an accidental push to the template repository.
