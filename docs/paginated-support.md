# Paginated retention (0.1.1, 2026-09-10)

Version 0.1.0 skipped every paginated archive while reporting verified schema
compatibility. Version 0.1.1 adds the paginated adapter and explicit format coverage.
Published 0.1.0 artifacts remain immutable.

## Source contract

Reviewed upstream Codex `rust-v0.153.4`, commit
`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`:

- [RolloutReferenceIndex](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/rollout/src/rollout_reference_index.rs)
  scans both session trees. SessionMeta.id identifies the owning thread; the
  filename identifies an immutable rollout. history_base.thread_id points to a
  rollout ID. Reverted threads can own multiple files.
- [Native deletion](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/thread-store/src/local/delete_thread.rs)
  refuses external references to any owned segment. Internal links within one
  owner can disappear together. Deleting a leaf does not delete its ancestors.
- [Fork request validation](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/app-server/src/request_processors/thread_processor.rs)
  calls read_stored_thread_for_resume and explicitly rejects archived_at before
  preparing a fork. The read includes archived records in order to produce the
  rejection; include_archived=true by itself does not permit archived forks.
- [Prepared fork reservation](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/thread-store/src/local/paginated_fork.rs)
  blocks same-store lifecycle operations through child persistence. Restore and
  revert additionally use the source OS writer lock.

Paginated storage remains authoritative JSONL, optionally zstd-compressed, with
rebuildable SQLite projections. Retain keeps its declared narrow scope: owned
rollout files and the reviewed main-state row/cascades. It does not purge the
separate thread_history_1.sqlite cache, names, logs, memories or global indexes.

## Implementation

The lineage scan retains paths only for candidate owners and tracks incoming
references by immutable rollout ID. It includes active, archived, compressed and
orphan files. Unknown or unreadable reference metadata fails closed; duplicate
known JSON fields, duplicate rollout IDs, links and malformed references cannot
be silently ignored. First-record reads and inventory size are bounded.

Each candidate must own only flat archived paginated rollouts, including its
selected DB path. Every segment is checked for external references. Ordinary
grace periods, pins, exclusions, spawn protection and writer locks still apply.
Preview's reference inventory is rebuilt under writer locks and BEGIN IMMEDIATE
before any mutation. Path-alias checks cover old segments as well as the selected
one. Groups contain at most 128 files and split before journaling if necessary.

Journal schema 3 binds stable thread owner and immutable rollout ID separately.
All files are named durably before staging, then owner rows commit atomically.
Recovery restores stages when their owner row survives; otherwise it finishes
removal. Staged inode identity and metadata owner are revalidated. Old journal
schemas remain readable; 0.1.0 cannot read schema 3, so complete pending recovery
with the newer executable before downgrading.

Doctor/status add history_coverage with fixed aggregate counts for legacy,
paginated and other formats. The existing compatible flag is explicitly scoped
to schema and the selected Codex binary. Format support does not establish
artifact ownership, archive age or deletion eligibility; preview is authoritative
only for its observation time.

## Coordination boundary

The ordinary API cannot begin a fork of a continuously archived source. Per-thread
locks prevent restore/revert during Retain's final checks. This is not a universal
cross-process reservation: native PreparedFork leases are in-process. A request
prepared in another store before archiving and delayed beyond the entire retention
period, or a lower-level caller bypassing archived-source rejection, falls outside
the demonstrated contract. A standalone source-lock/reference scan must not be
presented as proof against that broader case. No native API reproduction of such
a delayed dangling-reference outcome is claimed here.

## Verification

Synthetic Rust tests cover multi-segment grace/deletion, outgoing and incoming
references, orphan references, compressed histories, older-segment path aliases,
bounded batch splitting, partial staging, committed-prefix recovery, invalid
rollout IDs and misbound stable owners. Actual CLI tests distinguish format
coverage from schema validation and eligibility.

Native conformance is recorded separately with an immutable candidate hash. It
uses isolated HOME/CODEX_HOME, the real Codex 0.153.4 app-server, no credentials
or model turns, and no production LaunchAgent. Final local results:

- `make verify` passed; the final focused tests and reason-token check passed too
  (154 distinct Rust tests, 26 release/installer tests and 7 skill maintenance tests).
- Final local release-build SHA-256:
  `0594346e7a3af5b73a42257ebc1a01bf221c42682cd006e9ef6ae55433fdb9fa`.
- [npm Codex native conformance](evidence/paginated-support-2026-09-10/native-release.json):
  14 checks passed.
- [Installed Desktop embedded Codex conformance](evidence/paginated-support-2026-09-10/native-desktop.json):
  the same 14 checks passed on its 0.153.4 executable.
- A canonical synthetic completed turn was appended only after the source's native
  writer shut down; no model request was made. Native code persisted the fork
  reference and performed a revert. The child hydrated the complete inherited
  turn unchanged after attempted source cleanup. Both source segments were
  removed only after the dependent leaf was removed.
- The native revert regression verifies the exact
  `rollout-TIMESTAMP-OWNER_UUID_ROLLOUT_UUID.jsonl` filename and header owner,
  including compressed representations in the Rust tests.
- The user's real profile was inspected read-only, and the installed 0.1.0 binary
  and policy were not replaced. Publication and installed-version verification are recorded in the release handoff.

[Validation summary](evidence/paginated-support-2026-09-10/validation.json).
