# Competitor research and comparison contract

Reviewed 2026-09-09. Repository pages were checked online and the cloned source
was inspected at the revisions below. These are source findings, not performance
results or assertions about all future releases. No real Codex conversations were
read, changed, or deleted during this research. README descriptions establish
intended positioning only; the implementation links below support behavior.

| Project | Package version | Inspected commit |
| --- | --- | --- |
| zzy0222/codex-session-janitor | 0.1.0 | `32737c7cc68a74a63a21e5b8a403e92f4f1398e6` |
| gabrielhamalwa/codex-cliner | 0.1.1 | `fdf092c762a2528b7ab653e1e71d586f2ffb4798` |
| LeeeeTX/codex-history-cleaner | 0.1.0 | `7058d580170c2d47fbc3aac2c4742da8df62f8d8` |
| hapwi/codex-cleaner | 0.0.12 | `7f271007c52df381e79ffcdbd5e529cef2e48341` |
| kylianrain7-gif/codex-archive-cleaner | No package version | `9d192556cb637d067db576efcb9af6dec52f852b` |

## Findings that determine the product

None of the five inspected implementations provides the complete product:
expiration from the most recent archive transition, continued protection after
restore/rearchive, an installed automatic policy, pause/disable controls, and
bounded storage. This is an opportunity in behavior and usability. It does not
establish a speed advantage.

### Codex Session Janitor

- **Age and selection:** scans `sessions` and optionally `archived_sessions`;
  active sessions are always included. The CLI offers `--no-archived`, but no
  archive-only switch. Retention compares each file's `mtime` with the cutoff.
  Therefore a 60-day-old transcript archived today remains immediately eligible
  for a 30-day policy when the move preserves its modification time.
  [Roots][janitor-roots], [scan][janitor-scan], [retention predicate][janitor-plan].
- **Effects and races:** the executor validates lexical path containment, then
  uses Trash or `fs.rm(..., {force: true})`. It does not re-read archive status,
  file identity, or age. It does not update SQLite or JSONL indexes. A selected
  path that disappears before execution may still count as removed in permanent
  mode because force removal tolerates absence. No descendant traversal appears
  in this executor; active files are at risk because they are directly selected,
  not because of recursive thread deletion. [Executor][janitor-clean].
- **Storage reporting:** default Trash retains the bytes elsewhere. Both Trash
  and permanent deletion report the sum of candidate logical sizes as
  `freedBytes`; dry-run also uses that field. This is not measured filesystem
  free-space growth. [Executor][janitor-clean].
- **Operation and installation:** Node >=20 with React/Ink and a Trash package.
  `startup-clean` is a callable interval-gated command, not a scheduler installer;
  it writes one timestamp marker after execution. There is no durable archive
  retention policy or exclusion list in these CLI/core paths.
  [CLI][janitor-cli], [startup][janitor-startup], [package][janitor-package].
- **Performance detail:** metadata reads stream and stop after enough metadata
  or a 200-line threshold checked after nonempty lines. It is inaccurate to describe this competitor as
  reading every complete transcript. Very long lines are still relevant.
  [Metadata reader][janitor-metadata].

### Codex Cliner

- **Age and selection:** `prune-archived` selects archived records using
  `updatedAt`, which comes from `session_index.jsonl.updated_at` or the rollout
  file's modification time. The archive glob is `*.jsonl`, while active sessions
  use `**/*.jsonl`. It does not consult SQLite archive status or `archived_at`.
  Active and archive records are deduplicated separately. An ID occurring in
  both areas is therefore not an explicit ambiguity veto.
  [Prune command][cliner-cli], [scan and timestamp][cliner-scan],
  [inventory assembly][cliner-inventory].
