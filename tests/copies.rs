//! Synthetic physical-copy and cross-directory recovery contracts.

#[allow(dead_code)]
mod support;

use codex_retain::{engine, fsutil};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
use support::Fixture;

fn paginated(f: &Fixture, number: u128, archived: i64) -> String {
    let owner = f.add(number, archived);
    let path = f.path(&owner, archived != 0);
    write(&path, &owner, "archive version");
    f.c.execute(
        "UPDATE threads SET history_mode='paginated' WHERE id=?",
        [&owner],
    )
    .unwrap();
    if archived != 0 {
        f.age(&owner);
    }
    owner
}

fn write(path: &Path, owner: &str, content: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let header = json!({"type":"session_meta","payload":{"id":owner,"history_mode":"paginated"}});
    let bytes = format!(
        "{header}\n{}\n",
        json!({"type":"event_msg","payload":{"message":content}})
    )
    .into_bytes();
    let bytes = if path.extension().is_some_and(|extension| extension == "zst") {
        zstd::stream::encode_all(bytes.as_slice(), 0).unwrap()
    } else {
        bytes
    };
    fs::write(path, bytes).unwrap();
}

fn active_copy(f: &Fixture, owner: &str) -> PathBuf {
    let source = f.path(owner, true);
    let path = f
        .home
        .join("sessions/2025/01/01")
        .join(source.file_name().unwrap());
    write(&path, owner, "different older active-directory version");
    path
}

fn receipt(f: &Fixture, owner: &str, sources: &[PathBuf]) -> Vec<Value> {
    sources.iter().enumerate().map(|(slot, source)| {
        let metadata = fs::metadata(source).unwrap();
        let name = source.file_name().unwrap().to_str().unwrap();
        let stem = name.strip_suffix(".zst").unwrap_or(name).strip_suffix(".jsonl").unwrap();
        let rollout = &stem[stem.len()-36..];
        json!({"thread_id":owner,"rollout_id":rollout,"slot":slot as u32,
            "artifact":{"path":source,"identity":fsutil::Identity::of(&metadata),
                "bytes":metadata.len(),"allocated":metadata.blocks()*512},
            "staged":f.home.join("archived_sessions").join(format!(".codex-retain-pending-{owner}-{rollout}-{slot}"))})
    }).collect()
}

fn save(f: &Fixture, items: &[Value]) {
    fsutil::atomic_json(
        &f.store.root.join("pending.json"),
        &json!({"schema":4,"owner":f.policy.owner,"items":items}),
    )
    .unwrap();
}

fn staged(item: &Value) -> PathBuf {
    PathBuf::from(item["staged"].as_str().unwrap())
}

fn stage(item: &Value) {
    fs::rename(item["artifact"]["path"].as_str().unwrap(), staged(item)).unwrap();
}

#[test]
fn removes_all_same_owner_copies_even_with_different_contents_and_encodings() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 1, 1);
    let archive = f.path(&owner, true);
    let active = active_copy(&f, &owner);
    let compressed = archive.with_extension("jsonl.zst");
    write(&compressed, &owner, "third independently preserved version");
    assert_ne!(fs::read(&archive).unwrap(), fs::read(&active).unwrap());
    let expected_bytes = [&archive, &active, &compressed]
        .iter()
        .map(|path| fs::metadata(path).unwrap().len())
        .sum::<u64>();
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(report.logical_bytes_removed, expected_bytes);
    assert!(!f.exists(&owner));
    for path in [&archive, &active, &compressed] {
        assert!(!path.exists());
    }
}

#[test]
fn archive_directory_copy_does_not_authorize_deleting_an_unarchived_owner() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 2, 0);
    let archive = f.path(&owner, true);
    write(&archive, &owner, "stale archive copy of an active owner");
    assert_eq!(f.run(true).unwrap().deleted, 0);
    assert!(f.exists(&owner));
    assert!(archive.exists() && f.path(&owner, false).exists());
}

#[test]
fn physical_copy_limit_is_checked_before_staging_any_file() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 13, 1);
    let archived = f.path(&owner, true);
    let mut paths = vec![archived.clone()];
    for number in 0..128 {
        let path = f
            .home
            .join(format!("sessions/copy-{number}"))
            .join(archived.file_name().unwrap());
        write(&path, &owner, "synthetic duplicate version");
        paths.push(path);
    }
    let _ = f.run(true);
    assert!(f.exists(&owner));
    assert!(paths.iter().all(|path| path.exists()));
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn conflicting_owners_for_one_rollout_identity_fail_closed_globally() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 3, 1);
    let other = paginated(&f, 4, 1);
    let duplicate = active_copy(&f, &owner);
    write(&duplicate, &other, "conflicting owner");
    let _ = f.run(true);
    assert!(f.exists(&owner) && f.exists(&other));
    assert!(duplicate.exists() && f.path(&owner, true).exists() && f.path(&other, true).exists());
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn foreign_database_alias_to_sessions_copy_blocks_removal() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 5, 1);
    let alias_owner = f.add(6, 0);
    let active = active_copy(&f, &owner);
    f.c.execute(
        "UPDATE threads SET rollout_path=? WHERE id=?",
        rusqlite::params![active.to_str().unwrap(), alias_owner],
    )
    .unwrap();
    let _ = f.run(true);
    assert!(f.exists(&owner) && f.exists(&alias_owner));
    assert!(active.exists() && f.path(&owner, true).exists());
}

