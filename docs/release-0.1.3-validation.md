# Release 0.1.3 verification — 10 September 2026

[Codex Retain 0.1.3](https://github.com/Dankosik/codex-retain/releases/tag/0.1.3)
was published from annotated tag `0.1.3`, source commit
`1e99b901c1c1aae575e98fab50936df8286f909f`.

[The release workflow](https://github.com/Dankosik/codex-retain/actions/runs/34512746502)
completed successfully. Both archives were built, tested and smoke-tested after
extraction on their native macOS 15 runners: Apple Silicon and Intel. The tag's
Linux/macOS tests, minimum-Rust, quality and dependency checks also passed.

All five published asset digests matched GitHub's recorded SHA-256 values.
Both archive digests matched SHA256SUMS, and their complete file inventories
were validated. The installer matched the script in the tag. Regenerating the
Homebrew formula from the downloaded archive set produced exactly the published
formula; that asset is copied into `Formula/codex-retain.rb` in the follow-up
main-branch commit.

The published ARM64 binary was installed using the downloaded installer and
through a separate temporary Homebrew prefix. Both installed binaries match
SHA-256 `79d4275bfa913063998b59ee5d32416e163fb4ed59dc8025d6084951f90a3d27`.
Version, help and Bash completions passed isolated smoke tests without creating
profile or policy state. Homebrew style, readall for all OS/architecture
configurations, install and test passed. Homebrew's test sandbox emitted
nonfatal sysmond/pgrep diagnostics after its commands; the process exited 0,
and independent smoke tests confirmed the commands and absence of state.

The downloaded ARM64 executable also passed 13 native paginated/fork/revert
checks and 15 organizational-relation checks with embedded Codex 0.153.4. These
used isolated synthetic profiles, no model requests and no production LaunchAgent.
Organizational links were constructed after native startup; this is not proof
of model-driven spawning. Native Intel build/executable testing comes from CI;
Homebrew installation on Intel was not repeated on this ARM64 host.

Both binaries link only the inspected system libraries, libSystem and libiconv.
The existing `/opt/homebrew/bin/codex-retain` binary was unchanged; publication
and isolated package verification did not upgrade the user's active installation
or modify its retention policy or schedule.

[Machine-readable verification](evidence/release-0.1.3/verification.json) ·
[Published-binary paginated conformance](evidence/release-0.1.3/published-arm64-paginated.json) ·
[Published-binary organizational conformance](evidence/release-0.1.3/published-arm64-related.json) ·
[Final implementation performance](performance-0.1.3.md).
