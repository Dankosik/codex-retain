# Rust CLI Template

A ready-to-use starting point for small, fast Rust command-line utilities.
The project already connects argument parsing, configuration, errors, streaming
I/O, tests, native binary packaging, CI, and instructions for coding agents.

[Use this template](https://github.com/Dankosik/rust-cli-template/generate) ·
[First command](docs/first-command.md) · [Architecture](docs/architecture.md) ·
[Agent workflow](docs/agent-workflow.md)

Start with a useful synchronous command, then spend the first product change on
what your users need. The included `stats` example scans files or stdin using a
fixed 64 KiB buffer. It does not need a server, database, asynchronous runtime,
network access, or user configuration to run.

## Quickstart

Install [Rust through rustup](https://rust-lang.org/tools/install/), with your
platform's normal native linker. The repository selects Rust 1.98.1 and the
formatter/Clippy components automatically. Python 3.9+ is used for initialization
and repository maintenance; ordinary application development uses Cargo.

Create your repository using GitHub's **Use this template** button, or:

```sh
gh repo create my-tool --template Dankosik/rust-cli-template --public --clone
cd my-tool
python3 scripts/init.py --name my-tool --repository https://github.com/your-name/my-tool
cargo test --locked
cargo run --locked -- --help
```

On Windows, use `python` where your installation does not provide `python3`.
The optional Makefile shortcuts require Make; every underlying command is
available directly.

Try the included command before or after initialization:

```sh
cargo run --locked -- stats README.md
cargo run --locked -- --format json stats README.md
cargo run --locked -- --config examples/config.toml stats README.md
cargo run --locked -- completions bash
cargo build --locked --release
```

With `alpha\nbeta` on stdin, JSON output is exactly:

```json
{"bytes":10,"lines":1}
```

The initializer changes package/crate identity, executable references,
environment prefix, repository links, and optional description. It also writes
a consumer README from `.template/README.md`. Use `--dry-run`
to inspect the patch. It validates before writing, preserves dependency versions
and checksums, and leaves Git state for you to review and commit. Repeating the
same identity is a no-op; a later different rename is an explicit project change.
Vendored skills and upstream attribution remain unchanged.

## What is ready

| Area | Included |
| --- | --- |
| CLI contract | clap arguments/subcommands, typed values, help/version, generated shell completions |
| Configuration | Explicit bounded TOML file, environment overrides, CLI precedence, unknown-field rejection |
| Output and errors | Stable text/JSON, distinct stdout/stderr, intentional exit status, fallible final output |
| Streaming core | Byte-safe scanning with memchr, fixed buffer, short-read/Interrupted handling, checked count growth |
| Tests | Focused core tests plus real binaries, pipes, config, native paths, output failure and subprocess cleanup |
| Rust tooling | Pinned toolchain, lockfile, formatter, Clippy, profiling profile, editor recommendations |
| CI | Linux/macOS/Windows tests, tested MSRV 1.85.1, dependency advisories/licenses, renamed-consumer validation |
| Delivery | Native archives for Linux, both macOS architectures, and Windows; extracted-binary smoke tests and checksums |
| Agents | Shared AGENTS.md, Claude/Copilot pointers, 16 vendored Rust skills with immutable provenance |
| Adoption | Safe identity initializer, integrity checks, explicit pinned skill updates, first-command guide |

Direct dependencies have specific jobs: `clap` and `clap_complete` own command
parsing and completion generation; `serde`, `serde_json`, and `toml` handle typed
formats; `thiserror` preserves error meaning; `memchr` supplies optimized byte
search. `tempfile` is a development-only test dependency. [Cargo.toml](Cargo.toml)
and [Cargo.lock](Cargo.lock) are the dependency authorities.

The release profile uses ordinary optimized Rust with thin LTO and keeps unwind
semantics. It makes no CPU-native assumptions. Startup, throughput, resident
memory, and binary size still depend on the real workload; see
[performance measurement](docs/performance.md).

## Command contract

`stats [INPUT]` reads stdin when INPUT is omitted or `-`. It counts all raw bytes
and LF terminators, matching the newline convention of `wc -l`. An unterminated
tail contributes bytes but no extra line. Input does not need to be UTF-8.

Global options can appear before or after the subcommand:

- `--format text|json` overrides `RUST_CLI_TEMPLATE_FORMAT`, then the explicit
  config file's `format`, then the default `text`.
- `--config PATH` overrides `RUST_CLI_TEMPLATE_CONFIG`. No configuration file is
  discovered implicitly from the working directory or home directory.
- An explicitly selected config is validated even when a flag overrides its
  value. Invalid configuration fails before opening the command's input.
- Help, version, and `completions SHELL` do not load config or consume stdin.

After initialization, `my-tool` uses the prefix `MY_TOOL` instead. Available
completion shells are listed by `completions --help`.

Successful results go to stdout; operational diagnostics go to stderr. Exit 0
means success, including a downstream stdout pipe deliberately closed by its
reader; exit 1 means an operation failed; exit 2 means invalid command usage.
Read errors and unrelated output failures remain failures. Ctrl+C uses the
operating system's default behavior. A command that later owns persistent writes
or child processes must define the cleanup it needs.

## Develop and verify

| Command | Purpose |
| --- | --- |
| `cargo test --locked` | Test Rust behavior, including the real CLI |
| `cargo fmt --all -- --check` | Check formatting |
| `cargo clippy --locked --all-targets -- -D warnings` | Compiler/lint feedback |
| `cargo build --locked --release` | Build the optimized executable |
| `make check` | Formatting, Clippy, all-target tests and doctests |
| `make verify` | `check` plus pinned skills, local links, and maintenance tests |
| `make template-smoke` | Initialize and test a disposable consumer |
| `make audit` | Advisories, licenses, and sources using cargo-deny |

For local dependency policy checks, install the tool version used by CI:

```sh
cargo install cargo-deny --version 0.20.2 --locked
cargo deny check
```

The CI jobs use pinned action revisions and expose one aggregate `required`
check for branch protection. Ordinary changes need matching focused local
proof; they do not require repeated full validation or a new specification file.
See [CONTRIBUTING.md](CONTRIBUTING.md) for contribution and validation details.

## Start your real command

Ask your coding agent to implement the requested behavior in this repository.
[AGENTS.md](AGENTS.md) routes it to the relevant Rust skills and actual validation
commands. Clear tasks proceed directly to code. Larger tasks can record concise
decisions and split independent work across agents.

[docs/first-command.md](docs/first-command.md) shows where to add or replace a
subcommand and its tests. Replace the demonstration command, its tests, the
release smoke case in `scripts/release.py`, and performance examples together.
Initialized consumers' CI does not require keeping `stats` as a permanent feature.

The 16 skills are vendored from
[Dankosik/rust-cli-skills](https://github.com/Dankosik/rust-cli-skills). They are
available immediately, without an installer or live download. To update from a
reviewed local checkout, supply an exact upstream commit:

```sh
python3 scripts/sync_skills.py --source ../rust-cli-skills --revision FULL_COMMIT_SHA --check
python3 scripts/sync_skills.py --source ../rust-cli-skills --revision FULL_COMMIT_SHA --apply
```

`--check` returns 1 when an update is available; it writes nothing. Application
files and locally changed skills are protected from replacement. Review the
resulting diff and run the relevant checks before committing an update.

## Release

[docs/releasing.md](docs/releasing.md) describes the native target matrix,
compatibility boundaries, archive verification, and tag-driven GitHub Releases.
Manual release-workflow runs verify packages without publishing. A matching `v`
tag publishes only after CI and every native archive check succeeds. Source
publication to crates.io remains disabled until you intentionally configure it.

## Origin and license

This adapts the ready-to-run, contract, initialization, validation, and agent
workflow ideas of
[Dankosik/go-service-template-rest](https://github.com/Dankosik/go-service-template-rest)
to Rust CLI development. [Template design](docs/template-design.md) explains the
adaptation. The Rust skills provide the technical foundation.

[MIT license](LICENSE) · [Third-party notices](THIRD_PARTY_NOTICES.md) ·
[Security policy](SECURITY.md)
