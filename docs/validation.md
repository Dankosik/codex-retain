# Validation record

Local validation on 2026-09-09, Apple M5 / 16 GiB / macOS 26.4 ARM64.
Rust 1.98.1, Codex CLI 0.153.4, Codex Retain 0.1.0.

## Paginated support follow-up (0.1.1, 2026-09-10)

The [paginated support record](paginated-support.md) identifies the final local
candidate and its checks. It adds format diagnostics, multi-rollout ownership,
shared-history preservation, and schema-3 recovery. The native npm CLI and the
installed Desktop embedded 0.153.4 executable each passed 14 synthetic checks.
Release 0.1.0 remains unchanged; the fix is distributed in 0.1.1.

## Cleanup optimization follow-up

The updated executable has SHA-256
`8fcb4959a9b2df1864e100a8af05f8fec9fb631b6b4d1dafc3a42068e117a649`.

| Check | Evidence and scope |
| --- | --- |
| Rust formatting and diagnostics | Formatting check and Clippy for all targets with warnings denied passed |
| Rust behavior tests | 74 passed: 26 library, 14 batch, 9 CLI, 25 lifecycle; includes one subprocess fixture entry point |
| New failure coverage | Buffered JSON completeness and failure preservation; 128-member partial staging/unlink recovery; legacy 32-member recovery; oversized valid journal refusal; lock ownership during partial acquisition and contended cleanup |
| Native Codex conformance | [10 checks passed for the updated executable](evidence/cleanup-optimization/native-codex-final.json) |
| Native scheduling | [Temporary launchd job passed and was removed](evidence/cleanup-optimization/native-launchd-final.json); production label untouched |
| Cleanup comparison | [Baseline, candidate and Janitor](cleanup-performance.md), five samples plus three warmups at 1,000 and 10,000 chats, fresh synthetic fixtures and exact postconditions |
| Preview regression | Identical Retain JSON excluding only `started_at`; [unchanged fixture and SQLite integrity verified](evidence/cleanup-optimization/preview-postconditions.json) |

The dependency set and lockfile are unchanged. Current source checks and GitHub
CI supplement the original evidence below; the original binary's receipts are
not used as proof for the updated executable.

## Original implementation validation

Original measured executable SHA-256:
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

## Original support snapshot (before the first public release)

| Surface | Status |
| --- | --- |
| macOS 26.4, ARM64, Codex CLI 0.153.4 local legacy JSONL/zstd | Implemented and locally exercised |
| Intel macOS | Native packaging helper retained; no Intel runner or official release is claimed |
| Linux | Core/preview/doctor code and CI configured; destructive CLI and scheduler disabled; no local Linux run |
| Windows | Deferred; no release or compatibility claim |
| Codex desktop embedded server | Not certified by the installed PATH CLI version; all writers need the reviewed version/protocol |
| Paginated/shared histories and spawn-related threads | Deliberately skipped |
| Future Codex versions or schema changes | Rejected; disable Retain before upgrading Codex, then review the new adapter |
| Original local validation | The records above predate GitHub publication; they do not claim remote CI results |
| Public distribution | GitHub source only; no tags, Releases, registry packages, signed/notarized distribution or deployments |

The recorder is a private-schema extension, so future migrations require review.
Some Codex metadata operations can republish a file outside the cooperative
writer protocol. New objects are preserved; exact physical disk reclamation and
complete erasure are not claimed. These limitations are documented in the
[architecture](architecture.md) and [compatibility research](compatibility-research.md).
