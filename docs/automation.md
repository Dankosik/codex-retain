# Automatic cleanup on macOS

The macOS scheduler is a per-user LaunchAgent named
`io.github.codex-retain`. It requests one cleanup run every hour while your GUI
login session is available. It does not run a resident utility process between
checks. `launchd`, which already belongs to macOS, holds the schedule.

The retention deadline is the earliest time deletion is permitted, not a promise
to delete at that exact second. Sleep, logout, a busy database, compatibility
checks, and skipped candidates can delay cleanup. No run starts immediately just
because the LaunchAgent was loaded. There is no `KeepAlive`, shell wrapper,
copied hidden binary, or continuously running watcher.

## Installation and enabling

Install the normal `codex-retain` executable into a stable location using the
installation instructions in the [README](../README.md). Review the retention
rules and run doctor before enabling; preview becomes available after activation.
The enable command saves the policy
and registers the schedule; ordinary scheduled runs need no confirmation.

The registration stores the absolute path of the executable that enabled it and
the absolute state directory. It also captures the current `PATH`, so a Codex
installation whose launcher uses `/usr/bin/env node` can find the same Node.js
runtime during scheduled runs. The configured Codex executable and its runtime
must remain available; the utility itself has no Node.js runtime requirement.
Avoid enabling from a temporary Cargo build
directory. `cargo install` supplies a stable executable path. One user has one
LaunchAgent for this utility. Enabling with a different state directory is
rejected while the original schedule exists; disable it using its original
state directory first. Unknown or manually reformatted plists are also rejected
because their ownership cannot be established safely.

The only launchd configuration is:

```text
~/Library/LaunchAgents/io.github.codex-retain.plist
```

It contains executable and state paths, and invokes:

```text
/absolute/path/codex-retain --state-dir /absolute/state/directory run --scheduled
```

The file is staged privately, synchronized, and atomically replaced. The
scheduler invokes `/bin/launchctl` directly with separate arguments and targets
only `gui/<uid>/io.github.codex-retain`. Each system command has a five-second
timeout. Enabling requires a macOS GUI login session; do not use `sudo`.
A private, empty `.io.github.codex-retain.lock` file in the same directory
serializes installation and removal across different policy directories. It is
retained to keep the locking identity stable and cannot launch any process.

## Status, pause, and disable

Use the CLI's status command to inspect both the policy and the LaunchAgent.
`registered` means launchd knows the job. It does not mean cleanup is currently
running or that its most recent run succeeded. `plist_present` means the
configuration file exists; a file alone does not prove the job is registered.
`missing` means the service was positively reported absent. Query failures are
reported as `error`, not disguised as a disabled schedule.

Pause preserves the policy for later resumption. Disable removes the schedule.
Run disable before deleting the utility. Repeating disable is safe when the job
is already absent. An actual launchctl failure is returned as an error; check
status and resolve the failure instead of assuming background cleanup stopped.

The job uses normal short-job scheduling. Its stdout and
stderr go to `/dev/null`; it cannot accumulate a separate pair of growing log
files. Consult the CLI's retained run result for the cleanup outcome. macOS may
also record launch failures in its own managed system logs.

## Updating and uninstalling

An update that replaces the executable at the same absolute installation path
is used by subsequent runs. Disable the schedule before moving the binary to a
different path, then enable it from the new installation. Update from the
project's checked source/release instructions; no automatic network updater is
installed.
Re-enable after moving or replacing an NVM-managed Node.js runtime to capture its
new search path. Disabling an existing job validates its original saved search
path without requiring it to match the current terminal environment.

For removal, disable first and then remove the executable with the package
manager or installation method you used. For a Cargo installation, the final
package-removal step is `cargo uninstall codex-retain`. The policy and small
local result files remain available unless you separately remove the state
directory. Removing the utility does not itself remove additional chats.

If someone deletes the executable without first disabling, launchd can retain a
stale job and plist. Launch attempts then fail because the binary is absent;
there is no second hidden executable that continues deleting chats. Reinstall at
the same path and disable to cleanly remove the stale schedule. A raw binary
deletion cannot invoke an uninstall hook, so package removal alone is not a
substitute for disable.

If registration fails after a plist was written, the adapter attempts to unload
the exact job and removes the new plist, preventing a silently configured future
login job. A cleanup failure is included in the error. Inspect status before
retrying; an error is not a claim of successful enablement or disablement.

## Platform and verification boundary

Automatic scheduling is implemented only for macOS. The other platforms report
`unsupported`; no Linux timer, cron entry, or Windows scheduled task is installed.
The policy engine and isolated tests can be exercised separately, but this is not
an assertion of production compatibility on those platforms.

Scheduler tests render the plist, use temporary directories and mocked
launchctl responses, exercise replacement and failure handling, and verify
bounded child termination. They do not register a LaunchAgent or delete any real
conversation. A separate native check exercised the real `run --scheduled`
entrypoint with a temporary UUID label, work-local plist, synthetic profile and
two-second test interval. It verified automatic deletion, pause, idle state and
successful removal of that test registration; see
[the native receipt](evidence/launchd-integration.json). Production-label
install/disable handling remains covered by isolated unit tests, not by installing
a retention policy against the developer's real profile.

Normal job scheduling was selected after real startup failures under
Background/LowPriorityIO; [the diagnosis](launchd-startup.md) explains the decision.

The design follows Apple's [LaunchAgent and periodic-job documentation](https://developer.apple.com/library/archive/documentation/MacOSX/Conceptual/BPSystemStartup/Chapters/CreatingLaunchdJobs.html)
and the [launchd property-list reference](https://github.com/apple-oss-distributions/launchd/blob/main/man/launchd.plist.5).
The current machine's `launchctl help bootout` documents that `--wait` can block
indefinitely; this adapter does not use that flag.
