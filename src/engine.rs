use crate::{
    config::{Policy, Store},
    database::{self, Thread},
    fsutil::{self, Identity, ThreadLocks},
};
use anyhow::{Context, Result, ensure};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Read},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub title: String,
    pub reason: String,
    pub eligible_at: Option<i64>,
    pub bytes: Option<u64>,
    pub detail: Option<String>,
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Report {
    pub schema: u32,
    pub mode: String,
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
}
impl Report {
    fn new(mode: &str, now: i64) -> Self {
        Self {
            schema: 1,
            mode: mode.into(),
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
        }
    }
}

pub fn reason(t: &Thread, p: &Policy, now: i64) -> (&'static str, Option<i64>) {
    let deadline = t.epoch.and_then(|epoch| epoch.checked_add(p.duration()));
    let reason = if t.archived != 1 {
        "unknown_archive_status"
    } else if p.exclusions.contains(&t.id) {
        "excluded"
    } else if t.pinned != 0 {
        "pinned_or_unknown_pin"
    } else if t.history_mode != "legacy" {
        "unsupported_history_mode"
    } else if t.related {
        "related_thread"
    } else if t.archived_at.is_none() {
        "unknown_archive_time"
    } else if t.epoch.is_none() {
        "missing_capture"
    } else if t.archived_at != t.recorded_archive {
        "capture_mismatch"
    } else if t.epoch.is_some_and(|v| v <= 0 || v > now)
        || t.archived_at.is_some_and(|v| v <= 0 || v > now)
    {
        "invalid_or_future_clock"
    } else if deadline.is_none_or(|v| now <= v) {
        "within_retention"
    } else {
        "eligible"
    };
    (reason, deadline)
}

fn rollout_id(name: &str) -> Option<&str> {
    let stem = name
        .strip_suffix(".jsonl.zst")
        .or_else(|| name.strip_suffix(".jsonl"))?;
    if !stem.starts_with("rollout-") || stem.len() < 37 {
        return None;
    }
    let id = stem.get(stem.len() - 36..)?;
    (uuid::Uuid::parse_str(id).ok()?.to_string() == id).then_some(id)
}

