# Paginated retention (0.1.2 design, 2026-09-10)

Version 0.1.0 skipped every paginated archive while reporting verified schema
compatibility. Version 0.1.1 adds the paginated adapter and explicit format coverage.
Version 0.1.2 adds dependency-ordered cleanup and physical-copy accounting.
Published older artifacts remain immutable.

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
orphan files. Unknown or unreadable reference metadata fails closed. Duplicate
known JSON fields, conflicting owners, links and malformed references cannot be
silently ignored. Multiple physical files with one rollout ID and the same
owner are supported: every copy contributes its references and receives its own
identity checks. Plain/zstd copies need not contain equal bytes or equal content.
First-record reads and inventory size are bounded.

Archive eligibility comes from the current database row and capture epoch.
An archived owner can have physical copies in either managed tree, including
nested session directories, while an active owner's copies remain protected
wherever they are stored. The selected DB path must resolve to an owned copy.
Every segment/copy is checked for references and path aliases, including old
revert segments. Foreign path ownership and unsafe filesystem objects veto
deletion.

Organizational children and shared-history consumers form dependencies. Eligible
leaves are removed first; when their references disappear, an independently
eligible source can be removed later in the same run. A surviving consumer
preserves its source, including when that consumer is active, protected, too new,
or fails final validation. Cycles remain protected. Ordinary grace periods, pins
and exclusions still apply. Cleanup locks candidate owners and all organizational
ancestors, with at most 128 writer locks. It rebuilds the graph and inventory
inside `BEGIN IMMEDIATE` and rejects newly discovered unguarded ancestors.
See [related retention](related-support.md).

Journal schema 4 binds stable thread owner, immutable rollout ID and a unique
slot for each physical copy. All files are named durably before staging, then
owner rows and incident SQL spawn-edge removal commit atomically. Recovery
restores stages when their owner row survives; otherwise it finishes removal.
Staged inode identity and metadata owner are revalidated. The 128-file bound
still applies, independently of the writer-lock bound. Current recovery accepts
schemas 1–4; older versions cannot recover schema 4. Complete pending recovery
with the newer executable before downgrading.

Doctor/status add history_coverage with fixed aggregate counts for legacy,
paginated and other formats. The existing compatible flag is explicitly scoped
to schema and the selected Codex binary. Format support does not establish
artifact ownership, archive age or deletion eligibility; preview is authoritative
only for its observation time. `preview --at <RFC3339>` forecasts that same
snapshot at a future retention time without changing capture or deleting files.
Its `evaluated_at` is separate from actual `started_at`; a forecast does not
guarantee the result of a later cleanup.

## Coordination boundary

The ordinary API cannot begin a fork of a continuously archived source. Per-thread
locks prevent restore/revert during Retain's final checks. This is not a universal
cross-process reservation: native PreparedFork leases are in-process. A request
prepared in another store before archiving and delayed beyond the entire retention
period, or a lower-level caller bypassing archived-source rejection, falls outside
the demonstrated contract. A standalone source-lock/reference scan must not be
presented as proof against that broader case. No native API reproduction of such
a delayed dangling-reference outcome is claimed here.

## Verification scope

Synthetic Rust tests cover multi-segment grace/deletion, outgoing and incoming
references, orphan references, compressed histories, older-segment path aliases,
bounded batch splitting, partial staging, committed-prefix recovery, invalid
rollout IDs and misbound stable owners. Actual CLI tests distinguish format
coverage from schema validation and eligibility.

The 0.1.2 design additionally requires same-run dependency closure, protection
propagation, ancestor lock coverage, multiple same-owner copies across both
trees/encodings, and schema 4 recovery. The native paginated harness uses a
canonical synthetic completed turn after native writer shutdown, then asks real
Codex to fork, hydrate and revert it. This tests native persisted lineage without
submitting a model request. The related-thread harness constructs organizational
links explicitly; it is not evidence of a native model-driven spawn.

Final local 0.1.2 build SHA-256:
`0e0e2e202063aeab1a3e81f8d59634b17b4eac3f13b0d5363937a1297cf3c578`.
`make check` passed formatting, Clippy and 186 Rust tests; maintenance validation
passed 26 release/installer tests and 7 skill tests. Native receipts use isolated
HOME/CODEX_HOME, no credentials or model requests, and no production LaunchAgent:

- [npm Codex paginated conformance](evidence/related-0.1.2/cli-paginated.json):
  13 checks passed.
- [Desktop embedded Codex paginated conformance](evidence/related-0.1.2/desktop-paginated.json):
  the same 13 checks passed on its 0.153.4 executable.
- The native fork retains a complete synthetic inherited turn unchanged after
  attempted source cleanup. Once the child is also due, its removal and the
  removal of both native reverted source segments finish in one run.
- [Organizational CLI](evidence/related-0.1.2/cli-related.json) and
  [Desktop](evidence/related-0.1.2/desktop-related.json) runs each passed 15 checks;
  the constructed-hierarchy boundary is described in [related support](related-support.md).

These receipts establish the named local build and tested writer versions;
publication and installed-version verification remain separate release steps.
[Earlier 0.1.1 evidence](evidence/paginated-support-2026-09-10/validation.json)
remains historical evidence for its own candidate.
