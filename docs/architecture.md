# Architecture and failure model

## Decisions

One synchronous Rust CLI, an hourly macOS LaunchAgent, and a small transactional
extension to Codex's existing SQLite database. No async runtime, polling daemon,
model calls or app-server startup in a normal cleanup. The executable checks
`codex --version` but does not initialize the model or read credentials.

App-server `thread/delete` cannot express a conditional archived-only deletion
and cascades to spawned descendants. A separate read followed by that RPC races
with restore. The supported local storage adapter therefore deletes one reviewed
legacy thread under the locks that supported Codex writers already honor.

## Capture instead of inferred timestamps

`compatibility/capture.sql` owns exactly two tables and three triggers, with a
reserved `codex_retain_` prefix. `codex_retain_owner` binds the extension to one
canonical policy directory. `codex_retain_epochs` holds one row per archived
thread: ID, archive transition epoch, and the corresponding Codex timestamp.

Activation occurs in `BEGIN IMMEDIATE`: create/verify the extension and seed all
current archives with the database's current UTC second. No historical timestamp
is used to backdate their expiration. Native insertion of an already archived
thread also starts at its insertion time. Updates changing archive state,
archive timestamp or identity replace the epoch; unarchive/delete removes it.
Unrelated updates do not restart retention. Archive timestamp repairs restart
conservatively, addressing Codex's mtime-based backfill behavior.

The triggers are durable and execute in the writer's transaction, so even
restore/rearchive entirely between runs is captured. No append-only event log
is retained. The recorder adds a small indexed write to relevant transitions;
paused policies retain capture, disabled policies remove it. The capture schema,
owner, source timestamp, base schema and migration checksums are revalidated.
Missing/altered capture never becomes an inferred permission to delete.

Expiration requires `now > archived_since + days * 86400`, avoiding an early
deletion at the timestamp's one-second precision boundary. A backwards clock
before activation/last run is rejected; future epochs are skipped. The operating
system's wall clock must be correct: there is no independent offline time oracle
that could detect every forward clock adjustment.

## Adapter boundary

The immutable `compatibility/schema.json` and `migrations.json` were captured
from a new synthetic profile initialized by the actual supported CLI. Runtime
schema comparison includes tables, indexes and triggers, plus migration version,
success and checksum. This deliberately rejects unknown schema extensions and
future databases. No automatic SQL migration guesses are made.

Deletion supports only flat archived JSONL and zstd files with matching canonical
UUID filename and first-record identity and legacy history mode. A database
logical `.jsonl` path may resolve to its sole `.jsonl.zst` physical file, matching
Codex compression behavior. Multiple versions/locations, hardlinks, symlinks,
outside paths, absent/unreadable/invalid metadata and shared history markers are
skipped. Native pins use the current pinned-section ID as well as the legacy
pin flag. Every thread participating in a spawn edge is conservatively retained.

No active file or child is recursively removed. Other local stores and global
indexes are deliberately retained; deleting a selected row may cascade only
the reviewed foreign-key metadata owned by that same thread. The migration
contract does not establish support for a differently versioned desktop client.

## Mutation and crash protocol

The policy directory has a nonblocking operation lock shared by policy changes,
manual and scheduled runs. For each group of at most 128 eligible threads:

1. Acquire Codex's `.tmp/rollout-maintenance.lock`, excluding supported
   compression and rollout migration. Busy means skip.
2. Acquire `thread-writer-locks/.coordination.lock`, then each UUID writer lock.
   Release coordination once all UUID locks are owned, matching Codex's native
   acquisition protocol. Those UUID locks remain held through finalization;
   unrelated writers can acquire their own locks while cleanup does I/O.
3. Begin an immediate SQLite transaction. Verify schema, recorder, current row,
   epoch, native pin, exclusion, history mode, path and file identity for every
   member again.
4. Serialize the intent through a 64 KiB buffer, explicitly flush it, then
   atomically write and fsync a single `pending.json` naming every member.
   Rename those exact rollouts to fixed hidden staging filenames in the same
   archive directory; fsync the directory once for the group.
5. Conditionally delete only those individually checked rows and commit the
   group atomically with synchronous FULL.
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

After interruption, recovery obtains the same locks before doing new cleanup.
The complete journal's thread IDs and paths are validated before touching any
member. Each physical inode is checked before its own operation; a later inode
conflict can leave an already recovered prefix, which is safe to revisit.
If a staged file remains and its row exists, restore it without overwriting
an existing destination. If the row is absent, finish removal of the already
authorized staged file. If the intent preceded the rename or cleanup completed,
clear the receipt. Partial staging, restore and unlink prefixes are restartable.
Legacy single-file journals and earlier 32-item group journals are also recognized.
Before downgrading to a build limited to 32 items, finish recovery or disable
with the newer executable. The old reader rejects larger pending groups before
mutation; it cannot finish their recovery. Identity conflict, access failure or
an occupied restore destination preserves the receipt and data for inspection.
Only `NotFound` means absence. Config cannot switch profiles over a pending receipt.

The policy is saved disabled before scheduler installation and enabled only
after successful registration. A failed or interrupted automatic setup must be
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

The filesystem threat model is a user-owned local profile operated by supported
Codex clients. Arbitrary same-user hostile filesystem/SQL manipulation, foreign
writers, network filesystems, clock tampering and hardware that lies about fsync
are outside the tested contract. They are not silently treated as supported
concurrent Codex behavior. Ordinary unavailable data, recognized contention,
process termination and filesystem errors are expected and fail closed.

## Resources and reporting

The metadata query streams rows and caps the archive at 100,000. The complete
initial read finishes before deletion, so discovering an oversized archive
cannot leave a deleted prefix. Only initially eligible rows are retained for
file inspection. The expired-file inventory still visits and validates every
entry in both session trees, capped at 500,000 entries, but retains only
candidate IDs as missing, unique-path, or ambiguous. No transcript is read for
unexpired threads. Due files read at most a 1 MiB first record, with 16 KiB
read-ahead and an 8 MiB maximum zstd decoder window. A selective parser validates
the complete JSON record while retaining only the metadata checks; ignored
values, duplicate keys, invalid UTF-8, numbers and nesting keep the existing
JSON semantics. Adjacent file checks reuse the metadata of the same opened
handle, while inspection under writer locks and final staged identity checks
remain separate. At most 128 deletions are staged under one intent.
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

No SQLite VACUUM, global-index rewrite, snapshot deletion, or Trash accumulation
is used. File length, allocated blocks and volume free-space delta have separate
JSON fields; exact attributable physical reclamation remains null. Exit codes:
0 completed command (including ordinary policy skips), 1 operational failure,
2 CLI usage error, 3 cleanup completed with artifact errors or recovery warnings.
