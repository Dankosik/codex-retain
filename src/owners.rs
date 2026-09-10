//! Per-run alias evidence for registered rollout paths, bound to one connection.

use anyhow::{Result, ensure};
use rusqlite::{Connection, Transaction, types::ValueRef};
use std::collections::HashMap;

struct RegisteredOwner {
    expected_id: Box<str>,
    candidate_conflict: bool,
    database_conflict: bool,
}

pub(crate) struct RolloutOwners<'connection> {
    connection: &'connection Connection,
    archive_prefix: Box<str>,
    // A logical base and its single compressed counterpart share one owner.
    paths: HashMap<Box<str>, RegisteredOwner>,
    // Rare alternate logical spellings validated by the caller. These remain
    // exact SQL keys and do not expand into compressed counterparts.
    exact_paths: HashMap<Box<str>, RegisteredOwner>,
    bounds: Option<(String, String)>,
    data_version: Option<i64>,
    trusted_local_changes: Option<u64>,
    #[cfg(test)]
    scans: usize,
    #[cfg(test)]
    scanned_rows: usize,
}

impl<'connection> RolloutOwners<'connection> {
    pub(crate) fn new(connection: &'connection Connection, archive_prefix: &str) -> Self {
        Self {
            connection,
            archive_prefix: archive_prefix.into(),
            paths: HashMap::new(),
            exact_paths: HashMap::new(),
            bounds: None,
            data_version: None,
            // A baseline captured inside a transaction could include changes
            // subsequently rolled back. Such a cache must always scan afresh.
            trusted_local_changes: connection
                .is_autocommit()
                .then(|| connection.total_changes()),
            #[cfg(test)]
            scans: 0,
            #[cfg(test)]
            scanned_rows: 0,
        }
    }

    /// Register either physical representation of each candidate before
    /// deletion; its logical base and single .zst counterpart are both covered.
    /// Late registrations are safe but invalidate the previous complete scan.
    pub(crate) fn register(&mut self, path: String, expected_id: &str) {
        let Some(name) = path.strip_prefix(self.archive_prefix.as_ref()) else {
            // An out-of-prefix request will fail closed during verification.
            return;
        };
        let base = name.strip_suffix(".zst").unwrap_or(name);
        if let Some(owner) = self.paths.get_mut(base) {
            owner.candidate_conflict |= owner.expected_id.as_ref() != expected_id;
            return;
        }
        // No repeated absolute prefix or spare String capacity is retained.
        self.paths.insert(
            base.into(),
            RegisteredOwner {
                expected_id: expected_id.into(),
                candidate_conflict: false,
                database_conflict: false,
            },
        );
        self.data_version = None;
        let full_base = &path[..self.archive_prefix.len() + base.len()];
        self.include_bounds(full_base, ".zst");
    }

    /// Register a raw logical spelling only after the caller has established
    /// that it names this inspected rollout. Path equality may accept `/./` or
    /// repeated separators that SQL text equality deliberately keeps distinct.
    pub(crate) fn register_exact(&mut self, path: &str, expected_id: &str) {
        if let Some(owner) = self.exact_paths.get_mut(path) {
            owner.candidate_conflict |= owner.expected_id.as_ref() != expected_id;
            return;
        }
        self.exact_paths.insert(
            path.into(),
            RegisteredOwner {
                expected_id: expected_id.into(),
                candidate_conflict: false,
                database_conflict: false,
            },
        );
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

    fn matching_owners<'a>(&'a self, path: &str) -> impl Iterator<Item = &'a RegisteredOwner> {
        let name = path.strip_prefix(self.archive_prefix.as_ref());
        name.and_then(|name| self.paths.get(name))
            .into_iter()
            .chain(
                name.and_then(|name| name.strip_suffix(".zst"))
                    .and_then(|base| self.paths.get(base)),
            )
            .chain(self.exact_paths.get(path))
    }

    /// Call under BEGIN IMMEDIATE, after the live schema, policy, row and file
    /// checks, and before this transaction performs any mutations. This caches
    /// only the absence of foreign path owners; eligibility remains live.
    pub(crate) fn verify(
        &mut self,
        transaction: &Transaction<'_>,
        requested: &HashMap<String, &str>,
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
            let mut owners = self.matching_owners(path).peekable();
            ensure!(
                owners.peek().is_some()
                    && owners.all(|owner| {
                        owner.expected_id.as_ref() == id && !owner.candidate_conflict
                    }),
                "another thread references this rollout path"
            );
        }
        let data_version =
            transaction.query_row("PRAGMA main.data_version", [], |row| row.get(0))?;
        let local_changes = transaction.total_changes();
        if self.data_version != Some(data_version)
            || self.trusted_local_changes != Some(local_changes)
        {
            // A partial scan never becomes reusable evidence. Retain only the
            // fixed registered path set.
            self.data_version = None;
            for owner in self.paths.values_mut().chain(self.exact_paths.values_mut()) {
                owner.database_conflict = false;
            }
            #[cfg(test)]
            {
                self.scans += 1;
            }
            if let Some((minimum, maximum)) = &self.bounds {
                let mut statement = transaction.prepare(
                    "SELECT id,rollout_path FROM main.threads
                     WHERE rollout_path COLLATE BINARY BETWEEN ?1 AND ?2",
                )?;
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
                    let Ok(path) = std::str::from_utf8(path) else {
                        continue;
                    };
                    if let Some(owner) = self.exact_paths.get_mut(path) {
                        owner.database_conflict |= !row
                            .get_ref(0)?
                            .as_str()
                            .is_ok_and(|id| id == owner.expected_id.as_ref());
                    }
                    let Some(name) = path.strip_prefix(self.archive_prefix.as_ref()) else {
                        continue;
                    };
                    if let Some(owner) = self.paths.get_mut(name) {
                        owner.database_conflict |= !row
                            .get_ref(0)?
                            .as_str()
                            .is_ok_and(|id| id == owner.expected_id.as_ref());
                    }
                    if let Some(base) = name.strip_suffix(".zst")
                        && let Some(owner) = self.paths.get_mut(base)
                    {
                        owner.database_conflict |= !row
                            .get_ref(0)?
                            .as_str()
                            .is_ok_and(|id| id == owner.expected_id.as_ref());
                    }
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
                self.matching_owners(path)
                    .all(|owner| !owner.database_conflict),
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
}