- **Effects and races:** every prune copies selected files and the full
  `session_index.jsonl` and `history.jsonl` before removal. It then removes files
  and rewrites both JSONL indexes. Rename makes each replacement atomic, but
  does not synchronize the read/filter/write sequence with concurrent Codex
  appends. A restored ID can lose index/history entries selected earlier; there
  is no archive-state revalidation before this rewrite. Parsing silently ignores
  malformed lines and treats read errors as an empty input, so the rewrite is
  not fail-closed on unreadable or corrupt input.
  [Prune and index rewrite][cliner-actions], [JSONL and atomic-write helpers][cliner-fs].
- **Recovery and growth:** backups have manifests and a catalog; a pruning
  function supports age, keep-latest, and selected runs. The TUI invokes manual
  pruning, including a keep-10/older-than-30-days choice. Pruning archive chats
  does not itself prune backups. Consequently repeated pruning can preserve
  all removed transcript data until an additional backup-cleanup action.
  [Backup lifecycle][cliner-backups], [TUI backup pruning][cliner-backup-ui].
- **Operation and installation:** Node >=20, Bun >=1.3.11 in package metadata,
  plus React/Ink and other runtime dependencies. The CLI provides scan, restore,
  prune, delete, and config commands, without OS scheduling controls. The source
  reads the complete transcript before splitting off its first 50 lines, which
  gives a concrete hypothesis for memory and I/O comparisons on large files.
  [Package][cliner-package], [CLI][cliner-cli], [metadata extraction][cliner-scan].

### Codex History Cleaner

- **Selection and compatibility:** a Rust TUI/CLI for explicitly selected IDs,
  not a timed archive-retention policy. It hardcodes `state_5.sqlite` and a set
  of thread columns/tables. Listing defaults to nonarchived rows; explicit
  deletion does not require archived status. The current `CODEX_THREAD_ID`, when
  present, is protected, which is narrower than protecting every active chat.
  [CLI][history-cli], [store][history-store], [delete orchestration][history-delete].
- **Effects and descendants:** deletion removes edges touching the selected
  thread and deletes its thread row. It does not explicitly recurse to child
  thread rows. Selected rollouts, shell snapshots, and matching index records
  are affected. Thus it should not be accused of recursively deleting all
  descendants; it does remove relationship records involving surviving children.
  [Delete statements][history-delete-rows], [plan][history-delete].
- **Recovery and concurrency:** recoverable mode copies rollout/snapshot files
  and records a manifest and SQLite row snapshot. `--no-backup --yes` selects
  permanent deletion. A SQLite transaction covers row deletion, but JSONL index
  replacement occurs before commit and original file removal after commit.
  A crash therefore has cross-store partial-completion windows. The transaction
  does not validate archived status. These are source-level limitations, not a
  reproduced claim of corruption. No automatic backup expiry is exposed.
  [Orchestration, backups, file/index operations][history-delete], [CLI][history-cli].
- **Installation:** `cargo install --path .`; the helper may append a PATH block
  to `.zshrc`. A standalone binary is a feature already available here; using
  Rust is not a differentiator. [Manifest][history-package], [installer][history-install].

### Codex Cleaner (hapwi)

- **Different primary task:** archives active chats, rotates logs, and manages
  tool-created archive folders. Old-chat selection uses creation time, falling
  back to update time. Archiving does write `archived=1` and a current
  `archived_at` timestamp. Pinned IDs are protected by this workflow, and the
  CLI also accepts explicit thread IDs to keep. This is useful prior art for
  exclusions, but it is not expiration of an already archived chat.
  [Age source][hapwi-age], [archive operation][hapwi-archive], [arguments][hapwi-cli].
- **Existing archive cleanup:** only immediate items named `codex-cleaner-*`
  are selected from archive directories. Age is directory `mtime`; chat
  archives require `--include-chat-archives`. Native loose Codex archived
  rollout files are outside that selection. Selected folders are moved to
  `~/.Trash`, or a tool trash directory if the system Trash directory is absent.
  [Item filter][hapwi-items], [trash selection and move][hapwi-trash].
- **Storage and recovery:** cleanup creates metadata/config/index and SQLite
  backups. Moving to archive or Trash does not itself release the stored
  content. The implementation explicitly reports moved storage and explains
  that Trash must be emptied before space is freed. Transparent reporting is
  already a competitor strength, not an exclusive proposed advantage.
  [Backup][hapwi-backup], [report and disk-space note][hapwi-report].
