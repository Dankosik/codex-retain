mod support;

use codex_retain::{database, engine};
use rusqlite::{Connection, params};
use serde_json::json;
use std::{
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, Write},
    os::unix::fs::symlink,
    process::{Command, Stdio},
    time::{Duration, UNIX_EPOCH},
};
use support::{Fixture, now};

#[test]
fn first_enable_grants_full_retention_to_an_old_archive() {
    let mut f = Fixture::new();
    let id = f.add(1, 1);
    let path = f.path(&id, true);
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(1)))
        .unwrap();
    f.age(&id);
    let before = now();
    database::install_capture(&mut f.c, &f.policy.owner).unwrap();
    let epoch = f.epoch(&id).unwrap();
    assert!(epoch >= before);
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 0);
    assert_eq!(report.entries[0].reason.as_str(), "within_retention");
    assert!(path.exists() && f.exists(&id));
    assert!(report.actual_reclaimed_bytes.is_none());
}

#[test]
fn expiry_requires_strictly_more_than_retention_and_preview_is_read_only() {
    let mut f = Fixture::new();
    let id = f.add(2, 1);
    let epoch = f.epoch(&id).unwrap();
    let bytes = fs::read(f.path(&id, true)).unwrap();
    let boundary = epoch + f.policy.duration();
    let report = engine::execute(
        &mut f.c,
        &f.policy,
        &f.store,
        boundary,
        engine::ExecutionMode::Preview,
    )
    .unwrap();
    assert_eq!(report.eligible, 0);
    let preview = engine::execute(
        &mut f.c,
        &f.policy,
        &f.store,
        boundary + 1,
        engine::ExecutionMode::Preview,
    )
    .unwrap();
    assert_eq!(preview.eligible, 1);
    assert_eq!(preview.deleted, 0);
    assert_eq!(fs::read(f.path(&id, true)).unwrap(), bytes);
    assert!(f.exists(&id));
    let report = engine::execute(
        &mut f.c,
        &f.policy,
        &f.store,
        boundary + 1,
        engine::ExecutionMode::Run,
    )
    .unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(report.logical_bytes_removed, bytes.len() as u64);
    assert!(!f.path(&id, true).exists() && !f.exists(&id));
}

#[test]
fn restore_and_rearchive_between_cleaner_runs_resets_capture() {
    let mut f = Fixture::new();
    let id = f.add(3, 1);
    f.age(&id);
    assert!(f.epoch(&id).unwrap() < now() - f.policy.duration());
    f.c.execute(
        "UPDATE threads SET archived=0,archived_at=NULL WHERE id=?",
        [&id],
    )
    .unwrap();
    assert_eq!(f.epoch(&id), None);
    assert_eq!(
        database::lookup_thread(&mut database::prepare_thread_lookup(&f.c).unwrap(), &id)
            .unwrap()
            .unwrap()
            .archived,
        0
    );
    // No preview or cleanup is run between restore and rearchive.
    let before = now();
    f.c.execute(
        "UPDATE threads SET archived=1,archived_at=? WHERE id=?",
        params![before, &id],
    )
    .unwrap();
    assert!(f.epoch(&id).unwrap() >= before);
    assert_eq!(f.run(true).unwrap().deleted, 0);
    assert!(f.path(&id, true).exists());
}

#[test]
fn timestamp_repair_is_a_new_conservative_epoch_but_title_edit_is_not() {
    let mut f = Fixture::new();
    let id = f.add(4, 1);
    f.age(&id);
    let old = f.epoch(&id).unwrap();
    f.c.execute("UPDATE threads SET title='renamed' WHERE id=?", [&id])
        .unwrap();
    assert_eq!(f.epoch(&id), Some(old));
    let before = now();
    f.c.execute("UPDATE threads SET archived_at=2 WHERE id=?", [&id])
        .unwrap();
    assert!(f.epoch(&id).unwrap() >= before);
    assert_eq!(f.run(true).unwrap().deleted, 0);
}

