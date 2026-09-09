# Library choices for Rust CLI work

Use this guide when a task introduces technical mechanics. Start with the
matching standard-library operation or existing project API, then the relevant
crate below. A listed library can be added for the requested capability; it need
not already appear in Cargo.toml to be a valid choice. Use its API directly and
keep local code for the command's actual policy or a demonstrated semantic gap.

The product baseline is Rust **1.98.1**, edition 2024. This guide preserves the
template's toolbox choices as a reference; Cargo.toml is authoritative for what
this product actually declares. The example command and unused dependencies
were removed during adoption. Keep initialization out of help/version.

## Template toolbox reference

| Need | Crate/API | Use and important boundary |
| --- | --- | --- |
| Arguments, subcommands, help, environment options | `clap` | Keep one parser definition as the command contract |
| Shell completions | `clap_complete` | Generate from that same definition; preserve fallible output handling |
| Typed serialization and configuration syntax | `serde`, `serde_json`, `toml` | Use typed data; keep output framing and config precedence explicit |
| Errors whose kind determines behavior | `thiserror` | Preserve structured causes for exit status and recovery |
| Application error context | `anyhow` | Avoid repetitive wrappers when callers do not need a dedicated variant; do not erase meaningful classification |
| Extra iterator/collection operations | `itertools` | Merge, grouping and adapters beyond std; check retained state and materialization |
| String-like operations on bytes | `bstr` | Work with arbitrary byte data; enable `unicode` only for operations requiring those semantics |
| Fast byte/substring search | `memchr` | Use the existing implementation instead of handwritten scanning optimizations |
| Plain recursive traversal | `walkdir` | Control links, errors, depth and open handles using its builder |
| Source-tree traversal and ignore rules | `ignore` | Use for `.gitignore`/ignore-aware tools, including supported parallel traversal |
| Many glob filters | `globset` | Compile the pattern set once; avoid a custom glob engine |
| Temporary files and directories | `tempfile` | Let an owned guard manage cleanup; understand persistence/replace semantics |
| Progress and spinners | `indicatif` | Respect terminal detection and stderr; choose update frequency for the workload |
| Logging | `log`, `env_logger` | Initialize once when diagnostics need it; preserve stdout as result data |
| Ordinary CLI process tests | `assert_cmd` (dev) | Binary discovery, stdin/env/cwd, timeouts and output assertions; retain specialized pipe and capture-limit tests |

Directly used crates are declared directly even when another toolbox crate also
depends on them. This avoids accidentally relying on a transitive dependency.

`bstr` requests `std` without its Unicode feature; `indicatif` requests
`unicode-width` without WebAssembly timing support; `env_logger` requests
automatic color support without timestamps or message-regex filtering.
Cargo can unify features requested elsewhere in the graph, so inspect the
resolved features when resource cost matters. Add a feature when the task needs
it; do not silently substitute different Unicode, matching or formatting rules.

## Add for the matching capability

These are alternatives or extensions, not another default dependency bundle.
Follow the source link for the selected API, platform support, and current MSRV.

