# Rust CLI library research — 2026-09-08

This is the preserved research behind [the library guide](../library-guide.md).
The original comparison used the template at
`90893f1eb3f911e2394788c6577b6b4ef2aaea06`, with development Rust 1.98.1 and
MSRV 1.85.1. The template now uses 1.98.1 as both its pinned compiler and minimum.
Historical compatibility fields in the data deliberately retain their original
meaning.

## Method and limits

We surveyed **78 crates** using crates.io version metadata, declared Rust
requirements, licenses, feature declarations and repository links. For the
shortlist, we read primary API documentation and checked repository state and
default-branch activity through GitHub or Codeberg. The
[catalog](library-catalog.md) and [metadata snapshot](library-catalog.json)
preserve those observations.

Download counts include repeated builds and transitive dependencies; they do
not count unique applications. A recent commit can be documentation or a bot.
An old release of a small stable crate does not by itself mean abandonment.
A declared MSRV is not a build of the complete graph with selected features.
The survey was not a security audit or a performance comparison of 78 libraries.

## Main conclusions

Rust utility work is best served by the standard library plus focused crates.
The practical Commons-like extension points are
[itertools](https://docs.rs/itertools/latest/itertools/) for iterator operations,
[bstr](https://docs.rs/bstr/latest/bstr/) for string-like byte processing, and
specialized filesystem, format, terminal and process libraries. There is no
measured claim that the same dependencies belong in 80% or 90% of applications.
The current template intentionally predeclares a useful native CLI toolbox;
task-specific extensions remain selected by capability.

The existing clap/Serde/thiserror foundation was appropriate. Handwriting a
parser, JSON/CSV quoting, ignore-rule interpreter or process-pipeline builder
would generally increase maintenance when a suitable established API exists.
Conversely, error classification, permitted effects, resource limits, native-path
policy and output contracts remain application decisions.

High-value candidates include walkdir/ignore/globset for file tools,
tempfile/atomic-write-file for file lifecycle, indicatif for progress, config
for genuinely layered settings, duct for pipelines, csv/url for formats,
humantime/jiff for time, Rayon for CPU work, and ureq for synchronous HTTP.
Their APIs and selection boundaries are linked in the live guide.

## Framework comparison

[Clap](https://docs.rs/clap/latest/clap/) remains the parser owner in this
template. [bpaf](https://docs.rs/bpaf/latest/bpaf/) offers a different compositional
model, while [lexopt](https://docs.rs/lexopt/latest/lexopt/) offers a small parsing
surface with more presentation work left to the caller. Neither warrants a
migration merely from assumptions about size or speed.

[Abscissa](https://docs.rs/abscissa_core/latest/abscissa_core/) supplies application
components, commands, configuration, errors, logging and lifecycle. It can fit a
larger application with shared infrastructure, but adopting it means adopting
that application structure. Its default feature set is broader than this
template's synchronous streaming core. [Cling](https://docs.rs/cling/latest/cling/)
adds handler/state composition over clap; it was a much smaller ecosystem in
the observed data and remains a situational option.

## Concrete opportunity in the template

[assert_cmd](https://docs.rs/assert_cmd/latest/assert_cmd/) provides ordinary
CLI process setup, input/environment handling, timeouts and output assertions.
It is now available as a dev dependency. The existing specialized tests also
prove a 256 KiB capture bound, held-open stdin, downstream closure and cleanup.
Those properties must survive any later migration of the harness.

[snapbox](https://docs.rs/snapbox/latest/snapbox/),
[trycmd](https://docs.rs/trycmd/latest/trycmd/),
[insta](https://docs.rs/insta/latest/insta/), assert_fs and proptest cover different
testing needs. They are not a recommendation to install every overlapping test
framework. Development dependencies do not enter ordinary release runtime
dependencies. [Cargo dependency scopes](https://doc.rust-lang.org/cargo/reference/specifying-dependencies.html#development-dependencies).

## Findings that affected selection

- Current ignore/globset, Ratatui and encoding_rs releases required Rust 1.88;
  process-wrap and etcetera required 1.87; clap-cargo required 1.86. These exceeded
  the original 1.85.1 minimum. The new 1.98.1 baseline accommodates these declared
  requirements, subject to actual graph/platform validation when added.
- `directories`' old GitHub repository was archived because the project moved
  to [Codeberg](https://codeberg.org/dirs/directories-rs). The current project was
  not archived. Its `dirs` sibling released 7.0.0 on 2026-09-05. The old hosting
  flag was not treated as abandonment evidence.
- walkdir, figment, pico-args, directories-next and tap had quieter observed
  release/default-branch activity. Mature stable code and active new development
  were distinguished rather than conflated.
- [regex-lite](https://docs.rs/regex-lite/latest/regex_lite/) explicitly trades
  search speed and Unicode functionality for smaller code/shorter compilation.
  Smaller is not automatically faster.
- Iterator convenience can retain input; byte-string Unicode operations can
  differ from raw-byte operations. Feature selection must preserve semantics.
- [miette](https://docs.rs/miette/latest/miette/)'s fancy reports and
  [config](https://docs.rs/config/latest/config/)'s default formats/features are
  useful when requested, but should not arrive as an unexamined bundle.
- [camino](https://docs.rs/camino/latest/camino/) encodes a UTF-8 path contract,
  so replacing the template's native PathBuf everywhere would narrow behavior.
- Standard File locking (since 1.89), OnceLock and LazyLock can replace habitual
  helper dependencies when their semantics satisfy the task.

## Applying the research

Consult the live guide at the moment a task needs a technical mechanism. Name
what the chosen library replaces, select features, preserve local policy and
run appropriate checks. Refresh dated source/version information before a new
dependency decision. Keep this research available without requiring agents to
load the entire catalog for ordinary work.
