# Architecture and failure model

## Decisions

One synchronous Rust CLI, an hourly macOS LaunchAgent, and an external sidecar
epoch tracker (`state_dir/tracker.sqlite`) alongside Codex's local `$CODEX_HOME`
storage. No async runtime, polling daemon, model calls, app-server startup, or
private DDL inside Codex's database. `doctor` and `status` optionally discover
local Codex CLI and Desktop (`/Applications/Codex.app`) binaries for version
diagnostics, without requiring any `codex` binary to be installed or on `PATH`.

App-server `thread/delete` cannot express a conditional archived-only deletion
and cascades to spawned descendants. A separate read followed by that RPC races
with restore. The local storage adapter therefore validates the structural
`threads` contract on the highest-versioned `state_<N>.sqlite` and deletes
reviewed threads and their exclusively owned rollouts under the locks that
Codex writers honor.

## Sidecar epoch tracking instead of inferred timestamps

`state_dir/tracker.sqlite` (private `0o600`, `DELETE` journal mode, `synchronous=FULL`)
owns `tracker_meta` (binding the tracker to one canonical policy owner and
`codex_home`, plus `last_synced_at`) and `tracked_epochs` (`thread_id`,
`archived_since`, `codex_archived_at`). Codex's own `state_<N>.sqlite` receives
**zero** `codex_retain_*` tables or triggers, preventing conflicts with future
Codex SQLx migrations and allowing `state_5 -> state_6` upgrades without
disabling Retain first.

Explicit activation seeds `tracker.sqlite` from Codex's current archived rows'
recorded `archived_at`. Missing dates seed a zero epoch and remain ineligible;
nonpositive or future dates are also retained. The initial cleanup uses those
dates immediately after successful policy and scheduler setup. This deliberately
trusts Codex's existing timestamp, which may have been reconstructed by native
backfill and has no provenance marker; it is not proof of continuous archive
time before activation. No chat creation date, last message or file mtime is
used as a fallback.

On every `run` (and quietly on paused `run --scheduled`, opening Codex strictly
read-only), `tracker.sync` reconciles `tracked_epochs` against Codex's current
`threads` rows using a pure transition rule shared identically with read-only
`preview`:
- Unchanged `archived_at` preserves the recorded `archived_since` epoch
  (unrelated edits like renames do not reset retention).
- Unarchived or deleted threads are pruned from `tracked_epochs`.
- Newly archived threads or threads whose `archived_at` changed (restore +
  rearchive or timestamp repair) adopt `archived_at` only if it lies in
  `[last_synced_at, observed_now]`; older repaired/backfilled timestamps
  conservatively start a fresh period at `observed_now`.
