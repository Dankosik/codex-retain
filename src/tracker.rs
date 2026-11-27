//! External sidecar archive-transition tracker stored in `state_dir/tracker.sqlite`.
//!
//! Keeping tracked archive epochs outside Codex's own `state_<N>.sqlite` means
//! Codex Retain never injects private tables or triggers into Codex's database,
//! never breaks future Codex SQLx migrations, and survives `state_5 -> state_6`
//! upgrades cleanly.

use crate::{
    config::{Policy, Store},
    database::{self, SchemaCapabilities},
    fsutil,
    lineage::FastHashMap,
};
use anyhow::{Context, Result, ensure};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};
use uuid::Uuid;

pub const TRACKER_DB_NAME: &str = "tracker.sqlite";
const TRACKER_SCHEMA_VERSION: i64 = 1;

const INIT_TRACKER_SQL: &str = "
CREATE TABLE IF NOT EXISTS tracker_meta (
    singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
    schema_version INTEGER NOT NULL,
    owner TEXT NOT NULL,
    codex_home TEXT NOT NULL,
    seeded_at INTEGER NOT NULL,
    last_synced_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS tracked_epochs (
    thread_id TEXT PRIMARY KEY,
    archived_since INTEGER NOT NULL,
    codex_archived_at INTEGER
);
";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TrackedEpoch {
    pub archived_since: i64,
    pub codex_archived_at: Option<i64>,
}

/// Pure transition rule shared identically between write-mode `sync` and
/// read-only `preview`.
///
/// - If the thread is already tracked and Codex's `archived_at` has not changed,
///   its tracked epoch is preserved.
/// - If the thread is newly archived or Codex changed `archived_at` (restore +
///   rearchive or backfill/repair):
///   - A valid timestamp within `[last_synced_at, observed_now]` preserves the
///     exact native archive time.
///   - An older timestamp (`< last_synced_at`, e.g. mtime-based backfill or
///     timestamp repair) conservatively starts a fresh retention period at
///     `observed_now`.
///   - Missing/non-positive dates record `archived_since = 0` (ineligible);
///     future dates record `archived_since = observed_now` while keeping
///     `codex_archived_at = Some(future)` so assessment flags `InvalidOrFutureClock`.
pub fn compute_next_epoch(
    archived_at: Option<i64>,
    existing: Option<TrackedEpoch>,
    last_synced_at: i64,
    observed_now: i64,
) -> TrackedEpoch {
    if let Some(prev) = existing
        && prev.codex_archived_at == archived_at
    {
        return prev;
    }
    let archived_since = match archived_at {
        Some(ts) if ts > 0 && ts <= observed_now => {
            if ts >= last_synced_at {
                ts
            } else {
                observed_now
            }
        }
        Some(ts) if ts > observed_now => observed_now,
        _ => 0,
    };
    TrackedEpoch {
        archived_since,
        codex_archived_at: archived_at,
    }
}

pub fn tracker_path(store: &Store) -> PathBuf {
    store.root.join(TRACKER_DB_NAME)
}

pub fn remove_files(store: &Store) -> Result<()> {
    let base = tracker_path(store);
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut os = base.as_os_str().to_owned();
        os.push(suffix);
        fsutil::remove_durable(Path::new(&os))?;
    }
    Ok(())
}

fn configure_tracker_connection(c: &Connection, read_only: bool) -> Result<()> {
    c.busy_timeout(Duration::from_millis(200))?;
    c.execute_batch("PRAGMA trusted_schema=OFF;")?;
    if read_only {
        c.execute_batch("PRAGMA cache_size=-256;")?;
    } else {
        c.execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA cache_size=-512;",
        )?;
    }
    Ok(())
}