- **Concurrency and installation:** process/open-file checks and waiting are
  used in selected log-rotation paths; the apply routine does not globally
  require Codex to stop before chat archiving or archive trash operations.
  A SQLite backup uses SQLite's backup API. The package provides Node >=18
  wrappers that invoke Python 3, and also offers a Codex skill. No automatic
  retention schedule is installed by the inspected CLI. The Python script can
  run directly without an LLM. [Apply flow][hapwi-apply], [package][hapwi-package],
  [Python invocation][hapwi-runner].

### Codex Archive Cleaner (Windows)

- **Selection:** the PowerShell script enumerates loose `rollout-*.jsonl`
  files in `archived_sessions` with a UUID filename suffix. Every match is a
  candidate; there is no age threshold or exclusion policy. Preview is default
  and `-ConfirmDelete` enables effects. [Complete cleanup script][windows-clean].
- **Effects:** copies the index and rollouts, removes rollouts, then filters
  index lines containing any selected UUID substring and replaces the index.
  It does not consult SQLite, validate concurrent restore, or inspect child
  relationships. No shared lock covers the operations. Restoring has a separate
  script, and creates another pre-restore backup. Automatic backup expiry and
  scheduled policy controls are absent from these scripts.
  [Cleanup][windows-clean], [restore and pre-restore backup][windows-restore].
- **Scope:** useful Windows archive-file cleanup prior art, with a different
  installation model (copy a skill folder and invoke PowerShell). It is not a
  useful macOS runtime-performance baseline. The README documents that install
  route; it is not treated as proof of deletion safety.
  [Documented installation][windows-install].

## Measurable targets selected before measurement

The targets below are hypotheses and acceptance goals. They must not become
release claims until implementation tests or comparable measurements support
them. See the actual benchmark result document for attained values and limits.

| Area | Target | Evidence required |
| --- | --- | --- |
| Archive semantics | Zero premature deletions in all defined restore/rearchive, old-chat-newly-archived, missing/invalid timestamp, and exact-boundary cases | Isolated integration fixtures; actual selected/deleted IDs, not just counts |
| Safety under change | Zero active, excluded, ambiguous, or newly rearchived threads deleted in race/failure fixtures | Deterministic synchronization or injected failures around actual effects, plus unchanged survivor contents |
| First enable | Existing archive receives an explicit grace policy; no unexpected deletion on enable | Executable enable/preview/run scenario with old fixtures |
| Idle resources | No continuously running utility process between scheduled executions | Generated scheduler configuration plus observed process exit; competitors without daemons also have zero idle processes |
| Routine UX | Install plus one enable command; preview/status/pause/disable each one command | Executable help and isolated scheduler lifecycle tests; count semantic user actions, not pasted shell lines |
| Startup | Warm median help/version under 20 ms on the recorded macOS host | Release binary; `hyperfine --warmup 3`; report host, tool versions, run count and dispersion |
| Archive check | Warm median 1,000-rollout preview under 250 ms and peak RSS below 40 MiB | Identical local fixture; include required safety checks and runtime process costs |
| Relative efficiency | At least 2x lower median preview wall time or at least 2x lower peak RSS than an applicable baseline | Same fixture, mode, output suppression, cache state, installed-build conditions; report failed targets too |
| Storage | No retained transcript copy after successful permanent cleanup; logical bytes and observed free-space change are separate | Filesystem inventory before/after, backed-up/trashed bytes counted explicitly; APFS snapshots/clones/open handles caveat |

## Reproducible comparison design

Use a dedicated temporary test home and explicit Codex-home arguments for every
command. Never allow a tool to fall back to the operator's actual Codex home.
Some tools write their own data under the OS home even when Codex home is
overridden; run those in an isolated OS-home sandbox too. Record exact dependency
lockfiles and runtime versions. Do not measure `npx` download, source compilation,
or first-use package installation as cleanup runtime.

