//! Per-run alias evidence for registered rollout paths, bound to one connection.

use crate::lineage::FastHashMap;
use anyhow::{Result, ensure};
use rusqlite::{Connection, Statement, Transaction, types::ValueRef};
use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::{BuildHasher, Hash};

const CONFLICT_CANDIDATE: u8 = 1;
const CONFLICT_DATABASE: u8 = 2;

enum RegisteredOwner {
    Uuid { id: uuid::Uuid, flags: u8 },
    Other { id: Box<str>, flags: u8 },
}

const _: () = assert!(std::mem::size_of::<RegisteredOwner>() <= 24);

impl RegisteredOwner {
    fn new(expected_id: &str) -> Self {
        match crate::lineage::parse_canonical_uuid(expected_id) {
            Some(id) => Self::Uuid { id, flags: 0 },
            None => Self::Other {
                id: expected_id.into(),
                flags: 0,
            },
        }
    }

    #[inline]
    fn matches(&self, value: &str) -> bool {
        match self {
            Self::Uuid { id, .. } => crate::lineage::parse_canonical_uuid(value) == Some(*id),
            Self::Other { id, .. } => id.as_ref() == value,
        }
    }

    #[inline]
    fn flags_mut(&mut self) -> &mut u8 {
        match self {
            Self::Uuid { flags, .. } | Self::Other { flags, .. } => flags,
        }
    }

    #[inline]
    fn has_flag(&self, mask: u8) -> bool {
        let flags = match self {
            Self::Uuid { flags, .. } | Self::Other { flags, .. } => *flags,
        };
        flags & mask != 0
    }

    #[inline]
    fn mark_candidate_conflict(&mut self, expected_id: &str) {
        if !self.matches(expected_id) {
            *self.flags_mut() |= CONFLICT_CANDIDATE;
        }
    }

    #[inline]
    fn mark_database_conflict(&mut self, found: Option<&str>) {
        if !found.is_some_and(|id| self.matches(id)) {
            *self.flags_mut() |= CONFLICT_DATABASE;
        }
    }

    #[inline]
    fn clear_database_conflict(&mut self) {
        *self.flags_mut() &= !CONFLICT_DATABASE;
    }

    #[inline]
    fn has_candidate_conflict(&self) -> bool {
        self.has_flag(CONFLICT_CANDIDATE)
    }

    #[inline]
    fn has_database_conflict(&self) -> bool {
        self.has_flag(CONFLICT_DATABASE)
    }
}

const COMPACT_CONFLICT_CANDIDATE: u64 = 1u64 << 40;
const COMPACT_CONFLICT_DATABASE: u64 = 1u64 << 41;
const COMPACT_TS_MASK: u64 = (1u64 << 40) - 1;

const OWNER_BUCKET_EMPTY: u32 = 0;
const OWNER_BUCKET_IDX_MASK: u32 = 0x00FF_FFFF;
const OWNER_BUCKET_FP_MASK: u32 = 0xFF00_0000;
const OWNER_BUCKET_MAX_ENTRIES: usize = (OWNER_BUCKET_IDX_MASK as usize) - 1;

#[inline]
fn pack_owner_bucket(hash: u64, idx: usize) -> u32 {
    debug_assert!(idx < OWNER_BUCKET_MAX_ENTRIES);
    (((hash >> 56) as u32) << 24) | ((idx as u32) + 1)
}

#[derive(Clone, Copy)]
struct CompactOwner {
    ts_and_flags: u64,
    uuid: uuid::Uuid,
}

const _: () = assert!(std::mem::size_of::<CompactOwner>() == 24);

impl CompactOwner {
    #[inline]
    fn ts(&self) -> u64 {
        self.ts_and_flags & COMPACT_TS_MASK
    }

    #[inline]
    fn matches(&self, value: &str) -> bool {
        crate::lineage::parse_canonical_uuid(value) == Some(self.uuid)
    }

    #[inline]
    fn mark_candidate_conflict(&mut self, expected_id: &str) {
        if !self.matches(expected_id) {
            self.ts_and_flags |= COMPACT_CONFLICT_CANDIDATE;
        }
    }

    #[inline]
    fn mark_database_conflict(&mut self, found: Option<&str>) {
        if !found.is_some_and(|id| self.matches(id)) {
            self.ts_and_flags |= COMPACT_CONFLICT_DATABASE;
        }
    }

    #[inline]
    fn clear_database_conflict(&mut self) {
        self.ts_and_flags &= !COMPACT_CONFLICT_DATABASE;
    }

    #[inline]
    fn has_candidate_conflict(&self) -> bool {
        self.ts_and_flags & COMPACT_CONFLICT_CANDIDATE != 0
    }

    #[inline]
    fn has_database_conflict(&self) -> bool {
        self.ts_and_flags & COMPACT_CONFLICT_DATABASE != 0
    }
}

enum OwnerRef<'a> {
    Compact(&'a CompactOwner),
    Full(&'a RegisteredOwner),
}

impl OwnerRef<'_> {
    #[inline]
    fn matches(&self, value: &str) -> bool {
        match self {
            Self::Compact(owner) => owner.matches(value),
            Self::Full(owner) => owner.matches(value),
        }
    }

    #[inline]
    fn has_candidate_conflict(&self) -> bool {
        match self {
            Self::Compact(owner) => owner.has_candidate_conflict(),
            Self::Full(owner) => owner.has_candidate_conflict(),
        }
    }

    #[inline]
    fn has_database_conflict(&self) -> bool {
        match self {
            Self::Compact(owner) => owner.has_database_conflict(),
            Self::Full(owner) => owner.has_database_conflict(),
        }
    }
}

#[derive(Default)]
struct RegisteredPathMap {
    compact: Vec<CompactOwner>,
    buckets: Vec<u32>,
    seed0: u64,
    seed1: u64,
    fallback: FastHashMap<Box<str>, RegisteredOwner>,
}

impl RegisteredPathMap {
    fn with_capacity(cap: usize) -> Self {
        if cap == 0 {
            return Self::default();
        }
        let (seed0, seed1) = crate::lineage::random_hash_seeds();
        let mut bucket_len = 16usize;
        while cap * 20 >= bucket_len * 17 {
            bucket_len *= 2;
        }
        Self {
            compact: Vec::with_capacity(cap),
            buckets: vec![OWNER_BUCKET_EMPTY; bucket_len],
            seed0,
            seed1,
            fallback: FastHashMap::default(),
        }
    }

