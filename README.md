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
> **0.153.4** local history on macOS. Native conformance tests cover both the
> CLI and the Desktop-embedded 0.153.4 executable; a different Desktop build
> still needs its own compatibility check. Cleanup removes eligible dependent
> chats before their parents or shared history. Surviving dependents keep those
> sources protected. See [the full compatibility boundary](#compatibility).

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
For a specific release, use `CODEX_RETAIN_VERSION=0.1.3 sh /tmp/codex-retain-install.sh`.
Set `CODEX_RETAIN_INSTALL_DIR` to choose a different absolute installation directory.

### Homebrew

```sh
brew tap dankosik/codex-retain https://github.com/Dankosik/codex-retain
brew install dankosik/codex-retain/codex-retain
codex-retain doctor
```

This tap installs the same prebuilt binaries and shell completions. Enable using
`codex-retain` on PATH, so the schedule retains Homebrew's stable link.

### Cargo or manual download

To compile the tagged source, install Rust 1.98.1 and a C toolchain, then run:

```sh
cargo install --git https://github.com/Dankosik/codex-retain --tag 0.1.3 --locked codex-retain
```

Or download the matching archive and `SHA256SUMS` from
[GitHub Releases](https://github.com/Dankosik/codex-retain/releases/latest).
See [manual installation and verification](docs/releasing.md#manual-installation).
There is no crates.io package; use the explicit Git URL for Cargo.

### Enable a policy

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
| macOS with Codex CLI 0.153.4 | Supported adapter; native conformance tested on macOS 26.4 ARM64 |
| Local legacy JSONL and zstd rollouts in `state_5.sqlite` | Supported within the reviewed schema |
| Codex Desktop-embedded 0.153.4 | Native paginated and organizational conformance tested on macOS 26.4 ARM64; other embedded versions are not covered |
| Threads with spawn relationships (0.1.2+) | Eligible children can be removed; surviving descendants preserve ancestors |
| Paginated JSONL/zstd (0.1.1+) | All owned physical copies are checked; surviving references preserve their sources |
| Linux | Experimental core, doctor, and preview paths; destructive commands and scheduling disabled |
| Windows | Not supported |

Unknown Codex versions, schema changes, a replaced database, or a missing or
modified transition recorder stop cleanup. There is no unsafe override.
This is why some archived chats may remain after a run. See
[the source review and real Codex tests](docs/compatibility-research.md).

Release binaries target macOS 15 and later on Apple Silicon and Intel. Binary
startup is tested separately on each architecture; this does not extend the
Codex adapter compatibility boundary above. Builds are not Developer ID signed
or notarized. Browser downloads may require approval in macOS Privacy & Security.

## Measured performance

Version 0.1.3 reuses unchanged rollout headers between deletion groups and
removes a duplicate directory sync. Every group still discovers files and
rechecks dependencies, eligibility, ownership and writer locks. The cache lasts
for one run; it does not store deletion permission or persist across invocations.

The September 10, 2026 comparison used installed **0.1.2** and the optimization
candidate on ARM64 macOS 26.4. Each pair shared a synthetic profile and policy,
with three warmups and five timed runs. Fixture restoration and result checks
were outside the timer. These are warm-cache results on a shared desktop,
measured before the candidate's version was bumped to 0.1.3; they are not timings
of the subsequently published archives.

| Workload | 0.1.2 median | Optimization candidate median |
| --- | ---: | ---: |
| Delete 1,000 independent paginated archives | 1.369 s | 1.492 s |
| Delete 1,000 legacy archives | 557 ms | 537 ms |
| Delete 10,000 legacy archives | 12.799 s | **7.589 s (41% less time)** |
| Scheduled check: 100,000 archives, none due | 375 ms | 358 ms |
| Preview: 99,000 active + 1,000 unexpired archives | 179 ms | 148 ms |
| Preview: the same profile, one archive due | 13.561 s | 14.138 s |

The large cleanup improvement is supported by non-overlapping ranges:
12.351–14.504 s versus 7.260–7.858 s. Small workloads remain noisy. An additional
fixed series of five alternating paginated pairs gave medians of 911 ms versus
884 ms, while the candidate's mean remained higher. A small paginated regression
cannot be ruled out; a speedup for that workload is not established.

The first full header inventory remains expensive. Preview does not use the
run cache, and its 100,000-file case has no demonstrated improvement. The
scheduled no-candidate path remains short and skips that inventory entirely.
For the 10,000-file cleanup, separately sampled process RSS was 26.1 MiB versus
31.6 MiB; samples can miss the peak and exclude child processes. The header
cache trades memory proportional to the file inventory for fewer repeated reads.

All 70 timed runs passed their result checks. Native Codex conformance also
covers paginated fork/revert, retained history, organizational dependencies and
writer contention. These synthetic checks do not establish cold-cache behavior,
live-writer latency or performance on another host.

[Full comparison, limits and reproduction](docs/inventory-performance-2026-09-10.md) ·
[All main-series samples and RSS](docs/evidence/inventory-optimization-2026-09-10/comparison.json) ·
[Alternating paginated pairs](docs/evidence/inventory-optimization-2026-09-10/paginated-interleaved/comparison.json) ·
[Earlier narrower legacy measurements](docs/performance-implementation-2026-09-10.md)

We also [reviewed five existing Codex cleanup tools](docs/competitors.md) and
[compared seven retention scenarios](docs/evidence/semantic-comparison.json).
The differences concern their actual cleanup rules and effects.

## Recovery and compatibility changes

**Disable Retain before upgrading Codex** so the next storage version can be
reviewed before the SQLite extension is used with it. Your policy and exclusions
persist through ordinary Retain updates at the same installation path.

Before downgrading Retain, finish pending recovery or disable using the newer
executable. Versions 0.1.2 and 0.1.3 write schema 4 journals with a separate slot
for every physical file. Earlier versions cannot recover that format; older
32-chat builds also reject larger groups. The current reader accepts journal
schemas 1–4. Version 0.1.3 does not change the policy, capture or journal format.

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

The [latest optimization validation](docs/inventory-performance-2026-09-10.md)
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
