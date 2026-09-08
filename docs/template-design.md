# Why this template is shaped this way

The goal is to make the first product change useful command behavior. The
repository supplies a runnable program, dependencies for the features it
demonstrates, validation, distribution tooling, and focused agent instructions.
It does not assume that every CLI needs a daemon, async runtime, network client,
plugin framework, or database.

## Sources

The repository adapts the readiness and ownership ideas in
[go-service-template-rest at ef6a3bc](https://github.com/Dankosik/go-service-template-rest/tree/ef6a3bc9983431de065a421e6a3fced36ef886ba).
In particular, its README, initializer, agent instructions, first-feature guide,
and CI/release workflows informed this design.

The reusable engineering methods come from
[rust-cli-skills at 3ec323b](https://github.com/Dankosik/rust-cli-skills/tree/3ec323b33654173caff2c760b99d70c8923a6576).
All 16 skills are vendored under `.agents/skills/`; the recorded upstream
revision makes their origin explicit and their updates reviewable. Both source
repositories use the MIT license. Keep applicable license notices when copying
or redistributing their content.

## What carries over

- A minimal program that runs immediately after initialization.
- Explicit ownership of public contracts and generated output.
- A repeatable first-feature path, with code and tests already connected.
- People and coding agents using the same commands and boundaries.
- Proportionate local verification, with CI and publication making separate claims.
- Initialization and release checks that exercise the resulting product.

## What changes for a CLI

The command parser replaces OpenAPI as the syntax owner. Input streams,
filesystem paths, stdout/stderr, and exit status replace HTTP lifecycle and
transport contracts. Executable tests observe the interface used by shell
scripts. Release artifacts are command-line binaries rather than deployed
service images.

The source's persistence, jobs, messaging, authentication, telemetry, and
deployment profiles are intentionally absent. Its large workflow and adapter
system becomes a short direct-work default with optional shared notes and
disjoint delegation for larger changes. A future requirement can add a
capability without making every generated CLI carry it from the start.

## Performance stance

Use streaming algorithms, explicit ownership, and limited retained state to
make resource cost understandable. Treat zero-copy, concurrency, allocator
changes, link-time optimization, and profile-guided optimization as choices
with workload-dependent effects. Measure the relevant latency, throughput,
memory, or artifact size before claiming an improvement. A release profile
and a Rust implementation alone cannot guarantee a fast utility.
