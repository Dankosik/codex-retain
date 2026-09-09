# Codex Retain: v0.1 contract

Recorded before implementation and measurement, 2026-09-09.

Keep archived local conversations for a user-selected number of 24-hour days.
Default hypothesis: 30 days. New policies give every existing archive a full
grace period. Native archive/unarchive transitions are captured transactionally
in a small, explicitly owned SQLite extension, including while this program is
not running. Repairs that change Codex's archive timestamp restart the period
conservatively. File modification time is never a retention clock.

Automatic cleanup is an hourly macOS LaunchAgent invocation. No resident service,
cloud, LLM, account, or notification stream. Pause prevents deletion; disable
also removes scheduling and transition capture. Explicit exclusions and Codex
pins prevent deletion. Manual cleanup requires an enabled, unpaused policy.
After `enable --yes`, subsequent runs need no confirmation.

The first adapter targets Codex CLI 0.153.4 local state_5.sqlite and ordinary
legacy JSONL/zstd rollouts. Exact schema and capture checks are mandatory.
Unsupported history formats, ambiguous paths, shared histories, and live writers
are skipped. No recursive thread deletion is used. A cooperative Codex writer
lock, maintenance lock, SQLite transaction, and durable one-file intent protect
each deletion. Interrupted staging is recovered before new work. There is no
long-term trash or backup store.

Public reports distinguish logical data removed, allocated blocks unlinked,
and observed volume free-space change. APFS snapshots, compression, clones and
concurrent activity prevent attributing a precise physical reclaimed byte count.
Cloud history, global history files, logs, memories and all forensic traces are
outside the deletion promise.

## Targets selected before measurements

On the development Mac, warm-cache release build, isolated synthetic profiles:

| Target | Acceptance |
| --- | --- |
| Idle footprint | No living utility process between scheduled runs |
| CLI startup | `--help` median < 20 ms |
| Archive preview | 10,000 indexed archives median < 1 s; peak RSS < 64 MiB |
| Metadata-only scheduling | Unexpired chat bodies are not read |
| Installation size | Native executable < 20 MiB; no Python/Node runtime for the utility |
| Setup | Install binary, then one explicit enable command |
| Clock correctness | Old chat archived now, restore, rearchive, repair, and initial archive grace tests all prevent early deletion |
| Fault safety | Interrupted staging and DB commit recover without deleting active or unrelated chats |
| Concurrent safety | A real supported Codex writer excludes deletion; unarchive and cleanup cannot both win |
| Comparison | Same fixtures and permanent-removal scope for timing; separately disclose backup/Trash modes and semantic differences |

The comparison report must state unconfirmed and failed targets. Performance
claims apply only to the measured fixture, version, platform and cache mode.
Linux and Windows are considered separately; macOS safety is not relaxed for
portability. Neither a GitHub publication nor installation of a real policy is
part of local development validation.

## Implementation amendment after scale preflight

The first one-thread-per-transaction implementation took about 25 seconds for
1,000 removals and exceeded the 300-second harness limit at 10,000. The final
engine groups at most 32 individually validated, writer-locked threads under
one durable journal and one atomic SQLite commit. A group-wide directory flush
preserves durability while reducing repeated flushes. Legacy single-file
recovery and partial-group crash tests are retained. The acceptance targets
above are unchanged; initial failed scale evidence is preserved.

## Cleanup profiling amendment

A subsequent paired experiment raised the bounded group size to 128, buffered
JSON writes, and limited the global coordinator lock to UUID acquisition and
cleanup. Per-thread ownership and eligibility checks, synchronization ordering,
the 1 MiB journal limit, and legacy recovery remain in effect. At 10,000 chats,
median cleanup fell from 14.35 to 7.76 seconds in that experiment. The additional
2x median speedup hypothesis was not met. The [profiling report](cleanup-performance.md)
records the evidence, concurrency tradeoffs, and downgrade boundary.
