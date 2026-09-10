#![cfg(unix)]

#[allow(dead_code)]
mod support;

use codex_retain::{engine, fsutil};
use serde_json::json;
use std::{fs, os::unix::fs::MetadataExt, path::PathBuf};
use support::Fixture;

fn segment(f: &Fixture, owner: &str, rollout: &str, archived: bool, base: Option<&str>) -> PathBuf {
    let mut path = f.path(rollout, archived);
    if owner != rollout {
        path.set_file_name(format!(
            "rollout-2025-01-01T00-00-00-{owner}_{rollout}.jsonl"
        ));
    }
    let header = json!({"type":"session_meta","payload":{
        "id":owner,"history_mode":"paginated","history_base":base.map(|id| json!({
            "thread_id":id,"end_ordinal_exclusive":2,"end_byte_offset":200
        }))
    }});
    fs::write(
        &path,
        format!("{header}\n{{\"type\":\"event_msg\",\"payload\":{{\"message\":\"synthetic\"}}}}\n"),
    )
    .unwrap();
    path
}

fn paginated(f: &Fixture, n: u128, base: Option<&str>) -> String {
    let id = f.add(n, 1);
    segment(f, &id, &id, true, base);
    f.c.execute(
        "UPDATE threads SET history_mode='paginated' WHERE id=?",
        [&id],
    )
    .unwrap();
    id
}

fn selected_segment(f: &Fixture, owner: &str, n: u128, base: Option<&str>) -> PathBuf {
    let rollout = uuid::Uuid::from_u128(n).to_string();
    let path = segment(f, owner, &rollout, true, base);
    f.c.execute(
        "UPDATE threads SET rollout_path=? WHERE id=?",
        [path.to_str().unwrap(), owner],
    )
    .unwrap();
    path
}

fn pending(f: &Fixture, owner: &str, paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut staged = Vec::new();
    let items: Vec<_> = paths.iter().map(|path| {
            let filename = path.file_name().unwrap().to_str().unwrap();
            let name = filename.strip_suffix(".jsonl.zst").or_else(|| filename.strip_suffix(".jsonl")).unwrap();
        let rollout = &name[name.len()-36..];
        let stage = f.home.join("archived_sessions").join(format!(".codex-retain-pending-{owner}-{rollout}"));
        let meta = path.metadata().unwrap();
        staged.push(stage.clone());
        json!({"thread_id":owner,"rollout_id":rollout,"artifact":{
            "path":path,"identity":fsutil::Identity::of(&meta),"bytes":meta.len(),"allocated":meta.blocks()*512
        },"staged":stage})
    }).collect();
    fsutil::atomic_json(
        &f.store.root.join("pending.json"),
        &json!({"schema":3,"owner":f.policy.owner,"items":items}),
    )
    .unwrap();
    staged
}

#[test]
fn paginated_revert_segments_receive_grace_then_delete_as_one_thread() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 9001, None);
    let old = f.path(&owner, true);
    let selected = selected_segment(&f, &owner, 9002, Some(&owner));
    let active = f.add(9003, 0);
    let active_bytes = fs::read(f.path(&active, false)).unwrap();
    assert_eq!(f.run(true).unwrap().deleted, 0);
    assert!(old.exists() && selected.exists());
    f.age(&owner);
    let bytes = old.metadata().unwrap().len() + selected.metadata().unwrap().len();
    let preview = f.run(false).unwrap();
    assert_eq!(preview.eligible, 1);
    assert_eq!(preview.selected_bytes, bytes);
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(report.logical_bytes_removed, bytes);
    assert!(!old.exists() && !selected.exists() && !f.exists(&owner));
    assert_eq!(fs::read(f.path(&active, false)).unwrap(), active_bytes);
    assert_eq!(f.run(true).unwrap().deleted, 0);
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn a_reference_to_an_old_segment_protects_the_whole_thread() {
    for archived_child in [false, true] {
        let mut f = Fixture::new();
        let owner = paginated(&f, 9101, None);
        let selected = selected_segment(&f, &owner, 9102, Some(&owner));
        let child = uuid::Uuid::from_u128(9103).to_string();
        // Orphan files matter even without a child row in the state database.
        let path = segment(&f, &child, &child, archived_child, Some(&owner));
        let original = fs::read(&path).unwrap();
        f.age(&owner);
        let report = f.run(true).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(
            report.entries[0].reason,
            engine::EntryReason::ReferencedHistory
        );
        assert!(selected.exists() && f.path(&owner, true).exists() && f.exists(&owner));
        assert_eq!(fs::read(path).unwrap(), original);
    }
}

