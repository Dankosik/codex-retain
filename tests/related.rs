#![cfg(unix)]

#[allow(dead_code)]
mod support;

use codex_retain::{engine, fsutil};
use serde_json::{Value, json};
use std::fs;
use support::Fixture;

fn link(f: &Fixture, parent: &str, child: &str, status: &str, sql: bool, source_only: bool) {
    if sql {
        f.c.execute(
            "INSERT INTO thread_spawn_edges(parent_thread_id,child_thread_id,status) VALUES(?,?,?)",
            [parent, child, status],
        )
        .unwrap();
    }
    let archived: i64 =
        f.c.query_row("SELECT archived FROM threads WHERE id=?", [child], |row| {
            row.get(0)
        })
        .unwrap();
    let path = f.path(child, archived == 1);
    let bytes = fs::read(&path).unwrap();
    let boundary = bytes.iter().position(|byte| *byte == b'\n').unwrap();
    let mut header: Value = serde_json::from_slice(&bytes[..boundary]).unwrap();
    if !source_only {
        header["payload"]["parent_thread_id"] = json!(parent);
    }
    header["payload"]["source"] =
        json!({"subagent":{"thread_spawn":{"parent_thread_id":parent,"depth":1}}});
    let mut output = serde_json::to_vec(&header).unwrap();
    output.extend_from_slice(&bytes[boundary..]);
    fs::write(path, output).unwrap();
}

fn edge_count(f: &Fixture) -> i64 {
    f.c.query_row("SELECT count(*) FROM thread_spawn_edges", [], |row| {
        row.get(0)
    })
    .unwrap()
}

#[test]
fn fully_expired_chain_cleans_in_one_run_and_removes_its_edges() {
    let mut f = Fixture::new();
    let root = f.add(11001, 1);
    let middle = f.add(11002, 1);
    let leaf = f.add(11003, 1);
    link(&f, &root, &middle, "open", true, false);
    link(&f, &middle, &leaf, "closed", true, false);
    for id in [&root, &middle, &leaf] {
        f.age(id);
    }
    // Parent deletion must not precede child deletion even inside this connection.
    f.c.execute_batch("CREATE TEMP TRIGGER parent_order BEFORE DELETE ON main.threads WHEN EXISTS(SELECT 1 FROM thread_spawn_edges WHERE parent_thread_id=old.id) BEGIN SELECT RAISE(ABORT,'parent still has child edges'); END;").unwrap();
    assert_eq!(f.run(false).unwrap().eligible, 3);
    let result = f.run(true).unwrap();
    assert_eq!(result.deleted, 3);
    assert_eq!(edge_count(&f), 0);
    for id in [&root, &middle, &leaf] {
        assert!(!f.exists(id) && !f.path(id, true).exists());
    }
    assert_eq!(f.run(true).unwrap().deleted, 0);
}

#[test]
fn pinned_or_not_due_parent_survives_expired_child_removal() {
    for pinned in [false, true] {
        let mut f = Fixture::new();
        let parent = f.add(11101, 1);
        let child = f.add(11102, 1);
        link(&f, &parent, &child, "open", true, false);
        f.age(&child);
        if pinned {
            f.age(&parent);
            f.c.execute("UPDATE threads SET is_pinned=1 WHERE id=?", [&parent])
                .unwrap();
        }
        let bytes = fs::read(f.path(&parent, true)).unwrap();
        assert_eq!(f.run(true).unwrap().deleted, 1);
        assert!(f.exists(&parent) && !f.exists(&child));
        assert_eq!(fs::read(f.path(&parent, true)).unwrap(), bytes);
        assert_eq!(edge_count(&f), 0);
    }
}

