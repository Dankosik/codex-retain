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
    collections::{HashMap, HashSet},
    fmt, fs,
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
    ReferencedHistory,
    RelatedThread,
    DependentThread,
    DependencyCycle,
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
            Self::ReferencedHistory => "referenced_history",
            Self::RelatedThread => "related_thread",
            Self::DependentThread => "dependent_thread",
            Self::DependencyCycle => "dependency_cycle",
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluated_at: Option<i64>,
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
                    EntryReason::ChangedBusyOrError
                        | EntryReason::UnsafeOrUnavailableArtifact
                        | EntryReason::DependencyCycle
                )
            })
            || !self.warnings.is_empty()
    }

    fn new(mode: ExecutionMode, now: i64) -> Self {
        Self {
            schema: 1,
            mode: mode.as_str().into(),
            started_at: now,
            evaluated_at: None,
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
        self.had_attention |= matches!(
            reason,
            EntryReason::ChangedBusyOrError
                | EntryReason::UnsafeOrUnavailableArtifact
                | EntryReason::DependencyCycle
        );
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

/// Formats with an implemented retention adapter; artifacts still need validation.
pub fn supported_history_mode(mode: &str) -> bool {
    matches!(mode, "legacy" | "paginated")
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
    } else if !supported_history_mode(&t.history_mode) {
        EntryReason::UnsupportedHistoryMode
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
    paginated: bool,
}

#[derive(Debug)]
struct ReferencedHistory;

impl fmt::Display for ReferencedHistory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("another thread still references this paginated history")
    }
}

impl std::error::Error for ReferencedHistory {}

fn blocker_reason(blocker: crate::relations::Blocker) -> EntryReason {
    match blocker {
        crate::relations::Blocker::Children => EntryReason::DependentThread,
        crate::relations::Blocker::History => EntryReason::ReferencedHistory,
        crate::relations::Blocker::Cycle => EntryReason::DependencyCycle,
    }
}

fn dependency_reason(error: &anyhow::Error) -> Option<EntryReason> {
    if let Some(&blocker) = error.downcast_ref::<crate::relations::Blocker>() {
        Some(blocker_reason(blocker))
    } else {
        error
            .is::<ReferencedHistory>()
            .then_some(EntryReason::ReferencedHistory)
    }
}