#[test]
fn active_unknown_pinned_excluded_and_related_threads_are_preserved() {
    let mut f = Fixture::new();
    let active = f.add(10, 0);
    let unknown = f.add(11, 2);
    let pinned = f.add(12, 1);
    let excluded = f.add(13, 1);
    let parent = f.add(14, 1);
    let child = f.add(15, 0);
    let normal = f.add(16, 1);
    for id in [&pinned, &excluded, &parent, &normal] {
        f.age(id);
    }
    f.c.execute("UPDATE threads SET is_pinned=1 WHERE id=?", [&pinned])
        .unwrap();
    f.policy.exclusions.insert(excluded.clone());
    f.c.execute(
        "INSERT INTO thread_spawn_edges VALUES(?,?,'open')",
        params![parent, child],
    )
    .unwrap();
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 1);
    for id in [&active, &unknown, &pinned, &excluded, &parent, &child] {
        assert!(f.exists(id), "{id} was removed");
    }
    assert!(f.path(&active, false).exists() && f.path(&child, false).exists());
    assert!(!f.exists(&normal));
    for (id, reason) in [
        (&unknown, "unknown_archive_status"),
        (&pinned, "pinned_or_unknown_pin"),
        (&excluded, "excluded"),
        (&parent, "dependent_thread"),
    ] {
        assert_eq!(
            report
                .entries
                .iter()
                .find(|e| &e.id == id)
                .unwrap()
                .reason
                .as_str(),
            reason
        );
    }
    let edges: i64 =
        f.c.query_row("SELECT COUNT(*) FROM thread_spawn_edges", [], |r| r.get(0))
            .unwrap();
    assert_eq!(edges, 1);
}

#[test]
fn missing_or_ambiguous_clock_and_history_mode_fail_closed() {
    for variant in 0..5 {
        let mut f = Fixture::new();
        let id = f.add(20 + variant, 1);
        f.age(&id);
        match variant {
            0 => {
                f.c.execute("UPDATE threads SET archived_at=NULL WHERE id=?", [&id])
                    .unwrap();
            }
            1 => {
                f.c.execute("DELETE FROM codex_retain_epochs WHERE thread_id=?", [&id])
                    .unwrap();
            }
            2 => {
                f.c.execute(
                    "UPDATE codex_retain_epochs SET codex_archived_at=2 WHERE thread_id=?",
                    [&id],
                )
                .unwrap();
            }
            3 => {
                f.c.execute(
                    "UPDATE threads SET history_mode='future-format' WHERE id=?",
                    [&id],
                )
                .unwrap();
            }
            _ => {
                f.c.execute(
                    "UPDATE codex_retain_epochs SET archived_since=? WHERE thread_id=?",
                    params![now() + 100, &id],
                )
                .unwrap();
            }
        }
        assert_eq!(f.run(true).unwrap().deleted, 0);
        assert!(f.exists(&id) && f.path(&id, true).exists());
    }
}

#[test]
fn missing_trigger_and_schema_or_migration_drift_are_rejected() {
    let mut f = Fixture::new();
    let id = f.add(30, 1);
    f.age(&id);
    f.c.execute_batch("DROP TRIGGER codex_retain_update")
        .unwrap();
    assert!(f.run(true).is_err());
    assert!(f.exists(&id) && f.path(&id, true).exists());
    let f = Fixture::new();
    f.c.execute_batch("CREATE TABLE alien_extension(value TEXT)")
        .unwrap();
    assert!(database::open(&f.home, database::DatabaseAccess::ReadWrite).is_err());
    let f = Fixture::new();
    f.c.execute("UPDATE _sqlx_migrations SET checksum=x'00' WHERE version=(SELECT MAX(version) FROM _sqlx_migrations)",[]).unwrap();
    assert!(database::open(&f.home, database::DatabaseAccess::ReadWrite).is_err());
    let f = Fixture::new();
    f.c.execute("UPDATE backfill_state SET status='running'", [])
        .unwrap();
    assert!(database::open(&f.home, database::DatabaseAccess::ReadWrite).is_err());
}

#[test]
fn database_replacement_invalidates_saved_policy_identity() {
    let mut f = Fixture::new();
    let id = f.add(31, 1);
    f.age(&id);
    let db = f.home.join(database::DB_NAME);
    let old = f.home.join("old-database");
    fs::rename(&db, &old).unwrap();
    fs::copy(&old, &db).unwrap();
    assert!(f.run(true).is_err());
    assert!(f.exists(&id) && f.path(&id, true).exists());
}