#[test]
fn protected_or_unavailable_child_keeps_its_ancestors() {
    for protection in ["active", "pinned", "excluded", "not_due", "missing_file"] {
        let mut f = Fixture::new();
        let parent = f.add(11201, 1);
        let child = f.add(11202, if protection == "active" { 0 } else { 1 });
        link(&f, &parent, &child, "closed", true, false);
        f.age(&parent);
        if protection != "not_due" && protection != "active" {
            f.age(&child);
        }
        match protection {
            "pinned" => {
                f.c.execute("UPDATE threads SET is_pinned=1 WHERE id=?", [&child])
                    .unwrap();
            }
            "excluded" => {
                f.policy.exclusions.insert(child.clone());
            }
            "missing_file" => {
                fs::remove_file(f.path(&child, true)).unwrap();
            }
            _ => {}
        }
        let report = f.run(true).unwrap();
        assert_eq!(report.deleted, 0, "{protection}");
        assert!(f.exists(&parent) && f.exists(&child));
        assert_eq!(edge_count(&f), 1);
        assert_eq!(
            report
                .entries
                .iter()
                .find(|entry| entry.id == parent)
                .unwrap()
                .reason,
            engine::EntryReason::DependentThread
        );
    }
}

#[test]
fn persisted_parent_identity_protects_parent_when_sql_edge_is_absent() {
    for source_only in [false, true] {
        let mut f = Fixture::new();
        let parent = f.add(11301, 1);
        let child = f.add(11302, 0);
        link(&f, &parent, &child, "open", false, source_only);
        f.age(&parent);
        let report = f.run(true).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(
            report.entries[0].reason,
            engine::EntryReason::DependentThread
        );
        assert!(f.exists(&parent) && f.exists(&child));
    }
}

#[test]
fn parent_writer_lock_blocks_child_and_failure_blocks_later_parent_layer() {
    let mut f = Fixture::new();
    let parent = f.add(11401, 1);
    let child = f.add(11402, 1);
    link(&f, &parent, &child, "open", true, false);
    f.age(&parent);
    f.age(&child);
    let guard = fsutil::ThreadLocks::acquire(&f.home, &[&parent]).unwrap();
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 0);
    assert!(f.exists(&parent) && f.exists(&child));
    assert_eq!(edge_count(&f), 1);
    drop(guard);
    assert_eq!(f.run(true).unwrap().deleted, 2);
}

#[test]
fn wide_family_spans_batches_without_leaving_parent_or_edges_behind() {
    let mut f = Fixture::new();
    let parent = f.add(11500, 1);
    f.age(&parent);
    for number in 11501..11761 {
        let child = f.add(number, 1);
        f.age(&child);
        link(&f, &parent, &child, "open", true, false);
    }
    assert_eq!(f.run(true).unwrap().deleted, 261);
    assert_eq!(edge_count(&f), 0);
    assert!(!f.exists(&parent));
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn failed_edge_delete_rolls_back_rows_and_edges_then_recovery_restores_file() {
    let mut f = Fixture::new();
    let parent = f.add(11801, 1);
    let child = f.add(11802, 1);
    link(&f, &parent, &child, "open", true, false);
    f.age(&parent);
    f.age(&child);
    let child_bytes = fs::read(f.path(&child, true)).unwrap();
    f.c.execute_batch("CREATE TEMP TRIGGER fail_edge_delete BEFORE DELETE ON main.thread_spawn_edges BEGIN SELECT RAISE(ABORT,'injected edge deletion failure'); END;").unwrap();
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 0);
    assert!(f.exists(&parent) && f.exists(&child));
    assert_eq!(edge_count(&f), 1);
    assert!(f.store.root.join("pending.json").exists());
    f.c.execute_batch("DROP TRIGGER fail_edge_delete;").unwrap();
    engine::recover(&mut f.c, &f.policy, &f.store).unwrap();
    assert_eq!(fs::read(f.path(&child, true)).unwrap(), child_bytes);
    assert_eq!(edge_count(&f), 1);
    assert_eq!(f.run(true).unwrap().deleted, 2);
    assert_eq!(edge_count(&f), 0);
}