fn open_rw_file(path: &Path) -> Result<Connection> {
    if fsutil::path_exists(path)? {
        let _ = fsutil::regular(path, true, false)?;
    }
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open retention tracker {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set permissions on {}", path.display()))?;
    configure_tracker_connection(&c, false)?;
    Ok(c)
}

fn open_ro_file(path: &Path) -> Result<Connection> {
    let _ = fsutil::regular(path, false, false)?;
    let c = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open retention tracker {}", path.display()))?;
    configure_tracker_connection(&c, true)?;
    Ok(c)
}

fn verify_meta(c: &Connection, policy: &Policy) -> Result<i64> {
    let expected_home = policy
        .codex_home
        .to_str()
        .context("Codex home path must be UTF-8")?;
    let (schema_version, owner, codex_home, last_synced_at): (i64, String, String, i64) = c
        .query_row(
            "SELECT schema_version, owner, codex_home, last_synced_at FROM tracker_meta WHERE singleton=1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .context("retention tracker metadata is missing or corrupt; disable and re-enable")?;
    ensure!(
        schema_version == TRACKER_SCHEMA_VERSION,
        "unsupported retention tracker schema version {schema_version}"
    );
    ensure!(
        owner == policy.owner,
        "retention tracker belongs to another policy"
    );
    ensure!(
        codex_home == expected_home,
        "retention tracker belongs to another Codex home"
    );
    Ok(last_synced_at)
}

/// Seed a fresh sidecar tracker at explicit `enable` time, adopting Codex's
/// recorded `archived_at` for currently archived rows (`coalesce(archived_at, 0)`).
/// Also removes any legacy 0.1.x `codex_retain_*` triggers/tables owned by this
/// policy from Codex's database.
pub fn seed(store: &Store, codex_db: &mut Connection, policy: &Policy, now: i64) -> Result<u64> {
    // Clean up any legacy 0.1.x SQLite extension before starting sidecar tracking.
    let _ = database::remove_legacy_capture(codex_db, &policy.owner)?;

    let mut archived_rows: Vec<(String, Option<i64>)> = Vec::new();
    {
        let mut stmt = codex_db
            .prepare("SELECT id, archived_at FROM threads WHERE archived=1 LIMIT 100001")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            ensure!(
                archived_rows.len() < 100_000,
                "archive exceeds the 100,000-thread safety limit; no changes made"
            );
            archived_rows.push((row.get(0)?, row.get(1)?));
        }
    }

    let path = tracker_path(store);
    let mut conn = open_rw_file(&path)?;
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(INIT_TRACKER_SQL)?;
    tx.execute("DELETE FROM tracked_epochs", [])?;
    tx.execute("DELETE FROM tracker_meta", [])?;
    let codex_home = policy
        .codex_home
        .to_str()
        .context("Codex home path must be UTF-8")?;
    tx.execute(
        "INSERT INTO tracker_meta(singleton, schema_version, owner, codex_home, seeded_at, last_synced_at) \
         VALUES(1, ?1, ?2, ?3, ?4, ?4)",
        params![TRACKER_SCHEMA_VERSION, &policy.owner, codex_home, now],
    )?;
    {
        let mut insert = tx.prepare(
            "INSERT INTO tracked_epochs(thread_id, archived_since, codex_archived_at) VALUES(?1, ?2, ?3)",
        )?;
        for (id, archived_at) in &archived_rows {
            insert.execute(params![id, archived_at.unwrap_or(0), archived_at])?;
        }
    }
    tx.commit()?;
    fsutil::sync_dir(&store.root)?;
    Ok(archived_rows.len() as u64)
}

pub struct Tracker {
    conn: Connection,
    last_synced_at: i64,
    read_only: bool,
}

impl Tracker {
    /// Open the sidecar tracker for read-only inspection (`preview`, `status`).
    /// If an upgraded 0.1.x policy still has legacy `codex_retain_epochs` inside
    /// Codex's database and no `tracker.sqlite` yet, loads a read-only in-memory
    /// snapshot without mutating either database.
    pub fn open_readonly(
        store: &Store,
        codex_db: &Connection,
        policy: &Policy,
        now: i64,
    ) -> Result<Self> {
        let path = tracker_path(store);
        if fsutil::path_exists(&path)? {
            let conn = open_ro_file(&path)?;
            let last_synced_at = verify_meta(&conn, policy)?;
            return Ok(Self {
                conn,
                last_synced_at,
                read_only: true,
            });
        }
        if let Some(legacy) = database::read_legacy_capture_epochs(codex_db, &policy.owner)? {
            let conn = Connection::open_in_memory()?;
            conn.execute_batch(INIT_TRACKER_SQL)?;
            let codex_home = policy
                .codex_home
                .to_str()
                .context("Codex home path must be UTF-8")?;
            let synced = now.max(policy.enabled_at);
            conn.execute(
                "INSERT INTO tracker_meta(singleton, schema_version, owner, codex_home, seeded_at, last_synced_at) \
                 VALUES(1, ?1, ?2, ?3, ?4, ?5)",
                params![
                    TRACKER_SCHEMA_VERSION,
                    &policy.owner,
                    codex_home,
                    policy.enabled_at,
                    synced
                ],
            )?;
            {
                let mut insert = conn.prepare(
                    "INSERT INTO tracked_epochs(thread_id, archived_since, codex_archived_at) VALUES(?1, ?2, ?3)",
                )?;
                for epoch in legacy {
                    insert.execute(params![
                        epoch.thread_id,
                        epoch.archived_since,
                        epoch.codex_archived_at
                    ])?;
                }
            }
            return Ok(Self {
                conn,
                last_synced_at: synced,
                read_only: true,
            });
        }
        anyhow::bail!("retention tracker is missing; disable and re-enable to initialize tracking")
    }

    /// Open the sidecar tracker for read-write operations (`run`, `run --scheduled`).
    /// Seamlessly migrates any legacy 0.1.x `codex_retain_*` capture tables from
    /// Codex's DB into `tracker.sqlite` and removes the legacy SQLite triggers.
    pub fn open_readwrite(
        store: &Store,
        codex_db: &mut Connection,
        policy: &Policy,
        now: i64,
    ) -> Result<Self> {
        let path = tracker_path(store);
        if fsutil::path_exists(&path)? {
            let conn = open_rw_file(&path)?;
            let last_synced_at = verify_meta(&conn, policy)?;
            return Ok(Self {
                conn,
                last_synced_at,
                read_only: false,
            });
        }
        if let Some(legacy) = database::read_legacy_capture_epochs(codex_db, &policy.owner)? {
            let mut conn = open_rw_file(&path)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(INIT_TRACKER_SQL)?;
            let codex_home = policy
                .codex_home
                .to_str()
                .context("Codex home path must be UTF-8")?;
            tx.execute(
                "INSERT INTO tracker_meta(singleton, schema_version, owner, codex_home, seeded_at, last_synced_at) \
                 VALUES(1, ?1, ?2, ?3, ?4, ?5)",
                params![
                    TRACKER_SCHEMA_VERSION,
                    &policy.owner,
                    codex_home,
                    policy.enabled_at,
                    now
                ],
            )?;
            {
                let mut insert = tx.prepare(
                    "INSERT INTO tracked_epochs(thread_id, archived_since, codex_archived_at) VALUES(?1, ?2, ?3)",
                )?;
                for epoch in legacy {
                    insert.execute(params![
                        epoch.thread_id,
                        epoch.archived_since,
                        epoch.codex_archived_at
                    ])?;
                }
            }
            tx.commit()?;
            fsutil::sync_dir(&store.root)?;
            // Now that epochs are safely persisted in sidecar storage, remove
            // legacy triggers/tables from Codex's database.
            let _ = database::remove_legacy_capture(codex_db, &policy.owner)?;
            return Ok(Self {
                conn,
                last_synced_at: now,
                read_only: false,
            });
        }
        anyhow::bail!("retention tracker is missing; disable and re-enable to initialize tracking")
    }

    /// Open `tracker.sqlite` for read-write sync even when Codex's DB is opened
    /// read-only (e.g. paused scheduled runs that still observe transitions).
    pub fn open_readwrite_with_ro_codex(
        store: &Store,
        codex_db: &Connection,
        policy: &Policy,
        now: i64,
    ) -> Result<Self> {
        let path = tracker_path(store);
        if fsutil::path_exists(&path)? {
            let conn = open_rw_file(&path)?;
            let last_synced_at = verify_meta(&conn, policy)?;
            return Ok(Self {
                conn,
                last_synced_at,
                read_only: false,
            });
        }
        if let Some(legacy) = database::read_legacy_capture_epochs(codex_db, &policy.owner)? {
            let mut conn = open_rw_file(&path)?;
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute_batch(INIT_TRACKER_SQL)?;
            let codex_home = policy
                .codex_home
                .to_str()
                .context("Codex home path must be UTF-8")?;
            tx.execute(
                "INSERT INTO tracker_meta(singleton, schema_version, owner, codex_home, seeded_at, last_synced_at) \
                 VALUES(1, ?1, ?2, ?3, ?4, ?5)",
                params![
                    TRACKER_SCHEMA_VERSION,
                    &policy.owner,
                    codex_home,
                    policy.enabled_at,
                    now
                ],
            )?;
            {
                let mut insert = tx.prepare(
                    "INSERT INTO tracked_epochs(thread_id, archived_since, codex_archived_at) VALUES(?1, ?2, ?3)",
                )?;
                for epoch in legacy {
                    insert.execute(params![
                        epoch.thread_id,
                        epoch.archived_since,
                        epoch.codex_archived_at
                    ])?;
                }
            }
            tx.commit()?;
            fsutil::sync_dir(&store.root)?;
            return Ok(Self {
                conn,
                last_synced_at: now,
                read_only: false,
            });
        }
        anyhow::bail!("retention tracker is missing; disable and re-enable to initialize tracking")
    }

    pub fn last_synced_at(&self) -> i64 {
        self.last_synced_at
    }

    /// Synchronize tracked epochs against the current archived rows in Codex's
    /// database without writing anything to Codex's database.
    pub fn sync(
        &mut self,
        codex_db: &Connection,
        _caps: &SchemaCapabilities,
        now: i64,
    ) -> Result<()> {
        ensure!(!self.read_only, "cannot sync read-only retention tracker");
        ensure!(
            now >= self.last_synced_at,
            "system clock moved backwards since the last tracker sync"
        );

        let mut existing_uuids: FastHashMap<Uuid, TrackedEpoch> = FastHashMap::default();
        let mut existing_other: FastHashMap<Box<str>, TrackedEpoch> = FastHashMap::default();
        {
            let mut stmt = self.conn.prepare(
                "SELECT thread_id, archived_since, codex_archived_at FROM tracked_epochs",
            )?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let id = row.get_ref(0)?.as_str()?;
                let epoch = TrackedEpoch {
                    archived_since: row.get(1)?,
                    codex_archived_at: row.get(2)?,
                };
                if let Some(uuid) = crate::lineage::parse_canonical_uuid(id) {
                    existing_uuids.insert(uuid, epoch);
                } else {
                    existing_other.insert(id.into(), epoch);
                }
            }
        }

        let mut upserts: Vec<(String, TrackedEpoch)> = Vec::new();
        {
            let mut stmt = codex_db
                .prepare("SELECT id, archived_at FROM threads WHERE archived=1 LIMIT 100001")?;
            let mut rows = stmt.query([])?;
            let mut count = 0usize;
            while let Some(row) = rows.next()? {
                ensure!(
                    count < 100_000,
                    "archive exceeds the 100,000-thread safety limit; no changes made"
                );
                count += 1;
                let id = row.get_ref(0)?.as_str()?;
                let archived_at: Option<i64> = row.get(1)?;
                let prev = if let Some(uuid) = crate::lineage::parse_canonical_uuid(id) {
                    existing_uuids.remove(&uuid)
                } else {
                    existing_other.remove(id)
                };
                let next = compute_next_epoch(archived_at, prev, self.last_synced_at, now);
                if prev != Some(next) {
                    upserts.push((id.to_owned(), next));
                }
            }
        }

        let has_removals = !existing_uuids.is_empty() || !existing_other.is_empty();
        if !upserts.is_empty() || has_removals || now != self.last_synced_at {
            let tx = self
                .conn
                .transaction_with_behavior(TransactionBehavior::Immediate)?;
            if !upserts.is_empty() {
                let mut stmt = tx.prepare(
                    "INSERT OR REPLACE INTO tracked_epochs(thread_id, archived_since, codex_archived_at) \
                     VALUES(?1, ?2, ?3)",
                )?;
                for (id, epoch) in &upserts {
                    stmt.execute(params![id, epoch.archived_since, epoch.codex_archived_at])?;
                }
            }
            if has_removals {
                let mut del = tx.prepare("DELETE FROM tracked_epochs WHERE thread_id=?1")?;
                let mut buf = [0u8; 36];
                for uuid in existing_uuids.keys() {
                    let id = uuid.hyphenated().encode_lower(&mut buf);
                    del.execute([&*id])?;
                }
                for id in existing_other.keys() {
                    del.execute([id.as_ref()])?;
                }
            }
            tx.execute(
                "UPDATE tracker_meta SET last_synced_at=?1 WHERE singleton=1",
                [now],
            )?;
            tx.commit()?;
            self.last_synced_at = now;
        }
        Ok(())
    }

    /// Look up one tracked epoch by `thread_id`.
    pub fn lookup(&self, thread_id: &str) -> Result<Option<TrackedEpoch>> {
        let row = self
            .conn
            .query_row(
                "SELECT archived_since, codex_archived_at FROM tracked_epochs WHERE thread_id=?1",
                [thread_id],
                |r| {
                    Ok(TrackedEpoch {
                        archived_since: r.get(0)?,
                        codex_archived_at: r.get(1)?,
                    })
                },
            )
            .optional()?;
        Ok(row)
    }

    /// Load all tracked epochs into compact maps for O(1) assessment during a scan.
    pub(crate) fn load_map(&self) -> Result<EpochMap> {
        let mut uuids = FastHashMap::default();
        let mut other = FastHashMap::default();
        let mut stmt = self
            .conn
            .prepare("SELECT thread_id, archived_since, codex_archived_at FROM tracked_epochs")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id = row.get_ref(0)?.as_str()?;
            let epoch = TrackedEpoch {
                archived_since: row.get(1)?,
                codex_archived_at: row.get(2)?,
            };
            if let Some(uuid) = crate::lineage::parse_canonical_uuid(id) {
                uuids.insert(uuid, epoch);
            } else {
                other.insert(id.into(), epoch);
            }
        }
        Ok(EpochMap { uuids, other })
    }

    /// Stream tracked epochs sequentially (ordered by `thread_id` PK) for the
    /// compact JSON scan path.
    pub(crate) fn visit_all(
        &self,
        mut visit: impl FnMut(&str, TrackedEpoch) -> Result<()>,
    ) -> Result<()> {
        let mut stmt = self.conn.prepare(
            "SELECT thread_id, archived_since, codex_archived_at FROM tracked_epochs ORDER BY thread_id",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let id = row.get_ref(0)?.as_str()?;
            let epoch = TrackedEpoch {
                archived_since: row.get(1)?,
                codex_archived_at: row.get(2)?,
            };
            visit(id, epoch)?;
        }
        Ok(())
    }

    /// Return thread IDs whose tracked `archived_since` is strictly past the
    /// retention cutoff (`0 < archived_since < evaluated_at - duration`).
    pub fn due_ids(&self, evaluated_at: i64, duration: i64) -> Result<Vec<String>> {
        let Some(cutoff) = evaluated_at.checked_sub(duration) else {
            return Ok(Vec::new());
        };
        if cutoff <= 1 {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT thread_id FROM tracked_epochs WHERE archived_since > 0 AND archived_since < ?1",
        )?;
        let mut rows = stmt.query([cutoff])?;
        let mut ids = Vec::new();
        while let Some(row) = rows.next()? {
            ids.push(row.get(0)?);
        }
        Ok(ids)
    }

    /// Remove deleted thread IDs from `tracked_epochs` after their deletion
    /// commits in Codex's database.
    pub fn remove_deleted(&mut self, ids: &[&str]) -> Result<()> {
        if self.read_only || ids.is_empty() {
            return Ok(());
        }
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        {
            let mut del = tx.prepare("DELETE FROM tracked_epochs WHERE thread_id=?1")?;
            for &id in ids {
                del.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn shrink_memory(&self) {
        let _ = self.conn.execute_batch("PRAGMA shrink_memory;");
    }
}

pub(crate) struct EpochMap {
    uuids: FastHashMap<Uuid, TrackedEpoch>,
    other: FastHashMap<Box<str>, TrackedEpoch>,
}

impl EpochMap {
    #[inline]
    pub(crate) fn get(&self, id: &str) -> Option<TrackedEpoch> {
        if let Some(uuid) = crate::lineage::parse_canonical_uuid(id) {
            self.uuids.get(&uuid).copied()
        } else {
            self.other.get(id).copied()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compute_next_epoch_preserves_unchanged_and_resets_rearchived_or_repaired() {
        let last_sync = 10_000;
        let now = 13_600;

        // Unchanged tracked row stays intact, even with an older archived_at.
        let existing = TrackedEpoch {
            archived_since: 500,
            codex_archived_at: Some(500),
        };
        assert_eq!(
            compute_next_epoch(Some(500), Some(existing), last_sync, now),
            existing
        );

        // Native restore + rearchive between syncs (archived_at in [last_sync, now]) uses exact timestamp.
        assert_eq!(
            compute_next_epoch(Some(11_200), Some(existing), last_sync, now),
            TrackedEpoch {
                archived_since: 11_200,
                codex_archived_at: Some(11_200),
            }
        );

        // Backfill / timestamp repair to an old value (< last_sync) conservatively starts fresh at `now`.
        assert_eq!(
            compute_next_epoch(Some(2), Some(existing), last_sync, now),
            TrackedEpoch {
                archived_since: now,
                codex_archived_at: Some(2),
            }
        );

        // Newly backfilled row with an ancient archived_at (< last_sync) starts fresh at `now`.
        assert_eq!(
            compute_next_epoch(Some(100), None, last_sync, now),
            TrackedEpoch {
                archived_since: now,
                codex_archived_at: Some(100),
            }
        );

        // Newly archived row between syncs uses exact native timestamp.
        assert_eq!(
            compute_next_epoch(Some(12_000), None, last_sync, now),
            TrackedEpoch {
                archived_since: 12_000,
                codex_archived_at: Some(12_000),
            }
        );

        // Missing or non-positive dates stay ineligible (0).
        assert_eq!(
            compute_next_epoch(None, None, last_sync, now),
            TrackedEpoch {
                archived_since: 0,
                codex_archived_at: None,
            }
        );
        assert_eq!(
            compute_next_epoch(Some(0), None, last_sync, now),
            TrackedEpoch {
                archived_since: 0,
                codex_archived_at: Some(0),
            }
        );
    }
}