#[test]
fn malformed_identity_shared_history_and_oversized_headers_are_preserved() {
    for variant in 0..5 {
        let mut f = Fixture::new();
        let id = f.add(40 + variant, 1);
        f.age(&id);
        let value=match variant {
            0=>b"not-json\n".to_vec(),
            1=>format!("{}\n",json!({"type":"session_meta","payload":{"id":"wrong"}})).into_bytes(),
            2=>format!("{}\n",json!({"type":"session_meta","payload":{"id":id,"history_mode":"paginated"}})).into_bytes(),
            3=>format!("{}\n",json!({"type":"session_meta","payload":{"id":id,"history_base":{"thread_id":"other"}}})).into_bytes(),
            _=>vec![b'x';1024*1024+2],
        };
        fs::write(f.path(&id, true), &value).unwrap();
        let report = f.run(true).unwrap();
        assert_eq!(report.deleted, 0);
        assert_eq!(fs::read(f.path(&id, true)).unwrap(), value);
        assert!(f.exists(&id));
    }
}

#[test]
fn database_path_escape_never_touches_an_outside_file() {
    let mut f = Fixture::new();
    let id = f.add(50, 1);
    f.age(&id);
    let outside = f.temp.path().join("outside.jsonl");
    fs::write(&outside, b"outside sentinel").unwrap();
    f.c.execute(
        "UPDATE threads SET rollout_path=? WHERE id=?",
        params![outside.to_str().unwrap(), &id],
    )
    .unwrap();
    assert_eq!(f.run(true).unwrap().deleted, 0);
    assert_eq!(fs::read(&outside).unwrap(), b"outside sentinel");
    assert!(f.path(&id, true).exists());
}

#[test]
fn symlink_hardlink_and_database_alias_do_not_delete() {
    for variant in [0, 1, 3] {
        let mut f = Fixture::new();
        let id = f.add(60 + variant, 1);
        f.age(&id);
        let path = f.path(&id, true);
        let original = fs::read(&path).unwrap();
        let other = f.temp.path().join("other");
        match variant {
            0 => {
                fs::rename(&path, &other).unwrap();
                symlink(&other, &path).unwrap();
            }
            1 => {
                fs::hard_link(&path, &other).unwrap();
            }
            _ => {
                let alias = f.add(600, 0);
                f.c.execute(
                    "UPDATE threads SET rollout_path=? WHERE id=?",
                    params![path.to_str().unwrap(), alias],
                )
                .unwrap();
            }
        }
        if let Ok(report) = f.run(true) {
            assert_eq!(report.deleted, 0);
        }
        assert_eq!(fs::read(&path).unwrap(), original);
        assert!(f.exists(&id));
    }
}

#[test]
fn compressed_deletion_removes_real_representation_and_is_idempotent() {
    let mut f = Fixture::new();
    let id = f.add(70, 1);
    f.age(&id);
    let path = f.path(&id, true);
    let compressed = path.with_extension("jsonl.zst");
    let source = fs::read(&path).unwrap();
    let encoded = zstd::stream::encode_all(source.as_slice(), 1).unwrap();
    fs::write(&compressed, &encoded).unwrap();
    fs::remove_file(&path).unwrap();
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 1);
    assert_eq!(report.logical_bytes_removed, encoded.len() as u64);
    assert!(report.actual_reclaimed_bytes.is_none());
    assert!(!compressed.exists() && !path.exists() && !f.exists(&id));
    assert!(!f.store.root.join("pending.json").exists());
    let repeat = f.run(true).unwrap();
    assert_eq!(repeat.deleted, 0);
    assert_eq!(repeat.examined, 0);
}

