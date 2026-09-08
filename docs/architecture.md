# Architecture

The template is one synchronous command-line program. Its example command
counts a file or stdin incrementally. Dependencies support a concrete CLI
capability; there is no service container, network client, database, or background
runtime to initialize before a command can run.

## Responsibility boundaries

| File | Owns |
| --- | --- |
| `src/main.rs` | Thin executable entry point |
| `src/lib.rs` | Argument parsing, dispatch, process I/O, and final exit status |
| `src/cli.rs` | Subcommands, flags, help, and the source for completions |
| `src/config.rs` | Explicit file input, format precedence, and validation |
| `src/stats.rs` | Streaming computation and its result type |
| `src/output.rs` | Human or machine representation and output-write failures |
| `src/error.rs` | Error types, diagnostic rendering, and closed-stdout classification |
| `tests/cli.rs` | Actual executable arguments, streams, status, and effects |

The parser is the authority for accepted command syntax. Cargo owns package
identity, dependencies, and build settings. Generated output has a declared
source and regeneration path. Add modules when their responsibilities differ;
do not reproduce backend layers merely to arrange a small command.

## Input and resource ownership

Use native path types for filesystem operations. File and stdin handles belong
to the command invocation; the operation consumes a reader and retains only
the state required by its algorithm. Keep input-dependent memory growth visible
when extending that algorithm. A bounded read buffer does not bound a collection
of every record.

The statistics operation uses a 64 KiB read buffer and two counters. It counts
bytes exactly and counts LF bytes as lines, without decoding UTF-8 or retaining
whole lines. An unterminated final fragment adds bytes only. A read failure
returns an error before the summary is written. This bounds the operation's
input buffer, not the whole process's resident memory.

Configuration files are explicit: `--config PATH` overrides
`RUST_CLI_TEMPLATE_CONFIG`. Output format is selected by `--format`, then
`RUST_CLI_TEMPLATE_FORMAT`, then the selected TOML file, then `text`.
Unknown config fields and files larger than 64 KiB are rejected. An explicitly
selected invalid file still fails even when a flag overrides its format.

Keep startup paths cheap. Help, version, and completion generation should be
available without reading the data stream or validating an unrelated config
file. Resolve configuration only for operations that need it.

## Output and failure

Result data goes to stdout; diagnostics go to stderr. The JSON summary is
`{"bytes":N,"lines":N}` followed by one newline. Keep JSON free of human
decoration and treat its fields and framing as a public contract. Successful
commands return status 0; parser misuse returns 2; runtime failures return 1.
A closed stdout pipe is a quiet success. Other input or output failures remain
errors, including a broken pipe while reading input. Process exit and
broken-pipe policy have one owner so individual commands cannot silently disagree.

The template supplies a working baseline. Add file mutation, subprocesses,
network access, or concurrency when a command requires them, and give each
effect a clear failure and cleanup policy. Do not infer permission to publish
or change external state from the presence of a library or credential.

## Extending the repository

Keep a useful operation callable independently of process setup. Reuse the
existing parser and output path when adding a subcommand. Add tests at the
boundary that can observe the promised behavior. See [first command](first-command.md)
for the development path and [agent workflow](agent-workflow.md) for coordination.

Repository instructions are shared through `AGENTS.md`. The vendored Rust CLI
skills provide task-specific methods. Provider instruction files point to that
shared source; they do not define competing project policies.