    #[inline]
    fn ensure_seeds(&mut self) {
        if self.seed0 == 0 {
            let (s0, s1) = crate::lineage::random_hash_seeds();
            self.seed0 = s0;
            self.seed1 = s1;
        }
    }

    #[inline]
    fn hash_compact(&self, ts: u64, uuid: uuid::Uuid) -> u64 {
        let u = uuid.as_u128();
        let h = crate::lineage::wymix((u as u64) ^ self.seed0, ((u >> 64) as u64) ^ self.seed1);
        crate::lineage::wymix(h ^ ts, self.seed0 ^ 0xa076_1d64_78bd_642f)
    }

    fn find_compact(&self, ts: u64, uuid: uuid::Uuid) -> Option<usize> {
        if self.buckets.is_empty() {
            return None;
        }
        let hash = self.hash_compact(ts, uuid);
        let fp_shifted = ((hash >> 56) as u32) << 24;
        let mask = self.buckets.len() - 1;
        let mut pos = (hash as usize) & mask;
        loop {
            let slot = self.buckets[pos];
            if slot == OWNER_BUCKET_EMPTY {
                return None;
            }
            if (slot & OWNER_BUCKET_FP_MASK) == fp_shifted {
                let idx = ((slot & OWNER_BUCKET_IDX_MASK) - 1) as usize;
                let entry = &self.compact[idx];
                if entry.uuid == uuid && entry.ts() == ts {
                    return Some(idx);
                }
            }
            pos = (pos + 1) & mask;
        }
    }

    fn ensure_bucket_capacity(&mut self) {
        let needed = self.compact.len() + 1;
        if !self.buckets.is_empty() && needed * 20 < self.buckets.len() * 17 {
            return;
        }
        let mut new_len = (self.buckets.len() * 2).max(16);
        while needed * 20 >= new_len * 17 {
            new_len *= 2;
        }
        self.buckets = Vec::new();
        self.buckets = vec![OWNER_BUCKET_EMPTY; new_len];
        let mask = new_len - 1;
        for idx in 0..self.compact.len() {
            let entry = &self.compact[idx];
            let hash = self.hash_compact(entry.ts(), entry.uuid);
            let mut pos = (hash as usize) & mask;
            while self.buckets[pos] != OWNER_BUCKET_EMPTY {
                pos = (pos + 1) & mask;
            }
            self.buckets[pos] = pack_owner_bucket(hash, idx);
        }
    }

    fn register(&mut self, base: &str, expected_id: &str) -> bool {
        if !self.fallback.is_empty()
            && let Some(owner) = self.fallback.get_mut(base)
        {
            owner.mark_candidate_conflict(expected_id);
            return false;
        }
        if let Some((ts, uuid, false)) = crate::lineage::parse_compact_rollout_key(base.as_bytes())
        {
            if let Some(idx) = self.find_compact(ts, uuid) {
                self.compact[idx].mark_candidate_conflict(expected_id);
                return false;
            }
            if crate::lineage::parse_canonical_uuid(expected_id) == Some(uuid)
                && self.compact.len() < OWNER_BUCKET_MAX_ENTRIES
            {
                self.ensure_seeds();
                self.ensure_bucket_capacity();
                let idx = self.compact.len();
                let hash = self.hash_compact(ts, uuid);
                let mask = self.buckets.len() - 1;
                let mut pos = (hash as usize) & mask;
                while self.buckets[pos] != OWNER_BUCKET_EMPTY {
                    pos = (pos + 1) & mask;
                }
                self.compact.push(CompactOwner {
                    ts_and_flags: ts,
                    uuid,
                });
                self.buckets[pos] = pack_owner_bucket(hash, idx);
                return true;
            }
        }
        self.fallback
            .insert(base.into(), RegisteredOwner::new(expected_id));
        true
    }

    fn mark_candidate_conflict_if_present(&mut self, name: &str, expected_id: &str) -> bool {
        if !self.compact.is_empty()
            && let Some((ts, uuid, false)) =
                crate::lineage::parse_compact_rollout_key(name.as_bytes())
            && let Some(idx) = self.find_compact(ts, uuid)
        {
            self.compact[idx].mark_candidate_conflict(expected_id);
            return true;
        }
        if !self.fallback.is_empty()
            && let Some(owner) = self.fallback.get_mut(name)
        {
            owner.mark_candidate_conflict(expected_id);
            return true;
        }
        false
    }

    fn mark_database_conflict_if_present(&mut self, name: &str, found: Option<&str>) {
        if !self.compact.is_empty()
            && let Some((ts, uuid, false)) =
                crate::lineage::parse_compact_rollout_key(name.as_bytes())
            && let Some(idx) = self.find_compact(ts, uuid)
        {
            self.compact[idx].mark_database_conflict(found);
            return;
        }
        if !self.fallback.is_empty()
            && let Some(owner) = self.fallback.get_mut(name)
        {
            owner.mark_database_conflict(found);
        }
    }

    fn clear_database_conflicts(&mut self) {
        for owner in &mut self.compact {
            owner.clear_database_conflict();
        }
        for owner in self.fallback.values_mut() {
            owner.clear_database_conflict();
        }
    }

    fn get(&self, name: &str) -> Option<OwnerRef<'_>> {
        if !self.compact.is_empty()
            && let Some((ts, uuid, false)) =
                crate::lineage::parse_compact_rollout_key(name.as_bytes())
            && let Some(idx) = self.find_compact(ts, uuid)
        {
            return Some(OwnerRef::Compact(&self.compact[idx]));
        }
        if !self.fallback.is_empty()
            && let Some(owner) = self.fallback.get(name)
        {
            return Some(OwnerRef::Full(owner));
        }
        None
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.compact.len() + self.fallback.len()
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[cfg(test)]
    fn contains_key(&self, key: &str) -> bool {
        self.get(key).is_some()
    }
}

pub(crate) struct RolloutOwners<'connection> {
    connection: &'connection Connection,
    archive_prefix: Box<str>,
    // A logical base and its single compressed counterpart share one owner.
    paths: RegisteredPathMap,
    // Rare alternate logical spellings validated by the caller. These remain
    // exact SQL keys and do not expand into compressed counterparts.
    exact_paths: FastHashMap<Box<str>, RegisteredOwner>,
    bounds: Option<(String, String)>,
    data_version: Option<i64>,
    trusted_local_changes: Option<u64>,
    data_version_stmt: Option<Statement<'connection>>,
    scan_stmt: Option<Statement<'connection>>,
    #[cfg(test)]
    scans: usize,
    #[cfg(test)]
    scanned_rows: usize,
}

