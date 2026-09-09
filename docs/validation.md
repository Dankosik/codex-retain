# Validation record

Local validation on 2026-09-09, Apple M5 / 16 GiB / macOS 26.4 ARM64.
Rust 1.98.1, Codex CLI 0.153.4, Codex Retain 0.1.0.

Final measured executable SHA-256:
`af2aeb726b92d273c5e30b2e242264e6dde2fd73cd067ca7a5f37771130040f2`.

| Check | Evidence and scope |
| --- | --- |
| Rust formatting | `cargo fmt --all -- --check` |
| Rust static diagnostics | `cargo clippy --locked --all-targets -- -D warnings`, passed |
| Rust behavior tests | `cargo test --locked`, 66 passed: 23 library, 9 batch, 9 CLI, 25 lifecycle; includes one subprocess fixture entry point |
| Native Codex conformance | [10 checks passed](evidence/codex-integration.json): real archive/restore/rearchive, current pin section, live writer, selective removal, repeat run and disable |
| Native scheduling | [Temporary launchd test passed](evidence/launchd-integration.json): scheduled removal, pause, no idle PID, verified bootout; production label was never installed |
| Process interruption | Single/group journal crash-prefix tests and actual interrupted scale-run continuation; [receipt](evidence/interrupted-scale-recovery.json) |
| Dependency policy | cargo-deny 0.20.2: advisories, bans, licenses and sources passed; no ignored advisories |
| Attribution | [93 resolved packages, 3 supplemental entries, no unmatched licenses](evidence/dependency-licenses.json); generator check passed |
| Native archive | Extracted ARM64 executable passed version/help/completions in an empty isolated environment; no policy state created |
| Performance | [All eight paired cases](performance.md), 10 samples plus 3 warmups, fresh synthetic data and exact postconditions |
| Retention scenario comparison | [Seven scenario observations](evidence/semantic-comparison.json) using separate synthetic Retain/Janitor profiles |

Source/release maintenance also checks the release and skill-sync Python tests,
workflow syntax through actionlint, local document links, and a redacted source
scan through gitleaks. Reproduce with `make check`, `make maintenance-check` and
the documented explicit-path integration/benchmark scripts. No passing test
authorizes operating on a user's real history.

## Support matrix and remaining boundaries

| Surface | Status |
| --- | --- |
| macOS 26.4, ARM64, Codex CLI 0.153.4 local legacy JSONL/zstd | Implemented and locally exercised |
| Intel macOS | Native CI/release job configured; not executed in this local session |
| Linux | Core/preview/doctor code and CI configured; destructive CLI and scheduler disabled; no local Linux run |
| Windows | Deferred; no release or compatibility claim |
| Codex desktop embedded server | Not certified by the installed PATH CLI version; all writers need the reviewed version/protocol |
| Paginated/shared histories and spawn-related threads | Deliberately skipped |
| Future Codex versions or schema changes | Rejected; disable Retain before upgrading Codex, then review the new adapter |
| Remote CI, signed/notarized distribution, GitHub publication | Not performed by this local delivery |

The recorder is a private-schema extension, so future migrations require review.
Some Codex metadata operations can republish a file outside the cooperative
writer protocol. New objects are preserved; exact physical disk reclamation and
complete erasure are not claimed. These limitations are documented in the
[architecture](architecture.md) and [compatibility research](compatibility-research.md).
