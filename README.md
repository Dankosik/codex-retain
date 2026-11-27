# Codex Retain: automatic cleanup for archived Codex chats

Keep finished conversations for as long as you need them. Let the rest expire.

Codex Retain is an open-source CLI that automatically deletes eligible **local
OpenAI Codex chats after a configurable time in the archive**. Set a retention
period, protect important conversations, and let macOS run the cleanup each hour.

[![CI](https://github.com/Dankosik/codex-retain/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/Dankosik/codex-retain/actions/workflows/ci.yml)
[MIT license](LICENSE) · [Quick start](#quick-start) · [Commands](#everyday-commands) ·
[Compatibility](#compatibility) · [Benchmarks](#measured-performance)

## Why archived Codex chats need a retention policy

You finish a task, archive the conversation, and move on. The chat leaves your
active list, but its local history stays on disk. Repeat that over time and the
archive keeps growing, even when your working list looks tidy.

You may still need last week's context without keeping every conversation
indefinitely. Codex Retain lets you keep archived chats for 7 days, 30 days, or
another period that suits your work. It counts time **from archiving**, so an
old conversation archived today gets the full retention period. Restore it,
and it stops being a candidate. Archive it again, and the clock starts over.

You can preview deletions, protect specific chats, or pause the policy. Once
enabled, ordinary runs need no further confirmation. The utility needs no cloud
service, subscription, account, or LLM, and leaves no process running between checks.

> **Universal client and version compatibility.** Codex Retain validates the
> local `$CODEX_HOME` storage contract directly instead of pinning one exact
> Codex version or full database schema checksum. On macOS it works across
> **Codex CLI, Codex Desktop (`/Applications/Codex.app`), IDE extensions, and
> mixed setups**, and adapts across older, current, and future `state_<N>.sqlite`
> databases as long as the core archive contract holds. It writes no private
> tables or triggers into Codex's SQLite database. Cleanup removes eligible
> dependent chats before their parents or shared history; surviving dependents
> keep those sources protected. See [the full compatibility boundary](#compatibility).

## Quick start

Install the latest release on **macOS 15+ (Apple Silicon or Intel)**. Rust is
not required. Automatic cleanup also requires a compatible Codex installation
and a macOS GUI login session.

```sh
curl -fsSL https://github.com/Dankosik/codex-retain/releases/latest/download/install.sh -o /tmp/codex-retain-install.sh
sh /tmp/codex-retain-install.sh
export PATH="$HOME/.local/bin:$PATH"
codex-retain doctor
```

The installer checks SHA-256 and installs to `~/.local/bin`. Add that directory
to your shell's PATH permanently if needed. Installation never enables retention.
For a specific release, use `CODEX_RETAIN_VERSION=0.2.0 sh /tmp/codex-retain-install.sh`.
Set `CODEX_RETAIN_INSTALL_DIR` to choose a different absolute installation directory.

### Homebrew

```sh
brew tap dankosik/codex-retain https://github.com/Dankosik/codex-retain
brew install dankosik/codex-retain/codex-retain
codex-retain doctor
```

If Homebrew reports an untrusted formula, run
`brew trust --formula dankosik/codex-retain/codex-retain`, then repeat the tap
and install commands. This trust step applies to Homebrew versions that require
explicit approval for third-party formulae.

This tap installs the same prebuilt binaries and shell completions. Enable using
`codex-retain` on PATH, so the schedule retains Homebrew's stable link.

### Cargo or manual download

To compile the tagged source, install Rust 1.98.1 and a C toolchain, then run:

```sh
cargo install --git https://github.com/Dankosik/codex-retain --tag 0.2.0 --locked codex-retain
```

Or download the matching archive and `SHA256SUMS` from
[GitHub Releases](https://github.com/Dankosik/codex-retain/releases/latest).
See [manual installation and verification](docs/releasing.md#manual-installation).
There is no crates.io package; use the explicit Git URL for Cargo.

### Enable a policy

`doctor` checks the local Codex storage database, Desktop global state (if
present), and installed local Codex clients without enabling retention. If it
reports compatibility, choose your policy:

```sh
codex-retain enable --days 30 --yes
```

**Deletion is permanent.** `--yes` is your consent to remove eligible local
archives after the retention period, without asking on every run.
**The first cleanup happens during `enable`. Existing chats recorded as archived
more than 30 days ago can be deleted immediately.** Recent archives keep the
remaining part of their retention period; unknown or invalid dates stay protected.

Check what is configured and what will happen next:

```sh
codex-retain status
codex-retain preview
```

After this initial cleanup, macOS checks the policy hourly. For manual cleanup
only, enable with `--no-schedule --yes`: the first cleanup still runs immediately,
and later passes use `run`. Both modes honor the same rules. Initial results
appear in the command output and the retained last-run report. Exit code 3 means
some archives need attention; the policy remains enabled for later retries.

<details>
<summary>Choose a different Codex profile or diagnostic executable</summary>

The default profile is `$CODEX_HOME`, otherwise `~/.codex`. No `codex` binary on
`PATH` is required — Desktop-only and IDE-only setups work directly against
`$CODEX_HOME`. To specify a profile path (and optionally a Codex executable for
`doctor`/`status` version diagnostics):

```sh
codex-retain doctor --codex-home /path/to/codex-home
codex-retain enable --days 7 --codex-home /path/to/codex-home --yes
```

Pass `--codex-bin /path/to/codex` only if you want `doctor`/`status` to also
report a specific CLI or Desktop binary's version. Scheduled runs never spawn
`codex` or require Node.js. Codex Retain itself is one native executable; Rust
is needed only to build it.

Use `--state-dir PATH` for a separate utility policy directory. It must be
separate from, and not nested inside, the Codex profile. One automatic policy per
macOS user is supported; independent custom policies can use manual-only mode.

</details>

## Update or uninstall

For an installer-based installation, rerun the installation command to get the
latest release. Use the same `CODEX_RETAIN_INSTALL_DIR` if you customized it.
The executable is replaced at the same path; your policy and schedule remain.
For Homebrew:

```sh
brew update
brew upgrade dankosik/codex-retain/codex-retain
```

For Cargo, repeat the install command with the new release tag and `--force`.
Check `codex-retain --version` after updating. There is no background network
updater. Avoid mixing installation methods: disable before moving to a different
installation path, then enable from the new path.

Before removing the executable, run `codex-retain uninstall` to remove its
schedule and disable the policy. Then use `brew uninstall codex-retain`,
`cargo uninstall codex-retain`, or remove `~/.local/bin/codex-retain`, according
to how you installed it. Uninstalling does not delete additional chats.

## How archive retention works

| What happens | What Codex Retain does |
| --- | --- |
| You archive a conversation today | Gives it the full configured retention period, however old the conversation is |
| You restore it before cleanup | Removes it from the eligible archive |
| You archive it again | Starts a new retention period, even if both actions happened between cleanup runs |
| You enable retention on an existing archive | Uses Codex's recorded archive dates and immediately cleans eligible old archives |
| You pin a chat in Codex CLI/Desktop or exclude its ID | Preserves it during automatic and manual cleanup |
| A chat is active, busy, ambiguous, or unsupported | Preserves it; reports a skip or stops the run |

A day means 86,400 elapsed UTC seconds. Deletion is permitted only after the full
period has passed, on the next successful check. Sleep, logout, contention, or a
paused policy may delay cleanup.

Retain tracks archive epochs in its own sidecar SQLite database
(`tracker.sqlite` inside its state directory) rather than injecting private
tables or triggers into Codex's `state_<N>.sqlite`. Neither file modification
time nor the last message date determines expiration. Each sync (including quiet
hourly syncs while paused) reconciles Codex's current `archived` and
`archived_at` state: newly archived chats, restore-and-rearchive transitions
with an updated `archived_at`, and timestamp repairs (`archived_at` changed to
an older value) conservatively start a fresh full period. Codex Desktop pins in
`$CODEX_HOME/.codex-global-state.json`, SQLite pin flags/sections, and future
protective columns are all checked before deletion.

At first activation, Retain accepts Codex's existing `archived_at` as the archive
age. Codex may have reconstructed that value during metadata repair; it has no
separate provenance marker. This onboarding policy trusts the recorded date and
does not prove the chat was continuously archived before Retain was installed.
Retain never substitutes chat creation time, last-message time or file mtime for
a missing archive date. After activation, observed transitions and timestamp
repairs continue to start a new period.

The system clock must be correct. Recognized clock inconsistencies stop cleanup;
arbitrary forward adjustments cannot be independently detected.
[Retention design and failure model](docs/architecture.md).

## Everyday commands

| What you want to do | Command |
| --- | --- |
| Check policy, scheduler, compatibility, and last result | `codex-retain status` |
| Preview candidate chats and reasons for skips | `codex-retain preview` |
| Forecast the current archive at a future UTC time | `codex-retain preview --at 2027-01-01T00:00:00Z` |
| Run cleanup once under the enabled policy | `codex-retain run` |
| Pause all deletion | `codex-retain pause` |
| Resume the same policy | `codex-retain resume` |
| Keep archives for longer | `codex-retain policy --days 60` |
| Deliberately shorten retention | `codex-retain policy --days 7 --yes` |
| Protect a chat using its ID from preview | `codex-retain exclude THREAD_UUID` |
| Remove an explicit protection | `codex-retain include THREAD_UUID --yes` |
| Disable cleanup and remove its schedule | `codex-retain disable` |
| Generate shell completions | `codex-retain completions zsh` |

Pausing stops deletion while archive time continues to count. Resuming,
shortening retention, or removing a protection can therefore make a chat due
immediately. Shortening the period and removing an exclusion require `--yes`.
Manual `run` never bypasses a pause or force-deletes a skipped chat.

`preview --at` evaluates the current profile at a future RFC3339 timestamp. It
does not advance capture epochs, change the policy, or delete files. The forecast
shows what would qualify if the observed archive and dependencies stayed the
same; new children, pins, restores, and busy writers can change the eventual
result. Only `preview` accepts `--at`.

`status` reports policy, actual scheduler registration, and the last result.
A registered job does not prove a successful cleanup. Scheduled runs stay quiet
and replace one bounded `last-run.json`; stdout/stderr logs do not grow.
[Scheduling and troubleshooting](docs/automation.md).

## For scripts and coding agents

Use JSON output to inspect the policy or archive without parsing terminal text:

```sh
codex-retain --json status
codex-retain --json preview
```

JSON contains candidate IDs, eligibility times, skip reasons, counts, and space
measurements. `--json run` applies the enabled policy with the same checks as
text mode. Preview is a snapshot; deletion always rechecks eligibility.
Forecast JSON adds `evaluated_at` for the requested retention time while
`started_at` remains the actual observation time. Normal preview omits
`evaluated_at`.

Exit codes are `0` for a completed command, including ordinary policy skips;
`1` for an operational failure; `2` for invalid CLI usage; and `3` for cleanup
with artifact errors or recovery warnings. `--help`, `--version`, and completions
work without loading Codex data or a policy.

See the [CLI definition](src/cli.rs) and [validation record](docs/validation.md)
for the contract. No agent framework, MCP server, API key, or model call is required.

## What gets deleted, and what stays

Codex Retain removes eligible local rollout files, their selected SQLite thread
rows, incident spawn edges, and reviewed metadata those rows own. Each chat must
qualify independently. A fully expired family can finish in one run, with
children removed before parents; an active, protected, or otherwise surviving
child keeps its ancestors. Shared paginated history follows the same rule.

An archived database row is authoritative for archive state. Its validated owned
copies can exist under either `sessions` or `archived_sessions`, in plain or
compressed form. Cleanup checks and removes those copies together; directory
placement alone does not make an active chat eligible. See
[related-thread cleanup](docs/related-support.md).

Cloud history, global `history.jsonl` and `session_index.jsonl`, logs, separate
memory and queue databases, paginated projection caches (`thread_history_1.sqlite`),
exports, and disk snapshots remain. Cleanup does not
erase every conversation trace or run `VACUUM` against a live SQLite profile.

The utility keeps no growing Trash or backup archive. A durable journal covers
at most 128 temporarily staged rollouts; interrupted operations recover before
new deletion. Unresolved recovery stops the run. Some Codex metadata operations
can republish a rollout outside the shared locks; Retain preserves those new
objects and reports them when observed. [Coordination limits](docs/architecture.md#limits-of-coordination).

Space reports keep different measurements separate:

| JSON field | Meaning |
| --- | --- |
| `logical_bytes_removed` | Removed files' lengths on disk, using compressed length for `.zst` files |
| `allocated_bytes_unlinked` | Allocated file blocks unlinked, an estimate rather than exact physical reclamation |
| `observed_free_space_delta_bytes` | The volume's observed free-space change, including other concurrent activity |
| `actual_reclaimed_bytes` | `null`, because APFS snapshots, shared blocks, and open handles prevent exact attribution |

## Compatibility

| Environment or history type | Current status |
| --- | --- |
| macOS with Codex CLI, Codex Desktop (`/Applications/Codex.app`), IDE extensions, or mixed clients | Supported via capability-based `$CODEX_HOME` storage contract; no `codex` binary on `PATH` required |
| Older, current, and future `state_<N>.sqlite` databases | Automatically selects highest numeric `state_<N>.sqlite` and adapts to optional columns/tables (`history_mode`, `is_pinned`, `thread_section_id`, `thread_sections`, `thread_spawn_edges`, `name`, `title`, `backfill_state`, `_sqlx_migrations`) |
| Local legacy and paginated JSONL/zstd rollouts | All owned physical copies under `sessions` and `archived_sessions` are checked; unknown per-thread `history_mode` values are skipped |
| Codex Desktop pins & future pin/FK safeguards | Protects Desktop pins in `.codex-global-state.json`, dynamic `"Pinned"` sections, unknown protective columns (e.g. `pinned_at`, `is_starred`), and child tables with `RESTRICT`/`NO ACTION` foreign keys |
| Threads with spawn relationships | Eligible children can be removed; surviving descendants preserve ancestors |
| Linux | Experimental core, doctor, and preview paths; destructive commands and scheduling disabled |
| Windows | Not supported |

Retain fails closed whenever core safety invariants cannot be proven: missing
required `threads` columns (`id` PK, `rollout_path`, `archived`, `archived_at`),
failed SQLx migrations (`success = 0` or `NULL`), incomplete `backfill_state`,
self-referential `ON DELETE CASCADE` on `threads`, unreadable/corrupt/symlinked
`.codex-global-state.json`, or a missing/tampered `tracker.sqlite`. There is no
unsafe override. See [the source review and real Codex tests](docs/compatibility-research.md).

Release binaries target macOS 15 and later on Apple Silicon and Intel. Binary
startup is tested separately on each architecture; this does not extend the
Codex adapter compatibility boundary above. Builds are not Developer ID signed
or notarized. Browser downloads may require approval in macOS Privacy & Security.

## Measured performance

Version 0.1.5 reduces peak memory footprint and CPU cost across large archive
evaluations, previews, scheduled runs, and cleanups without altering policy,
durability, schema, or output contracts:

- Compact slab-backed report storage (`CompactEntries`), direct streaming JSON
  serialization without intermediate DOM trees, and bounded text/scheduled
  materialization.
- Decoupled SQLite archive scans (`visit_archived_compact`), due-count
  pre-sizing, reusable prepared batch statements, and bounded SQLite page
  caches with explicit `PRAGMA shrink_memory` before inventory peaks.
- Direct descriptor-based traversal (`rustix::fs::Dir` and `openat`/`statat`/
  `unlinkat`/`renameat_with`), compact 48-byte `HeaderCache` slabs, inline
  rollout filename packing in `OwnedRollout`, reusable `zstd` decoders, SIMD
  newline/NUL scanning (`memchr`) and UTF-8 validation (`simdutf8`), `itoa`
  integer formatting, `smallvec` batch scratch, and non-cryptographic internal
  hash maps seeded per process.

Paired local release measurements against `0.1.4` on ARM64 macOS using synthetic
profiles (`/usr/bin/time -l`, local Rust 1.98.1 release builds):

| Synthetic workload | 0.1.4 peak footprint | 0.1.5 peak footprint | 0.1.4 user CPU | 0.1.5 user CPU |
| --- | ---: | ---: | ---: | ---: |
| `enable --json` (100,000 archives, none due) | 111.05 MiB | **9.30 MiB (−91.6%)** | 0.28 s | **0.17 s** |
| `preview --json` (100,000 archives, none due) | 26.48 MiB | **7.00 MiB (−73.6%)** | 0.16 s | **0.07 s** |
| `preview` text (100,000 archives, none due) | 26.45 MiB | **2.16 MiB (−91.8%)** | 0.14 s | **< 0.01 s** |
| `run --scheduled` (100,000 archives, none due) | 4.42 MiB | **2.41 MiB (−45.5%)** | 0.14 s | **< 0.01 s** |
| `run --json` (delete 10,000 legacy archives) | 29.00 MiB | **7.94 MiB (−72.6%)** | 1.22 s | **0.32 s** |
| `run --scheduled` (delete 10,000 legacy archives) | 27.50 MiB | **7.80 MiB (−71.6%)** | 0.86 s | **0.34 s** |
| `preview --json` (99k active + 1k archived, 1 due) | 11.11 MiB | **3.83 MiB (−65.5%)** | 1.27 s | **0.41 s** |
| `run --json` (99k active + 1k archived, 1 due) | 50.25 MiB | **8.05 MiB (−84.0%)** | 0.77 s | **0.23 s** |

Release binary size on `aarch64-apple-darwin` also dropped from 4.43 MiB to
3.54 MiB (−20.1%) with full symbol stripping (`strip = true`) and removal of
`walkdir`. These local synthetic benchmarks on a shared desktop do not establish
cold-cache I/O latency, Intel timings, or live-writer contention.

Earlier 0.1.3 run-local header reuse and timestamp-freshness rules remain
unchanged: every group still discovers files freshly, rechecks dependencies,
eligibility, ownership and writer locks, and refuses to cache recently modified
files until both change and modification times precede the previous whole
wall-clock second.

[0.1.3 comparison and reproduction](docs/performance-0.1.3.md) ·
[0.1.3 samples and RSS](docs/evidence/performance-0.1.3/comparison.json) ·
[0.1.3 package verification](docs/release-0.1.3-validation.md) ·
[Earlier candidate and CI correction](docs/inventory-performance-2026-09-10.md)

We also [reviewed five existing Codex cleanup tools](docs/competitors.md) and
[compared seven retention scenarios](docs/evidence/semantic-comparison.json).
The differences concern their actual cleanup rules and effects.

## Recovery and compatibility changes

Because archive epochs now live in Retain's external `tracker.sqlite` with **zero
tables or triggers injected into Codex's database**, ordinary Codex upgrades
(including additive columns/tables and `state_5.sqlite -> state_6.sqlite`
migrations) no longer require disabling Retain first. Breaking future schemas
still fail closed without modifying data. Your policy and exclusions persist
through ordinary Retain updates at the same installation path.

Upgrading from `0.1.x` policies that used legacy `codex_retain_*` SQLite tables
and triggers inside `state_5.sqlite` is seamless: `status` and `preview` read
legacy epochs without mutating either database, and the next `run` migrates
those epochs into `tracker.sqlite` and cleans the legacy triggers/tables out of
Codex's database.

Before downgrading Retain, finish pending recovery or disable using the newer
executable. Versions 0.1.2+ write schema 4 journals with a separate slot for
every physical file. Earlier versions cannot recover that format or read
`tracker.sqlite`; older 32-chat builds also reject larger groups. The current
reader accepts journal schemas 1–4. The JSON response to `enable` is schema 2:
`existing_archives_assessed`, `initial_archive_clock` and `initial_cleanup`.

Policies enabled by 0.1.3 or earlier keep their captured periods when the binary
is updated. To explicitly adopt existing Codex archive dates, use the new binary
to `disable`, then `enable --days 30 --yes` with your original profile and
scheduling options. Exclusions are preserved. This reactivation can delete old
eligible archives immediately; an ordinary `run` never silently rewrites a
previous policy's recorded periods.

`uninstall` handles pending recovery as well as disabling deletion and removing
the schedule and transition recorder. The small policy and report remain for
inspection. If Codex data is unavailable, the policy is already disabled;
keep the executable until the reported unfinished step succeeds.

Deleting the binary directly cannot run an uninstall hook. It can leave a stale
launchd entry, but no hidden copy exists to keep cleaning chats. Reinstall at
the same path and run `uninstall` to remove that entry. See
[update and uninstall commands](#update-or-uninstall).

## Development and help

```sh
make verify
cargo build --release --locked
```

The [latest optimization validation](docs/performance-0.1.3.md)
covers file-cache invalidation, fresh orphan dependencies, recovery and native
Codex conformance. CI runs the Rust and maintenance checks on the declared
toolchain; release jobs additionally test and package each macOS architecture
on its native runner. The [validation record](docs/validation.md) preserves
earlier compatibility and scheduler evidence. All destructive tests use
synthetic profiles.

For setup problems or feature requests, [open an issue](https://github.com/Dankosik/codex-retain/issues).
Include your platform, tool versions, and relevant skip or error message; remove
private chat contents. For security issues, follow [SECURITY.md](SECURITY.md).

Read the [contributing guide](CONTRIBUTING.md),
[development guide](docs/first-command.md), or
[local packaging instructions](docs/releasing.md) to work on the project.

## License and origin

Codex Retain uses the [MIT license](LICENSE) and was built from
[Dankosik/rust-cli-template](https://github.com/Dankosik/rust-cli-template).
[Template provenance](docs/template-origin.md) and
[third-party notices](THIRD_PARTY_NOTICES.md) record the source and dependency
attributions.