| Capability | Preferred candidates | Decision |
| --- | --- | --- |
| Regex search | [regex](https://docs.rs/regex/latest/regex/) | Compile once outside repeated work; bound user-controlled patterns as appropriate |
| Small regex footprint | [regex-lite](https://docs.rs/regex-lite/latest/regex_lite/) | Explicitly trades search speed and Unicode features for size/compile time; measure against the actual requirement |
| CSV/TSV | [csv](https://docs.rs/csv/latest/csv/) | Replace manual delimiter/quote handling; use ByteRecord when UTF-8 is not guaranteed |
| URLs and query strings | [url](https://docs.rs/url/latest/url/) | Replace string assembly/parsing; destination authorization remains application policy |
| Version comparison | [semver](https://docs.rs/semver/latest/semver/) | Avoid a partial SemVer parser when the full version rules matter |
| Insertion-ordered maps | [indexmap](https://docs.rs/indexmap/latest/indexmap/) | Replace a map plus a parallel order list; distinguish insertion order from sorted order |
| Human-readable durations | [humantime](https://docs.rs/humantime/latest/humantime/) | Parse values such as `2h 30m`; use Duration internally |
| Calendar/timezone work | [jiff](https://docs.rs/jiff/latest/jiff/) | Use for calendar rules; use Instant for elapsed time |
| Legacy text encodings | [encoding_rs](https://docs.rs/encoding_rs/latest/encoding_rs/) | Choose an explicit decoding contract rather than assuming all files are UTF-8 |
| Repeated enum conversions | [strum](https://docs.rs/strum/latest/strum/) | Use where useful beyond Clap ValueEnum or Serde's existing derives |
| Standard-trait boilerplate | [derive_more](https://docs.rs/derive_more/latest/derive_more/) | Enable the needed derive features; preserve deliberate conversion semantics |
| File errors with path context | [fs-err](https://docs.rs/fs-err/latest/fs_err/) | Replace repeated filesystem context plumbing, without losing error classification |
| Atomic file replacement | [atomic-write-file](https://docs.rs/atomic-write-file/latest/atomic_write_file/) | Inspect staging/commit, metadata, platform and durability guarantees before replacing local mechanics |
| Config/cache/data locations | [directories](https://docs.rs/directories/latest/directories/) or [etcetera](https://docs.rs/etcetera/latest/etcetera/) | Choose a documented OS convention; etcetera allows explicit strategy selection |
| Executable lookup | [which](https://docs.rs/which/latest/which/) | Use for PATH/PATHEXT behavior across platforms |
| Layered/nested configuration | [config](https://docs.rs/config/latest/config/) | Add when simple typed TOML and a few overrides are insufficient; select only needed formats/features |
| Configuration provenance | [figment](https://docs.rs/figment/latest/figment/) | Alternative when source metadata and profiles matter; check its current maintenance status |
| Interactive questions | [dialoguer](https://docs.rs/dialoguer/latest/dialoguer/) or [inquire](https://docs.rs/inquire/latest/inquire/) | Choose one; preserve a usable noninteractive path and do not consume piped data as a prompt answer |
| Terminal styling/operations | [console](https://docs.rs/console/latest/console/) | Add when application code uses those APIs directly; do not import it transitively from indicatif |
| Rich source diagnostics | [miette](https://docs.rs/miette/latest/miette/) | Useful for parsers/validators; enable fancy reporting deliberately |
| Structured spans/events | [tracing](https://docs.rs/tracing/latest/tracing/) + [tracing-subscriber](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/) | Choose when contextual structured diagnostics justify replacing or bridging simple logging |
| Process pipelines | [duct](https://docs.rs/duct/latest/duct/) | Compose arguments, pipes and redirects without building shell command strings |
| Process groups/job objects | [process-wrap](https://docs.rs/process-wrap/latest/process_wrap/) | Use for Unix/Windows process-tree ownership; define termination and wait policy |
| Graceful interruption | [ctrlc](https://docs.rs/ctrlc/latest/ctrlc/) / [signal-hook](https://docs.rs/signal-hook/latest/signal_hook/) | Add only when cleanup is needed; an atomic flag does not unblock a blocking stdin read |
| CPU parallelism | [rayon](https://docs.rs/rayon/latest/rayon/) | Reuse its pool/iterators; bound queued and retained work as well as worker count |
| Synchronous HTTP | [ureq](https://docs.rs/ureq/latest/ureq/) | First candidate for simple synchronous API clients; set appropriate time/body limits |
| Broader HTTP/async work | [reqwest](https://docs.rs/reqwest/latest/reqwest/) / [tokio](https://docs.rs/tokio/latest/tokio/) | Select when required features or concurrent waiting justify them |
| Full-screen TUI | [ratatui](https://docs.rs/ratatui/latest/ratatui/) + [crossterm](https://docs.rs/crossterm/latest/crossterm/) | A different interface requirement from ordinary command output |
| CLI snapshots | [snapbox](https://docs.rs/snapbox/latest/snapbox/) / [trycmd](https://docs.rs/trycmd/latest/trycmd/) | Use as dev dependencies for many command/output cases |
| Value/JSON snapshots | [insta](https://docs.rs/insta/latest/insta/) | Review changes as contract changes; normalization must not hide defects |
| Filesystem fixtures | [assert_fs](https://docs.rs/assert_fs/latest/assert_fs/) | Use when tempfile plus direct assertions becomes repetitive |
| Property tests | [proptest](https://docs.rs/proptest/latest/proptest/) | Derive useful invariants and keep minimized failing cases |

## Standard-library choices at this baseline

Use ordinary iterator adapters, strings, slices, collections, Path/OsStr,
Read/BufRead/Write, and process::Command for the cases they already cover.
OnceLock and LazyLock cover common lazy initialization without once_cell.
File::lock/try_lock/unlock are available since Rust 1.89; use them when their
platform contract fits instead of introducing a locking crate by habit.
Additional filesystem APIs in a crate can still justify it.
[File locking](https://doc.rust-lang.org/std/fs/struct.File.html#method.lock),
[LazyLock](https://doc.rust-lang.org/std/sync/struct.LazyLock.html).

Do not replace all native paths with [camino](https://docs.rs/camino/latest/camino/):
its UTF-8 invariant is useful only when the command accepts that restriction.
Likewise, bytes/SmallVec/arenas or an alternate allocator need an actual ownership
or measured allocation problem, not a general instruction to optimize.

## Adding or using a library

Identify the helper or platform mechanism the library replaces. Reuse the
existing crate if it covers the contract; otherwise add the selected library
to dependencies or dev-dependencies with the necessary features. Preserve
typed errors, byte/path fidelity, bounds, stdout/stderr, cleanup and effect policy.
Do not replace a mature mechanism with local code merely to minimize dependency
count, or wrap it merely to give its functions different names.

Use Cargo to update the lockfile. Run checks matching the changed surface and
dependency policy, and measure the actual hot path if claiming a resource gain.
Large package/source documentation size is not a measurement of executable size.

## Research and maintenance

The [research record](research/2026-09-08-cli-libraries.md) and
[78-crate catalog](research/library-catalog.md) preserve the source observations.
They are dated evidence, not timeless version pins. The original research
compared against MSRV 1.85.1; the current 1.98.1 baseline removes those particular
compiler-version conflicts. Future upgrades still need a resolved-graph check.

For discovery, use [crates.io](https://crates.io/), [lib.rs](https://lib.rs/),
[Blessed.rs](https://blessed.rs/crates), and the
[Rust CLI resources](https://rust-cli.github.io/book/resources/index.html), then
verify the selected project's primary documentation and current source location.
For example, directories moved from archived GitHub hosting to Codeberg; a
hosting migration is not evidence that the library was abandoned.
