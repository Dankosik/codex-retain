# Cleanup performance: why it was slower and what changed

The original published 10,000-chat comparison was 19.35 seconds for Retain and 2.88 seconds for Janitor, a 6.72x difference. The operations have different effects: Janitor removes rollout files, while Retain also checks current eligibility, updates SQLite, and preserves an interrupted-deletion recovery protocol. Profiling nevertheless found avoidable work in Retain.

## Measured result

A fresh paired run used the same Apple M5 / 16 GiB / macOS 26.4 host, Rust 1.98.1 optimized binaries, Codex 0.153.4, and Janitor at `32737c7cc68a74a63a21e5b8a403e92f4f1398e6`. Each case had three warmups and five measured runs, with a newly generated synthetic profile before every run. All files were 4 KiB expired legacy archives; both Retain binaries used exactly the same policy and fixture preparation.

| Archived chats | Previous Retain median | Updated Retain median | Speedup | Janitor median |
| --- | --- | --- | --- |
| 1,000 | 1.196 s | 0.523 s | 2.29x | 0.308 s |
| 10,000 | 14.347 s | 7.762 s | 1.85x | 3.123 s |

At 10,000 chats, elapsed time fell by **45.9%**. The updated utility remains **2.49x slower than Janitor** in this comparison. The preselected target of at least 2x lower median cleanup time at 10,000 was **not met**; the 1,000-chat case exceeded 2x. We retained every sample rather than rerunning until the target passed.

| Case | Mean ± standard deviation | Range | Mean CPU time (user + system) | Recorded max RSS |
| --- | --- | --- | --- | --- |
| baseline, 1,000 | 1.181 ± 0.030 s | 1.138 to 1.211 s | 0.560 s | 50.61 MiB |
| retain, 1,000 | 0.524 ± 0.025 s | 0.501 to 0.563 s | 0.335 s | 50.52 MiB |
| janitor, 1,000 | 0.310 ± 0.006 s | 0.303 to 0.319 s | 0.397 s | 160.28 MiB |
| baseline, 10,000 | 16.674 ± 4.426 s | 13.049 to 23.727 s | 7.648 s | 50.48 MiB |
| retain, 10,000 | 7.198 ± 1.007 s | 5.703 to 7.964 s | 3.802 s | 50.59 MiB |
| janitor, 10,000 | 3.168 ± 0.177 s | 2.978 to 3.424 s | 3.705 s | 232.56 MiB |

The ordinary desktop remained active. These are warm-cache measurements, not controlled cold-disk results. The 10,000-chat baseline had substantial variance. RSS is one separate native `/usr/bin/time -l` observation per case, not a distribution or summed simultaneous process-tree peak. Physical disk-read bytes and exact physical space reclamation remain unmeasured.

[Environment, identities and conditions](evidence/cleanup-optimization/environment.json) · [All cleanup samples](evidence/cleanup-optimization/summary.json) · [Original experiment plan and amendments](evidence/cleanup-optimization/experiment.json)

## What the profile showed

A five-second native sample of the previous executable collected 4,311 thread samples. About 52% had `__fcntl` at the top of the stack through `File::sync_all`; about 13% had `write` through JSON serialization directly into a `File`. These are sampling observations, not exact wall-time fractions or counts of system calls. The profiled run took 13.02 seconds with sampling overhead, and is not substituted for the paired benchmark.