fn inspect_owned(
    id: &str,
    logical_path: &Path,
    p: &Policy,
    lineage: &crate::lineage::LineageIndex,
    paginated: bool,
    require_unreferenced: bool,
) -> Result<Vec<Artifact>> {
    let owner = uuid::Uuid::parse_str(id)?;
    ensure!(owner.to_string() == id, "noncanonical thread ID");
    let rollouts = lineage.owned(owner);
    ensure!(!rollouts.is_empty(), "paginated rollout files are missing");
    ensure!(
        rollouts.len() <= fsutil::MAX_BATCH_THREADS,
        "one thread owns more than 128 rollouts; automatic cleanup is bounded"
    );
    managed_source_parents(&p.codex_home, logical_path)?;
    let logical = RolloutVariants::from_path(logical_path);
    let exact_selected = rollouts.iter().any(|rollout| {
        let variants = RolloutVariants::from_path(&rollout.path);
        logical_path == variants.plain || logical_path == variants.compressed
    });
    let relocated = !fsutil::path_exists(&logical.plain)?
        && !fsutil::path_exists(&logical.compressed)?
        && rollouts.iter().any(|rollout| {
            RolloutVariants::from_path(&rollout.path).plain.file_name() == logical.plain.file_name()
        });
    ensure!(
        exact_selected || relocated,
        "selected rollout is not owned by the thread"
    );
    let mut artifacts = Vec::with_capacity(rollouts.len());
    for rollout in rollouts {
        ensure!(
            rollout.paginated == paginated,
            "database and owned rollout history modes differ"
        );
        managed_source_parents(&p.codex_home, &rollout.path)?;
        if require_unreferenced && lineage.externally_referenced(rollout.rollout_id, owner) {
            return Err(ReferencedHistory.into());
        }
        let (file, metadata) = fsutil::regular_with_metadata(&rollout.path, false, false)?;
        ensure!(
            Identity::of(&metadata) == rollout.identity,
            "paginated rollout changed after reference discovery"
        );
        let line = crate::metadata::first_record(
            &file,
            rollout
                .path
                .extension()
                .is_some_and(|extension| extension == "zst"),
        )?;
        if paginated {
            crate::lineage::validate_owner(&line, owner)?;
        } else {
            crate::metadata::validate_metadata(&line, id)?;
        }
        artifacts.push(Artifact {
            path: rollout.path.clone(),
            identity: rollout.identity.clone(),
            bytes: metadata.len(),
            allocated: metadata.blocks().saturating_mul(512),
        });
    }
    Ok(artifacts)
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rollout_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    slot: Option<u32>,
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
                ensure!(
                    matches!(batch.schema, 2..=4),
                    "unrecognized pending deletion schema"
                );
                Ok(batch)
            }
            Self::Single(one) => {
                ensure!(one.schema == 1, "unrecognized pending deletion schema");
                Ok(BatchIntent {
                    schema: 2,
                    owner: one.owner,
                    items: vec![PendingItem {
                        thread_id: one.thread_id,
                        rollout_id: None,
                        slot: None,
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

fn rollout_stage_path(home: &Path, thread_id: &str, rollout_id: &str) -> PathBuf {
    home.join("archived_sessions")
        .join(format!(".codex-retain-pending-{thread_id}-{rollout_id}"))
}

fn copy_stage_path(home: &Path, thread_id: &str, rollout_id: &str, slot: u32) -> PathBuf {
    home.join("archived_sessions").join(format!(
        ".codex-retain-pending-{thread_id}-{rollout_id}-{slot}"
    ))
}

/// Validate every ancestor; missing directories may only be created by recovery.
fn managed_source_parents(home: &Path, source: &Path) -> Result<Vec<PathBuf>> {
    use std::path::Component;
    ensure!(source.is_absolute(), "rollout source must be absolute");
    let relative = source
        .strip_prefix(home)
        .context("rollout source is outside Codex home")?;
    let first = relative.components().next();
    ensure!(
        matches!(first, Some(Component::Normal(name)) if name == "sessions" || name == "archived_sessions"),
        "rollout source is outside managed session roots"
    );
    ensure!(
        !relative
            .components()
            .any(|component| matches!(component, Component::ParentDir)),
        "rollout source contains parent traversal"
    );
    let parent = source.parent().context("rollout source has no parent")?;
    ensure!(
        parent != home,
        "rollout source is not inside a session directory"
    );
    let mut parents = vec![home.to_path_buf()];
    for component in parent.strip_prefix(home)?.components() {
        parents.push(
            parents
                .last()
                .context("missing source root")?
                .join(component),
        );
    }
    for directory in &parents {
        match fs::symlink_metadata(directory) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "rollout source ancestor is not a real directory: {}",
                directory.display()
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).context("inspect rollout source ancestor"),
        }
    }
    Ok(parents)
}

fn prepare_restore_parent(home: &Path, source: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let parents = managed_source_parents(home, source)?;
    for pair in parents.windows(2) {
        match fs::symlink_metadata(&pair[1]) {
            Ok(meta) => ensure!(
                meta.is_dir() && !meta.file_type().is_symlink(),
                "recovery directory changed"
            ),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                fs::DirBuilder::new().mode(0o700).create(&pair[1])?;
                fsutil::sync_dir(&pair[1])?;
                fsutil::sync_dir(&pair[0])?;
            }
            Err(error) => return Err(error).context("prepare recovery source directory"),
        }
    }
    Ok(())
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
    let mut ids = HashSet::new();
    let mut rollout_ids = HashSet::new();
    let mut sources = HashSet::new();
    for (index, item) in intent.items.iter().enumerate() {
        ensure!(
            sources.insert(&item.artifact.path),
            "duplicate pending source path"
        );
        ensure!(
            item.slot
                == if intent.schema == 4 {
                    Some(u32::try_from(index)?)
                } else {
                    None
                },
            "invalid pending slot"
        );
        ensure!(
            uuid::Uuid::parse_str(&item.thread_id)?.to_string() == item.thread_id,
            "invalid pending thread ID"
        );
        let first_thread_occurrence = ids.insert(&item.thread_id);
        let rollout = match (intent.schema, item.rollout_id.as_deref()) {
            (2, None) => {
                ensure!(first_thread_occurrence, "duplicate pending thread ID");
                ensure!(
                    item.staged == stage_path(&p.codex_home, &item.thread_id),
                    "invalid pending staging path"
                );
                item.thread_id.as_str()
            }
            (3 | 4, Some(rollout)) => {
                ensure!(
                    uuid::Uuid::parse_str(rollout)?.to_string() == rollout,
                    "invalid pending rollout ID"
                );
                ensure!(
                    item.staged
                        == if intent.schema == 4 {
                            copy_stage_path(
                                &p.codex_home,
                                &item.thread_id,
                                rollout,
                                u32::try_from(index)?,
                            )
                        } else {
                            rollout_stage_path(&p.codex_home, &item.thread_id, rollout)
                        },
                    "invalid pending staging path"
                );
                let filename = crate::lineage::filename(
                    item.artifact
                        .path
                        .file_name()
                        .and_then(|name| name.to_str())
                        .context("invalid recovery source filename")?,
                )?
                .context("invalid recovery rollout filename")?;
                ensure!(
                    filename.owner.to_string() == item.thread_id
                        && filename.rollout.to_string() == rollout,
                    "invalid recovery source owner or rollout identity"
                );
                rollout
            }
            _ => anyhow::bail!("pending rollout identity does not match journal schema"),
        };
        ensure!(
            rollout_ids.insert(rollout) || intent.schema == 4,
            "duplicate pending rollout ID"
        );
        if intent.schema == 4 {
            managed_source_parents(&p.codex_home, &item.artifact.path)?;
        }
        ensure!(
            intent.schema == 4
                || item.artifact.path.parent()
                    == Some(p.codex_home.join("archived_sessions").as_path()),
            "invalid recovery source path"
        );
        ensure!(
            item.artifact
                .path
                .file_name()
                .and_then(|v| v.to_str())
                .and_then(rollout_id)
                == Some(rollout),
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
    let mut ids: Vec<&str> = intent.items.iter().map(|v| v.thread_id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    let _writers = ThreadLocks::acquire(&p.codex_home, &ids)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    database::verify_base(&tx)?;
    database::verify_policy(&tx, p)?;
    let mut restored_parents = std::collections::BTreeSet::new();
    let mut restored = 0;
    let mut finished = 0;
    let mut exists_query = tx.prepare("SELECT EXISTS(SELECT 1 FROM threads WHERE id=?)")?;
    for item in &intent.items {
        let exists: bool = exists_query.query_row([&item.thread_id], |r| r.get(0))?;
        if exists && intent.schema == 4 {
            restored_parents.extend(managed_source_parents(&p.codex_home, &item.artifact.path)?);
        }
        if !fsutil::path_exists(&item.staged)? {
            if exists && intent.schema == 4 {
                // A previous recovery may have restored this file but stopped
                // before its directory barrier. Keep that barrier on every retry.
                let (file, metadata) =
                    fsutil::regular_with_metadata(&item.artifact.path, false, false)?;
                ensure!(
                    Identity::of(&metadata) == item.artifact.identity,
                    "restored source identity changed; manual recovery required"
                );
                let line = crate::metadata::first_record(
                    &file,
                    item.artifact
                        .path
                        .extension()
                        .is_some_and(|extension| extension == "zst"),
                )?;
                crate::lineage::validate_owner(&line, uuid::Uuid::parse_str(&item.thread_id)?)?;
                restored_parents.insert(
                    item.artifact
                        .path
                        .parent()
                        .context("missing restored parent")?
                        .to_path_buf(),
                );
            }
            continue;
        }
        let (file, metadata) = fsutil::regular_with_metadata(&item.staged, false, false)?;
        ensure!(
            Identity::of(&metadata) == item.artifact.identity,
            "staged file identity changed; manual recovery required"
        );
        if intent.schema >= 3 {
            let line = crate::metadata::first_record(
                &file,
                item.artifact
                    .path
                    .extension()
                    .is_some_and(|extension| extension == "zst"),
            )?;
            crate::lineage::validate_owner(&line, uuid::Uuid::parse_str(&item.thread_id)?)?;
        } else {
            drop(file);
        }
        if exists {
            prepare_restore_parent(&p.codex_home, &item.artifact.path)?;
            restored_parents.insert(
                item.artifact
                    .path
                    .parent()
                    .context("missing restore parent")?
                    .to_path_buf(),
            );
            fsutil::rename_without_overwrite_unsynced(&item.staged, &item.artifact.path)?;
            restored += 1;
        } else {
            fs::remove_file(&item.staged)?;
            finished += 1;
        }
    }
    drop(exists_query);
    for parent in restored_parents.into_iter().rev() {
        fsutil::sync_dir(&parent)?;
    }
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
    graph: &'run crate::relations::Dependencies,
    owners: RolloutOwners<'connection>,
    headers: crate::lineage::HeaderCache,
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
    ) -> Result<Vec<(u64, u64)>> {
        let p = self.policy;
        let store = self.store;
        let archive = archive_directory(p).context(GlobalCleanupFailure)?;
        let _maintenance = fsutil::maintenance(&p.codex_home)?;
        let deleting: HashSet<_> = ids
            .iter()
            .map(|id| uuid::Uuid::parse_str(id))
            .collect::<Result<_, _>>()?;
        let guards = self.graph.ancestors(&deleting, fsutil::MAX_BATCH_THREADS)?;
        let guard_ids: Vec<_> = guards.iter().map(ToString::to_string).collect();
        let guard_names: Vec<_> = guard_ids.iter().map(String::as_str).collect();
        let _writers = ThreadLocks::acquire(&p.codex_home, &guard_names)
            .context("acquire thread and owning ancestor writer locks")?;
        let tx = rusqlite::Transaction::new_unchecked(c, rusqlite::TransactionBehavior::Immediate)?;
        database::verify_base(&tx).context(GlobalCleanupFailure)?;
        database::verify_policy(&tx, p).context(GlobalCleanupFailure)?;
        let mut threads = Vec::with_capacity(ids.len());
        let mut thread_query = database::prepare_thread_lookup(&tx)?;
        for &id in ids {
            let thread = database::lookup_thread(&mut thread_query, id)?
                .context("thread was removed concurrently")?;
            let assessment = assess(&thread, p, self.now);
            ensure!(
                assessment.reason == EntryReason::Eligible,
                "thread {id} is no longer eligible: {}",
                assessment.reason
            );
            threads.push(thread);
        }
        drop(thread_query);
        // File metadata also carries parent ownership when SQL edges are absent.
        // Rebuild both dependency kinds after locks and the live state transaction.
        let lineage = crate::lineage::scan(&p.codex_home, &deleting, Some(&mut self.headers))
            .context(GlobalCleanupFailure)?;
        let mut fresh = crate::relations::Dependencies::default();
        database::add_spawn_dependencies(&tx, &mut fresh).context(GlobalCleanupFailure)?;
        lineage.add_dependencies(&mut fresh);
        let fresh_guards = fresh
            .ancestors(&deleting, fsutil::MAX_BATCH_THREADS)
            .context(GlobalCleanupFailure)?;
        ensure!(
            fresh_guards.is_subset(&guards),
            "owning ancestors changed before deletion; retry with a fresh snapshot"
        );
        fresh.verify_removal(&deleting)?;
        let mut intent = BatchIntent {
            schema: 4,
            owner: p.owner.clone(),
            items: Vec::new(),
        };
        for thread in &threads {
            let artifacts = inspect_owned(
                &thread.id,
                &thread.path,
                p,
                &lineage,
                thread.history_mode == "paginated",
                true,
            )?;
            for artifact in artifacts {
                let rollout = artifact
                    .path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .and_then(rollout_id)
                    .context("invalid rollout identity")?
                    .to_owned();
                let slot = u32::try_from(intent.items.len())?;
                let staged = copy_stage_path(&p.codex_home, &thread.id, &rollout, slot);
                ensure!(
                    !fsutil::path_exists(&staged)?,
                    "an untracked staged file exists; inspect it before cleanup"
                );
                intent.items.push(PendingItem {
                    thread_id: thread.id.clone(),
                    rollout_id: Some(rollout),
                    slot: Some(slot),
                    artifact,
                    staged,
                });
            }
        }
        // Keep the existing 128-file journal bound. A larger multi-thread group
        // splits before any journal or file write; a single oversized thread stays.
        ensure!(
            intent.items.len() <= fsutil::MAX_BATCH_THREADS,
            "deletion group exceeds 128 rollout files"
        );
        verify_rollout_owners(&tx, &threads, &intent.items, &mut self.owners)?;
        let delete_query = database::prepare_delete(&tx)?;
        *journal_write_attempted = true;
        fsutil::atomic_json(&intent_path(store), &intent)?;
        let mut source_parents = std::collections::BTreeSet::new();
        for item in &intent.items {
            managed_source_parents(&p.codex_home, &item.artifact.path)?;
            source_parents.insert(
                item.artifact
                    .path
                    .parent()
                    .context("missing source parent")?,
            );
            fsutil::rename_without_overwrite_unsynced(&item.artifact.path, &item.staged)?;
            let (file, metadata) = fsutil::regular_with_metadata(&item.staged, false, false)?;
            ensure!(
                Identity::of(&metadata) == item.artifact.identity,
                "staged file identity changed before commit"
            );
            let line = crate::metadata::first_record(
                &file,
                item.artifact
                    .path
                    .extension()
                    .is_some_and(|extension| extension == "zst"),
            )?;
            crate::lineage::validate_owner(&line, uuid::Uuid::parse_str(&item.thread_id)?)?;
        }
        source_parents.insert(&archive);
        for parent in source_parents {
            fsutil::sync_dir(parent)?;
        }
        database::delete_rows(delete_query, &threads)?;
        database::delete_spawn_edges(&tx, &threads)?;
        tx.commit()?;
        self.owners.acknowledge_local_deletes();
        let mut removed = HashMap::<String, (u64, u64)>::new();
        for item in &intent.items {
            let (file, meta) = fsutil::regular_with_metadata(&item.staged, false, false)?;
            ensure!(
                Identity::of(&meta) == item.artifact.identity,
                "staged file changed; cleanup stopped"
            );
            let totals = removed.entry(item.thread_id.clone()).or_default();
            totals.0 += meta.len();
            totals.1 += meta.blocks().saturating_mul(512);
            drop(file);
            fs::remove_file(&item.staged)?;
        }
        fsutil::sync_dir(&archive)?;
        fsutil::remove_durable(&intent_path(store))?;
        ids.iter()
            .map(|id| {
                removed
                    .remove(*id)
                    .context("missing completed thread accounting")
            })
            .collect()
    }
}

fn verify_rollout_owners(
    tx: &rusqlite::Transaction<'_>,
    threads: &[Thread],
    items: &[PendingItem],
    cache: &mut RolloutOwners<'_>,
) -> Result<()> {
    let mut owners = HashMap::new();
    for thread in threads {
        let path = thread.path.to_str().context("non-UTF-8 path")?;
        // Retain the exact SQL spelling as well as physical variants.
        cache.register_exact(path, &thread.id);
        if let Some(previous) = owners.insert(path.to_owned(), thread.id.as_str()) {
            ensure!(
                previous == thread.id,
                "multiple candidate threads reference one rollout"
            );
        }
    }
    for item in items {
        let variants = RolloutVariants::from_path(&item.artifact.path);
        cache.register(
            item.artifact
                .path
                .to_str()
                .context("non-UTF-8 path")?
                .to_owned(),
            &item.thread_id,
        );
        for path in [&variants.plain, &variants.compressed] {
            let path = path.to_str().context("non-UTF-8 path")?;
            cache.register_exact(path, &item.thread_id);
            if let Some(previous) = owners.insert(path.to_owned(), item.thread_id.as_str()) {
                ensure!(
                    previous == item.thread_id,
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
                    for (&index, (bytes, allocated)) in indices[range].iter().zip(artifacts) {
                        report.entries[index].reason = EntryReason::Deleted;
                        report.eligible += 1;
                        report.deleted += 1;
                        report.logical_bytes_removed += bytes;
                        report.allocated_bytes_unlinked += allocated;
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
                            dependency_reason(&error).unwrap_or(EntryReason::ChangedBusyOrError),
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
                    paginated: thread.history_mode == "paginated",
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
    let candidate_ids: HashSet<_> = candidates
        .iter()
        .filter_map(|candidate| {
            uuid::Uuid::parse_str(&report.entries[candidate.report_index].id).ok()
        })
        .collect();
    let mut graph = crate::relations::Dependencies::default();
    // Preview has no later scan to reuse these headers.
    let mut headers = crate::lineage::HeaderCache::default();
    let lineage = if candidates.is_empty() {
        Ok(crate::lineage::LineageIndex::default())
    } else {
        crate::lineage::scan(&p.codex_home, &candidate_ids, apply.then_some(&mut headers)).and_then(
            |lineage| {
                database::add_spawn_dependencies(c, &mut graph)?;
                lineage.add_dependencies(&mut graph);
                Ok(lineage)
            },
        )
    };
    let mut selected_paths = HashMap::new();
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
        let id = &report.entries[index].id;
        let artifacts = match &lineage {
            Ok(lineage) => {
                inspect_owned(id, &candidate.path, p, lineage, candidate.paginated, false)
            }
            Err(error) => Err(anyhow::anyhow!(
                "cannot verify history and ownership dependencies: {error:#}"
            )),
        };
        match artifacts {
            Ok(artifacts) => {
                let bytes = artifacts.iter().map(|artifact| artifact.bytes).sum();
                report.entries[index].bytes = Some(bytes);
                due.push(index);
                if apply {
                    selected_paths.insert(
                        index,
                        artifacts
                            .iter()
                            .map(|artifact| artifact.path.clone())
                            .collect::<Vec<_>>(),
                    );
                    for artifact in artifacts {
                        owners.register(
                            artifact
                                .path
                                .into_os_string()
                                .into_string()
                                .map_err(|_| anyhow::anyhow!("non-UTF-8 path"))?,
                            &report.entries[index].id,
                        );
                    }
                }
            }
            Err(error) => {
                report.skipped += 1;
                report.failure(
                    index,
                    if error.is::<ReferencedHistory>() {
                        EntryReason::ReferencedHistory
                    } else {
                        EntryReason::UnsafeOrUnavailableArtifact
                    },
                    &format!("{error:#}"),
                    detail,
                );
            }
        }
    }
    // File validation precedes dependency pruning: an unavailable or protected
    // child must keep its parents, even when their own files are valid.
    let mut indices = HashMap::new();
    for &index in &due {
        let id = uuid::Uuid::parse_str(&report.entries[index].id)?;
        match graph.ancestors(&HashSet::from([id]), fsutil::MAX_BATCH_THREADS) {
            Ok(_) => {
                indices.insert(id, index);
            }
            Err(error) => {
                report.failure(
                    index,
                    EntryReason::UnsafeOrUnavailableArtifact,
                    &format!("{error:#}"),
                    detail,
                );
            }
        }
    }
    let plan = graph.plan(&indices.keys().copied().collect());
    for (id, blocker) in plan.blocked {
        report.failure(
            indices[&id],
            blocker_reason(blocker),
            &blocker.to_string(),
            detail,
        );
    }
    let layers: Vec<Vec<usize>> = plan
        .layers
        .into_iter()
        .map(|layer| layer.into_iter().map(|id| indices[&id]).collect())
        .collect();
    report.selected_bytes = layers
        .iter()
        .flatten()
        .map(|&index| report.entries[index].bytes.unwrap_or(0))
        .sum();
    if !apply {
        report.eligible = layers.iter().map(Vec::len).sum::<usize>() as u64;
        report.skipped = report.examined.saturating_sub(report.eligible);
    }
    if apply {
        let mut cleanup = Cleanup {
            policy: p,
            store,
            now,
            graph: &graph,
            owners,
            headers,
            detail,
            #[cfg(test)]
            completed_batches: 0,
        };
        'layers: for layer in &layers {
            for group in layer.chunks(fsutil::MAX_BATCH_THREADS) {
                if cleanup.apply_group(c, &mut report, group)?.is_break() {
                    for entry in report
                        .entries
                        .iter_mut()
                        .filter(|entry| entry.reason == EntryReason::Eligible)
                    {
                        entry.reason = EntryReason::RunStopped;
                    }
                    break 'layers;
                }
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
        // Check every owned segment, including inactive segments of a reverted
        // thread, for a late materialization. Never remove a republished file.
        for index in due {
            if report.entries[index].reason != EntryReason::Deleted {
                continue;
            }
            if selected_paths.get(&index).is_some_and(|paths| {
                paths.iter().any(|path| {
                    let variants = RolloutVariants::from_path(path);
                    variants.plain.exists() || variants.compressed.exists()
                })
            }) {
                report.warning(
                    detail,
                    format!(
                        "{}: Codex republished a rollout; preserved it",
                        report.entries[index].id
                    ),
                );
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
    fn cached_inventory_rechecks_orphan_parents_between_committed_groups() {
        for change in ["new", "rewrite", "replace"] {
            let fixture = test_support::Fixture::new();
            let first = fixture.add(1, 1);
            let second = fixture.add(2, 1);
            fixture.age(&first);
            fixture.age(&second);
            let orphan = uuid::Uuid::from_u128(3);
            let path = fixture
                .home
                .join("sessions")
                .join(format!("rollout-2026-09-10T00-00-00-{orphan}.jsonl"));
            let record = |parent: uuid::Uuid| {
                format!(
                    "{}\n",
                    serde_json::json!({
                        "type":"session_meta", "payload": {
                            "id":orphan, "history_mode":"legacy", "parent_thread_id":parent
                        }
                    })
                )
            };
            if change != "new" {
                fs::write(&path, record(uuid::Uuid::from_u128(4))).unwrap();
            }
            let ids = [&first, &second].map(|id| uuid::Uuid::parse_str(id).unwrap());
            let first_path = fixture.path(&first, true);
            let second_path = fixture.path(&second, true);
            let mut paths = vec![first_path.as_path(), second_path.as_path()];
            if change != "new" {
                paths.push(&path);
            }
            let mut headers = crate::lineage::HeaderCache::after_files_settle(&paths);
            let lineage =
                crate::lineage::scan(&fixture.home, &HashSet::from(ids), Some(&mut headers))
                    .unwrap();
            let mut graph = crate::relations::Dependencies::default();
            lineage.add_dependencies(&mut graph);
            let mut cleanup = Cleanup {
                policy: &fixture.policy,
                store: &fixture.store,
                now: test_support::now(),
                graph: &graph,
                owners: RolloutOwners::new(&fixture.c, ""),
                headers,
                detail: ReportDetail::Full,
                completed_batches: 0,
            };
            let mut attempted = false;
            cleanup
                .delete_batch(&fixture.c, &[&first], &mut attempted)
                .unwrap();
            assert!(attempted);
            assert!(!fixture.exists(&first));
            let data_version: i64 = fixture
                .c
                .query_row("PRAGMA main.data_version", [], |row| row.get(0))
                .unwrap();
            if change == "replace" {
                fs::rename(&path, fixture.home.join("saved-orphan")).unwrap();
            }
            fs::write(&path, record(ids[1])).unwrap();
            assert_eq!(
                fixture
                    .c
                    .query_row("PRAGMA main.data_version", [], |row| row.get::<_, i64>(0))
                    .unwrap(),
                data_version
            );
            attempted = false;
            let error = cleanup
                .delete_batch(&fixture.c, &[&second], &mut attempted)
                .unwrap_err();
            assert!(
                error.to_string().contains("surviving child"),
                "{change}: {error:#}"
            );
            assert!(!attempted, "fresh dependency must veto before journaling");
            assert!(fixture.exists(&second));
            assert!(fixture.path(&second, true).exists());
            assert!(path.exists());
            assert!(!fixture.store.root.join("pending.json").exists());
        }
    }

    #[test]
    fn flat_batch_syncs_the_archive_once_per_durability_barrier() {
        let mut fixture = test_support::Fixture::new();
        let id = fixture.add(1, 1);
        fixture.age(&id);
        fsutil::trace_directory_syncs(None);
        let report = fixture.run(true).unwrap();
        let trace = fsutil::take_directory_sync_trace();
        assert_eq!(report.deleted, 1);
        assert_eq!(
            trace
                .iter()
                .filter(|path| **path == fixture.home.join("archived_sessions"))
                .count(),
            2,
            "one barrier before SQL commit and one after staged unlink"
        );
    }

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
            (EntryReason::ReferencedHistory, "referenced_history", false),
            (EntryReason::RelatedThread, "related_thread", false),
            (EntryReason::DependentThread, "dependent_thread", false),
            (EntryReason::DependencyCycle, "dependency_cycle", true),
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
        let owner = uuid::Uuid::parse_str(&due).unwrap();
        let candidates = HashSet::from([owner]);
        let inventory = crate::lineage::scan(&fixture.home, &candidates, None).unwrap();
        assert_eq!(inventory.owned(owner).len(), 1);
        assert_eq!(inventory.owned(owner)[0].path, fixture.path(&due, true));
        assert!(inventory.owned(uuid::Uuid::from_u128(2)).is_empty());
        let duplicate = fixture
            .home
            .join("sessions")
            .join(fixture.path(&due, true).file_name().unwrap());
        fs::copy(fixture.path(&due, true), &duplicate).unwrap();
        assert_eq!(
            crate::lineage::scan(&fixture.home, &candidates, None)
                .unwrap()
                .owned(owner)
                .len(),
            2
        );
        std::os::unix::fs::symlink("missing", fixture.home.join("sessions/unrelated-link"))
            .unwrap();
        assert!(crate::lineage::scan(&fixture.home, &candidates, None).is_err());
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

    #[test]
    fn recovery_retries_every_cross_directory_barrier_after_partial_failure() {
        for recreate_directories in [false, true] {
            let mut fixture = test_support::Fixture::new();
            let owner = fixture.add(1, 1);
            let archived = fixture.path(&owner, true);
            let source = fixture
                .home
                .join("sessions/2025/01/01")
                .join(archived.file_name().unwrap());
            fs::create_dir_all(source.parent().unwrap()).unwrap();
            fs::rename(&archived, &source).unwrap();
            let original = fs::read(&source).unwrap();
            let metadata = fs::metadata(&source).unwrap();
            let staged = copy_stage_path(&fixture.home, &owner, &owner, 0);
            let intent = BatchIntent {
                schema: 4,
                owner: fixture.policy.owner.clone(),
                items: vec![PendingItem {
                    thread_id: owner.clone(),
                    rollout_id: Some(owner.clone()),
                    slot: Some(0),
                    artifact: Artifact {
                        path: source.clone(),
                        identity: Identity::of(&metadata),
                        bytes: metadata.len(),
                        allocated: metadata.blocks() * 512,
                    },
                    staged: staged.clone(),
                }],
            };
            fsutil::atomic_json(&intent_path(&fixture.store), &intent).unwrap();
            fs::rename(&source, &staged).unwrap();
            if recreate_directories {
                for directory in ["sessions/2025/01/01", "sessions/2025/01", "sessions/2025"] {
                    fs::remove_dir(fixture.home.join(directory)).unwrap();
                }
            }
            let failed_directory = if recreate_directories {
                fixture.home.join("sessions")
            } else {
                source.parent().unwrap().to_path_buf()
            };
            fsutil::trace_directory_syncs(Some(failed_directory.clone()));
            let failure = recover(&mut fixture.c, &fixture.policy, &fixture.store);
            let first_trace = fsutil::take_directory_sync_trace();
            assert!(
                failure
                    .unwrap_err()
                    .to_string()
                    .contains("injected recovery directory sync failure")
            );
            assert!(first_trace.contains(&failed_directory));
            assert!(intent_path(&fixture.store).exists());
            if !recreate_directories {
                assert!(
                    source.exists() && !staged.exists(),
                    "retry must cover a copy already restored before its barrier failed"
                );
            }
            fsutil::trace_directory_syncs(None);
            let retried = recover(&mut fixture.c, &fixture.policy, &fixture.store);
            let retry_trace = fsutil::take_directory_sync_trace();
            retried.unwrap();
            let ancestors = managed_source_parents(&fixture.home, &source).unwrap();
            let mut previous = None;
            for ancestor in ancestors.iter().rev() {
                let position = retry_trace
                    .iter()
                    .rposition(|path| path == ancestor)
                    .unwrap_or_else(|| {
                        panic!(
                            "retry omitted durability barrier for {}",
                            ancestor.display()
                        )
                    });
                if let Some(previous) = previous {
                    assert!(
                        previous < position,
                        "source ancestry must be synced bottom-up"
                    );
                }
                previous = Some(position);
            }
            assert!(retry_trace.contains(&fixture.home.join("archived_sessions")));
            assert_eq!(fs::read(&source).unwrap(), original);
            assert!(fixture.exists(&owner));
            assert!(!staged.exists() && !intent_path(&fixture.store).exists());
        }
    }
}
