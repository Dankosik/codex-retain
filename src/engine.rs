use crate::{
    config::{Policy, Store},
    database::{self, Thread},
    fsutil::{self, Identity, ThreadLocks},
    owners::RolloutOwners,
};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt, fs,
    io::{BufRead, BufReader, Read},
    ops::ControlFlow,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryReason {
    UnknownArchiveStatus,
    Excluded,
    PinnedOrUnknownPin,
    UnsupportedHistoryMode,
    RelatedThread,
    UnknownArchiveTime,
    MissingCapture,
    CaptureMismatch,
    InvalidOrFutureClock,
    WithinRetention,
    Eligible,
    Deleted,
    ChangedBusyOrError,
    UnsafeOrUnavailableArtifact,
    RunStopped,
    // Preserve unrecognized strings when reading an existing report.
    #[serde(untagged)]
    Unknown(String),
}

impl EntryReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::UnknownArchiveStatus => "unknown_archive_status",
            Self::Excluded => "excluded",
            Self::PinnedOrUnknownPin => "pinned_or_unknown_pin",
            Self::UnsupportedHistoryMode => "unsupported_history_mode",
            Self::RelatedThread => "related_thread",
            Self::UnknownArchiveTime => "unknown_archive_time",
            Self::MissingCapture => "missing_capture",
            Self::CaptureMismatch => "capture_mismatch",
            Self::InvalidOrFutureClock => "invalid_or_future_clock",
            Self::WithinRetention => "within_retention",
            Self::Eligible => "eligible",
            Self::Deleted => "deleted",
            Self::ChangedBusyOrError => "changed_busy_or_error",
            Self::UnsafeOrUnavailableArtifact => "unsafe_or_unavailable_artifact",
            Self::RunStopped => "run_stopped",
            Self::Unknown(value) => value,
        }
    }
}

impl fmt::Display for EntryReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionMode {
    Preview,
    Run,
}

impl ExecutionMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Run => "run",
        }
    }
}

#[derive(Debug)]
pub struct Assessment {
    pub reason: EntryReason,
    /// Retention boundary in Unix seconds; `now` must strictly exceed it.
    /// `None` means the capture epoch is missing or the deadline overflowed.
    pub eligible_at: Option<i64>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    #[serde(deserialize_with = "deserialize_reason_string")]
    pub reason: EntryReason,
    /// Retention boundary in Unix seconds; `now` must strictly exceed it.
    /// `None` means the capture epoch is missing or the deadline overflowed.
    pub eligible_at: Option<i64>,
    pub bytes: Option<u64>,
    pub detail: Option<String>,
}

fn deserialize_reason_string<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<EntryReason, D::Error> {
    // The report field accepts strings only, not Serde's object form for enums.
    let value = String::deserialize(deserializer)?;
    EntryReason::deserialize(serde::de::value::StringDeserializer::new(value))
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub mode: String,
    /// Run or preview start time in whole seconds since the Unix epoch.
    pub started_at: i64,
    pub examined: u64,
    pub eligible: u64,
    pub deleted: u64,
    pub skipped: u64,
    pub selected_bytes: u64,
    pub logical_bytes_removed: u64,
    pub allocated_bytes_unlinked: u64,
    pub observed_free_space_delta_bytes: Option<i64>,
    pub actual_reclaimed_bytes: Option<u64>,
    pub entries: Vec<Entry>,
    pub warnings: Vec<String>,
    #[serde(skip)]
    had_attention: bool,
    #[serde(skip)]
    diagnostic_indices: Vec<usize>,
    #[cfg(test)]
    #[serde(skip)]
    completed_batches: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReportDetail {
    Full,
    Summary,
}

impl Report {
    pub(crate) fn requires_attention(&self) -> bool {
        self.had_attention
            || self.entries.iter().any(|entry| {
                matches!(
                    entry.reason,
                    EntryReason::ChangedBusyOrError | EntryReason::UnsafeOrUnavailableArtifact
                )
            })
            || !self.warnings.is_empty()
    }

    fn new(mode: ExecutionMode, now: i64) -> Self {
        Self {
            schema: 1,
            mode: mode.as_str().into(),
            started_at: now,
            examined: 0,
            eligible: 0,
            deleted: 0,
            skipped: 0,
            selected_bytes: 0,
            logical_bytes_removed: 0,
            allocated_bytes_unlinked: 0,
            observed_free_space_delta_bytes: None,
            actual_reclaimed_bytes: None,
            entries: Vec::new(),
            warnings: Vec::new(),
            had_attention: false,
            diagnostic_indices: Vec::new(),
            #[cfg(test)]
            completed_batches: 0,
        }
    }

    fn warning(&mut self, detail: ReportDetail, message: String) {
        self.had_attention = true;
        if detail == ReportDetail::Full || self.warnings.len() < 10 {
            self.warnings.push(message);
        }
    }

    fn failure(&mut self, index: usize, reason: EntryReason, error: &str, detail: ReportDetail) {
        self.had_attention = true;
        self.entries[index].reason = reason;
        if detail == ReportDetail::Full {
            self.entries[index].detail = Some(error.into());
        } else {
            // Keep only necessary candidate state and ten diagnostic strings.
            // Earlier IDs displace errors discovered earlier in time.
            let position = self
                .diagnostic_indices
                .partition_point(|&kept| kept < index);
            if position < 10 {
                if self.diagnostic_indices.get(position) != Some(&index) {
                    self.diagnostic_indices.insert(position, index);
                    if self.diagnostic_indices.len() > 10 {
                        let removed = self.diagnostic_indices[10];
                        self.diagnostic_indices.truncate(10);
                        self.entries[removed].detail = None;
                    }
                }
                self.entries[index].detail = Some(error.into());
            }
        }
    }
}

/// Assess retention at `now`, in whole seconds since the Unix epoch.
/// Expiry requires `now` to strictly exceed the calculated retention boundary.
pub fn assess(t: &Thread, p: &Policy, now: i64) -> Assessment {
    let deadline = t.epoch.and_then(|epoch| epoch.checked_add(p.duration()));
    let reason = if t.archived != 1 {
        EntryReason::UnknownArchiveStatus
    } else if p.exclusions.contains(&t.id) {
        EntryReason::Excluded
    } else if t.pinned != 0 {
        EntryReason::PinnedOrUnknownPin
    } else if t.history_mode != "legacy" {
        EntryReason::UnsupportedHistoryMode
    } else if t.related {
        EntryReason::RelatedThread
    } else if t.archived_at.is_none() {
        EntryReason::UnknownArchiveTime
    } else if t.epoch.is_none() {
        EntryReason::MissingCapture
    } else if t.archived_at != t.recorded_archive {
        EntryReason::CaptureMismatch
    } else if t.epoch.is_some_and(|v| v <= 0 || v > now)
        || t.archived_at.is_some_and(|v| v <= 0 || v > now)
    {
        EntryReason::InvalidOrFutureClock
    } else if deadline.is_none_or(|v| now <= v) {
        EntryReason::WithinRetention
    } else {
        EntryReason::Eligible
    };
    Assessment {
        reason,
        eligible_at: deadline,
    }
}

fn rollout_id(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))?;
    if !stem.starts_with("rollout-") || stem.len() < 37 {
        return None;
    }
    let id = stem.get(stem.len() - 36..)?;
    let parsed = uuid::Uuid::parse_str(id).ok()?;
    (parsed.hyphenated().encode_lower(&mut [0; 36]) == id).then_some(id)
}

