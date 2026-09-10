# Local performance experiments, 2026-09-10

Two bounded experiments tested recommendations from the supplied performance
audit. They compare native release executables, not `cargo run`. The baseline
is commit `5920f33e8f8d2a438be540d3302fb7fb5718cdab` plus the four files already
modified when this work began. Those pre-existing changes are included in every
candidate. No real conversations or installed schedules were changed.

## Method

Rust 1.98.1, locked dependencies, native Apple Silicon release profile with
Thin LTO. Hyperfine uses three warmups and five measured runs per case, with
4,096-byte plain JSONL rollouts and all archives expired. Each run gets a fresh
marker-checked synthetic fixture. Preparation and postcondition verification
are outside the timed command. The baseline runs first at 1,000 archives;
the candidate runs first at 10,000.

Every sample checks the exact surviving IDs, file content for preview, removal
of the expected rollouts and database rows for cleanup, and SQLite integrity.
The command is `--json preview` or `--json run`; terminal rendering is excluded.
Filesystem caches are warm. Other applications on this workstation were not
stopped. All samples, including outliers, are retained.

## Experiment 1: immutable reference caching

The candidate cached parsed embedded schema/migration JSON and the
SQLite-normalized capture reference with `LazyLock`. It continued to read the
live database on every validation. It also used `Decoder::with_buffer` and
one output `String` in `safe_text`.

| Command | Archives | Baseline mean ± SD | Candidate mean ± SD |
| --- | ---: | ---: | ---: |
| Preview | 1,000 | 65.6 ± 3.7 ms | 63.1 ± 0.8 ms |
| Preview | 10,000 | 309.2 ± 47.7 ms | 378.8 ± 178.0 ms |
| Cleanup | 1,000 | 456.6 ± 6.2 ms | 474.1 ± 12.9 ms |
| Cleanup | 10,000 | 6.318 ± 0.786 s | 6.366 ± 0.521 s |

This sample set does not establish a whole-command improvement. Reference
caching was removed from the final implementation. That is a decision about
this experiment, not proof that caching can never help a different workload.

## Experiment 2: reuse statements within each transaction

The final candidate prepares the thread lookup and conditional DELETE once per
batch and rebinds each member's parameters. Recovery prepares its existence
query once per journal. It does not cache rows or compatibility decisions.
SQL conditions, affected-row checks, locks, journal order, and durability
barriers remain in place. No dependency or SQLite schema change is needed.

The direct `with_buffer` and `safe_text` simplifications remain. These plain
JSON measurements do not exercise their performance; no separate speed or
memory claim is made for compressed input or text rendering.

| Archives | Baseline mean ± SD | Candidate mean ± SD | Baseline user CPU | Candidate user CPU |
| ---: | ---: | ---: | ---: | ---: |
| 1,000 | 470.6 ± 6.0 ms | 482.1 ± 17.7 ms | 64.8 ms | 47.7 ms |
| 10,000 | 5.985 ± 0.555 s | 6.003 ± 0.623 s | 0.554 s | 0.340 s |

User CPU time fell by about 26% and 39%, respectively. This is user-mode CPU
accounting, not total CPU time or elapsed time. Whole-command elapsed time did
not improve: the differences were approximately +2.4% and +0.3%. The broad
10,000-archive ranges overlap. We retain statement reuse for its measured
reduction in computation, with no claim of a faster complete cleanup.

The benchmark does not isolate the remaining cost. Determining the next
whole-command intervention requires profiling this candidate; old fsync
profiles alone cannot establish its current bottleneck.

[Raw reference-cache samples](evidence/performance-2026-09-10/references/summary.json),
[raw statement samples](evidence/performance-2026-09-10/statements/summary.json),
[exact comparison](evidence/performance-2026-09-10/comparison.json), and
[final postconditions](evidence/performance-2026-09-10/statements/final-postconditions.json)
are retained with environment, binary hashes and source patches.

## Validation and limits

All 74 Rust tests passed on the final implementation, including batch boundaries,
busy members, native pin guards, aliases, compressed files, decoder window
limits, and interrupted recovery. `cargo fmt --all -- --check`,
`cargo clippy --locked --all-targets -- -D warnings`, and the locked release build
passed. The comparison harness now supports `--retain-only`, so a Retain A/B
experiment does not require Node or a Janitor checkout.

The measurements do not cover cold caches, 100,000 archives, large compressed
headers, mixed active/archive profiles, busy fallback throughput, allocation
counts, or concurrent Codex latency. macOS `/usr/bin/time -l` output is retained,
but its maximum RSS is not a simultaneous peak of the process tree. Historical
profiling results are not treated as a profile of this candidate.

The proposed rollout-owner index/cache, conflict splitting, candidate-only
inventory, and compact scheduled reports remain separate experiments. This
change does not alter those algorithms or claim their potential gains.

## Reproduction

Build the baseline and candidate from the same Rust toolchain and retain both
executables, then run:

```sh
python3 scripts/benchmark.py run \
  --root /private/tmp/retain-new-comparison \
  --baseline /absolute/path/to/baseline \
  --retain /absolute/path/to/candidate \
  --codex-bin /absolute/path/to/compatible-codex \
  --retain-only --counts 1000 10000 --runs 5 --only cleanup
```

The output directory must not exist and must have no symlink components.
The harness records binary SHA-256 identities, environment, raw sample times,
preflight reports, and postcondition checks. Both experiments' retained source
patches are relative to the baseline commit and include the pre-existing dirty
changes so their runtime sources can be reconstructed.