#[test]
fn deleting_a_leaf_preserves_its_base_until_a_later_run() {
    let mut f = Fixture::new();
    let base = paginated(&f, 9201, None);
    let child = paginated(&f, 9202, Some(&base));
    f.age(&base);
    f.age(&child);
    let original = fs::read(f.path(&base, true)).unwrap();
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(f.exists(&base) && !f.exists(&child));
    assert_eq!(fs::read(f.path(&base, true)).unwrap(), original);
    assert_eq!(f.run(true).unwrap().deleted, 1);
    assert!(!f.exists(&base));
}

#[test]
fn compressed_paginated_rollouts_are_deleted_without_touching_unreferenced_neighbors() {
    let mut f = Fixture::new();
    let id = paginated(&f, 9301, None);
    let path = f.path(&id, true);
    let compressed = path.with_extension("jsonl.zst");
    fs::write(
        &compressed,
        zstd::stream::encode_all(fs::read(&path).unwrap().as_slice(), 0).unwrap(),
    )
    .unwrap();
    fs::remove_file(path).unwrap();
    f.age(&id);
    let bytes = compressed.metadata().unwrap().len();
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(report.logical_bytes_removed, bytes);
    assert!(!compressed.exists());
}

#[test]
fn partial_paginated_staging_recovers_every_segment_when_row_survives() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 9401, None);
    let paths = vec![
        f.path(&owner, true),
        selected_segment(&f, &owner, 9402, Some(&owner)),
    ];
    let before: Vec<_> = paths.iter().map(|path| fs::read(path).unwrap()).collect();
    let staged = pending(&f, &owner, &paths);
    fs::rename(&paths[0], &staged[0]).unwrap();
    engine::recover(&mut f.c, &f.policy, &f.store).unwrap();
    assert!(f.exists(&owner));
    for (path, bytes) in paths.iter().zip(before) {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert!(staged.iter().all(|path| !path.exists()));
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn committed_paginated_recovery_finishes_only_remaining_owned_stages() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 9501, None);
    let paths = vec![
        f.path(&owner, true),
        selected_segment(&f, &owner, 9502, Some(&owner)),
    ];
    let neighbor = paginated(&f, 9503, None);
    let original = fs::read(f.path(&neighbor, true)).unwrap();
    let staged = pending(&f, &owner, &paths);
    for (source, target) in paths.iter().zip(&staged) {
        fs::rename(source, target).unwrap();
    }
    f.c.execute("DELETE FROM threads WHERE id=?", [&owner])
        .unwrap();
    fs::remove_file(&staged[0]).unwrap();
    engine::recover(&mut f.c, &f.policy, &f.store).unwrap();
    assert!(paths.iter().chain(&staged).all(|path| !path.exists()));
    assert_eq!(fs::read(f.path(&neighbor, true)).unwrap(), original);
    assert!(f.exists(&neighbor));
    assert_eq!(
        f.c.query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))
            .unwrap(),
        "ok"
    );
}

