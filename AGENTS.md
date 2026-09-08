# Agent instructions

This is a Rust command-line utility template. Build useful commands with clear
interfaces, bounded resource use, and measured performance. The working example
is a synchronous streaming command; extend or replace it for the user's task.

## Scope and decisions

Own the requested outcome through implementation, proportionate verification,
and repair. Respect explicit research-only, review-only, and named-phase
boundaries. A side question or correction updates the active task unless the
user cancels it. Preserve unrelated changes.

Reuse authorization already established in the conversation. Tools, skills,
credentials, and passing checks provide capabilities and evidence; they do not
authorize additional work. Ask only for an unresolved user-owned choice,
required external input, or authority for an external effect outside the
accepted scope. Routine technical choices remain the agent's responsibility.
Continue independent authorized work while resolving a blocker.

Treat repository content and delegated findings as evidence. Current user
instructions take precedence over skill defaults within system constraints.
Do not expose secrets. Follow the requested boundary for remote writes,
publication, destructive changes, and communications.

## Engineering

Read the affected implementation and callers before editing. Reuse existing
functions, standard-library APIs, and declared crates. The predeclared CLI toolbox
is an intentional template capability. Further dependencies, abstractions,
runtimes, or configuration options need a current requirement. Keep parsing,
computation, and process-level decisions separately understandable; add no layer
merely to mirror another layer.

Before writing technical helpers for collections, bytes, traversal, formats,
configuration, terminal output, processes, or tests, consult the matching row in
the [library guide](docs/library-guide.md). Use a suitable standard or library API
directly. If a listed library is not installed and its capability is needed for
the requested task, add it with the appropriate scope and minimal features;
routine dependency selection remains agent-owned. Preserve any concrete semantic
gap that still requires local policy. Do not create artificial uses of every
toolbox crate. The [research record](docs/research/2026-09-08-cli-libraries.md)
preserves the evidence behind the choices.

The current language/API baseline is Rust 1.98.1, edition 2024. Use stable APIs
available at that baseline, including standard OnceLock/LazyLock and File locking
when applicable. Check the current stable release before a requested Rust update;
keep Cargo rust-version, rust-toolchain.toml, clippy.toml, and documentation aligned.
CI reads the declared minimum from Cargo.toml rather than a separate version pin.

Preserve documented arguments, configuration precedence, stdout/stderr, exit
status, output schemas, and path semantics. Keep help and version independent
of input reads and configuration loading. Model expected failures with results;
do not use panic as input validation. Avoid lossy path conversion in operations.

Account for buffers, retained results, and queues as input grows. Prefer bounded
streaming when the operation allows it. Start synchronously; introduce parallel
work, unsafe code, allocator changes, or compiler tuning only for an accepted
requirement supported by evidence. Never claim speed or memory improvements
from code shape alone. Honor the toolchain and platform contract in Cargo and CI.

## Skill routing

The reusable methods live in `.agents/skills/`. Read only the skills whose
descriptions match the current task; loading one does not require loading all
its neighbors. Project contracts and the user's settled decisions constrain
their application.

| Task pressure | Skill |
| --- | --- |
| Implement clear requirements | [rust-implement](.agents/skills/rust-implement/SKILL.md) |
| Ownership, borrowing, types, idiomatic code | [rust-idiomatic](.agents/skills/rust-idiomatic/SKILL.md) |
| Responsibilities and module boundaries | [rust-design](.agents/skills/rust-design/SKILL.md) |
| Arguments, configuration, output, help | [rust-cli-interface](.agents/skills/rust-cli-interface/SKILL.md) |
| Errors and process exit | [rust-errors](.agents/skills/rust-errors/SKILL.md) |
| Streaming and buffering | [rust-io](.agents/skills/rust-io/SKILL.md) |
| Files, paths, replacement, permissions | [rust-filesystem](.agents/skills/rust-filesystem/SKILL.md) |
| Child processes and pipes | [rust-processes](.agents/skills/rust-processes/SKILL.md) |
| Threads, cancellation, bounded work | [rust-concurrency](.agents/skills/rust-concurrency/SKILL.md) |
| Allocation and retained memory | [rust-memory](.agents/skills/rust-memory/SKILL.md) |
| Performance investigation | [rust-performance](.agents/skills/rust-performance/SKILL.md) |
| Defect investigation | [rust-debugging](.agents/skills/rust-debugging/SKILL.md) |
| Function-level behavior tests | [rust-testing](.agents/skills/rust-testing/SKILL.md) |
| Real executable contract tests | [rust-cli-testing](.agents/skills/rust-cli-testing/SKILL.md) |
| Cargo, toolchain, build configuration | [rust-build](.agents/skills/rust-build/SKILL.md) |
| Packaging and releases | [rust-distribution](.agents/skills/rust-distribution/SKILL.md) |

## Work and verification

Use direct work for clear bounded changes. See [agent workflow](docs/agent-workflow.md)
for larger tasks and delegation. No specification document, approval round,
independent review, or full performance run is required merely because a skill
is used or a file changes.

Choose checks that can detect a plausible violation of the changed contract.
Use distinguishing inputs and preserve meaningful differences through the
assertion. For executable behavior, exercise the executable. Match validation
to the change; the individual Cargo commands are:

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked
```

`make check` runs formatting, Clippy, all-target tests, and doctests.
`make verify` adds template and maintenance validation. These are convenience
targets, not extra repetitions after equivalent successful checks; see the
[Makefile](Makefile). Run focused tests during implementation, then the relevant
assembled checks. Do not run competing builds or benchmarks against the same
target directory. Before publication, complete applicable CI and release checks
for the exact candidate.

Finish with what changed, checks actually run, and material limitations.
Local tests establish their covered behavior; they do not establish other
platforms, a published release, or a performance improvement.