On Apple platforms, [Rust implements `File::sync_all` using `F_FULLFSYNC`](https://doc.rust-lang.org/src/std/sys/fs/unix.rs.html#1413-1434). The previous code also passed an unbuffered file to `serde_json::to_writer_pretty`, causing many small writes for JSON punctuation, indentation and fields. [The raw sample](evidence/cleanup-optimization/baseline-sample.txt) and [its concise interpretation](evidence/cleanup-optimization/profile-analysis.json) preserve the evidence.

## Changes

1. **Buffer JSON writes.** A 64 KiB `BufWriter` collects small writes. An explicit `flush()` occurs before checking the 1 MiB file bound, synchronizing the file, or publishing its name. Flush and serialization errors propagate; the previous state is preserved. This follows [the standard-library buffering contract](https://doc.rust-lang.org/std/io/struct.BufWriter.html).
2. **Use groups of at most 128 chats instead of 32.** Every member still receives the same current-row, pin, archive-period, file-identity and ownership checks. The same journal, archive-directory and SQLite durability ordering remains. On an uncontended 10,000-chat run, groups fall from 313 to 79. The five Rust `sync_all` calls per normal group therefore fall from 1,565 to 395, excluding setup/reporting and SQLite synchronization.
3. **Release global coordination during the work.** Retain acquires UUID locks while holding `.coordination.lock`, then releases coordination as native Codex does. UUID locks remain held through deletion and finalization. Cleanup reacquires coordination before closing and unlinking lock files; if unavailable, it closes without unlinking. Partial acquisition failures still clean only successfully owned paths under the original guard.

No safety override, disabled fsync, lower SQLite synchronous setting, parallel deletion, dependency change, or reduced archive-retention rule was introduced. The archive clock, exclusions, native pins, fail-closed checks and one-transaction-per-group protocol remain in effect.

## Concurrency and resource tradeoffs

The first 128-item experiment retained the old global lock lifetime and increased its sampled p95 to 179 ms. That was not the final design. Matching the native coordination protocol removed the global lock from the lengthy I/O phase:

| Diagnostic, 10,000 ordinary archives | Median | p95 | Maximum |
| --- | --- | --- | --- |
| Previous global coordinator hold | 40.298 ms | 49.347 ms | 104.145 ms |
| Updated coordinator acquisition phase | 5.777 ms | 6.575 ms | 11.234 ms |
| Updated coordinator cleanup phase | 5.822 ms | 7.951 ms | 8.768 ms |
| Updated UUID-lock lifetime | 70.925 ms | 82.879 ms | 90.730 ms |

These measurements came from separate instrumented scratch builds. Instrumentation is absent from the compared production binaries. The timings exclude the final handle close and diagnostic output; they are observations, not hard latency guarantees. [Previous lock samples](evidence/cleanup-optimization/trace32-small.json), [updated lock samples](evidence/cleanup-optimization/split128-small.json).

A separate 128-chat stress fixture with 900,000-byte compressed metadata recorded about 4.2 ms for coordinator acquisition and 3.1 ms for cleanup. Under SQLite writer contention, UUID locks remained held for about 250 ms while the coordinator phases remained about 4.5 and 10.8 ms. [Compressed-metadata case](evidence/cleanup-optimization/split128-compressed.json), [contention case](evidence/cleanup-optimization/split128-sqlite-busy.json).

The larger group can hold up to 128 UUID file descriptors and still holds a SQLite write transaction during its critical work. Individual UUID locks can be held longer than with 32-item groups; this is not a claim that every concurrent operation became faster. A conflicting member still causes individual fallback attempts, so busy archives can cost more than the all-expired, uncontended benchmark.

If cleanup cannot reacquire coordination, empty stale UUID lock files may remain until a fresh Codex coordinator cleans them. They contain no chat data. The 1 MiB journal bound is unchanged. Older builds capped at 32 reject larger pending journals before mutation; finish recovery or disable with the newer build before downgrading.

## Preview regression check

Both Retain binaries produced identical preview JSON after ignoring only `started_at`. Three warmups and five runs reused the same unchanged 10,000-chat fixture because preview is read-only. File hashes and SQLite integrity were checked afterward.

| Preview tool | Median | Mean ± standard deviation | Recorded max RSS |
| --- | --- | --- | --- |
| baseline | 312.4 ms | 306.0 ± 26.0 ms | 50.52 MiB |
| retain | 275.7 ms | 324.9 ± 117.6 ms | 50.56 MiB |
| janitor | 797.3 ms | 809.5 ± 25.0 ms | 187.78 MiB |

One updated-preview sample was an outlier and is retained. Its median did not regress; its mean was about 6% higher. The preview algorithm was not optimized by this change, so we do not attribute a preview speedup to the cleanup work. [Samples](evidence/cleanup-optimization/preview-regression.json), [postconditions](evidence/cleanup-optimization/preview-postconditions.json).

## Validation and reproducibility

74 Rust tests passed, including complete buffered writes, failed/oversized JSON preserving previous state, legacy 32-item recovery, maximum-size interrupted groups, busy-member positions, unchanged-inode reuse after contended cleanup, and the existing archive/pin/clock/alias/crash cases. Clippy with warnings denied passed. The final binary passed [native Codex conformance](evidence/cleanup-optimization/native-codex-final.json) and [temporary launchd integration](evidence/cleanup-optimization/native-launchd-final.json); the test job was removed.

Previous binary SHA-256: `af2aeb726b92d273c5e30b2e242264e6dde2fd73cd067ca7a5f37771130040f2`. Updated binary: `8fcb4959a9b2df1864e100a8af05f8fec9fb631b6b4d1dafc3a42068e117a649`. The baseline source is commit `25b42c1e4f18741bc163b5a49de006be8f413ce0`; that revision changed documentation only after the original runtime build.

Build each executable before measuring, then use the optional baseline argument:

```sh
python3 scripts/benchmark.py run \
  --root "$NEW_SCRATCH_DIRECTORY" \
  --retain "$UPDATED_BINARY" \
  --baseline "$PREVIOUS_BINARY" \
  --codex-bin "$SUPPORTED_CODEX_BINARY" \
  --janitor "$PINNED_JANITOR_CHECKOUT" \
  --counts 1000 10000 --runs 5 --only cleanup
```

The harness never operates on default user profiles. Preparation and exact postcondition checks stay outside the timed command. Both measured cleanup modes permanently remove the same synthetic file set without backups or Trash; Retain additionally maintains supported SQLite state. All raw samples remain available alongside the original benchmark evidence. No real conversations, production schedules, tags, registry packages or public binary releases were changed.
