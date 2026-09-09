# Performance and benchmark evidence

Status: completed native measurements and source-pinned comparison. The target
table below records the original hypotheses; measured acceptance appears at the end. The
[competitor source review](competitors.md) explains selection and effect
differences at pinned revisions.

## Targets fixed before measuring

The [product contract](product-contract.md) owns acceptance. The tighter
1,000-rollout and comparative targets below remain research hypotheses, and a
failed research hypothesis does not silently replace the product requirement.

| Metric | Target | Result |
| --- | --- | --- |
| Warm `--help` median | Under 20 ms on the recorded macOS host | See measured results below |
| Preview, 10,000 indexed archived rollouts | Under 1 s median; peak RSS under 64 MiB | See measured results below |
| Native executable size | Under 20 MiB | See measured results below |
| Preview, 1,000 × 4 KiB archived rollouts | Under 250 ms median | See measured results below |
| Preview peak process RSS, research hypothesis | Under 40 MiB | See measured results below |
| Preview versus applicable baseline | At least 2x lower median elapsed time or 2x lower peak RSS | See measured results below |
| 10,000-rollout scaling | Report time, variance, CPU and RSS without hiding regressions | See measured results below |
| Between scheduled executions | Zero resident utility processes | Validate scheduler/process lifecycle separately |
| Archive-age/restore/rearchive correctness | Zero ineligible deletions | Covered by separate correctness tests, not throughput fixtures |

If a target fails, retain the result and explain the tradeoff. A shared required
Codex version check may dominate fast scans; the harness measures `codex
--version` separately to make that cost visible. Do not subtract it from the
reported end-to-end Retain runtime.

## Fixture contract

`scripts/fixture.py` uses the captured supported schema and migration checksums
under `compatibility/`. All conversations are invented. Each archive has a
deterministic UUID, valid early session metadata, and exactly 4,096 bytes by
default. The default benchmark cases contain 1,000 and 10,000 files (approximately
3.9 and 39.1 MiB of logical transcript data), respectively.

The throughput fixture deliberately makes every chat eligible under both
products: creation/update/file-modification time is 100 days old, and archive
time is 40 days old. Retention is 30 days. There are no active chats, excluded
IDs, malformed rows, descendants, or retained backups. This establishes a
comparable selection set; it does not test the harder deletion rules.

Retain's real `enable --no-schedule` initializes its policy and onboarding grace.
The harness then ages only the generated fixture's retention epochs. That
test-only SQL is guarded by a synthetic-fixture marker and an absolute root
identity. There is no production clock override. The harness never installs a
scheduler, edits real Codex state, or uses the user's actual home directory for
cleanup commands.

Schema, migration, release-binary, Janitor lockfile, and Janitor CLI hashes are
included in the evidence. The fixture manifest records every expected file ID,
length, and content digest. A fresh fixture is generated before **every** warmup
and measurement, outside the timed interval. Each completed operation is checked
after timing: preview preserves every file's content; cleanup removes exactly
the expected file set; SQLite integrity and expected surviving row IDs are also
checked. A failed check aborts the benchmark.

## Baseline and fairness

The default baseline is `codex-session-janitor` 0.1.0 at
`32737c7cc68a74a63a21e5b8a403e92f4f1398e6`. Build that exact checkout before running
the harness. Tracked modifications or a different revision are rejected.

| Operation | Codex Retain | Janitor |
| --- | --- | --- |
| Preview | Full `--json preview` including safety checks and version probe | `scan --retention-days 30` |
| Cleanup | `--json run` with the configured policy | `clean --retention-days 30 --confirm --mode delete` |
| Effects | Removes eligible rollout and maintains supported SQLite thread state | Removes rollout files; SQLite rows remain |
| Recovery copies | None in this comparison | None with `--mode delete` |

The fixture data and selected file IDs match. Effect surfaces still differ and
must accompany any comparison. Retain's JSON preview contains more explanation
than Janitor's scan summary; both produce their normal output, redirected to a
pipe by Hyperfine. No tool receives a special stripped-down timed code path.

Janitor can select active chats under its normal retention command; this fixture
has an empty active directory. Performance on this artificial comparable subset
does not make its retention semantics interchangeable with Retain's.

Cliner's mandatory backup mode is not used as a permanent-deletion speed
baseline. History Cleaner targets a different SQLite schema and does not select
by retention; it is not silently modified to make a benchmark run. Rust language
choice, absence of a UI, or omitted backup work are not performance evidence.