struct RolloutVariants {
    plain: PathBuf,
    compressed: PathBuf,
}

impl RolloutVariants {
    // Derive representations only; callers own identity and existence checks.
    fn from_path(path: &Path) -> Self {
        let plain = if path.extension().is_some_and(|extension| extension == "zst") {
            path.with_extension("")
        } else {
            path.to_owned()
        };
        let mut compressed = plain.as_os_str().to_owned();
        compressed.push(".zst");
        Self {
            plain,
            compressed: compressed.into(),
        }
    }
}

struct Inventory {
    paths: HashMap<String, RolloutLocation>,
}

enum RolloutLocation {
    Missing,
    Unique(PathBuf),
    Ambiguous,
}

impl Inventory {
    fn scan<'a>(home: &Path, candidates: impl Iterator<Item = &'a str>) -> Result<Self> {
        let mut paths: HashMap<String, RolloutLocation> = candidates
            .map(|id| (id.to_owned(), RolloutLocation::Missing))
            .collect();
        let mut count = 0;
        for dir in [home.join("sessions"), home.join("archived_sessions")] {
            if !dir.exists() {
                continue;
            }
            let meta = fs::symlink_metadata(&dir)?;
            ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "session directory is not a real directory"
            );
            for entry in walkdir::WalkDir::new(&dir).follow_links(false).max_open(16) {
                let entry = entry.context("cannot inspect all session paths")?;
                count += 1;
                ensure!(
                    count <= 500000,
                    "session inventory exceeds the 500,000-entry safety limit"
                );
                ensure!(
                    !entry.file_type().is_symlink(),
                    "session inventory contains a symlink; resolve it before cleanup"
                );
                if let Some(id) = entry.file_name().to_str().and_then(rollout_id)
                    && let Some(location) = paths.get_mut(id)
                {
                    *location = match location {
                        RolloutLocation::Missing => RolloutLocation::Unique(entry.into_path()),
                        _ => RolloutLocation::Ambiguous,
                    };
                }
            }
        }
        Ok(Self { paths })
    }

    fn unique_path(&self, id: &str) -> Option<&Path> {
        match self.paths.get(id) {
            Some(RolloutLocation::Unique(path)) => Some(path),
            _ => None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Artifact {
    path: PathBuf,
    identity: Identity,
    bytes: u64,
    allocated: u64,
}

struct Candidate {
    report_index: usize,
    path: PathBuf,
}

fn inspect(id: &str, logical_path: &Path, p: &Policy, inventory: &Inventory) -> Result<Artifact> {
    let parsed = uuid::Uuid::parse_str(id)?;
    ensure!(
        parsed.hyphenated().encode_lower(&mut [0; 36]) == id,
        "noncanonical thread ID"
    );
    let archive = p.codex_home.join("archived_sessions");
    ensure!(
        logical_path.parent() == Some(archive.as_path()),
        "rollout path is outside the flat local archive"
    );
    let name = logical_path
        .file_name()
        .and_then(|v| v.to_str())
        .context("unsupported rollout filename")?;
    ensure!(
        rollout_id(name) == Some(id),
        "filename and thread identity differ"
    );
    let location = inventory.paths.get(id).context("rollout file is missing")?;
    ensure!(
        !matches!(location, RolloutLocation::Ambiguous),
        "multiple rollout paths share the thread ID"
    );
    let path = inventory
        .unique_path(id)
        .context("rollout file is missing")?;
    let compressed = PathBuf::from(format!("{}.zst", logical_path.display()));
    ensure!(
        path == logical_path || path == compressed,
        "rollout inventory disagrees with the database path"
    );
    let variants = RolloutVariants::from_path(path);
    ensure!(
        !(fsutil::path_exists(&variants.plain)? && fsutil::path_exists(&variants.compressed)?),
        "plain and compressed rollouts both exist"
    );
    let (file, metadata) = fsutil::regular_with_metadata(path, false, false)?;
    // Only the first record is needed. Bound decompressed metadata as well as
    // read-ahead; never scan a transcript to infer the retention clock.
    let reader: Box<dyn Read> = if path.extension().is_some_and(|v| v == "zst") {
        let mut decoder =
            zstd::stream::read::Decoder::with_buffer(BufReader::with_capacity(16384, file))?;
        // Cap the decoder window at 2^23 bytes (8 MiB) before allocation,
        // separately from the 1 MiB decompressed first-record limit below.
        decoder.window_log_max(23)?;
        Box::new(decoder)
    } else {
        Box::new(file)
    };
    let mut line = Vec::new();
    BufReader::with_capacity(16384, reader.take(1024 * 1024 + 1)).read_until(b'\n', &mut line)?;
    ensure!(
        line.len() <= 1024 * 1024 && line.last() == Some(&b'\n'),
        "missing or oversized rollout metadata"
    );
    crate::metadata::validate_metadata(&line, id)?;
    Ok(Artifact {
        path: path.to_owned(),
        identity: Identity::of(&metadata),
        bytes: metadata.len(),
        allocated: metadata.blocks().saturating_mul(512),
    })
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    schema: u32,
    owner: String,
    thread_id: String,
    artifact: Artifact,
    staged: PathBuf,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PendingItem {
    thread_id: String,
    artifact: Artifact,
    staged: PathBuf,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BatchIntent {
    schema: u32,
    owner: String,
    items: Vec<PendingItem>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum Journal {
    Batch(BatchIntent),
    Single(Intent),
}
impl Journal {
    fn into_batch(self) -> Result<BatchIntent> {
        match self {
            Self::Batch(batch) => {
                ensure!(batch.schema == 2, "unrecognized pending deletion schema");
                Ok(batch)
            }
            Self::Single(one) => {
                ensure!(one.schema == 1, "unrecognized pending deletion schema");
                Ok(BatchIntent {
                    schema: 2,
                    owner: one.owner,
                    items: vec![PendingItem {
                        thread_id: one.thread_id,
                        artifact: one.artifact,
                        staged: one.staged,
                    }],
                })
            }
        }
    }
}
fn intent_path(store: &Store) -> PathBuf {
    store.root.join("pending.json")
}
fn stage_path(home: &Path, id: &str) -> PathBuf {
    home.join("archived_sessions")
        .join(format!(".codex-retain-pending-{id}"))
}
fn archive_directory(p: &Policy) -> Result<PathBuf> {
    let archive = p.codex_home.join("archived_sessions");
    let meta = fs::symlink_metadata(&archive)
        .context("archive directory unavailable; cleanup cannot proceed")?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "archive directory must not be a symlink"
    );
    Ok(archive)
}
fn validate_intent(intent: &BatchIntent, p: &Policy) -> Result<()> {
    ensure!(
        intent.owner == p.owner,
        "unrecognized pending deletion owner"
    );
    ensure!(
        !intent.items.is_empty() && intent.items.len() <= fsutil::MAX_BATCH_THREADS,
        "invalid pending batch size"
    );
    let mut ids = std::collections::HashSet::new();
    for item in &intent.items {
        ensure!(
            uuid::Uuid::parse_str(&item.thread_id)?.to_string() == item.thread_id,
            "invalid pending thread ID"
        );
        ensure!(ids.insert(&item.thread_id), "duplicate pending thread ID");
        ensure!(
            item.staged == stage_path(&p.codex_home, &item.thread_id),
            "invalid pending staging path"
        );
        ensure!(
            item.artifact.path.parent() == Some(p.codex_home.join("archived_sessions").as_path()),
            "invalid recovery source path"
        );
        ensure!(
            item.artifact
                .path
                .file_name()
                .and_then(|v| v.to_str())
                .and_then(rollout_id)
                == Some(item.thread_id.as_str()),
            "invalid recovery source identity"
        );
    }
    Ok(())
}

/// Every member is named durably before any move. Recovery is restartable even
/// if only a prefix was staged, restored, or unlinked when the process stopped.
pub fn recover(c: &mut Connection, p: &Policy, store: &Store) -> Result<Option<String>> {
    let path = intent_path(store);
    if !fsutil::path_exists(&path)? {
        return Ok(None);
    }
    let intent: Journal = fsutil::read_json(&path)?;
    let intent = intent.into_batch()?;
    validate_intent(&intent, p)?; // Validate the entire receipt before touching any member.
    let archive = archive_directory(p)?;
    let _maintenance = fsutil::maintenance(&p.codex_home)?;
    let ids: Vec<&str> = intent.items.iter().map(|v| v.thread_id.as_str()).collect();
    let _writers = ThreadLocks::acquire(&p.codex_home, &ids)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    database::verify_base(&tx)?;
    database::verify_policy(&tx, p)?;
    let mut restored = 0;
    let mut finished = 0;
    let mut exists_query = tx.prepare("SELECT EXISTS(SELECT 1 FROM threads WHERE id=?)")?;
    for item in &intent.items {
        let exists: bool = exists_query.query_row([&item.thread_id], |r| r.get(0))?;
        if !fsutil::path_exists(&item.staged)? {
            continue;
        }
        let (file, metadata) = fsutil::regular_with_metadata(&item.staged, false, false)?;
        ensure!(
            Identity::of(&metadata) == item.artifact.identity,
            "staged file identity changed; manual recovery required"
        );
        drop(file);
        if exists {
            fsutil::rename_without_overwrite_unsynced(&item.staged, &item.artifact.path)?;
            restored += 1;
        } else {
            fs::remove_file(&item.staged)?;
            finished += 1;
        }
    }
    drop(exists_query);
    fsutil::sync_dir(&archive)?;
    tx.commit()?;
    fsutil::remove_durable(&path)?;
    Ok(Some(format!(
        "recovered pending deletion: restored {restored}, finished {finished}; byte accounting unavailable after interruption"
    )))
}

#[derive(Debug)]
struct GlobalCleanupFailure;

impl fmt::Display for GlobalCleanupFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("shared cleanup prerequisite unavailable")
    }
}

struct Cleanup<'run, 'connection> {
    policy: &'run Policy,
    store: &'run Store,
    now: i64,
    inventory: &'run Inventory,
    owners: RolloutOwners<'connection>,
    detail: ReportDetail,
    #[cfg(test)]
    completed_batches: usize,
}

impl Cleanup<'_, '_> {
    fn delete_batch(
        &mut self,
        c: &Connection,
        ids: &[&str],
        journal_write_attempted: &mut bool,
    ) -> Result<Vec<Artifact>> {
        let p = self.policy;
        let store = self.store;
        let now = self.now;
        let inventory = self.inventory;
        let archive = archive_directory(p).context(GlobalCleanupFailure)?;
        let _maintenance = fsutil::maintenance(&p.codex_home)?;
        let _writers = ThreadLocks::acquire(&p.codex_home, ids)?;
        // The connection is shared immutably with the owner cache for this run.
        // SQLite still rejects nesting, and no initial SELECT remains open here.
        let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)
            .context(GlobalCleanupFailure)?;
        database::verify_base(&tx).context(GlobalCleanupFailure)?;
        database::verify_policy(&tx, p).context(GlobalCleanupFailure)?;
        let mut threads = Vec::with_capacity(ids.len());
        let mut intent = BatchIntent {
            schema: 2,
            owner: p.owner.clone(),
            items: Vec::with_capacity(ids.len()),
        };
        let mut thread_query = database::prepare_thread_lookup(&tx)?;
        for &id in ids {
            let thread = database::lookup_thread(&mut thread_query, id)?
                .context("thread was removed concurrently")?;
            let assessment = assess(&thread, p, now);
            ensure!(
                assessment.reason == EntryReason::Eligible,
                "thread {id} is no longer eligible: {}",
                assessment.reason
            );
            let artifact = inspect(&thread.id, &thread.path, p, inventory)?;
            let staged = stage_path(&p.codex_home, id);
            ensure!(
                !fsutil::path_exists(&staged)?,
                "an untracked staged file exists; inspect it before cleanup"
            );
            intent.items.push(PendingItem {
                thread_id: id.into(),
                artifact,
                staged,
            });
            threads.push(thread);
        }
        drop(thread_query);
        verify_rollout_owners(&tx, &threads, &intent.items, &mut self.owners)?;
        let delete_query = database::prepare_delete(&tx)?;
        // One durable intent covers all names; directory barriers cover all moves
        // in this small group. Eligibility checks and SQLite commit remain atomic.
        *journal_write_attempted = true;
        fsutil::atomic_json(&intent_path(store), &intent)?;
        for item in &intent.items {
            fsutil::rename_without_overwrite_unsynced(&item.artifact.path, &item.staged)?;
        }
        fsutil::sync_dir(&archive)?;
        database::delete_rows(delete_query, &threads)?;
        tx.commit()?;
        self.owners.acknowledge_local_deletes();
        for item in &mut intent.items {
            let (file, meta) = fsutil::regular_with_metadata(&item.staged, false, false)?;
            ensure!(
                Identity::of(&meta) == item.artifact.identity,
                "staged file changed; cleanup stopped"
            );
            item.artifact.bytes = meta.len();
            item.artifact.allocated = meta.blocks().saturating_mul(512);
            drop(file);
            fs::remove_file(&item.staged)?;
        }
        fsutil::sync_dir(&archive)?;
        fsutil::remove_durable(&intent_path(store))?;
        Ok(intent.items.into_iter().map(|item| item.artifact).collect())
    }
}

fn verify_rollout_owners(
    tx: &rusqlite::Transaction<'_>,
    threads: &[Thread],
    items: &[PendingItem],
    cache: &mut RolloutOwners<'_>,
) -> Result<()> {
    let mut owners = HashMap::new();
    for (thread, item) in threads.iter().zip(items) {
        let variants = RolloutVariants::from_path(&item.artifact.path);
        if thread.path.as_os_str() != variants.plain.as_os_str()
            && thread.path.as_os_str() != variants.compressed.as_os_str()
        {
            // Path equality deliberately accepts redundant separators and
            // `.` components. Retain the exact live SQL spelling as an alias
            // guard too, including spelling changes since the initial read.
            cache.register_exact(thread.path.to_str().context("non-UTF-8 path")?, &thread.id);
        }
        for path in [&thread.path, &variants.plain, &variants.compressed] {
            let path = path.to_str().context("non-UTF-8 path")?;
            if let Some(previous) = owners.insert(path.to_owned(), thread.id.as_str()) {
                ensure!(
                    previous == thread.id,
                    "multiple candidate threads reference one rollout"
                );
            }
        }
    }
    cache.verify(tx, &owners)
}

impl Cleanup<'_, '_> {
    fn apply_group(
        &mut self,
        c: &Connection,
        report: &mut Report,
        indices: &[usize],
    ) -> Result<ControlFlow<()>> {
        let mut remaining: Vec<_> = std::iter::once(0..indices.len()).collect();
        while let Some(range) = remaining.pop() {
            let ids: Vec<&str> = indices[range.clone()]
                .iter()
                .map(|&index| report.entries[index].id.as_str())
                .collect();
            let mut journal_write_attempted = false;
            let outcome = self.delete_batch(c, &ids, &mut journal_write_attempted);
            drop(ids);
            match outcome {
                Ok(artifacts) => {
                    #[cfg(test)]
                    {
                        self.completed_batches += 1;
                    }
                    for (&index, artifact) in indices[range].iter().zip(artifacts) {
                        report.entries[index].reason = EntryReason::Deleted;
                        report.eligible += 1;
                        report.deleted += 1;
                        report.logical_bytes_removed += artifact.bytes;
                        report.allocated_bytes_unlinked += artifact.allocated;
                    }
                }
                Err(error) => {
                    let pending = fsutil::path_exists(&intent_path(self.store))?;
                    let database_busy = error.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
                        matches!(error, rusqlite::Error::SqliteFailure(code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
                    });
                    let lock_scope = error
                        .downcast_ref::<fsutil::LockAcquisitionError>()
                        .map(|error| &error.scope);
                    let global = error.is::<GlobalCleanupFailure>()
                        || matches!(lock_scope, Some(fsutil::LockScope::Global));
                    let can_retry =
                        !journal_write_attempted && !pending && !database_busy && !global;
                    if can_retry && range.len() > 1 {
                        if let Some(fsutil::LockScope::Thread(blocked_id)) = lock_scope
                            && let Some(offset) = indices[range.clone()]
                                .iter()
                                .position(|&index| report.entries[index].id == *blocked_id)
                        {
                            let blocked = range.start + offset;
                            report.failure(
                                indices[blocked],
                                EntryReason::ChangedBusyOrError,
                                &format!("{error:#}"),
                                self.detail,
                            );
                            // Keep both free sides grouped; the failed member's
                            // lock remains owned by its external writer.
                            if blocked + 1 < range.end {
                                remaining.push(blocked + 1..range.end);
                            }
                            if range.start < blocked {
                                remaining.push(range.start..blocked);
                            }
                            continue;
                        }
                        // Validation failures may concern one unidentified
                        // member. Split iteratively within the original batch.
                        let middle = range.start + range.len() / 2;
                        remaining.push(middle..range.end);
                        remaining.push(range.start..middle);
                        continue;
                    }
                    let message = format!("{error:#}");
                    for &index in &indices[range] {
                        report.failure(
                            index,
                            EntryReason::ChangedBusyOrError,
                            &message,
                            self.detail,
                        );
                    }
                    if pending {
                        report.warning(self.detail, "cleanup stopped with a durable pending group; next run recovers it first; counts cover completed groups".into());
                    } else if journal_write_attempted {
                        report.warning(self.detail, "group finalization failed after journal writing began; effects may have occurred; stopped with counts for earlier completed groups only".into());
                    }
                    if database_busy {
                        report.warning(self.detail, "Codex database is busy; stopped this run instead of retrying every archive".into());
                    } else if global {
                        report.warning(self.detail, "shared cleanup prerequisites are unavailable; stopped this run instead of retrying every archive".into());
                    }
                    if !can_retry {
                        return Ok(ControlFlow::Break(()));
                    }
                }
            }
        }
        Ok(ControlFlow::Continue(()))
    }
}

/// Preview or run retention at `now`, in whole seconds since the Unix epoch.
pub fn execute(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
    mode: ExecutionMode,
) -> Result<Report> {
    execute_with_detail(c, p, store, now, mode, ReportDetail::Full)
}

/// Run scheduled retention at `now`, in whole seconds since the Unix epoch.
pub(crate) fn execute_scheduled(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
) -> Result<Report> {
    execute_with_detail(c, p, store, now, ExecutionMode::Run, ReportDetail::Summary)
}

fn execute_with_detail(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
    mode: ExecutionMode,
    detail: ReportDetail,
) -> Result<Report> {
    let apply = mode == ExecutionMode::Run;
    ensure!(
        p.enabled,
        "policy is disabled; run enable to start a full grace period"
    );
    ensure!(
        now >= p.enabled_at,
        "system clock moved before policy activation"
    );
    database::verify_policy(c, p)?;
    let mut report = Report::new(mode, now);
    if apply {
        ensure!(
            !p.paused,
            "policy is paused; resume it before a cleanup run"
        );
        if let Some(message) = recover(c, p, store)? {
            report.warning(detail, message);
        }
    } else if fsutil::path_exists(&intent_path(store))? {
        report.warning(
            detail,
            "pending recovery; run cleanup or disable before relying on this preview".into(),
        );
    }

    let mut candidates = Vec::new();
    database::visit_archived(c, |thread| {
        report.examined += 1;
        let assessment = assess(&thread, p, now);
        let eligible = assessment.reason == EntryReason::Eligible;
        if detail == ReportDetail::Full || eligible {
            let index = report.entries.len();
            report.entries.push(Entry {
                id: thread.id,
                title: thread.title,
                reason: assessment.reason,
                eligible_at: assessment.eligible_at,
                bytes: None,
                detail: None,
            });
            if eligible {
                candidates.push(Candidate {
                    report_index: index,
                    path: thread.path,
                });
            }
        }
        if !eligible {
            report.skipped += 1;
        }
        Ok(())
    })?;
    // Finish the entire bounded snapshot before deletion, including the
    // 100,001st-row guard. Only candidate paths survive alongside report state.
    let inventory = if candidates.is_empty() {
        Inventory {
            paths: HashMap::new(),
        }
    } else {
        Inventory::scan(
            &p.codex_home,
            candidates
                .iter()
                .map(|candidate| report.entries[candidate.report_index].id.as_str()),
        )?
    };
    let before = apply
        .then(|| fsutil::volume_available(&p.codex_home))
        .flatten();
    let mut due = Vec::with_capacity(if apply { candidates.len() } else { 0 });
    // An empty prefix preserves full keys when a native profile path is not
    // UTF-8; it does not change filesystem path operations or skip checks.
    let archive_prefix = p
        .codex_home
        .join("archived_sessions")
        .to_str()
        .map(|path| format!("{path}/"))
        .unwrap_or_default();
    let mut owners = RolloutOwners::new(c, &archive_prefix);
    for candidate in candidates {
        let index = candidate.report_index;
        match inspect(&report.entries[index].id, &candidate.path, p, &inventory) {
            Ok(artifact) => {
                report.entries[index].bytes = Some(artifact.bytes);
                report.selected_bytes += artifact.bytes;
                if apply {
                    owners.register(
                        artifact
                            .path
                            .into_os_string()
                            .into_string()
                            .map_err(|_| anyhow::anyhow!("non-UTF-8 path"))?,
                        &report.entries[index].id,
                    );
                    due.push(index);
                } else {
                    report.eligible += 1;
                }
            }
            Err(error) => {
                report.skipped += 1;
                report.failure(
                    index,
                    EntryReason::UnsafeOrUnavailableArtifact,
                    &format!("{error:#}"),
                    detail,
                );
            }
        }
    }
    if apply {
        let mut cleanup = Cleanup {
            policy: p,
            store,
            now,
            inventory: &inventory,
            owners,
            detail,
            #[cfg(test)]
            completed_batches: 0,
        };
        for indices in due.chunks(fsutil::MAX_BATCH_THREADS) {
            if cleanup.apply_group(c, &mut report, indices)?.is_break() {
                for entry in report
                    .entries
                    .iter_mut()
                    .filter(|entry| entry.reason == EntryReason::Eligible)
                {
                    entry.reason = EntryReason::RunStopped;
                }
                break;
            }
        }
        #[cfg(test)]
        {
            report.completed_batches = cleanup.completed_batches;
        }
        drop(cleanup);
        report.skipped = report.examined.saturating_sub(report.deleted);
        report.observed_free_space_delta_bytes = before
            .zip(fsutil::volume_available(&p.codex_home))
            .and_then(|(b, a)| i64::try_from(i128::from(a) - i128::from(b)).ok());
        // Check after all groups so a later republish is still reported.
        for index in due {
            let entry = &report.entries[index];
            if entry.reason == EntryReason::Deleted
                && let Some(path) = inventory.unique_path(&entry.id)
            {
                let variants = RolloutVariants::from_path(path);
                if variants.plain.exists() || variants.compressed.exists() {
                    report.warning(
                        detail,
                        format!("{}: Codex republished a rollout; preserved it", entry.id),
                    );
                }
            }
        }
    }
    if detail == ReportDetail::Summary {
        // Release candidate capacity as well as values; the returned report is
        // bounded even when every initial candidate was due or unavailable.
        let mut diagnostics = Vec::with_capacity(report.diagnostic_indices.len());
        diagnostics.extend(
            report
                .entries
                .into_iter()
                .filter(|entry| entry.detail.is_some()),
        );
        report.entries = diagnostics;
        report.diagnostic_indices.clear();
    }
    Ok(report)
}

#[cfg(test)]
#[allow(dead_code)]
#[path = "../tests/support/mod.rs"]
mod test_support;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reason_tokens_keep_the_report_schema_and_attention_policy() {
        let cases = [
            (
                EntryReason::UnknownArchiveStatus,
                "unknown_archive_status",
                false,
            ),
            (EntryReason::Excluded, "excluded", false),
            (
                EntryReason::PinnedOrUnknownPin,
                "pinned_or_unknown_pin",
                false,
            ),
            (
                EntryReason::UnsupportedHistoryMode,
                "unsupported_history_mode",
                false,
            ),
            (EntryReason::RelatedThread, "related_thread", false),
            (
                EntryReason::UnknownArchiveTime,
                "unknown_archive_time",
                false,
            ),
            (EntryReason::MissingCapture, "missing_capture", false),
            (EntryReason::CaptureMismatch, "capture_mismatch", false),
            (
                EntryReason::InvalidOrFutureClock,
                "invalid_or_future_clock",
                false,
            ),
            (EntryReason::WithinRetention, "within_retention", false),
            (EntryReason::Eligible, "eligible", false),
            (EntryReason::Deleted, "deleted", false),
            (
                EntryReason::ChangedBusyOrError,
                "changed_busy_or_error",
                true,
            ),
            (
                EntryReason::UnsafeOrUnavailableArtifact,
                "unsafe_or_unavailable_artifact",
                true,
            ),
            (EntryReason::RunStopped, "run_stopped", false),
        ];
        for (reason, token, attention) in cases {
            let wire = serde_json::json!({
                "id": "synthetic", "title": "fixture", "reason": token,
                "eligible_at": 42, "bytes": 123, "detail": null,
            });
            let entry: Entry = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(entry.reason, reason, "deserialize {token}");
            assert_eq!(entry.reason.to_string(), token, "display {token}");
            assert_eq!(
                serde_json::to_value(&entry).unwrap(),
                wire,
                "serialize {token}"
            );
            let mut report = Report::new(ExecutionMode::Run, 42);
            report.entries.push(entry);
            assert_eq!(
                report.requires_attention(),
                attention,
                "attention for {token}"
            );
            report.warnings.push("synthetic recovery warning".into());
            assert!(
                report.requires_attention(),
                "warnings still require attention"
            );
        }
    }

    #[test]
    fn unknown_report_reason_is_preserved_without_becoming_eligible() {
        let wire = serde_json::json!({
            "id": "synthetic", "title": "fixture", "reason": "future_reason",
            "eligible_at": null, "bytes": null, "detail": null,
        });
        let entry: Entry = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(entry.reason, EntryReason::Unknown("future_reason".into()));
        assert_ne!(entry.reason, EntryReason::Eligible);
        assert_eq!(serde_json::to_value(&entry).unwrap(), wire);
        let mut report = Report::new(ExecutionMode::Preview, 42);
        report.entries.push(entry);
        assert!(!report.requires_attention());
        assert_eq!(report.mode, "preview");
        assert_eq!(Report::new(ExecutionMode::Run, 42).mode, "run");
    }

    #[test]
    fn report_reason_still_rejects_non_string_json() {
        for reason in [
            serde_json::json!(null),
            serde_json::json!(1),
            serde_json::json!([]),
            serde_json::json!({"eligible": null}),
        ] {
            let wire = serde_json::json!({
                "id": "synthetic", "title": "fixture", "reason": reason,
                "eligible_at": null, "bytes": null, "detail": null,
            });
            assert!(serde_json::from_value::<Entry>(wire).is_err());
        }
    }

    #[test]
    fn rollout_variants_share_paths_without_losing_native_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let plain = Path::new(std::ffi::OsStr::from_bytes(b"/archive-\xff/rollout.jsonl"));
        let compressed = Path::new(std::ffi::OsStr::from_bytes(
            b"/archive-\xff/rollout.jsonl.zst",
        ));
        for path in [plain, compressed] {
            let variants = RolloutVariants::from_path(path);
            assert_eq!(variants.plain, plain);
            assert_eq!(variants.compressed, compressed);
        }
    }

    #[test]
    fn inventory_retains_candidates_and_still_checks_unrelated_paths() {
        let fixture = test_support::Fixture::new();
        let due = fixture.add(1, 1);
        for number in 2..=100 {
            fixture.add(number, 0);
        }
        let inventory = Inventory::scan(&fixture.home, std::iter::once(due.as_str())).unwrap();
        assert_eq!(inventory.paths.len(), 1);
        assert_eq!(
            inventory.unique_path(&due),
            Some(fixture.path(&due, true).as_path())
        );

        let duplicate = fixture
            .home
            .join("sessions")
            .join(fixture.path(&due, true).file_name().unwrap());
        fs::write(&duplicate, b"duplicate identity").unwrap();
        let inventory = Inventory::scan(&fixture.home, std::iter::once(due.as_str())).unwrap();
        assert!(matches!(
            inventory.paths.get(&due),
            Some(RolloutLocation::Ambiguous)
        ));

        std::os::unix::fs::symlink("missing", fixture.home.join("sessions/unrelated-link"))
            .unwrap();
        assert!(Inventory::scan(&fixture.home, std::iter::once(due.as_str())).is_err());
    }

    #[test]
    fn scheduled_noop_retains_no_per_archive_entries() {
        let mut fixture = test_support::Fixture::new();
        for number in 1..=1000 {
            fixture.add(number, 1);
        }
        let report = execute_scheduled(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            test_support::now(),
        )
        .unwrap();
        assert_eq!(report.examined, 1000);
        assert_eq!(report.skipped, 1000);
        assert_eq!(report.deleted, 0);
        assert!(report.entries.is_empty());
        assert_eq!(report.entries.capacity(), 0);
        assert!(!report.requires_attention());
        assert_eq!(report.completed_batches, 0);
    }

    #[test]
    fn scheduled_errors_keep_first_ten_ids_even_when_earlier_failure_is_discovered_late() {
        let mut fixture = test_support::Fixture::new();
        let mut ids = Vec::new();
        for number in 1..=25 {
            let id = fixture.add(number, 1);
            fixture.age(&id);
            if number >= 15 {
                fs::remove_file(fixture.path(&id, true)).unwrap();
            }
            ids.push(id);
        }
        let _writer = fsutil::ThreadLock::acquire(&fixture.home, &ids[0]).unwrap();
        let report = execute_scheduled(
            &mut fixture.c,
            &fixture.policy,
            &fixture.store,
            test_support::now(),
        )
        .unwrap();
        let expected: Vec<&str> = std::iter::once(ids[0].as_str())
            .chain(ids[14..23].iter().map(String::as_str))
            .collect();
        assert_eq!(
            report
                .entries
                .iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(report.entries[0].reason, EntryReason::ChangedBusyOrError);
        assert!(report.entries.capacity() <= 10);
        assert!(
            report.entries[1..]
                .iter()
                .all(|entry| entry.reason == EntryReason::UnsafeOrUnavailableArtifact)
        );
        assert!(
            report
                .entries
                .iter()
                .all(|entry| entry.title == "fixture" && entry.detail.is_some())
        );
        assert_eq!(report.deleted, 13);
        assert_eq!(report.skipped, 12);
        assert!(report.requires_attention());
        assert!(fixture.exists(&ids[0]));
    }

    #[test]
    fn a_known_busy_member_keeps_free_peers_in_at_most_two_committed_groups() {
        for blocked in [
            0,
            fsutil::MAX_BATCH_THREADS / 2,
            fsutil::MAX_BATCH_THREADS - 1,
        ] {
            let mut fixture = test_support::Fixture::new();
            let ids: Vec<_> = (1..=fsutil::MAX_BATCH_THREADS)
                .map(|number| {
                    let id = fixture.add(number as u128, 1);
                    fixture.age(&id);
                    id
                })
                .collect();
            let _writer = fsutil::ThreadLock::acquire(&fixture.home, &ids[blocked]).unwrap();
            let report = fixture.run(true).unwrap();
            assert_eq!(report.deleted, 127);
            assert_eq!(report.skipped, 1);
            assert_eq!(
                report.completed_batches,
                if blocked == 0 || blocked == 127 { 1 } else { 2 }
            );
            for (index, id) in ids.iter().enumerate() {
                assert_eq!(fixture.exists(id), index == blocked);
                assert_eq!(fixture.path(id, true).exists(), index == blocked);
            }
        }
    }

    #[test]
    fn alias_conflict_splits_groups_without_stranding_safe_neighbors() {
        let mut fixture = test_support::Fixture::new();
        let ids: Vec<_> = (1..=128)
            .map(|number| {
                let id = fixture.add(number, 1);
                fixture.age(&id);
                id
            })
            .collect();
        let active = fixture.add(129, 0);
        fixture
            .c
            .execute(
                "UPDATE threads SET rollout_path=? WHERE id=?",
                rusqlite::params![fixture.path(&ids[0], true).to_str().unwrap(), active],
            )
            .unwrap();
        let report = fixture.run(true).unwrap();
        assert_eq!(report.deleted, 127);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.completed_batches, 7);
        assert!(fixture.exists(&ids[0]) && fixture.path(&ids[0], true).exists());
        assert!(fixture.exists(&active) && fixture.path(&active, false).exists());
    }

    #[test]
    fn equivalent_logical_path_spellings_keep_deletion_and_exact_alias_guards() {
        for dot_component in [false, true] {
            for compressed in [false, true] {
                for alias in [false, true] {
                    let mut fixture = test_support::Fixture::new();
                    let id = fixture.add(1, 1);
                    fixture.age(&id);
                    let plain = fixture.path(&id, true);
                    let original = fs::read(&plain).unwrap();
                    let physical = if compressed {
                        let path = plain.with_extension("jsonl.zst");
                        fs::write(
                            &path,
                            zstd::stream::encode_all(original.as_slice(), 1).unwrap(),
                        )
                        .unwrap();
                        fs::remove_file(&plain).unwrap();
                        path
                    } else {
                        plain.clone()
                    };
                    let name = plain.file_name().unwrap().to_str().unwrap();
                    let logical = if dot_component {
                        format!("{}/archived_sessions/./{name}", fixture.home.display())
                    } else {
                        format!("{}//archived_sessions/{name}", fixture.home.display())
                    };
                    fixture
                        .c
                        .execute(
                            "UPDATE threads SET rollout_path=? WHERE id=?",
                            rusqlite::params![logical, id],
                        )
                        .unwrap();
                    if alias {
                        let active = fixture.add(2, 0);
                        fixture
                            .c
                            .execute(
                                "UPDATE threads SET rollout_path=? WHERE id=?",
                                rusqlite::params![logical, active],
                            )
                            .unwrap();
                    }
                    let report = fixture.run(true).unwrap();
                    assert_eq!(
                        report.deleted,
                        u64::from(!alias),
                        "{logical}, compressed={compressed}, alias={alias}"
                    );
                    assert_eq!(fixture.exists(&id), alias);
                    assert_eq!(physical.exists(), alias);
                    if alias {
                        assert!(
                            report.entries[0]
                                .detail
                                .as_ref()
                                .unwrap()
                                .contains("another thread references")
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn global_lock_failure_stops_before_attempting_later_groups() {
        let mut fixture = test_support::Fixture::new();
        for number in 1..=fsutil::MAX_BATCH_THREADS + 1 {
            let id = fixture.add(number as u128, 1);
            fixture.age(&id);
        }
        let _maintenance = fsutil::maintenance(&fixture.home).unwrap();
        let report = fixture.run(true).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(report.completed_batches, 0);
        assert_eq!(
            report.entries.last().unwrap().reason,
            EntryReason::RunStopped
        );
        assert_eq!(report.skipped, report.examined);
        assert!(!fixture.store.root.join("pending.json").exists());
    }

    #[test]
    fn oversized_archive_is_rejected_before_removing_the_first_due_row() {
        let mut fixture = test_support::Fixture::new();
        let due = fixture.add(1, 1);
        fixture.age(&due);
        let original = fs::read(fixture.path(&due, true)).unwrap();
        fixture.c.execute_batch("WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM numbers WHERE n<100000) INSERT INTO threads(id,rollout_path,created_at,updated_at,source,model_provider,cwd,title,sandbox_policy,approval_mode,archived,archived_at) SELECT printf('limit-%06d',n),'/not-inspected',1,1,'cli','fixture','/fixture','x','{}','never',1,1 FROM numbers;").unwrap();
        let error = fixture.run(true).unwrap_err();
        assert!(error.to_string().contains("100,000-thread safety limit"));
        assert!(fixture.exists(&due));
        assert_eq!(fs::read(fixture.path(&due, true)).unwrap(), original);
        assert!(!fixture.store.root.join("pending.json").exists());
    }

    #[test]
    fn journal_unlink_sync_failure_never_retries_an_effectful_group() {
        let mut fixture = test_support::Fixture::new();
        let ids: Vec<_> = (1..=fsutil::MAX_BATCH_THREADS + 1)
            .map(|n| {
                let id = fixture.add(n as u128, 1);
                fixture.age(&id);
                id
            })
            .collect();
        fsutil::fail_journal_clear_sync_once(fixture.store.root.clone());
        let report = fixture.run(true).unwrap();
        assert_eq!(
            report.deleted, 0,
            "uncertain group completion must not invent accounting"
        );
        assert!(
            report
                .warnings
                .iter()
                .any(|v| v.contains("finalization failed"))
        );
        assert!(!fixture.store.root.join("pending.json").exists());
        for id in &ids[..fsutil::MAX_BATCH_THREADS] {
            assert!(!fixture.exists(id));
            assert!(!fixture.path(id, true).exists());
        }
        assert!(
            fixture.exists(&ids[fsutil::MAX_BATCH_THREADS]),
            "later groups must not continue after post-effect failure"
        );
        assert!(fixture.path(&ids[fsutil::MAX_BATCH_THREADS], true).exists());
        assert_eq!(fixture.run(true).unwrap().deleted, 1);
    }
}
