# Codex Retain

Keep local archived Codex chats for a chosen number of days, then remove eligible
archives automatically. A short hourly macOS job does the work; nothing stays
running between checks. No cloud service, subscription, account, or LLM is used
by the utility.

**The current development build has a deliberately narrow compatibility boundary:**
macOS and Codex CLI **0.153.4**, with the reviewed local `state_5.sqlite` schema and legacy JSONL or
zstd rollouts. Every Codex client writing the selected profile must use the
supported locking protocol. The version of a PATH CLI does **not** certify the
desktop app's embedded server. Paginated/shared histories and threads with
spawn relationships are skipped. [Compatibility evidence](docs/compatibility-research.md).

[Design](docs/architecture.md) ·
[Competitors](docs/competitors.md) · [Measurements](docs/performance.md)

Measured on macOS 26.4 / Apple M5: previewing 10,000 synthetic 4 KiB archives
took **521 ms median**, versus 1,368 ms for the pinned Janitor baseline. Recorded
maximum RSS was **50.56 MiB versus 180.14 MiB**. Cleanup was slower (19.35 s
versus 2.88 s median) because its effect scope and recovery protocol differ.
These are warm-cache local measurements, not a universal speed guarantee.
The [full report](docs/performance.md) includes all samples, failed targets and
unmeasured metrics; [validation](docs/validation.md) separates local proof from
untested platforms and publication.

## Install and enable

Clone the repository and install from source with the pinned Rust toolchain:

```sh
git clone https://github.com/Dankosik/codex-retain.git
cd codex-retain
cargo install --path . --locked
codex-retain enable --days 30 --yes
```

The native utility is one executable; Rust is needed only to build it. This
repository is currently published as **unreleased source**: no version tags,
GitHub Releases, registry packages, or deployments are published. The version
field required by Cargo identifies development builds and is not an official
release designation. [Local building and packaging](docs/releasing.md).

`enable --yes` is the one-time consent to permanent local deletion after the
selected retention period. It also installs a small, explicitly owned SQLite
transition recorder and an hourly per-user LaunchAgent. **All existing archives
receive a fresh full grace period. Nothing is deleted by enable.** The default
is 30 days; choose 7, 30, or any whole number from 1 through 36500.

For a particular installation, use explicit paths:

```sh
codex-retain doctor --codex-home /path/to/codex-home --codex-bin /path/to/codex
codex-retain enable --days 7 --codex-home /path/to/codex-home --codex-bin /path/to/codex --yes
```

The default profile is `$CODEX_HOME`, otherwise `~/.codex`. `doctor` checks the
selected binary and schema without enabling retention. Keep the configured
Codex executable, and its Node runtime if using the npm launcher, available.
The LaunchAgent captures the installation shell's PATH for that purpose.

## Everyday commands

| Action | Command |
| --- | --- |
| Inspect policy, compatibility, scheduling, last result | `codex-retain status` |
| Preview candidates and skip reasons | `codex-retain preview` |
| Perform one cleanup now | `codex-retain run` |
| Pause all deletion | `codex-retain pause` |
| Resume the same archive clock | `codex-retain resume` |
| Extend retention | `codex-retain policy --days 60` |
| Shorten retention deliberately | `codex-retain policy --days 7 --yes` |
| Protect an important chat | `codex-retain exclude THREAD_UUID` |
| Remove that protection deliberately | `codex-retain include THREAD_UUID --yes` |
| Disable scheduling and transition capture | `codex-retain disable` |
| Prepare for executable removal | `codex-retain uninstall` |
| JSON for scripts | `codex-retain --json preview` |
| Shell completion | `codex-retain completions zsh` |

Manual cleanup follows the same enabled policy and exclusions as automatic
cleanup. It does not bypass a pause or force-delete skipped chats. A preview is
a snapshot, not a stored deletion permission: the live row, pin, archive epoch,
file identity and locks are checked again when deleting.

Pausing preserves archive transition capture; elapsed archive time continues
to count. Consequently, resuming or removing a protection may make a chat due
immediately. Shortening retention and removing an explicit protection require
`--yes`. Ordinary runs after enable do not ask again.

For manual-only operation use `enable --no-schedule --yes`. Only one automatic
policy per macOS user is supported. Custom independent manual policies use
`--state-dir PATH`; policy storage and Codex home must be separate directories.

## Retention rules

