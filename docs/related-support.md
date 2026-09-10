# Related-thread retention (0.1.2 design)

Version 0.1.2 replaces blanket protection of spawn-related archives with
dependency-aware cleanup. A child can expire while its parent is retained.
An eligible parent can expire after its last dependent disappears, including
later in the same run. Every removed thread must independently satisfy the
archive, retention, pin, exclusion, ownership and compatibility checks.

## Two kinds of dependency

An organizational child names its parent through rollout metadata or
`thread_spawn_edges`. A paginated history consumer names a source rollout through
`history_base`. Retain combines those relationships into one directed graph:
remove dependents before the parents or history sources they need.

| Current snapshot | Outcome |
| --- | --- |
| Archived due child, active or pinned parent | Child may be removed; parent remains |
| Entire archived chain is due and unprotected | Leaves can be removed before ancestors in one invocation |
| Child is active, pinned, excluded, too new, or otherwise survives | Parent and any required ancestors remain |
| Shared-history consumer survives | Its source owner remains |
| All shared-history consumers are removed and source is independently due | Source can be removed later in the same invocation |
| Dependency cycle | Affected removal stays blocked |
| Parent has a native live writer | Child removal waits for the ancestor writer lock |

Public report entries remain in ID order. Their order is not a record of
execution order. A forecast or initial plan is also not permission to ignore
changes that occur before deletion.

## Reviewed upstream contract

Source review uses Codex `rust-v0.153.4`, immutable commit
`3d2ee51ca2d5db578f328aa75e20aa22c0197c9a`:

- [Protocol metadata](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/protocol/src/protocol.rs)
  defines `parent_thread_id`, `SubAgentSource::ThreadSpawn` and
  `MultiAgentVersion`. Parent metadata is not discarded merely because a SQL
  edge is absent.
- [State thread operations](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/state/src/runtime/threads.rs)
  store directional parent/child edges and remove incident edges when a thread
  is deleted. Retain performs its selected row and edge removal together inside
  the final SQLite transaction.
- [App-server deletion](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/app-server/src/request_processors/thread_delete.rs)
  expands the requested root to its spawned subtree and constructs a descendant
  deletion order. That unconditional subtree operation cannot express Retain's
  per-thread archive and retention policy, so Retain does not call it for cleanup.
- [Writer coordination](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/thread-store/src/local/writer_lock.rs)
  combines a coordination file with per-thread OS locks. A loaded thread keeps
  its writer lock even while idle. Retain acquires the candidate and all
  organizational ancestors before final validation.
- [Rollout references](https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/rollout/src/rollout_reference_index.rs)
  distinguish stable ownership from physical rollout identity. The separate
  [paginated review](paginated-support.md) explains fork/revert boundaries.

The local implementation lives in [relations.rs](../src/relations.rs),
[lineage.rs](../src/lineage.rs) and [engine.rs](../src/engine.rs). SQL and metadata
relationships both contribute protection. A missing or invalid dependency
inventory is an error rather than permission to treat a thread as independent.

## Final checks and physical copies

For each group, Retain acquires the maintenance lock and writer locks for the
deleting owners and all organizational ancestors. At most 128 writer locks are
allowed in a guard set. Oversized ancestry stays protected. An ancestor can be
active, pinned or not yet due; locking it does not authorize deleting it.
Shared-history sources participate in dependency ordering but are not treated
as organizational ancestors for lock traversal.

Inside `BEGIN IMMEDIATE`, Retain rechecks row/epoch/policy state and rebuilds
the dependency graph from current SQL and file headers. A new ancestor not
covered by held locks, a surviving dependent, or a cycle prevents that removal.
If a planned leaf cannot be removed, final checks keep its prerequisite from
being removed on the strength of the stale plan.

Archive eligibility comes from the database row and captured epoch. The owned
file inventory spans both `sessions` and `archived_sessions`, including nested
paths and plain/zstd encodings. Multiple physical copies of one rollout are
supported when ownership agrees. Content equality is not required: each copy is
independently validated and all observed dependency references are retained.
Conflicting owners, foreign path aliases, symlinks, hardlinks, malformed headers
or unknown formats still stop removal. This does not make an active thread's
files eligible because of where they happen to be stored.

Journal schema 4 gives every physical file an owner UUID, rollout UUID and unique
slot. This prevents staging collisions between copies with the same rollout ID.
The intent covers no more than 128 files; files move to the staging directory
before selected rows and incident spawn edges commit. Recovery restores exact
staged objects when their owner row survives, or finishes authorized unlinking
after the row is gone. Current recovery reads schemas 1–4. Older executables
cannot recover schema 4; finish recovery before downgrading.

## Forecasting retention

```sh
codex-retain preview --at 2027-01-01T00:00:00Z
codex-retain --json preview --at 2027-01-01T00:00:00Z
```

The timestamp must be current or future. Retain evaluates the present profile
at that retention time without changing the policy, capture epochs, archived
files or last-run receipt. JSON keeps actual observation time in `started_at`
and adds the requested `evaluated_at`. Ordinary preview omits that field; `run`
rejects `--at`.

This answers what could expire if today's archive, protection and dependencies
stayed the same. It does not predict future changes or guarantee that writer
locks will be available. Final deletion uses actual time and fresh checks.

## Verification and limits

[The related integration harness](../scripts/related_integration.py) initializes
new isolated HOME/CODEX_HOME directories with real Codex 0.153.4. It creates and
persists ordinary native legacy threads, shuts down app-server, then explicitly
constructs organizational headers and matching SQL edges. The headers use
`parent_thread_id`, `source.subagent.thread_spawn`, depth and
`multi_agent_version: "v2"`. This is a constructed organizational fixture,
not a native model-driven spawn test. Only synthetic capture epochs are aged.

The harness exercises native reads and a native child-delete control, actual
Retain cleanup blocked by a loaded parent, safe child deletion under a pinned
parent, and native parent resume/read afterward. It also verifies native root
archive cascading, deletion of a fully due three-level chain in one invocation,
incident-edge removal, protection propagation from pinned/excluded children,
and idempotency. It does not infer execution order from ID-sorted reports.

The final local 0.1.2 build has SHA-256
`0e0e2e202063aeab1a3e81f8d59634b17b4eac3f13b0d5363937a1297cf3c578`.
[CLI related conformance](evidence/related-0.1.2/cli-related.json) and
[Desktop related conformance](evidence/related-0.1.2/desktop-related.json) each
passed 15 checks with that executable. The independent paginated harness passed
13 checks through both [CLI](evidence/related-0.1.2/cli-paginated.json) and
[Desktop](evidence/related-0.1.2/desktop-paginated.json), exercising a complete
synthetic inherited turn through native fork/revert and same-run cleanup.
`make check` passed formatting, Clippy and 186 Rust tests. The Rust suite includes
recovery coverage for repeated source-ancestor directory barriers after
interruption. Maintenance validation passed 26 release/installer tests and
7 skill tests.

No model request, credentials, real conversation deletion or production
LaunchAgent was required. These are local candidate and protocol checks;
publication and installed-version verification are separate release steps.

The [coordination limitations](architecture.md#limits-of-coordination) still
apply, including native metadata writers that can republish files outside
the shared lock protocol. Separate projection caches, logs, global history,
memories, exports and snapshots are outside the deletion scope. These changes
do not establish complete erasure or a performance improvement.
