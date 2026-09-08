# Performance and memory

Optimize a measured user workload. For a CLI, startup, time to first useful
output, total elapsed time, throughput, peak resident memory, allocation rate,
and executable size are different properties. The example command deliberately
has no asynchronous runtime or worker pool. Its scan owns one 64 KiB heap buffer
and retains two counters, independent of total input and line length.

This does not make process RSS equal to 64 KiB: executable pages, libraries,
argument parsing, allocator state, and OS accounting also contribute. The CLI
summary is bounded independently of input. Explicit configuration is capped at
64 KiB before parsing. Reassess those bounds when replacing the example.

## Reproduce a measurement

Build once, then measure the executable directly. `cargo run` adds Cargo work to
each invocation. Keep compiler, features, profile, target, machine load, input,
and output destination comparable. Record whether the file cache is warm; do not
call a warmed measurement a cold-start result.

```sh
cargo build --locked --release
python3 -c 'from pathlib import Path; p=Path("benchmark-results"); p.mkdir(exist_ok=True); f=(p/"input.bin").open("wb"); chunk=b"x"*1023+b"\n"; [f.write(chunk*1024) for _ in range(64)]; f.close()'
hyperfine --warmup 3 --runs 20 './target/release/rust-cli-template --help' './target/release/rust-cli-template --format json stats benchmark-results/input.bin'
```

The fixture is 64 MiB with 65,536 LF terminators. Those commands measure two
different workloads separately; their ratio is not an optimization result.
When comparing implementations, use the same command, input, and destination
with two immutable binaries. Run one comparison at a time, verify output first,
and retain variance and all planned samples.

Use the equivalent executable path on Windows. After initialization the binary
has your project's name. Hyperfine is an optional benchmarking tool, not a build
dependency. Avoid rendering large outputs to a terminal when measuring parsing
or computation unless terminal behavior is the workload.

## Choose the evidence

CPU samples help locate computation. Allocation profiling explains churn and
retained owners. System-call evidence can expose repeated tiny I/O operations.
Record peak resident memory for the process budget, including the measurement
tool and OS-specific units. On macOS, `/usr/bin/time -l` reports maximum resident
set size in bytes; on GNU/Linux, `/usr/bin/time -v` reports it in KiB. Use an
appropriate equivalent on Windows and account for child processes separately.

Compare growing input sizes to test a memory-growth claim. A fixed number of
workers does not bound queues, results waiting for ordering, or complete-input
collections. A smaller archive does not establish a smaller peak working set.

```sh
cargo build --locked --profile profiling
```

The profiling profile inherits release optimization while retaining limited
debug information and unstripped symbols. Use it with the platform's profiler;
account for instrumentation overhead and preserve sensitive input.

Treat buffer sizes, LTO, codegen units, alternate allocators, mapping, SIMD, and
parallelism as measured alternatives. Preserve the CPU compatibility baseline,
I/O failures, output semantics, and resource ownership. Keep the simpler correct
implementation when a result is inconclusive.

## Sources

- [Rust Performance Book: profiling](https://nnethercote.github.io/perf-book/profiling.html)
- [Rust Performance Book: heap allocations](https://nnethercote.github.io/perf-book/heap-allocations.html)
- [Cargo profiles](https://doc.rust-lang.org/cargo/reference/profiles.html)
- [Hyperfine](https://github.com/sharkdp/hyperfine)