Suggested fixture matrix:

1. **Empty:** valid supported Codex schema and zero chats. Measures startup and
   schema/safety-check overhead.
2. **Ordinary:** 1,000 archived rollouts, each 64 KiB, with valid early metadata
   and 50% eligible. Same archive timestamp, update timestamp, and `mtime` for
   this throughput fixture so differing selection rules choose the same files.
   Use an empty active directory for the Janitor comparison.
3. **Large transcripts:** 100 rollouts at 1 MiB and 100 at 10 MiB, with early
   metadata. Tests full-file-read cost without manufacturing a pathological
   metadata position. Separately test very long lines as a stress case.
4. **Mixed correctness:** active old chats, newly archived old chats, expired
   archives, exclusions, missing timestamps, active descendants, duplicate IDs,
   restored/rearchived chats, and paths outside the allowed root. This is a
   correctness comparison; fewer checks must not be rewarded as faster cleanup.
5. **Interruptions:** stop at each durable boundary, restart twice, and verify
   eligible work converges while every survivor's metadata and content remain
   valid. Restoration and write contention require separate coordinated cases.

**Best throughput baseline:** Janitor's `clean --mode delete`, using a fixture
where its selected files match ours. It does not maintain thread database state,
so report its smaller effect surface. **Second useful baseline:** History
Cleaner's `delete --no-backup --yes` for the same predetermined IDs, documenting
its additional index/SQLite/snapshot work and its lack of retention selection.
**Cliner:** useful for scan memory and UX comparison. Its prune command always
backs up, so compare its elapsed time as a different recovery mode, or include
equivalent backup work in our measured operation. Do not present a no-backup
speedup over Cliner as a pure implementation improvement.

Commands below are command templates for an already built, isolated setup;
`BENCH_ROOT`, `BENCH_HOME`, `CODEX_FIXTURE`, `JANITOR_DIR`, `CLINER_DIR`, and
`HISTORY_BIN` must point to test-only paths, and `FIXTURE_THREAD_ID` must be a
fixture ID. They were not executed as measurements
when this research document was written.

```sh
# Scan / preview candidates; warm filesystem cache, installed builds.
/opt/homebrew/bin/rtk proxy hyperfine --warmup 3 --runs 20 \
  --export-json "$BENCH_ROOT/janitor-scan.json" \
  "node '$JANITOR_DIR/dist/cli.js' scan --codex-home '$CODEX_FIXTURE' --retention-days 30"

/opt/homebrew/bin/rtk proxy hyperfine --warmup 3 --runs 20 \
  --export-json "$BENCH_ROOT/cliner-scan.json" \
  "env HOME='$BENCH_HOME' node '$CLINER_DIR/dist/cli.js' --codex-home '$CODEX_FIXTURE' scan"

# A fresh duplicate fixture must be prepared outside the timed region BEFORE
# EACH warmup and measured deletion, not just once before the full benchmark.
/opt/homebrew/bin/rtk proxy node "$JANITOR_DIR/dist/cli.js" clean \
  --codex-home "$CODEX_FIXTURE" --retention-days 30 --confirm --mode delete

# IDs must come from the shared fixture manifest, not a tool-specific selection.
/opt/homebrew/bin/rtk proxy "$HISTORY_BIN" --codex-home "$CODEX_FIXTURE" \
  delete "$FIXTURE_THREAD_ID" --no-backup --yes

# macOS process accounting, collected separately from hyperfine timings.
/opt/homebrew/bin/rtk proxy /usr/bin/time -l node "$JANITOR_DIR/dist/cli.js" \
  scan --codex-home "$CODEX_FIXTURE" --retention-days 30
```

Capture stdout/stderr consistently and validate selected IDs independently.
Use `hyperfine --prepare` or an equivalent harness to recreate deletion fixtures
before each iteration. Report preparation cost separately. Randomize command
order or run balanced blocks to reduce drift. Report warm-cache results as warm
cache; do not silently clear system caches or label first invocation as a
controlled cold-cache measurement.