#[test]
fn interrupted_before_rename_clears_only_the_owned_intent() {
    let mut f = Fixture::new();
    let id = f.add(80, 1);
    let source = f.path(&id, true);
    let content = fs::read(&source).unwrap();
    f.pending(&id, &source);
    assert!(
        engine::recover(&mut f.c, &f.policy, &f.store)
            .unwrap()
            .is_some()
    );
    assert_eq!(fs::read(&source).unwrap(), content);
    assert!(f.exists(&id));
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn interrupted_uncommitted_sqlite_delete_restores_staged_rollout() {
    let mut f = Fixture::new();
    let id = f.add(81, 1);
    f.age(&id);
    let active = f.add(810, 0);
    let source = f.path(&id, true);
    let content = fs::read(&source).unwrap();
    let staged = f.pending(&id, &source);
    f.c.execute_batch("BEGIN IMMEDIATE").unwrap();
    f.c.execute("DELETE FROM threads WHERE id=?", [&id])
        .unwrap();
    fs::rename(&source, &staged).unwrap();
    drop(std::mem::replace(
        &mut f.c,
        Connection::open_in_memory().unwrap(),
    ));
    f.c = database::open(&f.home, database::DatabaseAccess::ReadWrite).unwrap();
    assert!(
        f.exists(&id),
        "SQLite did not roll back the uncommitted deletion"
    );
    assert!(
        engine::recover(&mut f.c, &f.policy, &f.store)
            .unwrap()
            .is_some()
    );
    assert_eq!(fs::read(&source).unwrap(), content);
    assert!(!staged.exists());
    assert!(f.exists(&active) && f.path(&active, false).exists());
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn interrupted_after_commit_finishes_only_recorded_staged_file() {
    let mut f = Fixture::new();
    let id = f.add(82, 1);
    let source = f.path(&id, true);
    let staged = f.pending(&id, &source);
    fs::rename(&source, &staged).unwrap();
    f.c.execute("DELETE FROM threads WHERE id=?", [&id])
        .unwrap();
    let unrelated = f.home.join("archived_sessions/.user-backup");
    fs::write(&unrelated, b"preserve unrelated hidden file").unwrap();
    assert!(
        engine::recover(&mut f.c, &f.policy, &f.store)
            .unwrap()
            .is_some()
    );
    assert!(!staged.exists() && !source.exists());
    assert_eq!(
        fs::read(&unrelated).unwrap(),
        b"preserve unrelated hidden file"
    );
    assert!(
        engine::recover(&mut f.c, &f.policy, &f.store)
            .unwrap()
            .is_none()
    );
}

#[test]
fn recovery_never_overwrites_a_new_original_or_unlinks_replaced_staging() {
    for replaced_stage in [false, true] {
        let mut f = Fixture::new();
        let id = f.add(83, 1);
        let source = f.path(&id, true);
        let staged = f.pending(&id, &source);
        fs::rename(&source, &staged).unwrap();
        if replaced_stage {
            let retained = f.temp.path().join("retained-original");
            fs::rename(&staged, &retained).unwrap();
            fs::write(&staged, b"new staged inode").unwrap();
        } else {
            fs::write(&source, b"new original inode").unwrap();
        }
        assert!(engine::recover(&mut f.c, &f.policy, &f.store).is_err());
        assert!(staged.exists() && f.store.root.join("pending.json").exists());
        if !replaced_stage {
            assert_eq!(fs::read(&source).unwrap(), b"new original inode");
        }
        assert!(f.exists(&id));
    }
}

#[test]
fn a_rolled_back_archive_transition_does_not_reset_the_epoch() {
    let f = Fixture::new();
    let id = f.add(90, 1);
    f.age(&id);
    let epoch = f.epoch(&id);
    f.c.execute_batch("BEGIN IMMEDIATE").unwrap();
    f.c.execute(
        "UPDATE threads SET archived=0,archived_at=NULL WHERE id=?",
        [&id],
    )
    .unwrap();
    assert_eq!(f.epoch(&id), None);
    f.c.execute_batch("ROLLBACK").unwrap();
    assert_eq!(f.epoch(&id), epoch);
}

#[test]
fn sqlite_writer_contention_preserves_rows_and_rollouts() {
    let mut f = Fixture::new();
    let id = f.add(91, 1);
    f.age(&id);
    f.c.busy_timeout(Duration::from_millis(30)).unwrap();
    let other = Connection::open(f.home.join(database::DB_NAME)).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 0);
    assert!(f.path(&id, true).exists() && f.exists(&id));
    assert!(!f.store.root.join("pending.json").exists());
    other.execute_batch("ROLLBACK").unwrap();
    assert_eq!(f.run(true).unwrap().deleted, 1);
}

#[test]
fn pause_disabled_and_backward_activation_clock_prevent_mutation() {
    let mut f = Fixture::new();
    let id = f.add(92, 1);
    f.age(&id);
    f.policy.paused = true;
    assert!(f.run(true).is_err());
    assert_eq!(f.run(false).unwrap().eligible, 1);
    f.policy.paused = false;
    f.policy.enabled = false;
    assert!(f.run(true).is_err());
    f.policy.enabled = true;
    let before_activation = f.policy.enabled_at - 1;
    assert!(
        engine::execute(
            &mut f.c,
            &f.policy,
            &f.store,
            before_activation,
            engine::ExecutionMode::Run
        )
        .is_err()
    );
    assert!(f.path(&id, true).exists() && f.exists(&id));
}

// Executed as a separate test process by the contention test below. The child
// acquires raw OS locks rather than calling the utility's lock implementation.
#[test]
fn external_lock_holder() {
    let Some(root) = std::env::var_os("CODEX_RETAIN_TEST_LOCK_HOME") else {
        return;
    };
    let home = std::path::PathBuf::from(root);
    let kind = std::env::var("CODEX_RETAIN_TEST_LOCK_KIND").unwrap();
    let guard = if kind == "maintenance" {
        fs::create_dir_all(home.join(".tmp")).unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(home.join(".tmp/rollout-maintenance.lock"))
            .unwrap();
        file.lock().unwrap();
        file
    } else {
        let dir = home.join("thread-writer-locks");
        fs::create_dir_all(&dir).unwrap();
        let coordination = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(".coordination.lock"))
            .unwrap();
        coordination.lock().unwrap();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dir.join(format!("{kind}.lock")))
            .unwrap();
        file.lock().unwrap();
        drop(coordination);
        file
    };
    println!("LOCK_READY");
    std::io::stdout().flush().unwrap();
    let mut input = String::new();
    std::io::stdin().read_line(&mut input).unwrap();
    drop(guard);
}

