# Installed 0.1.0 verification — 2026-09-10

This is a historical observation of **0.1.0**, not the current installed state.
Paginated support was subsequently added in 0.1.1; dependency-aware cleanup and
physical-copy recovery followed in 0.1.2. The receipts below retain their original
version and scope. Use `codex-retain status` for current installation state.

## Finding

The current user's archive cannot be cleaned by 0.1.0: all 2,113 archived rows
have `history_mode=paginated`. The adapter accepts only `legacy`.
The real preview returned zero eligible rows, with 78 `pinned_or_unknown_pin`
and 2,035 `unsupported_history_mode` skips. Pin protection is evaluated first;
the 78 protected rows are also paginated. Waiting 30 days does not remove the
format restriction. An expired paginated synthetic row reproduced this behavior.

`Compatibility: verified` currently reports prerequisite checks, without showing
whether the user's actual history formats are supported. This diagnostic can
create a false expectation of useful automatic cleanup on this profile.

## Installed system

Version 0.1.0 is enabled, unpaused, with 30-day retention. The macOS LaunchAgent
is registered at a 3,600-second interval, has RunAtLoad=false and no KeepAlive,
and points to `/opt/homebrew/bin/codex-retain`, the stable Homebrew link.
There was no completed production run at observation time. No pending recovery.
The 2,113 captured archive epochs all equal activation time, confirming the new
full grace period. Real-profile inspection was read-only. This verification did
not invoke real cleanup, change retention settings, or kickstart the production job.

## Installed-binary evidence

The tested installed executable SHA-256 is
`7d918421f0e8900290a1d445bc46188c99716c64a6815e2f3e6a5a0513803c9c`.

- A unique temporary native LaunchAgent used the installed executable, configured
  Codex 0.153.4 launcher, and the production job's captured PATH against synthetic
  data. It deleted two expired legacy chats, reached idle with exit 0, then
  preserved an expired candidate during a subsequent paused invocation. The test
  job was unloaded and its absence verified. The production label was untouched.
- Eight synthetic cases checked fresh grace, expired legacy, expired paginated,
  pin protection, explicit exclusion, active state, restore/rearchive, and pause.
  Only expired legacy was deleted; both its rollout and database row disappeared.
  Every preserved rollout retained its checksum. Repeated cleanup deleted zero;
  pause blocked a newly expired candidate. SQLite integrity remained `ok` and
  this separate manual test policy was disabled afterward.

These checks establish the covered behaviors of the installed ARM64 executable.
They do not prove universal absence of bugs, certify paginated history, or prove
that the production hourly job has already completed. Earlier release CI evidence
is separate from these current installed-binary tests.

## Follow-up identified at the time

Add explicit history-format coverage to doctor/status so that verified schema and
binary version do not imply cleanup coverage. Supporting this user's archive
requires a separately reviewed paginated-storage adapter: shared pages, ownership,
concurrent writers, deletion and recovery need real protocol/source evidence and
synthetic conformance tests. Do not bypass the existing format guard.

Evidence: [live summary](evidence/installed-verification-2026-09-10/live-summary.json),
[native scheduler](evidence/installed-verification-2026-09-10/native-launchd.json),
[behavior matrix](evidence/installed-verification-2026-09-10/behavior.json).
