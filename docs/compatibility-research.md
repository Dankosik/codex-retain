# Codex compatibility investigation

Investigation date: 2026-09-09. This document records source findings and isolated protocol experiments; the product's compatibility matrix determines its supported scope.

The initial recommendation to grant every existing archive a new grace period
has been superseded by the [initial archive-age amendment](product-contract.md#initial-archive-age-amendment-2026-09-10).
Explicit enable now trusts Codex's recorded archived_at and can clean immediately.
The source limitations about reconstructed timestamps below still apply; historical
validation receipts keep their original behavior and are not rewritten.

The initial implementation used the legacy-only scope below. The later
[paginated review](paginated-support.md) extends source support without changing
the pinned Codex version or treating different desktop writers as certified.

## Evidence identity

- Installed executable: codex-cli 0.153.4, npm distribution, macOS 26.4.0, Apple Silicon.
- Exact upstream tag: [rust-v0.153.4](https://github.com/openai/codex/tree/rust-v0.153.4).
- Resolved source commit: 3d2ee51ca2d5db578f328aa75e20aa22c0197c9a.
- Initialized database: state_5.sqlite; 52 SQLx migration receipts and 42 non-internal schema objects.
- All experiments used synthetic rollouts under a separate CODEX_HOME. No real conversation was read or deleted. No model turn was submitted.

A matching codex command on PATH does **not** establish the version of every desktop or CLI process accessing a profile. Compatibility must cover all writers, rather than just the binary used for a version check. This investigation does not certify an arbitrary desktop application build, older Codex versions, or third-party direct writers.

## The public delete operation is unsuitable

[thread/delete](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/app-server/src/request_processors/thread_delete.rs) resolves and deletes the entire spawned subtree. Its request has only a thread ID: no archive-state precondition, expected archive epoch, or option to preserve active descendants.

A client-side list followed by delete cannot prevent an intervening restore. Checking the root's archive status does not protect descendants. This API cannot directly implement the retention contract.

## Archive timestamps need explicit capture

Native [archive](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/thread-store/src/local/archive_thread.rs) sets archived_at using Utc::now(). [Unarchive](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/thread-store/src/local/unarchive_thread.rs) clears it. However, archived_at has no provenance bit:

- [Backfill](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/rollout/src/metadata.rs) can derive it from file modification time, falling back to the thread update time.
- [Filesystem reconciliation and read repair](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/rollout/src/state_db.rs) can derive it from updated_at.
- [Missing metadata reconstruction](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/thread-store/src/local/update_thread_metadata.rs) can synthesize it.

An old value cannot prove how long the thread was continuously archived. Daily polling also misses restore/rearchive cycles between polls.

A small owned SQLite epoch table, updated by triggers on native archive transitions, captures these events in Codex's transaction without a persistent observer. Seed existing archived rows with installation time to provide a full first retention period. Clear the epoch on restore; assign a fresh epoch on rearchive. Treat timestamp repair as a fresh epoch: this may retain data longer but cannot justify earlier deletion. Verify trigger integrity before cleanup; missing or changed capture objects invalidate eligibility.

This is an explicit, version-specific database extension, not an official integration contract. SQLx accepted the extension in the isolated 0.153.4 restart test. That does not establish compatibility with later migrations. Disabling the policy should remove the owned extension atomically.

Native timestamps and SQLite unixepoch() have one-second resolution. A cleanup rule must avoid rounding a partial second into early expiry. Clock changes remain a limitation of wall-clock policies.

## Cross-process coordination

### Lifecycle and live writers

The [writer lock coordinator](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/thread-store/src/local/writer_lock.rs) uses:

1. Exclusive OS lock on $CODEX_HOME/thread-writer-locks/.coordination.lock.
2. While coordination is held, open/create the thread UUID's .lock file and obtain an exclusive nonblocking OS lock.
3. Release coordination while retaining the thread lock.
4. On release, reacquire coordination, close the thread lock, and remove its path.

Coordination is essential because Codex removes stale lock files. Opening outside this protocol can lock an unlinked inode while another process owns its replacement pathname.

Retaining a closed stale thread-lock pathname is safe if close happens while coordination is held; a later Codex acquisition cleans it. Removing or replacing a pathname without coordination is unsafe.

Archive, unarchive, delete, revert, and live recorder create/resume participate. A resumed idle thread retains its writer lock; idle does not imply removable.

### Compression and migration

The [maintenance lock](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/rollout/src/maintenance.rs) is $CODEX_HOME/.tmp/rollout-maintenance.lock. It is a persistent pathname holding an exclusive nonblocking OS lock. The [compression worker](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/rollout/src/compression.rs) and legacy-to-paginated migration participate. Retention should hold it before inventory and mutation. Busy means skip and retry later.

Compression is controlled by local_thread_store_compression. When enabled, it can compress active and archived files whose modification time is at least seven days old. Plain-only retention therefore has a substantial limitation for compressed profiles.

### Lock boundaries

Not every operation takes these locks:

- SQLite backfill, reconciliation, and read repair do not take the per-thread lock.
- [Unloaded-thread metadata updates](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/thread-store/src/local/update_thread_metadata.rs) can append SessionMeta without a cross-process writer lock.
- Compressed metadata updates can materialize a plain sibling without the maintenance lock.

BEGIN IMMEDIATE serializes database mutations but cannot retract operations that already passed their database step. Plain-file append opens an existing file without creating a missing one; an open descriptor can survive unlink. Compressed materialization can republish a plain sibling. These details limit complete-cleanup and immediately-reclaimed-space claims. Do not describe the locks as excluding every Codex writer.

The supported archive/unarchive path still changes eligibility only after obtaining the thread lock. Unknown writers, unsupported profile structures and unchecked versions require a fail-closed result.

## Shared history and database deletion

Paginated history references earlier rollout segments. [Lineage resolution](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/thread-store/src/local/rollout_lineage.rs) requires every referenced segment to be paginated. [Legacy fork](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/app-server/src/request_processors/thread_processor.rs) copies source history; shared prepare_fork is selected for paginated sources.

Refuse paginated history, history_base, ambiguous ownership, malformed metadata, and unknown references before treating content as exclusively owned. The maintenance lock excludes concurrent migration of legacy files into paginated ones. A database flag does not replace rollout identity and metadata checks.

The [state deletion implementation](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/state/src/runtime/threads.rs) also removes queues, memory, goals, logs and names in other stores. A narrow utility must describe its actual scope:

- threads has no self-referential foreign key recursively deleting descendants.
- thread_dynamic_tools and thread_artifacts have thread-owned cascade relationships in the final inspected main schema. Earlier migrations created stage1_outputs and thread_goals there, but later migrations moved those stores out of the main database.
- thread_spawn_edges has no foreign key; deleting only a thread row leaves these edges.
- Separate log, queue, memory, name-index and history-projection data may remain unless explicitly handled.

Do not claim removal of all local traces, cloud history, or worktrees. Routine database vacuum is not required to delete rollouts and should not be used merely to inflate space-reclamation claims.

## Isolated experiments

The following tests used the installed binary:

| Experiment | Result |
| --- | --- |
| Hold external Python flock with the coordination protocol; call thread/archive | Rejected: already has an active writer |
| Release lock; call thread/archive | Succeeded |
| Hold lock; call thread/unarchive | Rejected with writer conflict |
| Release lock; call thread/unarchive | Succeeded |
| Hold lock; call thread/resume | Rejected with writer conflict |
| Release lock; resume and retain idle thread | Succeeded; external nonblocking thread lock rejected |
| Restart app-server with owned epoch table and triggers installed | Initialization and native operations succeeded |
| Native archive / restore / rearchive | Epoch inserted / removed / assigned a later timestamp |
| Hold maintenance lock with compression enabled | Old synthetic archived file stayed plain |
| Release maintenance lock and restart with same fixture | File compressed; original plain file removed |

The first app-server probe attempted an unauthenticated featured-plugin cache request and received HTTP 401. It used no user credentials and submitted no inference request. The maintenance probe disabled plugins and directed HTTP proxies at a closed local port. The retention utility should not launch an app-server or make network requests for routine cleanup.

## Limits and future compatibility

These experiments establish selected coordination and capture behavior. They do not replace end-to-end utility tests for journal recovery, malformed paths, schema drift, preservation of active descendants, scheduler lifecycle, and interrupted deletion.

CLI 0.153.4 evidence does not certify an unknown desktop writer. Future versions need source review and isolated conformance checks before accepting their version/schema fingerprint. A public archive-conditional, non-cascading delete API would remove much of this private-storage coupling and is the preferred upstream improvement.


## Implemented utility conformance run

The reproducible [integration harness](../scripts/codex_integration.py) ran the actual built utility against installed Codex 0.153.4, with separate HOME and CODEX_HOME, native thread/start and section/move persistence, and no model turns or LaunchAgents. The [JSON receipt](evidence/codex-integration.json) identifies both executable versions and SHA-256 hashes.

The run verified initial grace, restore/rearchive capture without an intervening utility run, native pinned-section protection with is_pinned=0, preservation of a native-loaded active thread, deletion after the native writer stopped, idempotence, and disable removing the capture extension. Codex rejects resuming archived threads; the writer-conflict case therefore explicitly injected stale archived metadata and moved only a synthetic file while its native writer remained loaded. This adversarial fixture checks the utility's lock guard; it is not presented as ordinary native behavior.

To reproduce, pass explicit --codex-bin and --retain-bin paths, a nonexistent --root directory, and optionally --output to the Python script. The fixture remains at that root for inspection. The harness does not read credentials, submit inference requests, or install background jobs. Its HTTP proxies point at a closed loopback port; plugin features are disabled.
