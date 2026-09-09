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

> **Check compatibility before enabling.** The current adapter supports Codex CLI
> **0.153.4** local legacy history on macOS. An installed CLI does not establish
> compatibility with Codex Desktop's embedded server. Spawn-related and paginated
> histories are skipped. See [the full compatibility boundary](#compatibility).

## Quick start

The project is **unreleased**: install from source with Rust 1.98.1, pinned in
`rust-toolchain.toml`. Automatic cleanup requires a compatible Codex installation
and a macOS GUI login session.

```sh
git clone https://github.com/Dankosik/codex-retain.git
cd codex-retain
cargo install --path . --locked
codex-retain doctor
```

`doctor` checks the selected Codex executable and local database without enabling
retention. If it reports compatibility, choose your policy:

```sh
codex-retain enable --days 30 --yes
```

**Deletion is permanent.** `--yes` is your consent to remove eligible local
archives after the retention period, without asking on every run.
**Every existing archive receives a fresh full grace period. Nothing is deleted
when you enable the policy.**

Check what is configured and what will happen next:

```sh
codex-retain status
codex-retain preview
```

macOS now runs the policy hourly. For manual cleanup only, enable with
`--no-schedule --yes` and use `run` when needed. Both modes honor the same rules.

<details>
<summary>Choose a different Codex executable or profile</summary>

The default profile is `$CODEX_HOME`, otherwise `~/.codex`. The default executable
is `codex` on PATH. To select both explicitly:

```sh
codex-retain doctor --codex-home /path/to/codex-home --codex-bin /path/to/codex
codex-retain enable --days 7 --codex-home /path/to/codex-home --codex-bin /path/to/codex --yes
```

Keep that executable available. If you use Codex's npm launcher, keep its Node.js
runtime available too. The LaunchAgent captures your installation shell's PATH.
Codex Retain itself is one native executable; Rust is needed only to build it.

Use `--state-dir PATH` for a separate utility policy directory. It must be
separate from, and not nested inside, the Codex profile. One automatic policy per
macOS user is supported; independent custom policies can use manual-only mode.

</details>

## How archive retention works

| What happens | What Codex Retain does |
| --- | --- |
| You archive a conversation today | Gives it the full configured retention period, however old the conversation is |
| You restore it before cleanup | Removes it from the eligible archive |
| You archive it again | Starts a new retention period, even if both actions happened between cleanup runs |
| You enable retention on an existing archive | Gives all existing archived chats a new full grace period |
| You pin a chat in Codex or exclude its ID | Preserves it during automatic and manual cleanup |
| A chat is active, busy, ambiguous, or unsupported | Preserves it; reports a skip or stops the run |

A day means 86,400 elapsed UTC seconds. Deletion is permitted only after the full
period has passed, on the next successful check. Sleep, logout, contention, or a
paused policy may delay cleanup.

A small SQLite extension records archive transitions in the same transaction
as Codex changes their state, including between utility runs. Neither file
modification time nor the last message date determines expiration. If Codex
repairs its archive timestamp, Retain conservatively starts a new full period.

The system clock must be correct. Recognized clock inconsistencies stop cleanup;
arbitrary forward adjustments cannot be independently detected.
[Retention design and failure model](docs/architecture.md).

## Everyday commands

| What you want to do | Command |
| --- | --- |
| Check policy, scheduler, compatibility, and last result | `codex-retain status` |
| Preview candidate chats and reasons for skips | `codex-retain preview` |
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

Exit codes are `0` for a completed command, including ordinary policy skips;
`1` for an operational failure; `2` for invalid CLI usage; and `3` for cleanup
with artifact errors or recovery warnings. `--help`, `--version`, and completions
work without loading Codex data or a policy.

See the [CLI definition](src/cli.rs) and [validation record](docs/validation.md)
for the contract. No agent framework, MCP server, API key, or model call is required.

## What gets deleted, and what stays

Codex Retain removes eligible local rollout files, their selected SQLite thread
rows, and the reviewed metadata those rows own. It never recursively deletes
a parent conversation and its descendants.

Cloud history, global `history.jsonl` and `session_index.jsonl`, logs, separate
memory and queue databases, exports, and disk snapshots remain. Cleanup does not
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
| macOS with Codex CLI 0.153.4 | Supported adapter; native conformance tested on macOS 26.4 ARM64 |
| Local legacy JSONL and zstd rollouts in `state_5.sqlite` | Supported within the reviewed schema |
| Codex Desktop's embedded server | Not certified by the version of a separate installed CLI; every writer must use the supported protocol |
| Threads with spawn relationships, including parents and children | Skipped |
| Paginated or shared histories | Skipped |
| Linux | Experimental core, doctor, and preview paths; destructive commands and scheduling disabled |
| Windows | Not supported |

Unknown Codex versions, schema changes, a replaced database, or a missing or
modified transition recorder stop cleanup. There is no unsafe override.
This is why some archived chats may remain after a run. See
[the source review and real Codex tests](docs/compatibility-research.md).

There are no version tags, GitHub Releases, registry packages, or deployments.
Cargo's version field identifies development builds, not an official release.

## Measured performance

On an Apple M5 with 16 GiB RAM and macOS 26.4, using 10,000 synthetic archived
chats of 4 KiB each:

| Measurement | Codex Retain | codex-session-janitor |
| --- | --- | --- |
| Preview, median | **276 ms** | 797 ms |
| Recorded maximum RSS during preview | **50.56 MiB** | 187.78 MiB |
| Permanent cleanup, median | 7.76 s | **3.12 s** |

Preview was 2.89 times faster, with a 3.71 times lower recorded RSS. Cleanup
remains slower: Retain rechecks eligibility, updates SQLite, and synchronizes
its recovery journal; the compared Janitor mode deletes files and leaves SQLite
rows. Neither measured mode makes a backup or uses Trash.

Profiling led to buffered JSON writes, larger bounded deletion groups, and
shorter global coordination. In a comparison against the previous Retain build,
10,000-chat cleanup fell from **14.35 to 7.76 seconds**, a **1.85x speedup**.
Archive timing, pin protection, and recovery checks remain in effect.

The updated comparison used a pinned Janitor revision, five timed runs and three
warmups. Cleanup regenerated equivalent fixtures before each run; read-only
preview reused one verified, unchanged fixture. These are warm-cache measurements
on a shared desktop. RSS is one separate native accounting observation per case,
not a measured peak for the combined process tree. The Retain executable was
3.83 MiB. [Current samples, causes, tradeoffs, and the missed 2x cleanup target](docs/cleanup-performance.md).
The [original ten-run benchmark](docs/performance.md) remains available separately.

We also [reviewed five existing Codex cleanup tools](docs/competitors.md) and
[compared seven retention scenarios](docs/evidence/semantic-comparison.json).
The differences concern their actual cleanup rules and effects, not just the
language they are written in.

## Update or uninstall

To update from this checkout, replace the executable at its existing path:

```sh
git pull --ff-only
cargo install --path . --locked --force
```

Your policy and exclusions persist. If you move the executable or its Node.js
launcher runtime, disable the schedule and re-enable from the new location.
**Disable Retain before upgrading Codex** so the next storage version can be
reviewed before the SQLite extension is used with it.

Before downgrading Retain to an older build with 32-chat groups, finish pending
recovery or disable using the newer executable. Older builds safely reject
pending journals containing more than 32 chats and cannot recover them.

To remove the utility:

```sh
codex-retain uninstall
cargo uninstall codex-retain
```

`uninstall` disables deletion, removes the schedule and transition recorder, and
handles pending recovery. The small policy and report remain for inspection. If
Codex data is unavailable, the policy is already disabled; keep the executable
until the reported unfinished step succeeds. Remove a manually installed binary
after `uninstall` succeeds.

Deleting the binary directly cannot run an uninstall hook. It can leave a stale
launchd entry, but no hidden copy exists to keep cleaning chats. Reinstall at
the same path and run `uninstall` to remove that entry.

## Development and help

```sh
make check
make maintenance-check
cargo build --release --locked
```

The [validation record](docs/validation.md) covers 74 Rust tests, native Codex
conformance, interruption recovery, and a temporary launchd integration test.
All destructive tests use synthetic profiles.

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