## Running the harness

Build the optimized utility and the pinned competitor first. The benchmark does
not install dependencies, compile either product, or change their source trees.
Do not run Cargo builds or other benchmarks concurrently with measurements.
`BENCH_ROOT` must be a new scratch directory outside the repository, and every
path variable below must refer to a local executable or checkout.

```sh
python3 scripts/benchmark.py run \
  --root "$BENCH_ROOT" \
  --retain "$RETAIN_RELEASE_BINARY" \
  --codex-bin "$COMPATIBLE_CODEX_BINARY" \
  --janitor "$PINNED_JANITOR_CHECKOUT" \
  --counts 1000 10000 --rollout-bytes 4096 --runs 10
```

Add `--prepare-only` to exercise fixture generation, policy setup, real preview
or cleanup, and postcondition checks without collecting timed results. This
option still performs deletion inside its generated fixtures. `--only preview`
or `--only cleanup` narrows the measured workload. Small smoke checks can use
`--counts 3`. A separate fixture can be created and checked with:

```sh
python3 scripts/fixture.py create --root "$NEW_FIXTURE_ROOT" --count 3
python3 scripts/fixture.py verify --root "$NEW_FIXTURE_ROOT"
```

The harness uses `hyperfine --shell=none --warmup 3 --runs 10 --output=pipe`,
with untimed preparation and post-run validation. Executables are invoked
directly; Python orchestration is excluded from timed commands. A minimal
environment and explicit synthetic home/state paths prevent default-home
fallback. The order of tools alternates between fixture sizes. Files are in
the warm filesystem cache because generation and verification precede timing;
these are not controlled cold-disk measurements.

## Evidence files and interpretation

| File | Meaning |
| --- | --- |
| `environment.json` | Host OS/CPU/RAM, tool versions, binary/lockfile hashes, fixture parameters and comparison limits |
| `*-hyperfine.json` | Individual elapsed-time samples, mean, median, standard deviation, and CPU statistics reported by Hyperfine |
| `*-preflight.json` | Untimed actual output and validated filesystem/database postconditions |
| `*-time.txt` | Separate macOS `/usr/bin/time -l` native process accounting |
| `preparation.jsonl` | Fixture preparation elapsed times, excluded from cleanup measurements |
| `summary.json` | Combined raw timing results and extracted macOS peak RSS |
| `fixtures/*/manifest.json` | Synthetic expected IDs, content digests and data/schema sizes |

Record raw values and compute ratios from the same metric. Keep median and
dispersion visible; do not select the best trial. macOS reports maximum resident
set size in bytes. Native command accounting is not a sum of simultaneous RSS
across an entire process tree; disclose that limitation when a child Codex
process is involved. CPU time, wall time and memory are separate metrics.

Physical disk-read bytes are **unmeasured** by this harness. Block-I/O counters,
syscall counts, logical file sizes, and source-code read limits do not establish
physical reads. Likewise, the release executable size is recorded, but complete
installation footprint requires separate measurements of production dependencies
and prerequisite runtimes; a preinstalled Node runtime is a legitimate user case.

Logical transcript bytes removed, allocated file bytes unlinked, and observed
filesystem free-space delta have different meanings. APFS snapshots, clones,
open handles, SQLite allocation, and concurrent filesystem activity can prevent
observed free-space delta from matching logical bytes. No benchmark number should
be labeled exact physical bytes reclaimed unless that fact was measured with a
suitable isolated filesystem procedure.

## Recorded results

Measured on Apple M5, 16 GiB RAM, macOS 26.4 ARM64; Rust 1.98.1 release build, Node v25.8.2, Codex 0.153.4 and Janitor 0.1.0 at the pinned revision above. Ten timed runs plus three warmups per case. All expired rollouts are 4,096 bytes; freshly generated fixtures and postcondition checks are outside timing. The ordinary desktop remained active: this is a warm-cache shared-machine experiment, not a dedicated lab or cold-I/O measurement.

Measured executable: **4,031,424 bytes (3.84 MiB)**, SHA-256 `af2aeb726b92d273c5e30b2e242264e6dde2fd73cd067ca7a5f37771130040f2`. [Environment and binary identities](evidence/benchmarks/environment.json), [all samples](evidence/benchmarks/summary.json).

