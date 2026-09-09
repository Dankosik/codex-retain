//! Persistence-boundary fixtures for the bounded multi-thread deletion journal.
//! All files and SQLite connections belong to fresh synthetic profiles.

#[allow(dead_code)]
mod support;

use std::{
    fs::{self, File, OpenOptions},
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};

use codex_retain::{database, engine, fsutil::MAX_BATCH_THREADS};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use support::Fixture;

fn add_archives(fixture: &Fixture, count: u128) -> Vec<String> {
    (1..=count)
        .map(|number| {
            let id = fixture.add(number, 1);
            fixture.age(&id);
            id
        })
        .collect()
}

#[test]
fn active_logical_or_physical_alias_preserves_a_compressed_archive() {
    for database_uses_compressed_path in [false, true] {
        let mut fixture = Fixture::new();
        let archived = fixture.add(7001, 1);
        let active = fixture.add(7002, 0);
        let plain = fixture.path(&archived, true);
        let compressed = PathBuf::from(format!("{}.zst", plain.display()));
        let bytes = fs::read(&plain).unwrap();
        fs::write(
            &compressed,
            zstd::stream::encode_all(bytes.as_slice(), 3).unwrap(),
        )
        .unwrap();
        fs::remove_file(&plain).unwrap();
        let (selected, alias) = if database_uses_compressed_path {
            (&compressed, &plain)
        } else {
            (&plain, &compressed)
        };
        fixture
            .c
            .execute(
                "UPDATE threads SET rollout_path=? WHERE id=?",
                params![selected.to_str().unwrap(), archived],
            )
            .unwrap();
        fixture
            .c
            .execute(
                "UPDATE threads SET rollout_path=? WHERE id=?",
                params![alias.to_str().unwrap(), active],
            )
            .unwrap();
        fixture.age(&archived);
        let report = fixture.run(true).unwrap();
        assert_eq!(report.deleted, 0);
        assert!(fixture.exists(&archived) && fixture.exists(&active));
        assert!(compressed.exists());
        assert!(fixture.path(&active, false).exists());
        assert!(!fixture.store.root.join("pending.json").exists());
    }
}

fn source(fixture: &Fixture, item: &Value) -> PathBuf {
    let path = PathBuf::from(item["artifact"]["path"].as_str().unwrap());
    assert!(path.starts_with(&fixture.home));
    path
}

fn staged(item: &Value) -> PathBuf {
    PathBuf::from(item["staged"].as_str().unwrap())
}

fn batch_items(fixture: &Fixture, ids: &[String]) -> Vec<Value> {
    ids.iter()
        .map(|id| {
            let original = fixture.path(id, true);
            let metadata = original.metadata().unwrap();
            json!({
                "thread_id": id,
                "artifact": {
                    "path": original,
                    "identity": {"device": metadata.dev(), "inode": metadata.ino()},
                    "bytes": metadata.len(),
                    "allocated": metadata.blocks() * 512
                },
                "staged": fixture.home.join("archived_sessions").join(format!(".codex-retain-pending-{id}"))
            })
        })
        .collect()
}

fn save_batch(fixture: &Fixture, items: &[Value]) -> Vec<u8> {
    // Deliberately construct the public persistence format independently of
    // production journal structs, so incompatible or incomplete reads fail.
    let bytes = serde_json::to_vec(&json!({
        "schema": 2,
        "owner": fixture.policy.owner,
        "items": items
    }))
    .unwrap();
    fs::write(fixture.store.root.join("pending.json"), &bytes).unwrap();
    bytes
}

fn raw_lock(path: &Path) -> File {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .unwrap();
    file.try_lock().unwrap();
    file
}

fn hold_native_writer(fixture: &Fixture, id: &str) -> File {
    let directory = fixture.home.join("thread-writer-locks");
    fs::create_dir_all(&directory).unwrap();
    let coordination = raw_lock(&directory.join(".coordination.lock"));
    let writer = raw_lock(&directory.join(format!("{id}.lock")));
    drop(coordination);
    writer
}

fn release_native_writer(fixture: &Fixture, id: &str, writer: File) {
    let directory = fixture.home.join("thread-writer-locks");
    let _coordination = raw_lock(&directory.join(".coordination.lock"));
    drop(writer);
    fs::remove_file(directory.join(format!("{id}.lock"))).unwrap();
}

