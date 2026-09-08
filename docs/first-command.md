# Add your first command

Start from an initialized checkout using the README's quickstart. Check the
current interface and run the streaming example:

```sh
cargo run --locked -- --help
printf 'hello world\nsecond line\n' | cargo run --locked -- stats
printf 'a\nb' | cargo run --locked -- stats --format json
```

The final command prints `{"bytes":3,"lines":1}` followed by a newline. Lines
count LF terminators, matching `wc -l`; the final `b` adds a byte, not a line.
Input need not be UTF-8. Use `stats PATH` for a file and `stats -` for explicit
stdin. See `examples/config.toml` for the opt-in configuration file.

The example provides a path through real parsing, configuration, input,
computation, output, and process exit. Replace it once your utility has its own
purpose; you do not need to retain a statistics command in the final product.

## Define the command contract

Write down the input sources, argument grammar, output format, ordering,
expected failures, and exit status. Decide which streams carry data and
diagnostics, whether an operation writes files or starts processes, and how
resource use grows with input. Keep a short task note only if these decisions
need to be shared or revisited.

## Implement the useful operation

Put computation behind a function with explicit inputs and results. Use `Read`
or `Write` when they express the actual operation and help test a stream
without global process state. Keep parser types and process exit decisions at
the command boundary. Prefer concrete types and existing crates to a new
framework or a trait for each structure.

For input that can grow, choose an algorithm and buffers whose memory use you
can explain. Avoid collecting the entire input merely to iterate it afterward.
If the new operation needs global ordering or indexing, document and enforce
its resource policy rather than claiming that a buffered reader bounds all memory.

## Connect the CLI

Add the command and arguments to `src/cli.rs`, wire the operation into dispatch
in `src/lib.rs`, and reuse configuration and output conventions where they apply.
Preserve native path values and end-of-options behavior. Ensure `--help` and
`--version` do not read input, load unrelated configuration, or perform effects.
Generated completions should continue to use the parser's command definition.

The existing format precedence is `--format`, `RUST_CLI_TEMPLATE_FORMAT`, the
selected TOML file, then `text`. A config file is selected only by `--config`
or `RUST_CLI_TEMPLATE_CONFIG`; no home-directory files are searched implicitly.
The initializer renames this environment prefix with your executable identity.

## Prove the behavior

Test the operation with inputs that distinguish it from plausible mistakes.
For streaming logic, include the relevant chunk boundary, empty input, and I/O
failure. Test the executable for the affected arguments, stdout/stderr, status,
and side effects. For a file-writing command, inspect the resulting file through
an independent read and verify the failure path preserves its promised state.

Run focused tests while editing and the applicable standard checks before
finishing. Update the README and help examples with the new behavior. If you
claim lower latency or memory use, measure a release build under a documented,
comparable workload; passing functional tests alone does not establish that gain.