#[test]
fn cross_process_thread_and_maintenance_locks_skip_then_allow_deletion() {
    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    for maintenance in [false, true] {
        let mut f = Fixture::new();
        let id = f.add(100, 1);
        f.age(&id);
        let mut child = ChildGuard(
            Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "external_lock_holder", "--nocapture"])
                .env("CODEX_RETAIN_TEST_LOCK_HOME", &f.home)
                .env(
                    "CODEX_RETAIN_TEST_LOCK_KIND",
                    if maintenance { "maintenance" } else { &id },
                )
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .unwrap(),
        );
        let mut output = BufReader::new(child.0.stdout.take().unwrap());
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let reader = std::thread::spawn(move || {
            loop {
                let mut line = String::new();
                if output.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                if line.contains("LOCK_READY") {
                    let _ = ready_tx.send(());
                }
            }
        });
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("external lock helper did not become ready");
        let report = f.run(true).unwrap();
        assert_eq!(report.deleted, 0);
        assert!(f.exists(&id) && f.path(&id, true).exists());
        child
            .0
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"release\n")
            .unwrap();
        assert!(child.0.wait().unwrap().success());
        reader.join().unwrap();
        assert_eq!(f.run(true).unwrap().deleted, 1);
    }
}

#[test]
fn native_pinned_section_protects_even_a_pre_pin_deletion_snapshot() {
    let mut f = Fixture::new();
    let id = f.add(110, 1);
    f.age(&id);
    let before_pin =
        database::lookup_thread(&mut database::prepare_thread_lookup(&f.c).unwrap(), &id)
            .unwrap()
            .unwrap();
    assert_eq!(before_pin.pinned, 0);
    f.c.execute(
        "INSERT INTO thread_sections(id,name) VALUES(?,'Pinned')",
        [database::PINNED_SECTION_ID],
    )
    .unwrap();
    f.c.execute(
        "UPDATE threads SET thread_section_id=?,is_pinned=0 WHERE id=?",
        params![database::PINNED_SECTION_ID, &id],
    )
    .unwrap();
    let raw_pin: i64 =
        f.c.query_row("SELECT is_pinned FROM threads WHERE id=?", [&id], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        raw_pin, 0,
        "exercise native section pin rather than the legacy bit"
    );
    let preview = f.run(false).unwrap();
    assert_eq!(preview.eligible, 0);
    assert_eq!(preview.entries[0].reason.as_str(), "pinned_or_unknown_pin");
    assert_eq!(f.run(true).unwrap().deleted, 0);
    assert!(
        database::delete_row(&mut database::prepare_delete(&f.c).unwrap(), &before_pin).is_err(),
        "final SQL guard accepted a thread pinned after its snapshot"
    );
    assert!(f.exists(&id) && f.path(&id, true).exists());
}