Wall time, user/system CPU time, and peak RSS are distinct measurements.
`/usr/bin/time -l` block-I/O counters do not establish bytes read, especially with
filesystem cache. If permitted OS instrumentation cannot measure physical reads,
mark that metric unmeasured; a source estimate is not a physical-I/O result.
For a subprocess-based design, include child processes in memory/CPU accounting.
Report release artifact bytes, installed production files, and prerequisite
runtime footprint separately. Existing runtime reuse is a legitimate user case.

No throughput, memory, CPU, I/O, or installation-size superiority is claimed by
this source review. The clearest potential advantage is the complete automatic
archive-retention contract, provided the implementation proves its safety.

[janitor-roots]: https://github.com/zzy0222/codex-session-janitor/blob/32737c7cc68a74a63a21e5b8a403e92f4f1398e6/src/core/paths.ts#L8-L18
[janitor-scan]: https://github.com/zzy0222/codex-session-janitor/blob/32737c7cc68a74a63a21e5b8a403e92f4f1398e6/src/core/scan.ts#L9-L34
[janitor-plan]: https://github.com/zzy0222/codex-session-janitor/blob/32737c7cc68a74a63a21e5b8a403e92f4f1398e6/src/core/plan.ts#L5-L30
[janitor-clean]: https://github.com/zzy0222/codex-session-janitor/blob/32737c7cc68a74a63a21e5b8a403e92f4f1398e6/src/core/clean.ts#L7-L55
[janitor-cli]: https://github.com/zzy0222/codex-session-janitor/blob/32737c7cc68a74a63a21e5b8a403e92f4f1398e6/src/cli.ts#L44-L114
[janitor-startup]: https://github.com/zzy0222/codex-session-janitor/blob/32737c7cc68a74a63a21e5b8a403e92f4f1398e6/src/core/startup.ts#L10-L52
[janitor-package]: https://github.com/zzy0222/codex-session-janitor/blob/32737c7cc68a74a63a21e5b8a403e92f4f1398e6/package.json#L1-L39
[janitor-metadata]: https://github.com/zzy0222/codex-session-janitor/blob/32737c7cc68a74a63a21e5b8a403e92f4f1398e6/src/core/metadata.ts#L21-L49
[cliner-cli]: https://github.com/gabrielhamalwa/codex-cliner/blob/fdf092c762a2528b7ab653e1e71d586f2ffb4798/src/cli.ts#L17-L155
[cliner-scan]: https://github.com/gabrielhamalwa/codex-cliner/blob/fdf092c762a2528b7ab653e1e71d586f2ffb4798/src/inventory/scan.ts#L75-L130
[cliner-inventory]: https://github.com/gabrielhamalwa/codex-cliner/blob/fdf092c762a2528b7ab653e1e71d586f2ffb4798/src/inventory/scan.ts#L209-L354
[cliner-actions]: https://github.com/gabrielhamalwa/codex-cliner/blob/fdf092c762a2528b7ab653e1e71d586f2ffb4798/src/commands/actions.ts#L52-L97
[cliner-fs]: https://github.com/gabrielhamalwa/codex-cliner/blob/fdf092c762a2528b7ab653e1e71d586f2ffb4798/src/utils/fs.ts#L17-L54
[cliner-backups]: https://github.com/gabrielhamalwa/codex-cliner/blob/fdf092c762a2528b7ab653e1e71d586f2ffb4798/src/backups/manager.ts#L77-L180
[cliner-backup-ui]: https://github.com/gabrielhamalwa/codex-cliner/blob/fdf092c762a2528b7ab653e1e71d586f2ffb4798/src/ui/App.tsx#L550-L597
[cliner-package]: https://github.com/gabrielhamalwa/codex-cliner/blob/fdf092c762a2528b7ab653e1e71d586f2ffb4798/package.json#L1-L57
[history-cli]: https://github.com/LeeeeTX/codex-history-cleaner/blob/7058d580170c2d47fbc3aac2c4742da8df62f8d8/src/cli.rs#L9-L158
[history-store]: https://github.com/LeeeeTX/codex-history-cleaner/blob/7058d580170c2d47fbc3aac2c4742da8df62f8d8/src/store.rs#L23-L98
[history-delete]: https://github.com/LeeeeTX/codex-history-cleaner/blob/7058d580170c2d47fbc3aac2c4742da8df62f8d8/src/delete.rs#L50-L315
[history-delete-rows]: https://github.com/LeeeeTX/codex-history-cleaner/blob/7058d580170c2d47fbc3aac2c4742da8df62f8d8/src/delete.rs#L318-L324
[history-package]: https://github.com/LeeeeTX/codex-history-cleaner/blob/7058d580170c2d47fbc3aac2c4742da8df62f8d8/Cargo.toml#L1-L29
[history-install]: https://github.com/LeeeeTX/codex-history-cleaner/blob/7058d580170c2d47fbc3aac2c4742da8df62f8d8/scripts/install.sh#L1-L28
[hapwi-age]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/scripts/codex_cleaner.py#L913-L951
[hapwi-archive]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/scripts/codex_cleaner.py#L1230-L1299
[hapwi-cli]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/scripts/codex_cleaner.py#L1623-L1655
[hapwi-items]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/scripts/codex_cleaner.py#L206-L223
[hapwi-trash]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/scripts/codex_cleaner.py#L1428-L1518
[hapwi-backup]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/scripts/codex_cleaner.py#L352-L473
[hapwi-report]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/scripts/codex_cleaner.py#L1544-L1570
[hapwi-apply]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/scripts/codex_cleaner.py#L1573-L1620
[hapwi-package]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/package.json#L1-L28
[hapwi-runner]: https://github.com/hapwi/codex-cleaner/blob/7f271007c52df381e79ffcdbd5e529cef2e48341/bin/codex-cleaner.js#L401-L440
[windows-clean]: https://github.com/kylianrain7-gif/codex-archive-cleaner/blob/9d192556cb637d067db576efcb9af6dec52f852b/scripts/clear_archived_codex_sessions.ps1#L1-L75
[windows-restore]: https://github.com/kylianrain7-gif/codex-archive-cleaner/blob/9d192556cb637d067db576efcb9af6dec52f852b/scripts/restore_archived_codex_sessions.ps1#L145-L195
[windows-install]: https://github.com/kylianrain7-gif/codex-archive-cleaner/blob/9d192556cb637d067db576efcb9af6dec52f852b/README.md#L55-L98