#[test]
fn due_archives_cross_two_batch_boundaries_without_losing_counts() {
    let mut fixture = Fixture::new();
    let count = 2 * MAX_BATCH_THREADS + 1;
    let ids = add_archives(&fixture, count as u128);
    let expected_bytes: u64 = ids
        .iter()
        .map(|id| fs::metadata(fixture.path(id, true)).unwrap().len())
        .sum();
    let active = fixture.add(count as u128 + 1000, 0);
    let active_bytes = fs::read(fixture.path(&active, false)).unwrap();

    let report = fixture.run(true).unwrap();

    assert_eq!(report.examined, count as u64);
    assert_eq!(report.deleted, count as u64);
    assert_eq!(report.skipped, 0);
    assert_eq!(report.logical_bytes_removed, expected_bytes);
    assert_eq!(report.entries.len(), count);
    assert!(report.entries.iter().all(|entry| entry.reason == "deleted"));
    for id in &ids {
        assert!(!fixture.exists(id));
        assert!(!fixture.path(id, true).exists());
    }
    assert!(fixture.exists(&active));
    assert_eq!(
        fs::read(fixture.path(&active, false)).unwrap(),
        active_bytes
    );
    assert_eq!(
        fs::read_dir(fixture.home.join("archived_sessions"))
            .unwrap()
            .count(),
        0
    );
    assert!(!fixture.store.root.join("pending.json").exists());
    assert_eq!(fixture.run(true).unwrap().deleted, 0);
}