#[test]
fn permission_denied_for_staging_preserves_the_durable_recovery_intent() {
    use std::os::unix::fs::PermissionsExt;
    struct RestorePermissions {
        path: std::path::PathBuf,
        permissions: fs::Permissions,
    }
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            let _ = fs::set_permissions(&self.path, self.permissions.clone());
        }
    }
    let mut f = Fixture::new();
    let id = f.add(111, 1);
    let source = f.path(&id, true);
    let content = fs::read(&source).unwrap();
    let staged = f.pending(&id, &source);
    fs::rename(&source, &staged).unwrap();
    let journal = f.store.root.join("pending.json");
    let original_intent = fs::read(&journal).unwrap();
    let archive = source.parent().unwrap().to_path_buf();
    let guard = RestorePermissions {
        permissions: archive.metadata().unwrap().permissions(),
        path: archive.clone(),
    };
    fs::set_permissions(&archive, fs::Permissions::from_mode(0o000)).unwrap();
    let denied = match fs::symlink_metadata(&staged) {
        Err(error) => error.kind() == std::io::ErrorKind::PermissionDenied,
        Ok(_) => false,
    };
    if !denied {
        drop(guard);
        eprintln!("permission regression not exercised: account bypasses directory permissions");
        return;
    }
    let result = engine::recover(&mut f.c, &f.policy, &f.store);
    assert!(
        result.is_err(),
        "PermissionDenied must not be treated as a missing staged file"
    );
    assert_eq!(
        fs::read(&journal).unwrap(),
        original_intent,
        "recovery discarded evidence while staging was inaccessible"
    );
    assert!(f.exists(&id));
    drop(guard);
    assert_eq!(fs::read(&staged).unwrap(), content);
    assert!(
        engine::recover(&mut f.c, &f.policy, &f.store)
            .unwrap()
            .is_some()
    );
    assert_eq!(fs::read(&source).unwrap(), content);
}

#[test]
fn oversized_zstd_window_is_rejected_before_processing_a_small_valid_frame() {
    let mut f = Fixture::new();
    let id = f.add(112, 1);
    f.age(&id);
    let plain = f.path(&id, true);
    let content = fs::read(&plain).unwrap();
    // Zstandard frame: no advertised content size, 16 MiB window (log=24),
    // one last raw block. The actual transcript remains under one kilobyte.
    let mut frame = vec![0x28, 0xb5, 0x2f, 0xfd, 0x00, 0x70];
    let block_header = ((content.len() as u32) << 3) | 1;
    frame.extend_from_slice(&block_header.to_le_bytes()[..3]);
    frame.extend_from_slice(&content);
    assert_eq!(
        zstd::stream::decode_all(frame.as_slice()).unwrap(),
        content,
        "the fixture must be a valid frame, not merely corrupt bytes"
    );
    let compressed = plain.with_extension("jsonl.zst");
    fs::write(&compressed, &frame).unwrap();
    fs::remove_file(&plain).unwrap();
    let report = f.run(true).unwrap();
    assert_eq!(
        report.deleted, 0,
        "16 MiB declared window exceeded the 8 MiB decoder cap"
    );
    assert_eq!(
        report.entries[0].reason.as_str(),
        "unsafe_or_unavailable_artifact"
    );
    assert_eq!(fs::read(&compressed).unwrap(), frame);
    assert!(f.exists(&id));
}

#[test]
fn capture_installation_seeds_preexisting_archive_without_changing_native_rows() {
    let mut f = Fixture::new();
    database::uninstall_capture(&mut f.c, &f.policy).unwrap();
    let archived = f.add(113, 1);
    let active = f.add(114, 0);
    fn snapshot(c: &Connection) -> Vec<(String, String, i64, Option<i64>, String)> {
        c.prepare("SELECT id,rollout_path,archived,archived_at,title FROM threads ORDER BY id")
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap()
    }
    let before = snapshot(&f.c);
    let archive_bytes = fs::read(f.path(&archived, true)).unwrap();
    let active_bytes = fs::read(f.path(&active, false)).unwrap();
    let before_install = now();
    assert_eq!(
        database::install_capture(&mut f.c, &f.policy.owner).unwrap(),
        1
    );
    assert_eq!(snapshot(&f.c), before);
    assert!(f.epoch(&archived).unwrap() >= before_install);
    assert_eq!(f.epoch(&active), None);
    assert_eq!(fs::read(f.path(&archived, true)).unwrap(), archive_bytes);
    assert_eq!(fs::read(f.path(&active, false)).unwrap(), active_bytes);
    assert_eq!(f.policy.schema, 1);
    assert_eq!(f.run(true).unwrap().deleted, 0);
}

#[test]
fn legacy_owner_cleanup_includes_same_owner_sessions_copy() {
    let mut f = Fixture::new();
    let id = f.add(63, 1);
    f.age(&id);
    fs::copy(f.path(&id, true), f.path(&id, false)).unwrap();
    assert_eq!(f.run(true).unwrap().deleted, 1);
    assert!(!f.exists(&id));
    assert!(!f.path(&id, true).exists() && !f.path(&id, false).exists());
}
