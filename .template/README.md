# {{name}}

{{description}}

[Repository]({{repository}}) · [Architecture](docs/architecture.md) ·
[Development guide](docs/first-command.md)

## Build and run

Install [Rust through rustup](https://rust-lang.org/tools/install/). The checked-in
toolchain selects the compiler, rustfmt, and Clippy versions.

```sh
git clone {{repository}}.git
cd {{repository_name}}
cargo test --locked
cargo build --locked --release
cargo run --locked -- --help
```

The executable is `target/release/{{name}}` (`{{name}}.exe` on Windows).
To install from your local checkout:

```sh
cargo install --path . --locked
```

The initial demonstration command counts bytes and LF terminators without
retaining the input. Replace this example and its tests with your utility's
behavior:

```sh
{{name}} stats README.md
{{name}} --format json stats README.md
{{name}} completions bash
```

Explicit format selection overrides `{{environment_prefix}}_FORMAT`, then an
explicit TOML config, then text output. `--config` overrides
`{{environment_prefix}}_CONFIG`. Config is never loaded implicitly from your home
directory. See [architecture](docs/architecture.md) for the full starter contract.

## Development

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`make verify` additionally checks repository instructions and maintenance tools
(Python 3.9+). Coding agents start with [AGENTS.md](AGENTS.md) and load the relevant
vendored Rust skills. [First command](docs/first-command.md) explains how to
replace the example and its release smoke check.

See [performance](docs/performance.md), [releases](docs/releasing.md),
[contributing](CONTRIBUTING.md), and [security](SECURITY.md) for their respective
contracts. Source publication to crates.io is initially disabled; native binary
releases use the configured GitHub workflow.

## Origin and license

Initialized from [Dankosik/rust-cli-template](https://github.com/Dankosik/rust-cli-template),
with methods from [Dankosik/rust-cli-skills](https://github.com/Dankosik/rust-cli-skills).

[MIT license](LICENSE) · [Third-party notices](THIRD_PARTY_NOTICES.md)