impl<'connection> RolloutOwners<'connection> {
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn new(connection: &'connection Connection, archive_prefix: &str) -> Self {
        Self::with_capacity(connection, archive_prefix, 0)
    }

    pub(crate) fn with_capacity(
        connection: &'connection Connection,
        archive_prefix: &str,
        capacity: usize,
    ) -> Self {
        Self {
            connection,
            archive_prefix: archive_prefix.into(),
            paths: RegisteredPathMap::with_capacity(capacity),
            exact_paths: FastHashMap::default(),
            bounds: None,
            data_version: None,
            // A baseline captured inside a transaction could include changes
            // subsequently rolled back. Such a cache must always scan afresh.
            trusted_local_changes: connection
                .is_autocommit()
                .then(|| connection.total_changes()),
            data_version_stmt: None,
            scan_stmt: None,
            #[cfg(test)]
            scans: 0,
            #[cfg(test)]
            scanned_rows: 0,
        }
    }

    /// Register either physical representation of each candidate before
    /// deletion; its logical base and single .zst counterpart are both covered.
    /// Late registrations are safe but invalidate the previous complete scan.
    #[cfg(test)]
    pub(crate) fn register(&mut self, path: String, expected_id: &str) {
        self.register_str(&path, expected_id);
    }

    pub(crate) fn register_str(&mut self, path: &str, expected_id: &str) {
        let Some(name) = path.strip_prefix(self.archive_prefix.as_ref()) else {
            // An out-of-prefix request will fail closed during verification.
            return;
        };
        let base = name.strip_suffix(".zst").unwrap_or(name);
        if !self.paths.register(base, expected_id) {
            return;
        }
        self.data_version = None;
        let full_base = &path[..self.archive_prefix.len() + base.len()];
        self.include_bounds(full_base, ".zst");
    }

    /// Register a validated path in `exact_paths` only when it is not already
    /// covered by `self.paths` (preserving the common-prefix representation and
    /// avoiding redundant scan invalidation for normal archive paths).
    pub(crate) fn register_covered_or_exact(&mut self, path: &str, expected_id: &str) {
        if let Some(name) = path.strip_prefix(self.archive_prefix.as_ref()) {
            let mut covered = self
                .paths
                .mark_candidate_conflict_if_present(name, expected_id);
            if let Some(base) = name.strip_suffix(".zst") {
                covered |= self
                    .paths
                    .mark_candidate_conflict_if_present(base, expected_id);
            }
            if covered {
                if !self.exact_paths.is_empty()
                    && let Some(owner) = self.exact_paths.get_mut(path)
                {
                    owner.mark_candidate_conflict(expected_id);
                }
                return;
            }
        }
        self.register_exact(path, expected_id);
    }

    /// Register a raw logical spelling only after the caller has established
    /// that it names this inspected rollout. Path equality may accept `/./` or
    /// repeated separators that SQL text equality deliberately keeps distinct.
    pub(crate) fn register_exact(&mut self, path: &str, expected_id: &str) {
        if let Some(owner) = self.exact_paths.get_mut(path) {
            owner.mark_candidate_conflict(expected_id);
            return;
        }
        self.exact_paths
            .insert(path.into(), RegisteredOwner::new(expected_id));
        self.data_version = None;
        self.include_bounds(path, "");
    }

    fn include_bounds(&mut self, path: &str, upper_suffix: &str) {
        match &mut self.bounds {
            None => {
                self.bounds = Some((path.to_owned(), format!("{path}{upper_suffix}")));
            }
            Some((minimum, maximum)) => {
                if path < minimum.as_str() {
                    minimum.clear();
                    minimum.push_str(path);
                }
                // Appending a common suffix need not preserve ordering when
                // one base is a prefix of another. Compare the actual upper
                // endpoint, without allocating a temporary full path.
                let (candidate, previous) = match (
                    path.strip_prefix(self.archive_prefix.as_ref()),
                    maximum.strip_prefix(self.archive_prefix.as_ref()),
                ) {
                    (Some(candidate), Some(previous)) => (candidate, previous),
                    _ => (path, maximum.as_str()),
                };
                if candidate
                    .bytes()
                    .chain(upper_suffix.bytes())
                    .cmp(previous.bytes())
                    .is_gt()
                {
                    maximum.clear();
                    maximum.push_str(path);
                    maximum.push_str(upper_suffix);
                }
            }
        }
    }