#[test]
fn precommit_partial_cross_directory_recovery_recreates_missing_source_parents() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 7, 1);
    let archive = f.path(&owner, true);
    let active = active_copy(&f, &owner);
    let bytes = fs::read(&active).unwrap();
    let items = receipt(&f, &owner, &[archive.clone(), active.clone()]);
    save(&f, &items);
    stage(&items[1]); // Interrupted before the other physical copy was staged.
    fs::remove_dir(active.parent().unwrap()).unwrap();
    fs::remove_dir(f.home.join("sessions/2025/01")).unwrap();
    fs::remove_dir(f.home.join("sessions/2025")).unwrap();
    engine::recover(&mut f.c, &f.policy, &f.store).unwrap();
    assert_eq!(fs::read(&active).unwrap(), bytes);
    assert!(archive.exists() && f.exists(&owner));
    assert!(!staged(&items[1]).exists() && !f.store.root.join("pending.json").exists());
}

#[test]
fn postcommit_recovery_finishes_every_staged_copy_without_recreating_sources() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 8, 1);
    let archive = f.path(&owner, true);
    let active = active_copy(&f, &owner);
    let items = receipt(&f, &owner, &[archive.clone(), active.clone()]);
    save(&f, &items);
    for item in &items {
        stage(item);
    }
    f.c.execute("DELETE FROM threads WHERE id=?", [&owner])
        .unwrap();
    fs::remove_dir(active.parent().unwrap()).unwrap();
    engine::recover(&mut f.c, &f.policy, &f.store).unwrap();
    for item in &items {
        assert!(!staged(item).exists());
    }
    assert!(!archive.exists() && !active.exists() && !f.exists(&owner));
    assert!(!active.parent().unwrap().exists());
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn duplicate_journal_slots_or_source_paths_are_rejected_before_effects() {
    for invalid in ["slot", "source"] {
        let mut f = Fixture::new();
        let owner = paginated(&f, 9, 1);
        let active = active_copy(&f, &owner);
        let mut items = receipt(&f, &owner, &[f.path(&owner, true), active]);
        let originals = items.clone();
        for item in &items {
            stage(item);
        }
        if invalid == "slot" {
            items[1]["slot"] = items[0]["slot"].clone();
            items[1]["staged"] = items[0]["staged"].clone();
        } else {
            items[1]["artifact"]["path"] = items[0]["artifact"]["path"].clone();
        }
        save(&f, &items);
        assert!(engine::recover(&mut f.c, &f.policy, &f.store).is_err());
        for item in &originals {
            assert!(staged(item).exists());
        }
        assert!(f.exists(&owner) && f.store.root.join("pending.json").exists());
    }
}

#[test]
fn unsafe_source_ancestry_is_rejected_before_restoring_any_copy() {
    for invalid in ["symlink", "traversal"] {
        let mut f = Fixture::new();
        let owner = paginated(&f, 10, 1);
        let active = active_copy(&f, &owner);
        let mut items = receipt(&f, &owner, &[f.path(&owner, true), active.clone()]);
        for item in &items {
            stage(item);
        }
        if invalid == "symlink" {
            fs::remove_dir(active.parent().unwrap()).unwrap();
            let outside = f.temp.path().join("outside");
            fs::create_dir(&outside).unwrap();
            std::os::unix::fs::symlink(outside, active.parent().unwrap()).unwrap();
        } else {
            items[1]["artifact"]["path"] = json!(
                f.home
                    .join("sessions/../archived_sessions")
                    .join(active.file_name().unwrap())
            );
        }
        save(&f, &items);
        assert!(engine::recover(&mut f.c, &f.policy, &f.store).is_err());
        for item in &items {
            assert!(staged(item).exists());
        }
        assert!(f.exists(&owner));
    }
}

#[test]
fn wrong_journal_owner_cannot_delete_a_surviving_owners_copy() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 11, 1);
    let absent_owner = uuid::Uuid::from_u128(999).to_string();
    let items = receipt(&f, &absent_owner, &[f.path(&owner, true)]);
    stage(&items[0]);
    save(&f, &items);
    assert!(engine::recover(&mut f.c, &f.policy, &f.store).is_err());
    assert!(f.exists(&owner) && staged(&items[0]).exists());
}

#[test]
fn missing_stale_selected_location_can_resolve_the_same_archived_filename() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 12, 1);
    let archived = f.path(&owner, true);
    let missing = f
        .home
        .join("sessions/2025/01/01")
        .join(archived.file_name().unwrap());
    f.c.execute(
        "UPDATE threads SET rollout_path=? WHERE id=?",
        rusqlite::params![missing.to_str().unwrap(), owner],
    )
    .unwrap();
    assert_eq!(f.run(true).unwrap().deleted, 1);
    assert!(!f.exists(&owner) && !archived.exists());
}