* A newly archived old conversation receives the full retention period.
* Restoring a conversation removes its archive epoch. Archiving it again starts
  a new epoch, even if both actions happen between hourly utility runs.
* File creation time, last-message time and file modification time never decide
  expiration. If Codex repairs or changes its archive timestamp, the recorder
  conservatively starts a new full period.
* Native Codex pins, explicit exclusions, active threads, unknown archive state,
  absent capture, unsupported history, related threads and unsafe file paths
  prevent deletion. A busy writer or maintenance job is skipped for a later run.
* Unknown versions, schema changes, a replaced database or modified recorder
  stop cleanup. There is no unsafe compatibility override.
* A day means 86400 elapsed UTC seconds. Removal happens only after the whole
  period has elapsed, on the next successful check. Sleep, logout, pause and
  contention can delay cleanup; they cannot shorten the configured period.

The transition recorder is necessary because Codex 0.153.4 can reconstruct its
own `archived_at` field from file mtime. Merely querying that field does not
provide the promised clock. [Why this design](docs/architecture.md).

## Space and recovery

Eligible local rollout files and their selected SQLite thread rows are removed
permanently. The tool never recursively deletes a parent and its descendants.
There is no growing backup or Trash store. One durable intent and groups of at
most 32 temporarily staged rollouts allow interrupted operations to recover on the next
run. Unresolved recovery stops new deletion. Run `disable` before uninstalling
so it can finish or roll back any pending operation.

Reports distinguish:

* `logical_bytes_removed`: removed files' lengths on disk (compressed length for
  `.zst`, not the decompressed conversation size).
* `allocated_bytes_unlinked`: their allocated blocks, an estimate of attributable
  reclamation rather than a promise about physical storage.
* `observed_free_space_delta_bytes`: the observed change in free space on the
  volume. Other activity can make this negative or larger than the cleanup.
* `actual_reclaimed_bytes`: `null`; APFS clones, snapshots and concurrent activity
  prevent a reliable exact attribution.

This does **not** erase cloud history, global `history.jsonl` or
`session_index.jsonl`, logs, separate memory/queue databases, exports, snapshots,
or every forensic trace. SQLite reuses deleted pages; the tool does not VACUUM a
live Codex database. Some unlocked Codex metadata operations can republish a
rollout; newly published files are preserved and noticed when observed. This
limitation is recorded in the compatibility report.

## Quiet operation and removal

The LaunchAgent runs hourly in the user GUI domain with normal short-job
scheduling, no KeepAlive and no persistent process. Automatic stdout/stderr go to
`/dev/null`. One bounded `last-run.json` replaces its predecessor; detailed
interactive preview JSON is available on demand. `status` distinguishes policy
state, LaunchAgent registration, compatibility failure and pending recovery.
Registration alone is not evidence of a successful cleanup.

```sh
codex-retain uninstall
cargo uninstall codex-retain
```

`uninstall` disables the policy before attempting other cleanup, removes its
LaunchAgent and transition recorder, and retains the small policy/report for
inspection. If Codex data is unavailable, the command reports the incomplete
step; the policy is already disabled. Keep the executable until recovery and
recorder removal succeed. For a manually installed binary, remove that binary
after the first command. There is no hidden copied executable: removing the
scheduled binary directly leaves a stale launchd entry that cannot perform
cleanup. [Automation details](docs/automation.md).

To update the utility at the same path, run `cargo install --path . --locked
--force`; the policy and exclusions persist. Disable Retain before upgrading
Codex: future Codex migrations are not certified with this SQLite extension.
Unknown Codex versions suspend cleanup until a compatible adapter is available.
Do not edit the policy JSON to bypass
version or schema errors. Disable and explicitly re-enable when resetting a
repaired recorder; existing archives receive another full grace period.

## Development and evidence

```sh
make check
make maintenance-check
cargo build --release --locked
```

All deletion tests use synthetic profiles. [Development guide](docs/first-command.md)
documents fixtures and checks. See [measured results](docs/performance.md) before
making speed or memory claims; Rust alone is not a competitive advantage.

Initialized from [Dankosik/rust-cli-template](https://github.com/Dankosik/rust-cli-template).
The template's CLI/parser foundation, pinned toolchain, Rust methods, native
packaging and CI were retained and adapted; the example statistics command was
replaced. [MIT license](LICENSE) · [Third-party notices](THIRD_PARTY_NOTICES.md).