#[test]
fn groups_split_on_the_rollout_count_bound_before_any_journal_write() {
    let mut f = Fixture::new();
    let mut paths = Vec::new();
    for number in [9601, 9701] {
        let owner = paginated(&f, number, None);
        paths.push(f.path(&owner, true));
        for offset in 1..65 {
            paths.push(selected_segment(&f, &owner, number + offset, Some(&owner)));
        }
        f.age(&owner);
    }
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 2);
    assert!(paths.iter().all(|path| !path.exists()));
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn a_foreign_database_alias_to_an_old_segment_blocks_deletion() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 9801, None);
    let selected = selected_segment(&f, &owner, 9802, Some(&owner));
    let foreign = f.add(9803, 0);
    let old = f.path(&owner, true);
    f.c.execute(
        "UPDATE threads SET rollout_path=? WHERE id=?",
        [old.to_str().unwrap(), &foreign],
    )
    .unwrap();
    f.age(&owner);
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 0);
    assert!(f.exists(&owner) && old.exists() && selected.exists());
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn recovery_rejects_mismatched_rollout_identity_before_mutation() {
    let mut f = Fixture::new();
    let owner = paginated(&f, 9901, None);
    let paths = vec![
        f.path(&owner, true),
        selected_segment(&f, &owner, 9902, Some(&owner)),
    ];
    let staged = pending(&f, &owner, &paths);
    fs::rename(&paths[0], &staged[0]).unwrap();
    let journal = f.store.root.join("pending.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&journal).unwrap()).unwrap();
    value["items"][0]["rollout_id"] = json!(uuid::Uuid::from_u128(9999).to_string());
    fsutil::atomic_json(&journal, &value).unwrap();
    assert!(engine::recover(&mut f.c, &f.policy, &f.store).is_err());
    assert!(staged[0].exists() && !paths[0].exists() && paths[1].exists());
    assert!(journal.exists() && f.exists(&owner));
}

#[test]
fn recovery_checks_staged_metadata_owner_instead_of_trusting_the_receipt() {
    let mut f = Fixture::new();
    let surviving = paginated(&f, 10001, None);
    let absent = uuid::Uuid::from_u128(10002).to_string();
    let original_path = f.path(&surviving, true);
    let path = original_path.with_file_name(format!(
        "rollout-2025-01-01T00-00-00-{absent}_{surviving}.jsonl"
    ));
    fs::rename(original_path, &path).unwrap();
    // The filename UUID is valid but distinct from the receipt's claimed owner.
    let staged = pending(&f, &absent, std::slice::from_ref(&path));
    let original = fs::read(&path).unwrap();
    fs::rename(&path, &staged[0]).unwrap();
    let error = engine::recover(&mut f.c, &f.policy, &f.store).unwrap_err();
    assert!(format!("{error:#}").contains("different thread"));
    assert_eq!(fs::read(&staged[0]).unwrap(), original);
    assert!(f.exists(&surviving) && f.store.root.join("pending.json").exists());
}

#[test]
fn database_and_rollout_history_formats_must_agree() {
    for mismatch in ["database", "header"] {
        let mut f = Fixture::new();
        let id = f.add(10101, 1);
        if mismatch == "database" {
            f.c.execute(
                "UPDATE threads SET history_mode='paginated' WHERE id=?",
                [&id],
            )
            .unwrap();
        } else {
            segment(&f, &id, &id, true, None);
        }
        f.age(&id);
        let report = f.run(true).unwrap();
        assert_eq!(report.deleted, 0);
        assert!(f.exists(&id) && f.path(&id, true).exists());
        assert!(!f.store.root.join("pending.json").exists());
    }
}

#[test]
fn compressed_schema_three_recovery_validates_owner_before_finishing() {
    let mut f = Fixture::new();
    let id = paginated(&f, 10201, None);
    let plain = f.path(&id, true);
    let compressed = plain.with_extension("jsonl.zst");
    fs::write(
        &compressed,
        zstd::stream::encode_all(fs::read(&plain).unwrap().as_slice(), 0).unwrap(),
    )
    .unwrap();
    fs::remove_file(plain).unwrap();
    let stages = pending(&f, &id, std::slice::from_ref(&compressed));
    fs::rename(&compressed, &stages[0]).unwrap();
    f.c.execute("DELETE FROM threads WHERE id=?", [&id])
        .unwrap();
    engine::recover(&mut f.c, &f.policy, &f.store).unwrap();
    assert!(!stages[0].exists() && !compressed.exists());
    assert!(!f.store.root.join("pending.json").exists());
}