## Executed retention-contract comparison

The [reproducible comparison script](../scripts/compare_semantics.py) ran final
Codex Retain and the pinned Janitor against separate, equivalent seven-chat
synthetic profiles. It used permanent removal in both tools, old file mtimes,
native-shaped SQLite transitions, and test-only aged capture epochs where
necessary. No real chats or background jobs were involved.

| Scenario | Required by this product | Retain | Janitor |
| --- | --- | --- | --- |
| Old conversation archived today | Preserve | Preserved | Removed |
| Old active conversation | Preserve | Preserved | Removed |
| Restored then rearchived between utility runs | Preserve | Preserved | Removed |
| Unknown archive timestamp | Preserve | Preserved | Removed |
| Current native pinned section, legacy pin bit zero | Preserve | Preserved | Removed |
| Explicit Retain exclusion | Preserve | Preserved | Removed |
| Known, unpinned, genuinely expired archive | Remove | Removed | Removed |

[Exact observations and executable identity](evidence/semantic-comparison.json).
This measures agreement with the requested archived-chat retention policy.
Janitor advertises a broader old-session cleanup purpose and has no equivalent
explicit-exclusion option; these are capability/contract differences, not a claim
that it violated its own documented contract. Native Codex trigger and lock
behavior was tested separately with its real app-server. Performance comparisons
use an all-expired archive subset so their selected file sets actually match.