#[test]
fn precommit_batch_recovery_restores_partial_staging_and_sqlite_rollback() {
    for staged_count in [1, 3] {
        let mut fixture = Fixture::new();
        let ids = add_archives(&fixture, 3);
        let items = batch_items(&fixture, &ids);
        let original_bytes: Vec<_> = items
            .iter()
            .map(|item| fs::read(source(&fixture, item)).unwrap())
            .collect();
        save_batch(&fixture, &items);
        fixture.c.execute_batch("BEGIN IMMEDIATE").unwrap();
        for item in items.iter().take(staged_count) {
            fs::rename(source(&fixture, item), staged(item)).unwrap();
        }
        if staged_count == items.len() {
            // Rows are changed only once every rollout has been staged. Drop
            // the connection without COMMIT to model interruption here.
            for id in &ids {
                fixture
                    .c
                    .execute("DELETE FROM threads WHERE id=?", [id])
                    .unwrap();
            }
        }
        drop(std::mem::replace(
            &mut fixture.c,
            Connection::open_in_memory().unwrap(),
        ));
        fixture.c = database::open(&fixture.home, true).unwrap();

        assert!(ids.iter().all(|id| fixture.exists(id)));
        assert!(
            engine::recover(&mut fixture.c, &fixture.policy, &fixture.store)
                .unwrap()
                .is_some()
        );
        for (item, expected) in items.iter().zip(&original_bytes) {
            assert_eq!(fs::read(source(&fixture, item)).unwrap(), *expected);
            assert!(!staged(item).exists());
        }
        assert!(ids.iter().all(|id| fixture.exists(id)));
        assert!(!fixture.store.root.join("pending.json").exists());
        assert!(
            engine::recover(&mut fixture.c, &fixture.policy, &fixture.store)
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn committed_batch_recovery_finishes_remaining_stages_without_sweeping_neighbors() {
    let mut fixture = Fixture::new();
    let ids = add_archives(&fixture, 3);
    let items = batch_items(&fixture, &ids);
    save_batch(&fixture, &items);
    for item in &items {
        fs::rename(source(&fixture, item), staged(item)).unwrap();
    }
    fixture.c.execute_batch("BEGIN IMMEDIATE").unwrap();
    for id in &ids {
        fixture
            .c
            .execute("DELETE FROM threads WHERE id=?", [id])
            .unwrap();
    }
    fixture.c.execute_batch("COMMIT").unwrap();
    fs::remove_file(staged(&items[0])).unwrap();
    let unrelated = fixture.home.join("archived_sessions").join(format!(
        ".codex-retain-pending-{}",
        uuid::Uuid::from_u128(999)
    ));
    fs::write(&unrelated, b"not recorded in this deletion journal").unwrap();
    let active = fixture.add(1000, 0);
    let active_bytes = fs::read(fixture.path(&active, false)).unwrap();

    assert!(
        engine::recover(&mut fixture.c, &fixture.policy, &fixture.store)
            .unwrap()
            .is_some()
    );

    for (item, id) in items.iter().zip(&ids) {
        assert!(!staged(item).exists());
        assert!(!source(&fixture, item).exists());
        assert!(!fixture.exists(id));
    }
    assert_eq!(
        fs::read(&unrelated).unwrap(),
        b"not recorded in this deletion journal"
    );
    assert!(fixture.exists(&active));
    assert_eq!(
        fs::read(fixture.path(&active, false)).unwrap(),
        active_bytes
    );
    assert!(!fixture.store.root.join("pending.json").exists());
    assert!(
        engine::recover(&mut fixture.c, &fixture.policy, &fixture.store)
            .unwrap()
            .is_none()
    );
}

#[test]
fn conflicting_batch_item_retains_journal_and_can_resume_after_partial_recovery() {
    for replaced_staging in [false, true] {
        let mut fixture = Fixture::new();
        let ids = add_archives(&fixture, 3);
        let items = batch_items(&fixture, &ids);
        let original_bytes: Vec<_> = items
            .iter()
            .map(|item| fs::read(source(&fixture, item)).unwrap())
            .collect();
        let journal = save_batch(&fixture, &items);
        for item in &items {
            fs::rename(source(&fixture, item), staged(item)).unwrap();
        }
        let saved_original = fixture.temp.path().join("preserve-original-inode");
        let conflict = if replaced_staging {
            fs::rename(staged(&items[1]), &saved_original).unwrap();
            staged(&items[1])
        } else {
            source(&fixture, &items[1])
        };
        fs::write(&conflict, b"new unrelated inode").unwrap();
        let conflict_inode = conflict.metadata().unwrap().ino();
        let outside = fixture.temp.path().join("outside-user-file");
        fs::write(&outside, b"preserve outside bytes").unwrap();

        assert!(engine::recover(&mut fixture.c, &fixture.policy, &fixture.store).is_err());

        assert_eq!(
            fs::read(fixture.store.root.join("pending.json")).unwrap(),
            journal
        );
        assert_eq!(fs::read(&conflict).unwrap(), b"new unrelated inode");
        assert_eq!(conflict.metadata().unwrap().ino(), conflict_inode);
        assert_eq!(fs::read(&outside).unwrap(), b"preserve outside bytes");
        assert!(ids.iter().all(|id| fixture.exists(id)));
        for index in [0, 2] {
            let original = source(&fixture, &items[index]);
            let remaining = if original.exists() {
                original
            } else {
                staged(&items[index])
            };
            assert_eq!(fs::read(remaining).unwrap(), original_bytes[index]);
        }
        if replaced_staging {
            assert_eq!(fs::read(&saved_original).unwrap(), original_bytes[1]);
        } else {
            assert_eq!(fs::read(staged(&items[1])).unwrap(), original_bytes[1]);
        }

        fs::remove_file(&conflict).unwrap();
        if replaced_staging {
            fs::rename(saved_original, staged(&items[1])).unwrap();
        }
        engine::recover(&mut fixture.c, &fixture.policy, &fixture.store).unwrap();
        for (item, expected) in items.iter().zip(&original_bytes) {
            assert_eq!(fs::read(source(&fixture, item)).unwrap(), *expected);
            assert!(!staged(item).exists());
        }
        assert!(!fixture.store.root.join("pending.json").exists());
    }
}

#[test]
fn invalid_second_item_is_rejected_before_touching_valid_first_item() {
    for invalid_kind in ["source", "staging", "duplicate_id"] {
        let mut fixture = Fixture::new();
        let ids = add_archives(&fixture, 2);
        let mut items = batch_items(&fixture, &ids);
        let original_first = source(&fixture, &items[0]);
        let staged_first = staged(&items[0]);
        let first_bytes = fs::read(&original_first).unwrap();
        fs::rename(&original_first, &staged_first).unwrap();
        let outside = fixture.temp.path().join("outside-journal-target");
        fs::write(&outside, b"unrelated outside bytes").unwrap();
        match invalid_kind {
            "source" => items[1]["artifact"]["path"] = json!(outside),
            "staging" => items[1]["staged"] = json!(outside),
            "duplicate_id" => items[1] = items[0].clone(),
            _ => unreachable!(),
        }
        let journal = save_batch(&fixture, &items);

        assert!(engine::recover(&mut fixture.c, &fixture.policy, &fixture.store).is_err());

        assert!(
            !original_first.exists(),
            "first item was mutated before validating {invalid_kind}"
        );
        assert_eq!(fs::read(staged_first).unwrap(), first_bytes);
        assert_eq!(fs::read(outside).unwrap(), b"unrelated outside bytes");
        assert_eq!(
            fs::read(fixture.store.root.join("pending.json")).unwrap(),
            journal
        );
        assert!(ids.iter().all(|id| fixture.exists(id)));
    }
}

#[test]
fn oversized_batch_journal_is_rejected_before_any_recovery_mutation() {
    let mut fixture = Fixture::new();
    let ids = add_archives(&fixture, (MAX_BATCH_THREADS + 1) as u128);
    let items = batch_items(&fixture, &ids);
    let first = source(&fixture, &items[0]);
    let first_staged = staged(&items[0]);
    let first_bytes = fs::read(&first).unwrap();
    fs::rename(&first, &first_staged).unwrap();
    let journal = save_batch(&fixture, &items);

    assert!(engine::recover(&mut fixture.c, &fixture.policy, &fixture.store).is_err());

    assert!(!first.exists());
    assert_eq!(fs::read(first_staged).unwrap(), first_bytes);
    assert_eq!(
        fs::read(fixture.store.root.join("pending.json")).unwrap(),
        journal
    );
    assert!(ids.iter().all(|id| fixture.exists(id)));
}

#[test]
fn one_busy_writer_is_preserved_while_safe_fallback_finishes_other_archives() {
    let mut fixture = Fixture::new();
    let count = 2 * MAX_BATCH_THREADS + 1;
    let ids = add_archives(&fixture, count as u128);
    let busy = &ids[MAX_BATCH_THREADS - 1];
    let writer = hold_native_writer(&fixture, busy);
    let writer_path = fixture
        .home
        .join("thread-writer-locks")
        .join(format!("{busy}.lock"));
    let writer_inode = writer.metadata().unwrap().ino();
    let busy_bytes = fs::read(fixture.path(busy, true)).unwrap();

    let report = fixture.run(true).unwrap();

    assert_eq!(
        report.deleted,
        (count - 1) as u64,
        "a pre-journal lock failure should fall back without stranding unrelated candidates"
    );
    assert_eq!(report.skipped, 1);
    assert_eq!(report.entries.len(), count);
    let busy_entry = report
        .entries
        .iter()
        .find(|entry| entry.id == *busy)
        .unwrap();
    assert_eq!(busy_entry.reason, "changed_busy_or_error");
    assert!(busy_entry.detail.is_some());
    assert!(
        report
            .entries
            .iter()
            .filter(|entry| entry.id != *busy)
            .all(|entry| entry.reason == "deleted")
    );
    assert!(fixture.exists(busy));
    assert_eq!(fs::read(fixture.path(busy, true)).unwrap(), busy_bytes);
    for id in ids.iter().filter(|id| *id != busy) {
        assert!(!fixture.exists(id));
        assert!(!fixture.path(id, true).exists());
    }
    assert_eq!(
        writer_path.metadata().unwrap().ino(),
        writer_inode,
        "failed acquisition unlinked another writer's lock"
    );
    assert!(!fixture.store.root.join("pending.json").exists());
    release_native_writer(&fixture, busy, writer);
    assert_eq!(fixture.run(true).unwrap().deleted, 1);
}

#[test]
fn pinned_and_excluded_live_writers_are_filtered_before_forming_batches() {
    let mut fixture = Fixture::new();
    let ids = add_archives(&fixture, (MAX_BATCH_THREADS + 2) as u128);
    let pinned = &ids[0];
    let excluded = &ids[1];
    fixture
        .c
        .execute(
            "INSERT INTO thread_sections(id,name) VALUES(?,'Pinned')",
            [database::PINNED_SECTION_ID],
        )
        .unwrap();
    fixture
        .c
        .execute(
            "UPDATE threads SET thread_section_id=?,is_pinned=0 WHERE id=?",
            params![database::PINNED_SECTION_ID, pinned],
        )
        .unwrap();
    fixture.policy.exclusions.insert(excluded.clone());
    let pinned_writer = hold_native_writer(&fixture, pinned);
    let excluded_writer = hold_native_writer(&fixture, excluded);
    let protected_bytes: Vec<_> = [pinned, excluded]
        .iter()
        .map(|id| fs::read(fixture.path(id, true)).unwrap())
        .collect();

    let report = fixture.run(true).unwrap();

    assert_eq!(report.deleted, MAX_BATCH_THREADS as u64);
    assert_eq!(report.skipped, 2);
    for (id, expected) in [pinned, excluded].into_iter().zip(&protected_bytes) {
        assert!(fixture.exists(id));
        assert_eq!(fs::read(fixture.path(id, true)).unwrap(), *expected);
    }
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.id == *pinned && entry.reason == "pinned_or_unknown_pin")
    );
    assert!(
        report
            .entries
            .iter()
            .any(|entry| entry.id == *excluded && entry.reason == "excluded")
    );
    for id in &ids[2..] {
        assert!(!fixture.exists(id));
        assert!(!fixture.path(id, true).exists());
    }
    assert!(!fixture.store.root.join("pending.json").exists());
    release_native_writer(&fixture, pinned, pinned_writer);
    release_native_writer(&fixture, excluded, excluded_writer);
}

#[test]
fn legacy_thirty_two_item_journal_remains_recoverable() {
    let mut fixture = Fixture::new();
    // Deliberately fixed to the former writer's limit, independently of today's cap.
    let ids = add_archives(&fixture, 32);
    let items = batch_items(&fixture, &ids);
    let originals: Vec<_> = items
        .iter()
        .map(|item| fs::read(source(&fixture, item)).unwrap())
        .collect();
    save_batch(&fixture, &items);
    for item in &items {
        fs::rename(source(&fixture, item), staged(item)).unwrap();
    }

    engine::recover(&mut fixture.c, &fixture.policy, &fixture.store).unwrap();

    for ((item, expected), id) in items.iter().zip(&originals).zip(&ids) {
        assert_eq!(fs::read(source(&fixture, item)).unwrap(), *expected);
        assert!(!staged(item).exists());
        assert!(fixture.exists(id));
    }
    assert!(!fixture.store.root.join("pending.json").exists());
}

#[test]
fn maximum_batch_recovers_a_partially_staged_prefix_without_losing_any_member() {
    let mut fixture = Fixture::new();
    let ids = add_archives(&fixture, MAX_BATCH_THREADS as u128);
    let items = batch_items(&fixture, &ids);
    let originals: Vec<_> = items
        .iter()
        .map(|item| fs::read(source(&fixture, item)).unwrap())
        .collect();
    save_batch(&fixture, &items);
    let partial = MAX_BATCH_THREADS / 2 + 1;
    for item in items.iter().take(partial) {
        fs::rename(source(&fixture, item), staged(item)).unwrap();
    }
    let active = fixture.add(MAX_BATCH_THREADS as u128 + 1000, 0);
    let active_bytes = fs::read(fixture.path(&active, false)).unwrap();

    engine::recover(&mut fixture.c, &fixture.policy, &fixture.store).unwrap();

    for ((item, expected), id) in items.iter().zip(&originals).zip(&ids) {
        assert_eq!(fs::read(source(&fixture, item)).unwrap(), *expected);
        assert!(!staged(item).exists());
        assert!(fixture.exists(id));
    }
    assert!(fixture.exists(&active));
    assert_eq!(
        fs::read(fixture.path(&active, false)).unwrap(),
        active_bytes
    );
    assert!(!fixture.store.root.join("pending.json").exists());
    assert!(
        engine::recover(&mut fixture.c, &fixture.policy, &fixture.store)
            .unwrap()
            .is_none()
    );
}

#[test]
fn maximum_committed_batch_recovers_after_a_prefix_was_already_unlinked() {
    let mut fixture = Fixture::new();
    let ids = add_archives(&fixture, MAX_BATCH_THREADS as u128);
    let items = batch_items(&fixture, &ids);
    save_batch(&fixture, &items);
    for item in &items {
        fs::rename(source(&fixture, item), staged(item)).unwrap();
    }
    fixture.c.execute_batch("BEGIN IMMEDIATE").unwrap();
    for id in &ids {
        fixture
            .c
            .execute("DELETE FROM threads WHERE id=?", [id])
            .unwrap();
    }
    fixture.c.execute_batch("COMMIT").unwrap();
    for item in items.iter().take(MAX_BATCH_THREADS / 2 + 1) {
        fs::remove_file(staged(item)).unwrap();
    }
    let unrelated = fixture
        .home
        .join("archived_sessions/.unrelated-hidden-file");
    fs::write(&unrelated, b"never part of the journal").unwrap();
    let active = fixture.add(MAX_BATCH_THREADS as u128 + 1000, 0);
    let active_bytes = fs::read(fixture.path(&active, false)).unwrap();

    engine::recover(&mut fixture.c, &fixture.policy, &fixture.store).unwrap();

    for (item, id) in items.iter().zip(&ids) {
        assert!(!source(&fixture, item).exists());
        assert!(!staged(item).exists());
        assert!(!fixture.exists(id));
    }
    assert_eq!(fs::read(&unrelated).unwrap(), b"never part of the journal");
    assert!(fixture.exists(&active));
    assert_eq!(
        fs::read(fixture.path(&active, false)).unwrap(),
        active_bytes
    );
    assert!(!fixture.store.root.join("pending.json").exists());
    assert!(
        engine::recover(&mut fixture.c, &fixture.policy, &fixture.store)
            .unwrap()
            .is_none()
    );
}

#[test]
fn oversized_valid_json_journal_preserves_all_staging_and_the_receipt() {
    let mut fixture = Fixture::new();
    let ids = add_archives(&fixture, MAX_BATCH_THREADS as u128);
    let items = batch_items(&fixture, &ids);
    let first = source(&fixture, &items[0]);
    let first_staged = staged(&items[0]);
    let first_bytes = fs::read(&first).unwrap();
    fs::rename(&first, &first_staged).unwrap();
    let mut journal = save_batch(&fixture, &items);
    let expected: Value = serde_json::from_slice(&journal).unwrap();
    // Valid JSON permits trailing whitespace. Keep every semantic field valid
    // while exceeding the bounded reader, rather than relying on malformed JSON.
    journal.extend(std::iter::repeat_n(b' ', 1024 * 1024 + 1 - journal.len()));
    assert_eq!(serde_json::from_slice::<Value>(&journal).unwrap(), expected);
    let journal_path = fixture.store.root.join("pending.json");
    fs::write(&journal_path, &journal).unwrap();

    let error = engine::recover(&mut fixture.c, &fixture.policy, &fixture.store).unwrap_err();

    assert!(format!("{error:#}").contains("1 MiB"));
    assert!(!first.exists());
    assert_eq!(fs::read(first_staged).unwrap(), first_bytes);
    assert_eq!(fs::read(journal_path).unwrap(), journal);
    assert!(ids.iter().all(|id| fixture.exists(id)));
    for item in &items[1..] {
        assert!(source(&fixture, item).exists());
        assert!(!staged(item).exists());
    }
}

#[test]
fn busy_first_middle_and_last_members_leave_foreign_locks_and_finish_safe_peers() {
    const MEMBERS: usize = 5;
    for busy_index in [0, MEMBERS / 2, MEMBERS - 1] {
        let mut fixture = Fixture::new();
        let ids = add_archives(&fixture, MEMBERS as u128);
        let busy = &ids[busy_index];
        let writer = hold_native_writer(&fixture, busy);
        let writer_path = fixture
            .home
            .join("thread-writer-locks")
            .join(format!("{busy}.lock"));
        let foreign_inode = writer.metadata().unwrap().ino();
        let busy_bytes = fs::read(fixture.path(busy, true)).unwrap();

        let report = fixture.run(true).unwrap();

        assert_eq!(
            report.deleted,
            (MEMBERS - 1) as u64,
            "busy position {busy_index}"
        );
        assert_eq!(report.skipped, 1);
        assert_eq!(fs::read(fixture.path(busy, true)).unwrap(), busy_bytes);
        assert!(fixture.exists(busy));
        assert_eq!(writer_path.metadata().unwrap().ino(), foreign_inode);
        for id in ids.iter().filter(|id| *id != busy) {
            assert!(!fixture.exists(id));
            assert!(!fixture.path(id, true).exists());
            assert!(
                !fixture
                    .home
                    .join("thread-writer-locks")
                    .join(format!("{id}.lock"))
                    .exists()
            );
        }
        assert!(!fixture.store.root.join("pending.json").exists());
        release_native_writer(&fixture, busy, writer);
        assert_eq!(fixture.run(true).unwrap().deleted, 1);
    }
}