    fn matching_owners<'a>(&'a self, path: &str) -> impl Iterator<Item = OwnerRef<'a>> {
        let name = path.strip_prefix(self.archive_prefix.as_ref());
        name.and_then(|name| self.paths.get(name))
            .into_iter()
            .chain(
                name.and_then(|name| name.strip_suffix(".zst"))
                    .and_then(|base| self.paths.get(base)),
            )
            .chain(self.exact_paths.get(path).map(OwnerRef::Full))
    }

    /// Call under BEGIN IMMEDIATE, after the live schema, policy, row and file
    /// checks, and before this transaction performs any mutations. This caches
    /// only the absence of foreign path owners; eligibility remains live.
    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn verify(
        &mut self,
        transaction: &Transaction<'_>,
        requested: &HashMap<String, &str>,
    ) -> Result<()> {
        self.verify_borrowed(transaction, requested)
    }

    pub(crate) fn verify_borrowed<K: Borrow<str> + Eq + Hash, S: BuildHasher>(
        &mut self,
        transaction: &Transaction<'_>,
        requested: &HashMap<K, &str, S>,
    ) -> Result<()> {
        // data_version values are meaningful only on the same connection. The
        // retained reference also prevents its address from being recycled.
        ensure!(
            std::ptr::eq(self.connection, &**transaction),
            "rollout owner cache belongs to another database connection"
        );
        ensure!(
            !transaction.is_autocommit(),
            "rollout owner verification requires a transaction"
        );
        for (path, &id) in requested {
            let mut owners = self.matching_owners(path.borrow()).peekable();
            ensure!(
                owners.peek().is_some()
                    && owners.all(|owner| owner.matches(id) && !owner.has_candidate_conflict()),
                "another thread references this rollout path"
            );
        }
        let data_version: i64 = {
            if self.data_version_stmt.is_none() {
                self.data_version_stmt = Some(self.connection.prepare("PRAGMA main.data_version")?);
            }
            let stmt = self.data_version_stmt.as_mut().expect("prepared above");
            match stmt.query_row([], |row| row.get(0)) {
                Ok(version) => version,
                Err(error) => {
                    self.data_version_stmt = None;
                    return Err(error.into());
                }
            }
        };
        let local_changes = transaction.total_changes();
        if self.data_version != Some(data_version)
            || self.trusted_local_changes != Some(local_changes)
        {
            // A partial scan never becomes reusable evidence. Retain only the
            // fixed registered path set.
            self.data_version = None;
            self.paths.clear_database_conflicts();
            for owner in self.exact_paths.values_mut() {
                owner.clear_database_conflict();
            }
            #[cfg(test)]
            {
                self.scans += 1;
            }
            if let Some((minimum, maximum)) = &self.bounds {
                if self.scan_stmt.is_none() {
                    self.scan_stmt = Some(self.connection.prepare(
                        "SELECT id,rollout_path FROM main.threads
                         WHERE rollout_path COLLATE BINARY BETWEEN ?1 AND ?2",
                    )?);
                }
                let statement = self.scan_stmt.as_mut().expect("prepared above");
                let scan_result: Result<()> = (|| {
                    let mut rows = statement.query([minimum, maximum])?;
                    while let Some(row) = rows.next()? {
                        #[cfg(test)]
                        {
                            self.scanned_rows += 1;
                        }
                        let ValueRef::Text(path) = row.get_ref(1)? else {
                            // Non-text values did not match the former SQL IN
                            // query's registered UTF-8 text parameters either.
                            continue;
                        };
                        let Ok(path) = simdutf8::basic::from_utf8(path) else {
                            continue;
                        };
                        let row_id = row.get_ref(0)?.as_str().ok();
                        if !self.exact_paths.is_empty()
                            && let Some(owner) = self.exact_paths.get_mut(path)
                        {
                            owner.mark_database_conflict(row_id);
                        }
                        let Some(name) = path.strip_prefix(self.archive_prefix.as_ref()) else {
                            continue;
                        };
                        self.paths.mark_database_conflict_if_present(name, row_id);
                        if let Some(base) = name.strip_suffix(".zst") {
                            self.paths.mark_database_conflict_if_present(base, row_id);
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = scan_result {
                    self.scan_stmt = None;
                    return Err(error);
                }
            }
            // Unknown local writes can be uncommitted and later rolled back.
            // Their scan is usable in this transaction but never across one.
            if self.trusted_local_changes == Some(local_changes) {
                self.data_version = Some(data_version);
            }
        }
        for path in requested.keys() {
            ensure!(
                self.matching_owners(path.borrow())
                    .all(|owner| !owner.has_database_conflict()),
                "another thread references this rollout path"
            );
        }
        Ok(())
    }

    /// Call immediately after a successful commit containing only our checked
    /// conditional deletions and the verified schema's deletion side effects.
    /// Those operations cannot introduce an alias. No other local write may be
    /// acknowledged here. Unacknowledged writes conservatively disable reuse.
    pub(crate) fn acknowledge_local_deletes(&mut self) {
        if self.connection.is_autocommit() && self.data_version.is_some() {
            self.trusted_local_changes = Some(self.connection.total_changes());
        } else {
            self.data_version = None;
        }
        // Keep the old external version: an external commit after ours still
        // invalidates the next verification, even if it precedes this call.
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::TransactionBehavior;
    use tempfile::TempDir;

    fn fixture() -> (TempDir, Connection, Connection) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("owners.sqlite");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL);
                 INSERT INTO threads VALUES
                   ('a','a.jsonl'), ('b','b.jsonl'), ('outside','outside.jsonl');",
            )
            .unwrap();
        let other = Connection::open(path).unwrap();
        (directory, connection, other)
    }

    fn cache(connection: &Connection) -> RolloutOwners<'_> {
        let mut owners = RolloutOwners::new(connection, "");
        for id in ["a", "b"] {
            owners.register(format!("{id}.jsonl"), id);
            owners.register(format!("{id}.jsonl.zst"), id);
        }
        owners
    }

    fn requested(id: &str) -> HashMap<String, &str> {
        HashMap::from([(format!("{id}.jsonl"), id), (format!("{id}.jsonl.zst"), id)])
    }

    fn verify(owners: &mut RolloutOwners<'_>, id: &str) -> Result<()> {
        let transaction =
            Transaction::new_unchecked(owners.connection, TransactionBehavior::Immediate)?;
        let result = owners.verify(&transaction, &requested(id));
        transaction.rollback()?;
        result
    }

    fn verify_paths(owners: &mut RolloutOwners<'_>, paths: &[(&str, &str)]) -> Result<()> {
        let transaction =
            Transaction::new_unchecked(owners.connection, TransactionBehavior::Immediate)?;
        let requested = paths
            .iter()
            .map(|&(path, id)| (path.to_owned(), id))
            .collect();
        let result = owners.verify(&transaction, &requested);
        transaction.rollback()?;
        result
    }

    fn assert_conflict(result: Result<()>) {
        assert_eq!(
            result.unwrap_err().to_string(),
            "another thread references this rollout path"
        );
    }

    #[test]
    fn unchanged_database_reuses_one_scan_for_all_registered_paths() {
        let (_directory, connection, _other) = fixture();
        let mut owners = cache(&connection);
        verify(&mut owners, "a").unwrap();
        verify(&mut owners, "b").unwrap();
        verify(&mut owners, "a").unwrap();
        assert_eq!(owners.scans, 1);
        assert_eq!(owners.paths.len(), 2);
    }

    #[test]
    fn external_insert_and_deletion_invalidate_conflicts_for_requested_paths() {
        let (_directory, connection, other) = fixture();
        let mut owners = cache(&connection);
        verify(&mut owners, "a").unwrap();
        other
            .execute("INSERT INTO threads VALUES ('alias','a.jsonl')", [])
            .unwrap();
        assert_conflict(verify(&mut owners, "a"));
        verify(&mut owners, "b").unwrap();
        assert_eq!(owners.scans, 2);
        other
            .execute("DELETE FROM threads WHERE id='alias'", [])
            .unwrap();
        verify(&mut owners, "a").unwrap();
        assert_eq!(owners.scans, 3);
    }

    #[test]
    fn external_path_changes_invalidate_compressed_variant_ownership() {
        let (_directory, connection, other) = fixture();
        let mut owners = cache(&connection);
        verify(&mut owners, "a").unwrap();
        other
            .execute(
                "UPDATE threads SET rollout_path='a.jsonl.zst' WHERE id='outside'",
                [],
            )
            .unwrap();
        assert_conflict(verify(&mut owners, "a"));
        other
            .execute(
                "UPDATE threads SET rollout_path='elsewhere.jsonl' WHERE id='outside'",
                [],
            )
            .unwrap();
        verify(&mut owners, "a").unwrap();
        assert_eq!(owners.scans, 3);
    }

    #[test]
    fn acknowledged_local_deletes_preserve_reuse_but_not_external_changes() {
        let (_directory, connection, other) = fixture();
        let mut owners = cache(&connection);
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        owners.verify(&transaction, &requested("a")).unwrap();
        transaction
            .execute("DELETE FROM threads WHERE id='a'", [])
            .unwrap();
        transaction.commit().unwrap();
        owners.acknowledge_local_deletes();
        verify(&mut owners, "b").unwrap();
        assert_eq!(owners.scans, 1);

        // Exercise an external commit between our commit and acknowledgement.
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        owners.verify(&transaction, &requested("b")).unwrap();
        transaction
            .execute("DELETE FROM threads WHERE id='outside'", [])
            .unwrap();
        transaction.commit().unwrap();
        other
            .execute("INSERT INTO threads VALUES ('alias','b.jsonl')", [])
            .unwrap();
        owners.acknowledge_local_deletes();
        assert_conflict(verify(&mut owners, "b"));
        assert_eq!(owners.scans, 2);
    }

    #[test]
    fn unacknowledged_local_writes_always_force_a_fresh_scan() {
        let (_directory, connection, _other) = fixture();
        let mut owners = cache(&connection);
        verify(&mut owners, "a").unwrap();
        connection
            .execute("INSERT INTO threads VALUES ('alias','a.jsonl')", [])
            .unwrap();
        assert_conflict(verify(&mut owners, "a"));
        connection
            .execute("DELETE FROM threads WHERE id='alias'", [])
            .unwrap();
        verify(&mut owners, "a").unwrap();
        verify(&mut owners, "b").unwrap();
        assert_eq!(owners.scans, 4);
    }

    #[test]
    fn a_scan_after_uncommitted_local_deletion_cannot_survive_rollback() {
        let (_directory, connection, other) = fixture();
        other
            .execute("INSERT INTO threads VALUES ('alias','a.jsonl')", [])
            .unwrap();
        let mut owners = cache(&connection);
        assert_conflict(verify(&mut owners, "a"));
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        transaction
            .execute("DELETE FROM threads WHERE id='alias'", [])
            .unwrap();
        owners.verify(&transaction, &requested("a")).unwrap();
        transaction.rollback().unwrap();
        assert_conflict(verify(&mut owners, "a"));
        assert_eq!(owners.scans, 3);
    }

    #[test]
    fn partial_local_delete_rollback_does_not_require_acknowledgement() {
        let (_directory, connection, _other) = fixture();
        let mut owners = cache(&connection);
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        owners.verify(&transaction, &requested("a")).unwrap();
        transaction
            .execute("DELETE FROM threads WHERE id='a'", [])
            .unwrap();
        transaction.rollback().unwrap();
        verify(&mut owners, "a").unwrap();
        verify(&mut owners, "b").unwrap();
        assert_eq!(owners.scans, 3);
    }

    #[test]
    fn another_connection_cannot_reuse_a_numerically_equal_data_version() {
        let (_directory, connection, other) = fixture();
        let mut owners = cache(&connection);
        verify(&mut owners, "a").unwrap();
        other
            .execute("INSERT INTO threads VALUES ('alias','a.jsonl')", [])
            .unwrap();
        let transaction =
            Transaction::new_unchecked(&other, TransactionBehavior::Immediate).unwrap();
        let other_version: i64 = transaction
            .query_row("PRAGMA main.data_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(owners.data_version, Some(other_version));
        assert_eq!(
            owners
                .verify(&transaction, &requested("a"))
                .unwrap_err()
                .to_string(),
            "rollout owner cache belongs to another database connection"
        );
        transaction.rollback().unwrap();
        assert_conflict(verify(&mut owners, "a"));
    }

    #[test]
    fn unregistered_paths_and_conflicting_registrations_fail_closed() {
        let (_directory, connection, _other) = fixture();
        let mut owners = cache(&connection);
        assert_conflict(verify(&mut owners, "unregistered"));
        owners.register("a.jsonl".into(), "a");
        verify(&mut owners, "a").unwrap();
        owners.register("a.jsonl".into(), "different");
        assert_conflict(verify(&mut owners, "a"));
        owners.register("a.jsonl".into(), "a");
        assert_conflict(verify(&mut owners, "a"));
        verify(&mut owners, "b").unwrap();
    }

    #[test]
    fn late_registration_invalidates_previous_scan() {
        let (_directory, connection, _other) = fixture();
        let mut owners = RolloutOwners::new(&connection, "");
        owners.register("a.jsonl".into(), "a");
        owners.register("a.jsonl.zst".into(), "a");
        verify(&mut owners, "a").unwrap();
        owners.register("outside.jsonl".into(), "unexpected");
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        assert_conflict(owners.verify(
            &transaction,
            &HashMap::from([("outside.jsonl".into(), "unexpected")]),
        ));
        assert_eq!(owners.scans, 2);
    }

    #[test]
    fn failed_scans_never_publish_reusable_evidence() {
        let (_directory, connection, other) = fixture();
        let mut owners = cache(&connection);
        verify(&mut owners, "a").unwrap();
        other.execute("DROP TABLE threads", []).unwrap();
        assert!(verify(&mut owners, "a").is_err());
        assert!(owners.data_version.is_none());
        assert!(verify(&mut owners, "b").is_err());
        assert_eq!(owners.scans, 3);
        other
            .execute_batch(
                "CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL);
                 INSERT INTO threads VALUES ('a','a.jsonl'),('b','b.jsonl');",
            )
            .unwrap();
        verify(&mut owners, "a").unwrap();
        verify(&mut owners, "b").unwrap();
        assert_eq!(owners.scans, 4);
    }

    #[test]
    fn malformed_owner_ids_conflict_only_with_their_own_path() {
        let (_directory, connection, other) = fixture();
        other
            .execute("INSERT INTO threads VALUES (X'FF','a.jsonl')", [])
            .unwrap();
        let mut owners = cache(&connection);
        assert_conflict(verify(&mut owners, "a"));
        verify(&mut owners, "b").unwrap();
        assert_eq!(owners.scans, 1);
    }

    #[test]
    fn scanning_retains_only_registered_paths_and_ignores_nonmatching_values() {
        let (_directory, connection, other) = fixture();
        let transaction =
            Transaction::new_unchecked(&other, TransactionBehavior::Immediate).unwrap();
        for number in 0..256 {
            transaction
                .execute(
                    "INSERT INTO threads VALUES (?1,?2)",
                    [format!("unrelated-{number}"), format!("{number}.jsonl")],
                )
                .unwrap();
        }
        transaction
            .execute("INSERT INTO threads VALUES (X'FF',X'FF')", [])
            .unwrap();
        transaction.commit().unwrap();
        let mut owners = cache(&connection);
        verify(&mut owners, "a").unwrap();
        verify(&mut owners, "b").unwrap();
        assert_eq!(owners.paths.len(), 2);
        assert_eq!(owners.scans, 1);
        assert_eq!(owners.scanned_rows, 2);
    }

    #[test]
    fn either_representation_registers_both_and_retains_one_base() {
        for (registered, alias) in [("a.jsonl", "a.jsonl.zst"), ("a.jsonl.zst", "a.jsonl")] {
            let (_directory, connection, other) = fixture();
            let mut owners = RolloutOwners::new(&connection, "");
            owners.register(registered.into(), "a");
            verify(&mut owners, "a").unwrap();
            assert_eq!(owners.paths.len(), 1);
            assert!(owners.paths.contains_key("a.jsonl"));
            other
                .execute("INSERT INTO threads VALUES ('alias',?1)", [alias])
                .unwrap();
            assert_conflict(verify(&mut owners, "a"));
        }
    }

    #[test]
    fn opposite_representation_registrations_preserve_identity_conflicts() {
        let (_directory, connection, _other) = fixture();
        let mut owners = RolloutOwners::new(&connection, "");
        owners.register("a.jsonl.zst".into(), "a");
        owners.register("a.jsonl".into(), "a");
        verify(&mut owners, "a").unwrap();
        assert_eq!(owners.paths.len(), 1);
        owners.register("a.jsonl.zst".into(), "different");
        assert_conflict(verify(&mut owners, "a"));
        owners.register("a.jsonl".into(), "a");
        assert_conflict(verify(&mut owners, "a"));
    }

    #[test]
    fn prefix_is_literal_and_outside_paths_never_match_a_registered_basename() {
        let (_directory, connection, other) = fixture();
        let prefix = "/tmp/archive%_['\\\"é]/";
        let plain = format!("{prefix}a.jsonl");
        let compressed = format!("{plain}.zst");
        let unrelated = "/tmp/archiveXX['\\\"é]/a.jsonl";
        other
            .execute("UPDATE threads SET rollout_path=?1 WHERE id='a'", [&plain])
            .unwrap();
        other
            .execute("INSERT INTO threads VALUES ('alias',?1)", [unrelated])
            .unwrap();
        let mut owners = RolloutOwners::new(&connection, prefix);
        owners.register(compressed.clone(), "a");
        owners.register(unrelated.into(), "alias");
        assert_eq!(owners.paths.len(), 1);
        assert!(owners.paths.contains_key("a.jsonl"));
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        owners
            .verify(
                &transaction,
                &HashMap::from([(plain.clone(), "a"), (compressed.clone(), "a")]),
            )
            .unwrap();
        assert_conflict(owners.verify(&transaction, &HashMap::from([(unrelated.into(), "alias")])));
        assert_conflict(owners.verify(&transaction, &HashMap::from([("a.jsonl".into(), "a")])));
        transaction.rollback().unwrap();
        other
            .execute(
                "UPDATE threads SET rollout_path=?1 WHERE id='alias'",
                [&compressed],
            )
            .unwrap();
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        assert_conflict(owners.verify(&transaction, &HashMap::from([(plain, "a")])));
    }

    #[test]
    fn double_zst_rows_and_requests_do_not_alias_an_ordinary_rollout() {
        let (_directory, connection, other) = fixture();
        other
            .execute(
                "INSERT INTO threads VALUES ('double','a.jsonl.zst.zst')",
                [],
            )
            .unwrap();
        let mut owners = cache(&connection);
        // b makes the double-zst row fall within the scan's lexical bounds.
        verify(&mut owners, "a").unwrap();
        assert_eq!(owners.scanned_rows, 3);
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        assert_conflict(owners.verify(
            &transaction,
            &HashMap::from([("a.jsonl.zst.zst".into(), "a")]),
        ));
        transaction.rollback().unwrap();
        other
            .execute(
                "UPDATE threads SET rollout_path='a.jsonl.zst' WHERE id='double'",
                [],
            )
            .unwrap();
        assert_conflict(verify(&mut owners, "a"));
    }

    #[test]
    fn exact_lower_and_compressed_upper_bound_aliases_are_included() {
        let (_directory, connection, other) = fixture();
        let mut owners = cache(&connection);
        assert_eq!(
            owners.bounds,
            Some(("a.jsonl".into(), "b.jsonl.zst".into()))
        );
        verify(&mut owners, "a").unwrap();
        other
            .execute("INSERT INTO threads VALUES ('lower','a.jsonl')", [])
            .unwrap();
        assert_conflict(verify(&mut owners, "a"));
        other
            .execute("INSERT INTO threads VALUES ('upper','b.jsonl.zst')", [])
            .unwrap();
        assert_conflict(verify(&mut owners, "b"));
    }

    #[test]
    fn upper_bound_compares_appended_suffix_for_prefix_related_bases() {
        let (_directory, connection, other) = fixture();
        let prefix = "/archive/";
        other
            .execute_batch(
                "UPDATE threads SET rollout_path='/archive/a' WHERE id='a';
                 UPDATE threads SET rollout_path='/archive/a!' WHERE id='b';
                 INSERT INTO threads VALUES ('alias','/archive/a.zst');",
            )
            .unwrap();
        let mut owners = RolloutOwners::new(&connection, prefix);
        // a! sorts after a, but a.zst sorts after a!.zst.
        owners.register("/archive/a!".into(), "b");
        owners.register("/archive/a".into(), "a");
        assert_eq!(
            owners.bounds,
            Some(("/archive/a".into(), "/archive/a.zst".into()))
        );
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        assert_conflict(owners.verify(&transaction, &HashMap::from([("/archive/a".into(), "a")])));
    }

    #[test]
    fn lexical_filter_explicitly_uses_binary_collation() {
        let (_directory, connection, other) = fixture();
        other
            .execute_batch(
                "DROP TABLE threads;
                 CREATE TABLE threads(id TEXT PRIMARY KEY, rollout_path TEXT COLLATE NOCASE);
                 INSERT INTO threads VALUES ('a','Z.jsonl'),('b','a.jsonl'),('alias','Z.jsonl');",
            )
            .unwrap();
        let mut owners = RolloutOwners::new(&connection, "");
        owners.register("Z.jsonl".into(), "a");
        owners.register("a.jsonl".into(), "b");
        let transaction =
            Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
        assert_conflict(owners.verify(&transaction, &HashMap::from([("Z.jsonl".into(), "a")])));
        owners
            .verify(&transaction, &HashMap::from([("a.jsonl".into(), "b")]))
            .unwrap();
    }

    #[test]
    fn exact_logical_dot_component_is_registered_without_normalizing_other_spellings() {
        let (_directory, connection, other) = fixture();
        let prefix = "/tmp/archive/";
        let plain = "/tmp/archive/a.jsonl";
        let logical = "/tmp/archive/./a.jsonl";
        other
            .execute("UPDATE threads SET rollout_path=?1 WHERE id='a'", [logical])
            .unwrap();
        let mut owners = RolloutOwners::new(&connection, prefix);
        owners.register(plain.into(), "a");
        assert!(owners.exact_paths.is_empty());
        assert_conflict(verify_paths(&mut owners, &[(logical, "a")]));
        owners.register_exact(logical, "a");
        verify_paths(
            &mut owners,
            &[
                (logical, "a"),
                (plain, "a"),
                ("/tmp/archive/a.jsonl.zst", "a"),
            ],
        )
        .unwrap();
        assert_eq!(owners.paths.len(), 1);
        assert_eq!(owners.exact_paths.len(), 1);
        assert_conflict(verify_paths(&mut owners, &[("/tmp/archive//a.jsonl", "a")]));
        other
            .execute("INSERT INTO threads VALUES ('alias',?1)", [logical])
            .unwrap();
        assert_conflict(verify_paths(&mut owners, &[(logical, "a"), (plain, "a")]));
        assert_eq!(owners.scans, 2);
    }

    #[test]
    fn exact_parent_spelling_outside_prefix_extends_bounds_before_and_after_physical_paths() {
        let (_directory, connection, other) = fixture();
        let prefix = "/tmp/.codex/archived_sessions/";
        let plain = "/tmp/.codex/archived_sessions/a.jsonl";
        let logical = "/tmp//.codex/archived_sessions/a.jsonl";
        other
            .execute("UPDATE threads SET rollout_path=?1 WHERE id='a'", [logical])
            .unwrap();
        let mut owners = RolloutOwners::new(&connection, prefix);
        owners.register(logical.into(), "a");
        assert!(owners.paths.is_empty());
        assert_conflict(verify_paths(&mut owners, &[(logical, "a")]));
        owners.register_exact(logical, "a");
        owners.register(plain.into(), "a");
        owners.register("/tmp/.codex/archived_sessions/b.jsonl".into(), "b");
        // The doubled slash before a hidden directory sorts after its dot, so
        // the upper bound is an exact spelling outside archive_prefix.
        assert_eq!(owners.bounds, Some((plain.into(), logical.into())));
        verify_paths(&mut owners, &[(logical, "a"), (plain, "a")]).unwrap();
        other
            .execute("INSERT INTO threads VALUES ('alias',?1)", [logical])
            .unwrap();
        assert_conflict(verify_paths(&mut owners, &[(logical, "a")]));
    }

    #[test]
    fn exact_logical_registration_does_not_add_a_compressed_alias() {
        let (_directory, connection, other) = fixture();
        let prefix = "/tmp/archive/";
        let logical = "/tmp/archive/./a.jsonl";
        let alternate = "/tmp/archive/./a.jsonl.zst";
        other
            .execute("UPDATE threads SET rollout_path=?1 WHERE id='a'", [logical])
            .unwrap();
        other
            .execute("INSERT INTO threads VALUES ('alternate',?1)", [alternate])
            .unwrap();
        let mut owners = RolloutOwners::new(&connection, prefix);
        owners.register("/tmp/archive/a.jsonl".into(), "a");
        owners.register_exact(logical, "a");
        verify_paths(&mut owners, &[(logical, "a")]).unwrap();
        assert_eq!(owners.scanned_rows, 2);
        assert_conflict(verify_paths(&mut owners, &[(alternate, "a")]));
        assert_eq!(owners.exact_paths.len(), 1);
    }

    #[test]
    fn late_exact_registration_invalidates_scan_and_tracks_logical_spelling_changes() {
        let (_directory, connection, other) = fixture();
        let prefix = "/tmp/archive/";
        let first = "/tmp/archive/./a.jsonl";
        let second = "/tmp/archive//a.jsonl";
        other
            .execute("UPDATE threads SET rollout_path=?1 WHERE id='a'", [first])
            .unwrap();
        other
            .execute("INSERT INTO threads VALUES ('alias',?1)", [second])
            .unwrap();
        let mut owners = RolloutOwners::new(&connection, prefix);
        owners.register("/tmp/archive/a.jsonl".into(), "a");
        owners.register_exact(first, "a");
        verify_paths(&mut owners, &[(first, "a")]).unwrap();
        assert_eq!(owners.scans, 1);
        let old_version = owners.data_version;
        owners.register_exact(second, "a");
        assert!(owners.data_version.is_none());
        assert_conflict(verify_paths(&mut owners, &[(second, "a")]));
        assert_eq!(owners.data_version, old_version);
        assert_eq!(owners.scans, 2);
        other
            .execute("UPDATE threads SET rollout_path=?1 WHERE id='a'", [second])
            .unwrap();
        assert_conflict(verify_paths(&mut owners, &[(second, "a")]));
        other
            .execute("DELETE FROM threads WHERE id='alias'", [])
            .unwrap();
        verify_paths(&mut owners, &[(second, "a")]).unwrap();
        assert_eq!(owners.scans, 4);
    }

    #[test]
    fn exact_registration_identity_conflicts_remain_fail_closed() {
        let (_directory, connection, _other) = fixture();
        let logical = "/tmp/archive/./a.jsonl";
        let mut owners = RolloutOwners::new(&connection, "/tmp/archive/");
        owners.register_exact(logical, "a");
        owners.register_exact(logical, "a");
        verify_paths(&mut owners, &[(logical, "a")]).unwrap();
        owners.register_exact(logical, "different");
        assert_conflict(verify_paths(&mut owners, &[(logical, "a")]));
        owners.register_exact(logical, "a");
        assert_conflict(verify_paths(&mut owners, &[(logical, "a")]));
        assert_eq!(owners.exact_paths.len(), 1);
    }

    #[test]
    fn covered_exact_registration_reuses_single_scan_across_batches() {
        let (_directory, connection, other) = fixture();
        let prefix = "/tmp/archive/";
        let a = "/tmp/archive/a.jsonl";
        let a_zst = "/tmp/archive/a.jsonl.zst";
        let b = "/tmp/archive/b.jsonl";
        let b_zst = "/tmp/archive/b.jsonl.zst";
        other
            .execute("UPDATE threads SET rollout_path=?1 WHERE id='a'", [a])
            .unwrap();
        other
            .execute("UPDATE threads SET rollout_path=?1 WHERE id='b'", [b])
            .unwrap();
        let mut owners = RolloutOwners::new(&connection, prefix);
        owners.register_str(a, "a");
        owners.register_str(b, "b");
        assert!(owners.exact_paths.is_empty());

        // Batch 1: registering covered plain and compressed variants must not
        // populate exact_paths or invalidate the cached scan for batch 2.
        {
            let tx =
                Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
            owners.register_str(a, "a");
            owners.register_covered_or_exact(a, "a");
            owners.register_covered_or_exact(a_zst, "a");
            owners
                .verify_borrowed(&tx, &HashMap::from([(a, "a"), (a_zst, "a")]))
                .unwrap();
            tx.execute("DELETE FROM threads WHERE id='a'", []).unwrap();
            tx.commit().unwrap();
        }
        owners.acknowledge_local_deletes();
        assert_eq!(owners.scans, 1);
        assert!(owners.exact_paths.is_empty());

        // Batch 2 must reuse batch 1's scan with zero additional table scans.
        {
            let tx =
                Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
            owners.register_str(b, "b");
            owners.register_covered_or_exact(b, "b");
            owners.register_covered_or_exact(b_zst, "b");
            owners
                .verify_borrowed(&tx, &HashMap::from([(b, "b"), (b_zst, "b")]))
                .unwrap();
            tx.execute("DELETE FROM threads WHERE id='b'", []).unwrap();
            tx.commit().unwrap();
        }
        owners.acknowledge_local_deletes();
        assert_eq!(owners.scans, 1);
        assert!(owners.exact_paths.is_empty());
    }

    #[test]
    fn compact_path_table_handles_ordinary_conflicts_fallbacks_and_statement_recovery() {
        let (_directory, connection, other) = fixture();
        let prefix = "/tmp/archive/";
        let u1 = "01950000-0000-7000-8000-000000000001";
        let u2 = "01950000-0000-7000-8000-000000000002";
        let p1 = format!("{prefix}rollout-2026-09-10T00-00-00-{u1}.jsonl");
        let p1_zst = format!("{p1}.zst");
        let p1_double_zst = format!("{p1_zst}.zst");
        let p2 = format!("{prefix}rollout-2026-09-10T00-00-00-{u2}.jsonl");
        let fallback_path = format!("{prefix}custom-noncompact.jsonl");

        other.execute("DELETE FROM threads", []).unwrap();
        other
            .execute(
                "INSERT INTO threads(id, rollout_path) VALUES (?1, ?2), (?3, ?4), ('fb', ?5)",
                rusqlite::params![u1, &p1, u2, &p2, &fallback_path],
            )
            .unwrap();

        let mut owners = RolloutOwners::with_capacity(&connection, prefix, 4);
        owners.register_str(&p1, u1);
        owners.register_str(&p2, u2);
        owners.register_str(&fallback_path, "fb");
        // Ordinary compact rollouts must stay in compact table (zero Box<str> fallback entries);
        // only the non-compact path uses fallback.
        assert_eq!(owners.paths.compact.len(), 2);
        assert_eq!(owners.paths.fallback.len(), 1);
        assert_eq!(owners.paths.len(), 3);
        assert!(!owners.paths.is_empty());

        // Verify ordinary compact hit + .zst alias works cleanly.
        {
            let tx =
                Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
            owners
                .verify_borrowed(
                    &tx,
                    &HashMap::from([(p1.as_str(), u1), (p1_zst.as_str(), u1)]),
                )
                .unwrap();
            // Double-.zst must NOT alias p1.
            assert!(
                owners
                    .verify_borrowed(&tx, &HashMap::from([(p1_double_zst.as_str(), u1)]))
                    .is_err()
            );
            tx.commit().unwrap();
        }

        // Database conflict via another thread pointing to p1.zst must be detected on next scan.
        other
            .execute(
                "INSERT INTO threads(id, rollout_path) VALUES ('intruder', ?1)",
                [&p1_zst],
            )
            .unwrap();
        {
            let tx =
                Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
            assert!(
                owners
                    .verify_borrowed(&tx, &HashMap::from([(p1.as_str(), u1)]))
                    .is_err()
            );
            // Unrelated compact entry p2 must still pass.
            owners
                .verify_borrowed(&tx, &HashMap::from([(p2.as_str(), u2)]))
                .unwrap();
            tx.commit().unwrap();
        }

        // Removing intruder clears the database conflict on re-scan.
        other
            .execute("DELETE FROM threads WHERE id='intruder'", [])
            .unwrap();
        {
            let tx =
                Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
            owners
                .verify_borrowed(&tx, &HashMap::from([(p1.as_str(), u1)]))
                .unwrap();
            tx.commit().unwrap();
        }

        // Candidate conflict on a compact entry (different owner registers same compact path).
        owners.register_str(&p2, u1);
        {
            let tx =
                Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
            assert!(
                owners
                    .verify_borrowed(&tx, &HashMap::from([(p2.as_str(), u2)]))
                    .is_err()
            );
            tx.commit().unwrap();
        }

        // Dropping threads invalidates cached scan statement and fails closed cleanly without panicking.
        other.execute("DROP TABLE threads", []).unwrap();
        {
            let tx =
                Transaction::new_unchecked(&connection, TransactionBehavior::Immediate).unwrap();
            assert!(
                owners
                    .verify_borrowed(&tx, &HashMap::from([(p1.as_str(), u1)]))
                    .is_err()
            );
            assert!(owners.scan_stmt.is_none());
        }
    }
}
