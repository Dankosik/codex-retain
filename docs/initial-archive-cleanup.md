# Immediate cleanup when enabling retention

Implemented for release 0.1.4. The receipts below describe the pre-release local
candidate; publication, package installation and real-profile execution are
separate checks.

`enable --days N --yes` now imports Codex's existing archive dates and completes
one cleanup before returning. An existing archived thread is eligible only when
its recorded archive date is more than N full 24-hour days old and all normal
pin, exclusion, dependency, writer-lock and artifact checks allow removal.
Recent archives keep their remaining time. Missing, nonpositive or future dates
never get an inferred age from creation time, last messages or file timestamps.

This deliberately trusts existing `archived_at`, which Codex can reconstruct
without recording the timestamp's provenance. It does not prove continuous
archive time before activation. New archive/restore/rearchive transitions and
repairs after activation still use the current transaction capture rules.

Binary/package installation still has no policy or deletion effect. Both
scheduled and manual-only `enable --no-schedule` perform the first cleanup;
subsequent automatic invocations remain hourly. The operation lock spans setup
and that first run. Scheduler setup must succeed before cleanup begins.

The same cleanup engine and bounded last-run receipt are used by enable and
later run commands. Initial artifact failures return exit 3 and an attention
receipt while retaining the enabled policy for retry. A fatal initial-run error
explicitly says the policy is enabled. No successful enable response is emitted
before the first cleanup finishes. `enable` JSON is schema 2 with the initial
cleanup report; the obsolete fresh-grace fields are removed. Run/preview,
policy/capture and journal formats are unchanged.

Older active policies keep their captured periods. Adoption requires explicit
disable/re-enable with the desired profile, executable, days and scheduling
options; exclusions survive. Ordinary runs and package updates never silently
reseed existing archive clocks.

## Verification

- `make check`: formatting, Clippy with warnings denied, **195 Rust tests**, doctests.
- Python script suite: **53 tests** passed.
- Locked ARM64 release build passed with the declared Rust 1.98.1 toolchain.
- Executable tests prove immediate legacy/paginated deletion, retained recent,
  pinned/excluded/active/dependent owners, initial attention and scheduled retry.
- Exact expiry boundary, missing/invalid/future dates, native row preservation
  during seeding and unchanged restore/rearchive/repair behavior are covered.
- Real Codex 0.153.4: **11 lifecycle checks**, including immediate deletion of a
  native-created archived thread whose synthetic archived_at is aged before
  activation; fresh native archive dates remain unchanged.
- Real Codex paginated/fork/revert: **13 checks** passed.
- A separate temporary LaunchAgent proved later automatic deletion of two aged
  synthetic archives, pause protection and successful test-job removal. Its
  initial activation used recent dates so the scheduler's work was still present.

Benchmark fixtures now choose a recent initial archive date, then age only the
marked synthetic capture state outside their timed command. Initial cleanup
cannot consume the benchmark workload during setup. Historical benchmark and
release evidence is preserved unchanged.

[Candidate identity](evidence/initial-archive-cleanup/candidate.json) ·
[Native lifecycle](evidence/initial-archive-cleanup/native-lifecycle.json) ·
[Paginated conformance](evidence/initial-archive-cleanup/native-paginated.json) ·
[Temporary scheduler](evidence/initial-archive-cleanup/native-launchd.json).