struct Inventory {
    paths: HashMap<String, Vec<PathBuf>>,
}
impl Inventory {
    fn scan(home: &Path) -> Result<Self> {
        let mut paths: HashMap<String, Vec<PathBuf>> = HashMap::new();
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
                if let Some(id) = entry.file_name().to_str().and_then(rollout_id) {
                    paths
                        .entry(id.to_owned())
                        .or_default()
                        .push(entry.path().to_owned());
                }
                ensure!(
                    !entry.file_type().is_symlink(),
                    "session inventory contains a symlink; resolve it before cleanup"
                );
            }
        }
        Ok(Self { paths })
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct Artifact {
    path: PathBuf,
    identity: Identity,
    bytes: u64,
    allocated: u64,
}
fn inspect(t: &Thread, p: &Policy, inventory: &Inventory) -> Result<Artifact> {
    ensure!(
        uuid::Uuid::parse_str(&t.id)?.to_string() == t.id,
        "noncanonical thread ID"
    );
    let archive = p.codex_home.join("archived_sessions");
    ensure!(
        t.path.parent() == Some(archive.as_path()),
        "rollout path is outside the flat local archive"
    );
    let name = t
        .path
        .file_name()
        .and_then(|v| v.to_str())
        .context("unsupported rollout filename")?;
    ensure!(
        rollout_id(name) == Some(t.id.as_str()),
        "filename and thread identity differ"
    );
    let candidates = inventory
        .paths
        .get(&t.id)
        .context("rollout file is missing")?;
    ensure!(
        candidates.len() == 1,
        "multiple rollout paths share the thread ID"
    );
    let path = &candidates[0];
    let compressed = PathBuf::from(format!("{}.zst", t.path.display()));
    ensure!(
        path == &t.path || path == &compressed,
        "rollout inventory disagrees with the database path"
    );
    let plain = if path.extension().is_some_and(|v| v == "zst") {
        path.with_extension("")
    } else {
        path.clone()
    };
    let compressed_sibling = PathBuf::from(format!("{}.zst", plain.display()));
    ensure!(
        !(fsutil::path_exists(&plain)? && fsutil::path_exists(&compressed_sibling)?),
        "plain and compressed rollouts both exist"
    );
    let file = fsutil::regular(path, false, false)?;
    let metadata = file.metadata()?;
    // Only the first record is needed. Bound decompressed metadata as well as
    // read-ahead; never scan a transcript to infer the retention clock.
    let reader: Box<dyn Read> = if path.extension().is_some_and(|v| v == "zst") {
        let mut decoder = zstd::stream::read::Decoder::new(BufReader::with_capacity(16384, file))?;
        decoder.window_log_max(23)?; // Reject oversized decoder windows before allocation.
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
    let meta: serde_json::Value =
        serde_json::from_slice(&line).context("invalid rollout metadata")?;
    ensure!(
        meta["type"] == "session_meta" && meta["payload"]["id"] == t.id,
        "rollout metadata identity mismatch"
    );
    let mode = &meta["payload"]["history_mode"];
    ensure!(
        mode.is_null() || mode == "legacy",
        "rollout metadata has unsupported history mode"
    );
    ensure!(
        meta["payload"]["history_base"].is_null(),
        "rollout contains a shared history reference"
    );
    Ok(Artifact {
        path: path.clone(),
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
    for item in &intent.items {
        let exists: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM threads WHERE id=?)",
            [&item.thread_id],
            |r| r.get(0),
        )?;
        if !fsutil::path_exists(&item.staged)? {
            continue;
        }
        let file = fsutil::regular(&item.staged, false, false)?;
        ensure!(
            Identity::of(&file.metadata()?) == item.artifact.identity,
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
    fsutil::sync_dir(&archive)?;
    tx.commit()?;
    fsutil::remove_durable(&path)?;
    Ok(Some(format!(
        "recovered pending deletion: restored {restored}, finished {finished}; byte accounting unavailable after interruption"
    )))
}

fn delete_batch(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    ids: &[&str],
    now: i64,
    inventory: &Inventory,
    journal_started: &mut bool,
) -> Result<Vec<Artifact>> {
    let archive = archive_directory(p)?;
    let _maintenance = fsutil::maintenance(&p.codex_home)?;
    let _writers = ThreadLocks::acquire(&p.codex_home, ids)?;
    let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
    database::verify_base(&tx)?;
    database::verify_policy(&tx, p)?;
    let mut threads = Vec::with_capacity(ids.len());
    let mut intent = BatchIntent {
        schema: 2,
        owner: p.owner.clone(),
        items: Vec::with_capacity(ids.len()),
    };
    for &id in ids {
        let thread = database::thread(&tx, id)?.context("thread was removed concurrently")?;
        let (current_reason, _) = reason(&thread, p, now);
        ensure!(
            current_reason == "eligible",
            "thread {id} is no longer eligible: {current_reason}"
        );
        let artifact = inspect(&thread, p, inventory)?;
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
    verify_rollout_owners(&tx, &threads, &intent.items)?;
    // One durable intent covers all names; directory barriers cover all moves
    // in this small group. Eligibility checks and SQLite commit remain atomic.
    *journal_started = true;
    fsutil::atomic_json(&intent_path(store), &intent)?;
    for item in &intent.items {
        fsutil::rename_without_overwrite_unsynced(&item.artifact.path, &item.staged)?;
    }
    fsutil::sync_dir(&archive)?;
    for thread in &threads {
        database::delete_row(&tx, thread)?;
    }
    tx.commit()?;
    for item in &mut intent.items {
        let file = fsutil::regular(&item.staged, false, false)?;
        let meta = file.metadata()?;
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

fn verify_rollout_owners(c: &Connection, threads: &[Thread], items: &[PendingItem]) -> Result<()> {
    let mut owners = HashMap::new();
    for (thread, item) in threads.iter().zip(items) {
        let plain = if item.artifact.path.extension().is_some_and(|v| v == "zst") {
            item.artifact.path.with_extension("")
        } else {
            item.artifact.path.clone()
        };
        let compressed = PathBuf::from(format!("{}.zst", plain.display()));
        for path in [&thread.path, &plain, &compressed] {
            let path = path.to_str().context("non-UTF-8 path")?;
            if let Some(previous) = owners.insert(path.to_owned(), thread.id.as_str()) {
                ensure!(
                    previous == thread.id,
                    "multiple candidate threads reference one rollout"
                );
            }
        }
    }
    // Codex has no rollout_path index. One IN query scans its metadata once per
    // bounded group, preserving the alias check without N scans for N members.
    let placeholders = std::iter::repeat_n("?", owners.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut statement = c.prepare(&format!(
        "SELECT id,rollout_path FROM threads WHERE rollout_path IN ({placeholders})"
    ))?;
    let rows = statement.query_map(rusqlite::params_from_iter(owners.keys()), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (id, path) = row?;
        ensure!(
            owners.get(path.as_str()).copied() == Some(id.as_str()),
            "another thread references this rollout path"
        );
    }
    Ok(())
}

fn apply_group(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
    inventory: &Inventory,
    report: &mut Report,
    indices: &[usize],
) -> Result<bool> {
    let ids: Vec<&str> = indices
        .iter()
        .map(|&i| report.entries[i].id.as_str())
        .collect();
    let mut journal_started = false;
    match delete_batch(c, p, store, &ids, now, inventory, &mut journal_started) {
        Ok(artifacts) => {
            for (&index, artifact) in indices.iter().zip(artifacts) {
                report.entries[index].reason = "deleted".into();
                report.eligible += 1;
                report.deleted += 1;
                report.logical_bytes_removed += artifact.bytes;
                report.allocated_bytes_unlinked += artifact.allocated;
            }
            Ok(true)
        }
        Err(error) => {
            let pending = fsutil::path_exists(&intent_path(store))?;
            let database_busy = error.downcast_ref::<rusqlite::Error>().is_some_and(|error| {
                matches!(error, rusqlite::Error::SqliteFailure(code, _) if matches!(code.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
            });
            if !journal_started && !pending && !database_busy && indices.len() > 1 {
                // A busy member must not strand other independent eligible
                // archives. Retry singly only when no intent/effect was begun.
                for &index in indices {
                    if !apply_group(c, p, store, now, inventory, report, &[index])? {
                        return Ok(false);
                    }
                }
                return Ok(true);
            }
            for &index in indices {
                report.entries[index].reason = "changed_busy_or_error".into();
                report.entries[index].detail = Some(format!("{error:#}"));
            }
            if pending {
                report.warnings.push("cleanup stopped with a durable pending group; next run recovers it first; counts cover completed groups".into());
            } else if journal_started {
                report.warnings.push("group finalization failed after journal writing began; effects may have occurred; stopped with counts for earlier completed groups only".into());
            }
            if database_busy {
                report.warnings.push(
                    "Codex database is busy; stopped this run instead of retrying every archive"
                        .into(),
                );
            }
            Ok(!journal_started && !pending && !database_busy)
        }
    }
}

pub fn execute(
    c: &mut Connection,
    p: &Policy,
    store: &Store,
    now: i64,
    apply: bool,
) -> Result<Report> {
    ensure!(
        p.enabled,
        "policy is disabled; run enable to start a full grace period"
    );
    ensure!(
        now >= p.enabled_at,
        "system clock moved before policy activation"
    );
    database::verify_policy(c, p)?;
    let mut report = Report::new(if apply { "run" } else { "preview" }, now);
    if apply {
        ensure!(
            !p.paused,
            "policy is paused; resume it before a cleanup run"
        );
        if let Some(message) = recover(c, p, store)? {
            report.warnings.push(message);
        }
    } else if fsutil::path_exists(&intent_path(store))? {
        report
            .warnings
            .push("pending recovery; run cleanup or disable before relying on this preview".into());
    }
    let threads = database::archived(c)?;
    report.examined = threads.len() as u64;
    let has_due = threads.iter().any(|t| reason(t, p, now).0 == "eligible");
    let inventory = has_due
        .then(|| Inventory::scan(&p.codex_home))
        .transpose()?;
    let before = apply
        .then(|| fsutil::volume_available(&p.codex_home))
        .flatten();
    let mut due = Vec::new();
    for thread in threads {
        let (why, eligible_at) = reason(&thread, p, now);
        let mut entry = Entry {
            id: thread.id.clone(),
            title: thread.title.clone(),
            reason: why.into(),
            eligible_at,
            bytes: None,
            detail: None,
        };
        if why == "eligible" {
            match inspect(&thread, p, inventory.as_ref().context("missing inventory")?) {
                Ok(artifact) => {
                    entry.bytes = Some(artifact.bytes);
                    report.selected_bytes += artifact.bytes;
                    if apply {
                        due.push(report.entries.len());
                    } else {
                        report.eligible += 1;
                    }
                }
                Err(error) => {
                    entry.reason = "unsafe_or_unavailable_artifact".into();
                    entry.detail = Some(format!("{error:#}"));
                    report.skipped += 1;
                }
            }
        } else {
            report.skipped += 1;
        }
        report.entries.push(entry);
    }
    if apply {
        for indices in due.chunks(fsutil::MAX_BATCH_THREADS) {
            if !apply_group(
                c,
                p,
                store,
                now,
                inventory.as_ref().context("missing inventory")?,
                &mut report,
                indices,
            )? {
                for entry in report.entries.iter_mut().filter(|v| v.reason == "eligible") {
                    entry.reason = "run_stopped".into();
                }
                break;
            }
        }
        report.skipped = report.examined.saturating_sub(report.deleted);
        report.observed_free_space_delta_bytes = before
            .zip(fsutil::volume_available(&p.codex_home))
            .and_then(|(b, a)| i64::try_from(i128::from(a) - i128::from(b)).ok());
        // Unlocked metadata materialization may republish a new plain sibling.
        // Never remove that new object on the strength of an old file identity.
        for entry in report.entries.iter().filter(|v| v.reason == "deleted") {
            if let Some(paths) = inventory.as_ref().and_then(|v| v.paths.get(&entry.id)) {
                for path in paths {
                    let plain = if path.extension().is_some_and(|v| v == "zst") {
                        path.with_extension("")
                    } else {
                        path.clone()
                    };
                    if plain.exists() || PathBuf::from(format!("{}.zst", plain.display())).exists()
                    {
                        report.warnings.push(format!(
                            "{}: Codex republished a rollout; preserved it",
                            entry.id
                        ));
                    }
                }
            }
        }
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