- Legacy `0.1.x` policies with `codex_retain_epochs` inside `state_5.sqlite` are
  read in-memory during read-only `status`/`preview` and seamlessly migrated to
  `tracker.sqlite` (with legacy triggers/tables removed from Codex's DB) on the
  first `run`.

Missing or tampered `tracker.sqlite` fails closed and never becomes an inferred
permission to delete. Note one inherent polling boundary of zero-DDL external
tracking: if an external writer unarchives and re-archives a thread entirely
between two syncs while preserving the exact identical old `archived_at` value,
that transition is indistinguishable from an unchanged archive; normal Codex
clients stamp a fresh `archived_at` on re-archive, which is always detected.

Expiration requires `now > archived_since + days * 86400`, avoiding an early
deletion at the timestamp's one-second precision boundary. A backwards clock
before activation/last sync is rejected; future epochs are skipped. The
operating system's wall clock must be correct: there is no independent offline
time oracle that could detect every forward clock adjustment.

## Adapter boundary

Instead of pinning one exact Codex executable version or full `sqlite_schema` /
`_sqlx_migrations` checksums (`compatibility/schema.json` and `migrations.json`
remain as synthetic baseline test fixtures), `database::discover_state_db`
selects the highest numeric `state_<N>.sqlite` in `$CODEX_HOME` and
`database::probe_schema` validates a capability-based structural contract:

- **Required core contract (fail closed if violated):** `threads` table with
  primary key `id`, plus `rollout_path`, `archived`, and `archived_at`; no
  self-referential `ON DELETE CASCADE` on `threads`; no failed or NULL rows in
  `_sqlx_migrations` (when present); `backfill_state.status == 'complete'`
  (when present and populated).
- **Dynamic optional capabilities:** adapts automatically to the presence or
  absence of `name`, `title`, `history_mode` (absent implies `legacy`),
  `is_pinned`, `thread_section_id`, `thread_sections`, `thread_spawn_edges`,
  and `idx_threads_archived`.
- **Forward-compatible safeguards:** dynamically discovers any `thread_sections`
  row named `"Pinned"` (case-insensitive) alongside the legacy `PINNED_SECTION_ID`;
  flags unknown protective columns on `threads` (names containing `pin`,
  `protect`, `keep`, `retain`, `star`, `fav`, `bookmark`, `preserve`, `hold`)
  so non-zero/non-empty/non-null values protect the thread; detects child tables
  referencing `threads` with `ON DELETE RESTRICT` or `NO ACTION` and protects
  referenced rows as `dependent_thread` before staging files; and skips unknown
  per-thread `history_mode` values cleanly.
- **Codex Desktop pin protection:** `desktop::DesktopPins` reads
  `$CODEX_HOME/.codex-global-state.json` (when present) with `O_NOFOLLOW`,
  regular-file verification, and a 16 MiB cap, protecting pinned thread UUIDs
  across known JSON representations and failing closed on unreadable, corrupt,
  or symlinked global state.

Deletion supports legacy and paginated JSONL/zstd files below both managed roots,
`sessions` and `archived_sessions`, including nested directories. The database
row's archive state and captured epoch authorize retention; a path's directory
name does not. An active row is never eligible merely because its file is in
the archive, and an archived owner's validated copy in `sessions` is not lost
from the cleanup inventory.

Legacy files require filename UUID and first-record owner identity to match.
Paginated immutable rollout IDs can differ from their stable owner after revert.
Native reverted filenames encode `OWNER_UUID_ROLLOUT_UUID`; metadata identifies
the owner while `history_base.thread_id` identifies the referenced rollout.
The scan reads bounded first records from both trees, including orphan files.
It retains every physical copy for candidate owners, including simultaneous
plain/compressed copies and copies of the same rollout in both roots. Copies
must agree on owner; their contents need not be equal. Each copy's metadata,
references, path ownership and inode are checked independently. A conflicting
owner remains an error. The selected database path must resolve to an owned copy.

Organizational parent links from rollout metadata and SQL spawn edges combine
with shared-history references in a dependency graph. Deletion proceeds from
leaves toward their prerequisites. A fully eligible family can close in one run;
a surviving child or history consumer protects the parent/base and propagates
that protection upward. Internal history links within one owner disappear with
that owner's files. Cycles stay protected. A child's outgoing link does not
grant permission to delete its parent or base. Each source must qualify itself.

Unreadable/invalid metadata, conflicting ownership, unknown formats, symlinks,
hardlinks and ambiguous path aliases stop unsafe removal. Discovery is bounded
to 500,000 entries and 1 MiB per first record, with an 8 MiB zstd decoder window.
The graph and inventory are rebuilt under writer locks and the state transaction
before staging. A run-local cache reuses parsed headers only after a fresh
non-following file stat matches device/inode, length, nanosecond mtime/ctime,
mode, owner/group and link count. Nanosecond fields can still contain coarse
clock updates, so both timestamps must precede the previous whole wall-clock
second at the start of each scan. Recent, future-dated or unknown-clock headers
are read freshly and are not cached; they can enter the cache in a later scan
after that clock window closes. This is a cache freshness rule, never archive
retention authority. New or changed paths are reopened with the
regular-file guards and parsed again; metadata from that same handle must stay
stable through the read before it can enter the cache. Every scan still walks
both trees, discovers orphan files and copies, and reconstructs dependencies.
Missing paths leave the cache after a successful scan; a failed scan clears it.
Candidate and staged-file inspections still open and validate their headers
independently. Preview does not retain the cache. See the
[inventory performance comparison](performance-0.1.3.md) for
measurements and the remaining first-scan cost. Native pins use the current
pinned-section ID and legacy bit.
See [related-thread design](related-support.md) and
[paginated ownership](paginated-support.md).

No active thread is recursively removed. Other local stores and global indexes
are retained. Deleting individually checked rows also removes their incident
spawn edges and CASCADE-owned child metadata in the same transaction; child
tables with `RESTRICT`/`NO ACTION` foreign keys protect referenced rows.

## Mutation and crash protocol

The policy directory has a nonblocking operation lock shared by policy changes,
manual and scheduled runs. A journal covers at most 128 rollout files and at most
128 eligible threads. Groups split before writing if a multi-segment group exceeds
that bound; a single owner with more than 128 files is retained. For each group:

1. Acquire Codex's `.tmp/rollout-maintenance.lock`, excluding supported
   compression and rollout migration. Busy means skip.
2. Acquire `thread-writer-locks/.coordination.lock`, then each candidate and every
   organizational ancestor's UUID writer lock. The whole guard set is capped at
   128; exceeding it retains the affected candidate. A loaded parent can therefore
   block deletion of an archived child. Shared-history sources are dependencies,
   not organizational ancestors for this guard traversal.
   Release coordination once all UUID locks are owned, matching Codex's native
   acquisition protocol. Those UUID locks remain held through finalization;
   unrelated writers can acquire their own locks while cleanup does I/O.
3. Begin an immediate SQLite transaction. Re-probe schema capabilities (recompiling
   batch SQL if optional columns or pinned sections changed), refresh Desktop
   global-state pins, and re-verify current row, sidecar epoch, native/Desktop
   pin, exclusion, history mode, path and file identity for every member. Rebuild
   parent/history/restricting-FK dependencies and verify that the fresh ancestor
   set is covered by the acquired guards. A new ancestor, pin, or surviving
   dependent blocks the group before any journal write; stale planning never
   authorizes removal.
4. Serialize the intent through a 64 KiB buffer, explicitly flush it, then
   atomically write and fsync a single `pending.json` naming every member.
   Rename those exact rollouts to hidden staging files in `archived_sessions`.
   Schema 4 records owner UUID, rollout UUID and a unique per-file slot, so
   physical copies cannot collide. Fsync the distinct set of affected source
   and staging directories.
5. Conditionally delete only those individually checked rows, remove incident
   spawn edges, and commit the group atomically with synchronous FULL.
6. Verify each staged inode and unlink it, fsync the archive once, then durably
   remove the intent. No long-term backup is created.

The row lookup and conditional deletion statements are prepared once per batch,
then rebound and executed for each member. Recovery similarly reuses its row
existence statement within the journal transaction. This reuses SQL compilation,
not query results; all per-thread guards and affected-row checks still execute.

Thread-lock cleanup reacquires coordination before closing and unlinking owned
lock files. If coordination is busy or unavailable, it closes the UUID handles
without unlinking their paths. This avoids a stale-inode race and an unbounded
wait in a destructor. Empty stale lock files can remain until a fresh Codex
coordinator performs its cleanup; they contain no conversation data. Partial
acquisition failure still uses the original coordination guard to remove only
successfully owned lock files.

A pre-effect UUID lock conflict identifies the blocked member and retries its
free neighbors in groups. Other per-member validation failures split the group
iteratively until the conflicting member is isolated. A pending intent or any
journal-write attempt stops new deletion. Global maintenance/coordination,
schema prerequisites, and SQLite write contention stop the run instead of
retrying every archive. Later unattempted entries are reported as `run_stopped`.
Directory barriers cover the whole group, preserving ordering while avoiding
six flushes for every individual chat. The rollout-owner alias check retains
only candidate logical and compressed paths. A run-scoped cache is bound to
one live SQLite connection and checks `main.data_version` inside each
`BEGIN IMMEDIATE`: external commits require a new complete metadata scan.
The common archive prefix and each plain/zstd basename are retained once;
inclusive binary-comparison bounds filter unrelated rows inside SQLite.
Validated logical spellings containing redundant separators or `.` components
retain exact SQL keys as well, including spelling changes between groups.
`total_changes` detects unexpected same-connection writes; those disable reuse
for the rest of the run, including across a rollback. Only the verified local
deletion commit is explicitly acknowledged as unable to introduce an alias.
Schema, policy, row, epoch and file checks remain live for every group; this
cache neither changes the SQLite schema nor caches deletion eligibility.
The earlier
32-item implementation replaced a one-thread implementation that required
about 25 seconds for a 1,000-chat preflight and exceeded a 300-second test limit
at 10,000. The subsequent profiling and 128-item optimization are documented
in [the cleanup performance report](cleanup-performance.md).

After interruption, recovery obtains the maintenance lock and journal owners'
writer locks before doing new cleanup.
The complete journal's thread IDs and paths are validated before touching any
member. Each physical inode is checked before its own operation; a later inode
conflict can leave an already recovered prefix, which is safe to revisit.
If a staged file remains and its row exists, restore it without overwriting
an existing destination. If the row is absent, finish removal of the already
authorized staged file. If the intent preceded the rename or cleanup completed,
clear the receipt. Partial staging, restore and unlink prefixes are restartable.
Schema 4 records a distinct slot for every physical file alongside owner and
immutable rollout IDs. Recovery validates source paths inside either managed
tree, staged identities, slots and each header's stable owner before restoring
or unlinking. A missing safe source directory may be recreated for recovery;
symlinked or otherwise unsafe ancestors are rejected. Schema 1 single-file,
schema 2 legacy group and schema 3 rollout journals remain recognized. Older
versions cannot recover schema 4: finish recovery using the newer executable
before downgrading. Earlier 32-item group journals are also recognized.
Before downgrading to a build limited to 32 items, finish recovery or disable
with the newer executable. The old reader rejects larger pending groups before
mutation; it cannot finish their recovery. Identity conflict, access failure or
an occupied restore destination preserves the receipt and data for inspection.
Only `NotFound` means absence. Config cannot switch profiles over a pending receipt.

The policy is saved disabled before scheduler installation and enabled only
after successful registration. While still holding the operation lock, `enable`
runs the same cleanup engine and saves the same bounded last-run receipt as
manual/scheduled cleanup. Initial artifact errors return exit 3 with the policy
still enabled for later retries. A setup failure does not enter cleanup.
`--no-schedule` also runs the initial cleanup. Installing a binary alone never
selects a profile or retention policy and still deletes nothing.

The enable JSON envelope is schema 2 and includes `existing_archives_assessed`,
`initial_archive_clock: "codex_archived_at"` and the full `initial_cleanup`
report; obsolete grace fields are absent. Run/preview/last-run schemas are unchanged.
Already enabled older policies retain their epochs across binary updates; only
explicit disable/re-enable adopts the historical dates. Reactivation retains
exclusions and refuses pending recovery or incomplete scheduler removal.

A failed or interrupted automatic setup must be
disabled before retrying, preventing an old LaunchAgent from activating a new
manual-only policy. Disable saves `enabled=false` first, then unregisters and
removes capture; failed later steps cannot reauthorize deletion.

## Limits of coordination

Supported Codex archive/unarchive and live resume honor the writer lock. Tests
exercise both the real binary and independent OS locks. SQLite serializes pin
and index writes against final eligibility checks. However, some unloaded
metadata appends/materializations bypass those locks after their SQL update.
They can append to an open inode or republish a compressed rollout's plain
sibling. The cleaner does not delete a new object based on an old file identity.
Observed republishing is reported; future republishing cannot be ruled out.
This is a remaining upstream coordination limitation, not a reason to cascade
through more files or claim complete erasure.

For paginated history, supported app-server/CLI fork requests first read current
state and reject archived sources; restore and revert take the source OS writer
lock. Same-store fork reservations also block archive until child persistence.
Those invariants cover ordinary operations against continuously archived threads.
They are not a global cross-process fork lease. A lower-level caller that bypasses
archive rejection, or a prepared request in another process that survives the
entire archive grace interval, is not covered. See the
[paginated source review](paginated-support.md) for evidence and this boundary.

The filesystem threat model is a user-owned local profile operated by supported
Codex clients. Arbitrary same-user hostile filesystem/SQL manipulation, foreign
writers, network filesystems, clock tampering and hardware that lies about fsync
are outside the tested contract. They are not silently treated as supported
concurrent Codex behavior. Ordinary unavailable data, recognized contention,
process termination and filesystem errors are expected and fail closed.

## Resources and reporting

The metadata query streams rows and caps the archive at 100,000. The complete
initial read finishes before deletion, so discovering an oversized archive
cannot leave a deleted prefix. When no row is initially eligible, the full
lineage scan is skipped. Otherwise it visits both session trees, capped at
500,000 entries, and reads bounded first records for ownership and dependencies,
including active and unexpired owners. It retains physical paths and identities
for candidate owners plus the graph's ownership/reference information. Cleanup
also retains file stamps and parsed header fields by physical path for reuse
between groups; it retains no header bytes or open file handles. It does not
read full transcripts. First records are limited to 1 MiB, with 16 KiB
read-ahead and an 8 MiB maximum zstd decoder window. A selective parser validates
the complete JSON record while retaining only the metadata checks; ignored
values, duplicate keys, invalid UTF-8, numbers and nesting keep the existing
JSON semantics. Adjacent file checks reuse the metadata of the same opened
handle, while inspection under writer locks and final staged identity checks
remain separate. At most 128 rollout files are staged under one intent.
Reported deletion counts cover fully completed groups; recovery reports
interrupted finalization separately and does not invent lost byte accounting.
Ordinary JSON and text commands retain one result per indexed archive; this is
O(number of archives), not an unbounded streaming claim. Scheduled runs keep
necessary candidate state during processing, but retain at most ten diagnostic
strings in initial ID order and ten warnings. The returned report releases
candidate capacity and keeps only those diagnostic entries. A separate attention
flag preserves failure status even when detail retention is capped. Pending
candidate paths, IDs, inventory and owner keys still grow with the number of
due candidates. The replace-in-place last-run
report keeps its existing schema. Stdout uses a 64 KiB buffer with explicit
flush and BrokenPipe handling, independently of the durable journal buffer.

`preview --at <RFC3339>` runs retention assessment and dependency planning at a
requested current-or-future timestamp against the profile as it exists now.
It opens the database read-only, does not age capture, write a last-run receipt,
or apply deletion. Forecast JSON uses `evaluated_at` for the requested time and
keeps `started_at` at the actual observation time; ordinary preview omits the
extra field. The forecast cannot predict future restores, pins, dependencies
or writer contention. `run` has no time override. These behaviors do not claim
an independent clock oracle or guarantee a future cleanup.

Separate paginated projection caches are not purged; the same narrow deletion
scope retains other secondary stores and global traces. No SQLite VACUUM,
global-index rewrite, snapshot deletion, or Trash accumulation
is used. File length, allocated blocks and volume free-space delta have separate
JSON fields; exact attributable physical reclamation remains null. Exit codes:
0 completed command (including ordinary policy skips), 1 operational failure,
2 CLI usage error, 3 cleanup completed with artifact errors or recovery warnings.