| Operation | Retain median (mean ± SD) | Janitor median (mean ± SD) | Interpretation |
| --- | --- | --- | --- |
| preview, 1,000 chats | 84.4 ms (86.6 ± 12.0) | 248.8 ms (273.1 ± 77.4) | Retain 2.95× faster |
| cleanup, 1,000 chats | 1280.8 ms (1284.4 ± 51.7) | 485.0 ms (499.4 ± 69.0) | Retain 2.64× slower |
| preview, 10,000 chats | 521.2 ms (540.3 ± 65.5) | 1367.6 ms (1604.3 ± 682.4) | Retain 2.62× faster |
| cleanup, 10,000 chats | 19350.1 ms (21102.4 ± 6092.1) | 2878.4 ms (2935.2 ± 311.2) | Retain 6.72× slower |

Help startup median: **2.67 ms versus 140.40 ms**, 52.5× faster in this fixture. Help never starts Codex or loads policy. The separate Codex version probe showed substantial variance; its cost remains included in end-to-end Retain commands, never subtracted.

| Operation | Retain max RSS | Janitor max RSS | Retain / Janitor CPU seconds (mean user + system) |
| --- | --- | --- | --- |
| preview, 1,000 | 50.53 MiB | 146.66 MiB | 0.073 / 0.343 |
| cleanup, 1,000 | 50.62 MiB | 153.45 MiB | 0.620 / 0.629 |
| preview, 10,000 | 50.56 MiB | 180.14 MiB | 0.512 / 2.132 |
| cleanup, 10,000 | 50.41 MiB | 230.67 MiB | 11.222 / 3.256 |

**RSS is one separate `/usr/bin/time -l` observation per case**, not a distribution or sum of simultaneous parent/child RSS. It includes native accounting for commands that launch children; exact process-tree peak memory is not established. Raw accounting files are beside the timing JSON. CPU figures come from Hyperfine and are not physical disk-read measurements.

Confirmed advantages in this experiment: faster help startup, faster archive preview, lower preview CPU time, and lower recorded maximum RSS. Permanent cleanup is **slower** than Janitor in both fixture sizes. Retain additionally performs fresh compatibility/ownership checks, SQLite state changes and fsynced recovery journaling; Janitor removes files and leaves its SQLite rows. Neither measured mode backs up files or uses Trash, so omitted backup work does not explain the comparison.

Some Janitor preview series contain statistical outliers; all samples and dispersion are retained. Do not generalize these ratios to every Mac, data distribution, cold disk, compressed history or Codex version.

### Acceptance against targets

| Target | Result |
| --- | --- |
| Help <20 ms | PASS: 2.67 ms median |
| 10,000 preview <1 s, RSS <64 MiB | PASS: 521.2 ms; 50.56 MiB |
| Executable <20 MiB | PASS: 3.84 MiB |
| Research 1,000 preview <250 ms | PASS: 84.4 ms |
| Research RSS <40 MiB | FAIL: 50.53–50.56 MiB for preview |
| Research ≥2× preview time or RSS improvement | PASS: 2.62× faster 10,000 preview, 3.56× lower recorded RSS |
| Zero resident utility processes | PASS in the native temporary launchd test: idle registrations have no PID |
| Archive timing / preservation | PASS in native Codex conformance and Rust lifecycle tests; see validation record |
| Faster cleanup than baseline | NOT demonstrated; measured slower at both sizes |
| Physical disk reads, exact physical reclamation | UNMEASURED; no superiority claim |
| Complete prerequisite installation footprint | UNMEASURED; executable size only, existing Codex/Node prerequisite disclosed |

### Failed first scale attempt and repair

The first implementation synchronized and committed one thread at a time. Its 1,000-chat preflight took about 25 seconds; 10,000 hit a 300-second harness timeout. Those are diagnostic preflight observations, not repeated benchmark medians. [Original receipt](evidence/initial-scale-failure.json). The final engine groups at most 32 independently validated threads into one durable journal/transaction and scans rollout ownership once per group. Group locks, crash-prefix recovery and post-effect failure handling have dedicated regression tests.

The final binary subsequently completed the remaining 1,279 files from that actually interrupted synthetic run in 2.41 seconds, preserved SQLite integrity, and left no staged files. The interruption happened between intents; simulated single/group intent crash tests cover mid-operation boundaries separately. [Recovery receipt](evidence/interrupted-scale-recovery.json).

The native hourly-entrypoint test initially exposed a background-priority startup stall; normal short-job scheduling fixed it without relaxing the version guard. [Diagnosis](launchd-startup.md), [final native schedule receipt](evidence/launchd-integration.json).
