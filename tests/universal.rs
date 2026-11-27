//! Universal multi-client (CLI, Codex Desktop, IDE) and multi-version
//! compatibility tests. All tests use isolated synthetic profiles.

#![cfg(target_os = "macos")]

#[allow(dead_code)]
mod support;

use assert_cmd::Command;
use codex_retain::{database, desktop, tracker};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};
use support::{Fixture, now};

fn command(root: &Path, state: &Path, json_mode: bool) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codex-retain"));
    cmd.args(["--state-dir"])
        .arg(state)
        .env("HOME", root)
        .env_remove("CODEX_RETAIN_STATE_DIR")
        .timeout(std::time::Duration::from_secs(10));
    if json_mode {
        cmd.arg("--json");
    }
    cmd
}

fn json_ok(cmd: &mut Command) -> Value {
    let output = cmd
        .assert()
        .success()
        .stderr("")
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

fn write_rollout(path: &Path, id: &str, history_mode: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let header = json!({
        "timestamp": "2025-01-01T00:00:00Z",
        "type": "session_meta",
        "payload": {
            "id": id,
            "timestamp": "2025-01-01T00:00:00Z",
            "history_mode": history_mode
        }
    });
    fs::write(
        path,
        format!("{header}\n{{\"type\":\"event_msg\",\"payload\":{{\"message\":\"synthetic\"}}}}\n"),
    )
    .unwrap();
}

fn fake_binary(dir: &Path, name: &str, version_output: &str) -> PathBuf {
    let bin = dir.join(name);
    fs::write(
        &bin,
        format!("#!/bin/sh\nprintf '%s\\n' '{version_output}'\n"),
    )
    .unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

fn legacy_schema_object_count(c: &Connection) -> i64 {
    c.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'codex_retain_%'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn desktop_only_lifecycle_works_without_codex_binary_or_codex_db_ddl() {
    let f = Fixture::new();
    let expired = f.add(8001, 1);
    let recent = f.add(8002, 1);
    let active = f.add(8003, 0);
    f.c.execute(
        "UPDATE threads SET archived_at=? WHERE id=?",
        params![now(), &recent],
    )
    .unwrap();
    tracker::remove_files(&f.store).unwrap();
    let state = f.store.root.clone();
    let home = f.home.clone();
    let root = f.temp.path().to_path_buf();
    let expired_path = f.path(&expired, true);
    let recent_path = f.path(&recent, true);
    let active_path = f.path(&active, false);
    drop(f.store);

    // Run with an empty PATH and no --codex-bin flag (Desktop-only / IDE-only environment).
    let doctor = json_ok(
        command(&root, &state, true)
            .env("PATH", "")
            .args(["doctor", "--codex-home"])
            .arg(&home),
    );
    assert_eq!(doctor["compatible"], true);
    assert_eq!(doctor["compatibility_scope"], "storage_contract");
    assert_eq!(doctor["database"], "state_5.sqlite");

    let enabled = json_ok(
        command(&root, &state, true)
            .env("PATH", "")
            .args([
                "enable",
                "--days",
                "30",
                "--no-schedule",
                "--yes",
                "--codex-home",
            ])
            .arg(&home),
    );
    assert_eq!(enabled["enabled"], true);
    assert_eq!(enabled["initial_cleanup"]["deleted"], 1);
    assert_eq!(enabled["initial_cleanup"]["skipped"], 1);
    assert!(!expired_path.exists());
    assert!(recent_path.exists() && active_path.exists());

    // Policy has no mandatory codex_bin, and Codex's SQLite DB has zero private objects.
    let policy_json: Value =
        serde_json::from_slice(&fs::read(state.join("policy.json")).unwrap()).unwrap();
    assert!(policy_json.get("codex_bin").is_none());
    assert_eq!(legacy_schema_object_count(&f.c), 0);
    assert!(state.join("tracker.sqlite").exists());

    let status = json_ok(command(&root, &state, true).env("PATH", "").arg("status"));
    assert!(status["compatibility_error"].is_null());
    assert_eq!(status["database"], "state_5.sqlite");

    let preview = json_ok(command(&root, &state, true).env("PATH", "").arg("preview"));
    assert_eq!(preview["examined"], 1);
    assert_eq!(preview["eligible"], 0);

    let run = json_ok(command(&root, &state, true).env("PATH", "").arg("run"));
    assert_eq!(run["deleted"], 0);
    assert_eq!(legacy_schema_object_count(&f.c), 0);

    json_ok(command(&root, &state, true).env("PATH", "").arg("disable"));
    assert!(!state.join("tracker.sqlite").exists());
    assert_eq!(legacy_schema_object_count(&f.c), 0);
}

#[test]
fn arbitrary_past_and_future_codex_versions_are_accepted() {
    for (idx, version) in [
        "codex-cli 0.120.0",
        "codex-cli 0.250.1-alpha.2",
        "Codex Desktop 2.0.0 (build 999)",
    ]
    .into_iter()
    .enumerate()
    {
        let f = Fixture::new();
        tracker::remove_files(&f.store).unwrap();
        let bin = fake_binary(f.temp.path(), &format!("codex-{idx}"), version);
        let state = f.store.root.clone();
        let home = f.home.clone();
        let root = f.temp.path().to_path_buf();
        drop(f.store);

        let doctor = json_ok(
            command(&root, &state, true)
                .args(["doctor", "--codex-home"])
                .arg(&home)
                .arg("--codex-bin")
                .arg(&bin),
        );
        assert_eq!(doctor["compatible"], true);
        assert_eq!(doctor["codex_version"], version);

        let enabled = json_ok(
            command(&root, &state, true)
                .args(["enable", "--no-schedule", "--yes", "--codex-home"])
                .arg(&home)
                .arg("--codex-bin")
                .arg(&bin),
        );
        assert_eq!(enabled["enabled"], true);
    }
}

#[test]
fn older_minimal_state_4_schema_works_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().canonicalize().unwrap();
    let home = root.join("codex");
    let state = root.join("state");
    fs::create_dir_all(home.join("archived_sessions")).unwrap();
    fs::create_dir_all(home.join("sessions")).unwrap();

    // Minimal pre-0.153 schema: no history_mode, is_pinned, thread_section_id,
    // thread_sections, thread_spawn_edges, name, backfill_state, or _sqlx_migrations.
    let db_path = home.join("state_4.sqlite");
    let c = Connection::open(&db_path).unwrap();
    c.execute_batch(
        "CREATE TABLE threads (
            id TEXT PRIMARY KEY,
            rollout_path TEXT NOT NULL,
            archived INTEGER NOT NULL DEFAULT 0,
            archived_at INTEGER,
            title TEXT NOT NULL DEFAULT ''
        );",
    )
    .unwrap();

    let expired = uuid::Uuid::from_u128(8101).to_string();
    let recent = uuid::Uuid::from_u128(8102).to_string();
    let active = uuid::Uuid::from_u128(8103).to_string();
    let expired_path = home
        .join("archived_sessions")
        .join(format!("rollout-2025-01-01T00-00-00-{expired}.jsonl"));
    let recent_path = home
        .join("archived_sessions")
        .join(format!("rollout-2025-01-01T00-00-00-{recent}.jsonl"));
    let active_path = home
        .join("sessions")
        .join(format!("rollout-2025-01-01T00-00-00-{active}.jsonl"));
    write_rollout(&expired_path, &expired, "legacy");
    write_rollout(&recent_path, &recent, "legacy");
    write_rollout(&active_path, &active, "legacy");

    c.execute(
        "INSERT INTO threads VALUES(?,?,1,?,'old')",
        params![&expired, expired_path.to_str().unwrap(), now() - 40 * 86400],
    )
    .unwrap();
    c.execute(
        "INSERT INTO threads VALUES(?,?,1,?,'recent')",
        params![&recent, recent_path.to_str().unwrap(), now()],
    )
    .unwrap();
    c.execute(
        "INSERT INTO threads VALUES(?,?,0,NULL,'active')",
        params![&active, active_path.to_str().unwrap()],
    )
    .unwrap();
    drop(c);

    let doctor = json_ok(
        command(&root, &state, true)
            .args(["doctor", "--codex-home"])
            .arg(&home),
    );
    assert_eq!(doctor["compatible"], true);
    assert_eq!(doctor["database"], "state_4.sqlite");
    assert_eq!(doctor["schema_capabilities"]["has_history_mode"], false);
    assert_eq!(doctor["schema_capabilities"]["has_is_pinned"], false);
    assert_eq!(doctor["schema_capabilities"]["has_spawn_edges"], false);
    assert_eq!(doctor["history_coverage"]["formats"]["legacy"], 2);

    let enabled = json_ok(
        command(&root, &state, true)
            .args([
                "enable",
                "--days",
                "30",
                "--no-schedule",
                "--yes",
                "--codex-home",
            ])
            .arg(&home),
    );
    assert_eq!(enabled["initial_cleanup"]["deleted"], 1);
    assert_eq!(enabled["initial_cleanup"]["skipped"], 1);
    assert!(!expired_path.exists());
    assert!(recent_path.exists() && active_path.exists());
}

#[test]
fn highest_state_db_wins_and_upgrade_from_state_5_to_state_6_preserves_tracking() {
    let f = Fixture::new();
    // Create an older leftover state_4.sqlite that must be ignored in favor of state_5.sqlite.
    let old_db = Connection::open(f.home.join("state_4.sqlite")).unwrap();
    old_db
        .execute_batch("CREATE TABLE unrelated(x INTEGER);")
        .unwrap();
    drop(old_db);

    let stable = f.add(8201, 1);
    let rearchived = f.add(8202, 1);
    f.age(&stable);
    f.age(&rearchived);
    let stable_path = f.path(&stable, true);
    let rearchived_path = f.path(&rearchived, true);
    f.store.save(&f.policy).unwrap();

    // Simulate Codex upgrading from state_5.sqlite -> state_6.sqlite with additive
    // schema changes (new table + new column + new successful migration).
    let state6_path = f.home.join("state_6.sqlite");
    fs::copy(f.home.join("state_5.sqlite"), &state6_path).unwrap();
    let c6 = Connection::open(&state6_path).unwrap();
    c6.execute_batch(
        "ALTER TABLE threads ADD COLUMN workspace_group TEXT;
         CREATE TABLE thread_tags (thread_id TEXT NOT NULL, tag TEXT NOT NULL);
         INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time)
         VALUES(999,'future additive migration',1,x'deadbeef',0);",
    )
    .unwrap();
    // Simulate rearchived thread having a fresh archived_at in state_6.sqlite.
    c6.execute(
        "UPDATE threads SET archived_at=? WHERE id=?",
        params![now(), &rearchived],
    )
    .unwrap();
    drop(c6);

    let state = f.store.root.clone();
    let root = f.temp.path().to_path_buf();
    drop(f.store);

    let status = json_ok(command(&root, &state, true).arg("status"));
    assert!(status["compatibility_error"].is_null());
    assert_eq!(status["database"], "state_6.sqlite");

    let preview = json_ok(command(&root, &state, true).arg("preview"));
    assert_eq!(preview["examined"], 2);
    assert_eq!(preview["eligible"], 1);

    let run = json_ok(command(&root, &state, true).arg("run"));
    assert_eq!(run["deleted"], 1);
    assert!(!stable_path.exists());
    assert!(rearchived_path.exists());

    // Policy seamlessly recorded the upgraded database name.
    let policy_json: Value =
        serde_json::from_slice(&fs::read(state.join("policy.json")).unwrap()).unwrap();
    assert_eq!(policy_json["database_name"], "state_6.sqlite");

    // state_6.sqlite has only rearchived; leftover state_5.sqlite was untouched.
    let c6 = Connection::open(&state6_path).unwrap();
    assert_eq!(
        c6.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        f.c.query_row("SELECT count(*) FROM threads", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        2
    );
}

#[test]
fn future_schema_custom_pinned_section_protective_columns_and_restrict_fk_are_safe() {
    let mut f = Fixture::new();
    // Add future protective columns, custom Pinned section UUID, and child FK ON DELETE RESTRICT.
    f.c.execute_batch(
        "ALTER TABLE threads ADD COLUMN pinned_at INTEGER;
         ALTER TABLE threads ADD COLUMN is_starred INTEGER NOT NULL DEFAULT 0;
         CREATE TABLE thread_annotations (
             id TEXT PRIMARY KEY,
             thread_id TEXT NOT NULL REFERENCES threads(id) ON DELETE RESTRICT
         );",
    )
    .unwrap();
    let custom_pinned_section = uuid::Uuid::from_u128(999_001).to_string();
    f.c.execute(
        "INSERT INTO thread_sections(id, name) VALUES(?, 'Pinned')",
        [&custom_pinned_section],
    )
    .unwrap();

    let t_section = f.add(8301, 1);
    let t_pinned_at = f.add(8302, 1);
    let t_starred = f.add(8303, 1);
    let t_fk = f.add(8304, 1);
    let t_future_mode = f.add(8305, 1);
    let t_normal = f.add(8306, 1);
    for id in [
        &t_section,
        &t_pinned_at,
        &t_starred,
        &t_fk,
        &t_future_mode,
        &t_normal,
    ] {
        f.age(id);
    }

    f.c.execute(
        "UPDATE threads SET thread_section_id=? WHERE id=?",
        params![&custom_pinned_section, &t_section],
    )
    .unwrap();
    f.c.execute(
        "UPDATE threads SET pinned_at=1700000000 WHERE id=?",
        [&t_pinned_at],
    )
    .unwrap();
    f.c.execute("UPDATE threads SET is_starred=1 WHERE id=?", [&t_starred])
        .unwrap();
    f.c.execute(
        "INSERT INTO thread_annotations(id, thread_id) VALUES('ann-1', ?)",
        [&t_fk],
    )
    .unwrap();
    f.c.execute(
        "UPDATE threads SET history_mode='v3-stream' WHERE id=?",
        [&t_future_mode],
    )
    .unwrap();

    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!f.exists(&t_normal) && !f.path(&t_normal, true).exists());

    for (id, expected_reason) in [
        (&t_section, "pinned_or_unknown_pin"),
        (&t_pinned_at, "pinned_or_unknown_pin"),
        (&t_starred, "pinned_or_unknown_pin"),
        (&t_future_mode, "unsupported_history_mode"),
        (&t_fk, "dependent_thread"),
    ] {
        assert!(f.exists(id), "protected thread {id} was deleted");
        assert!(f.path(id, true).exists(), "file for {id} was removed");
        let entry = report.entries.iter().find(|e| &e.id == id).unwrap();
        assert_eq!(entry.reason.as_str(), expected_reason, "thread {id}");
    }
    assert!(!f.store.root.join("pending.json").exists());
}

#[test]
fn breaking_future_schemas_fail_closed() {
    // Case A: missing required archived_at column.
    {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("codex");
        fs::create_dir(&home).unwrap();
        let c = Connection::open(home.join("state_7.sqlite")).unwrap();
        c.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT NOT NULL, archived INTEGER NOT NULL);",
        )
        .unwrap();
        drop(c);
        assert!(
            database::open_with_capabilities(&home, database::DatabaseAccess::ReadOnly).is_err()
        );
    }

    // Case B: self-referential ON DELETE CASCADE on threads.
    {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("codex");
        fs::create_dir(&home).unwrap();
        let c = Connection::open(home.join("state_7.sqlite")).unwrap();
        c.execute_batch(
            "CREATE TABLE threads (
                id TEXT PRIMARY KEY,
                rollout_path TEXT NOT NULL,
                archived INTEGER NOT NULL,
                archived_at INTEGER,
                parent_id TEXT REFERENCES threads(id) ON DELETE CASCADE
            );",
        )
        .unwrap();
        drop(c);
        let err = database::open_with_capabilities(&home, database::DatabaseAccess::ReadOnly)
            .unwrap_err();
        assert!(format!("{err:#}").contains("CASCADE"));
    }

    // Case C: failed _sqlx_migrations row (success = 0).
    {
        let f = Fixture::new();
        f.c.execute(
            "INSERT INTO _sqlx_migrations(version,description,success,checksum,execution_time)
             VALUES(999,'failed migration',0,x'00',0)",
            [],
        )
        .unwrap();
        assert!(
            database::open_with_capabilities(&f.home, database::DatabaseAccess::ReadOnly).is_err()
        );
    }
}

#[test]
fn desktop_global_state_pins_protect_threads_and_corrupt_state_fails_closed() {
    let mut f = Fixture::new();
    let pin_array = f.add(8401, 1);
    let pin_camel = f.add(8402, 1);
    let pin_map = f.add(8403, 1);
    let pin_obj = f.add(8404, 1);
    let pin_live = f.add(8405, 1);
    let unpinned = f.add(8406, 1);
    for id in [
        &pin_array, &pin_camel, &pin_map, &pin_obj, &pin_live, &unpinned,
    ] {
        f.age(id);
    }

    let global_state = f.home.join(desktop::GLOBAL_STATE_FILE);
    fs::write(
        &global_state,
        serde_json::to_vec(&json!({
            "pinned-thread-ids": [&pin_array],
            "pinnedThreadIds": [&pin_camel],
            "pinnedMap": {
                &pin_map: true,
                &unpinned: false
            },
            "threads": [
                {"id": &pin_obj, "pinned": true},
                {"id": &unpinned, "pinned": false}
            ]
        }))
        .unwrap(),
    )
    .unwrap();

    // Preview sees 2 eligible (pin_live + unpinned) and 4 Desktop-pinned.
    let preview = f.run(false).unwrap();
    assert_eq!(preview.eligible, 2);

    // User pins pin_live in Codex Desktop after preview, before run!
    fs::write(
        &global_state,
        serde_json::to_vec(&json!({
            "pinned-thread-ids": [&pin_array, &pin_live],
            "pinnedThreadIds": [&pin_camel],
            "pinnedMap": { &pin_map: true },
            "threads": [{ "id": &pin_obj, "pinned": true }]
        }))
        .unwrap(),
    )
    .unwrap();

    let report = f.run(true).unwrap();
    assert_eq!(report.deleted, 1);
    assert!(!f.exists(&unpinned) && !f.path(&unpinned, true).exists());
    for id in [&pin_array, &pin_camel, &pin_map, &pin_obj, &pin_live] {
        assert!(f.exists(id), "Desktop-pinned thread {id} was deleted");
        assert!(f.path(id, true).exists());
        let entry = report.entries.iter().find(|e| &e.id == id).unwrap();
        assert_eq!(entry.reason.as_str(), "pinned_or_unknown_pin");
    }

    // Corrupt Desktop global state fails closed without deleting anything.
    let survivor = f.add(8407, 1);
    f.age(&survivor);
    fs::write(&global_state, b"{not valid json").unwrap();
    assert!(f.run(false).is_err());
    assert!(f.run(true).is_err());
    assert!(f.exists(&survivor) && f.path(&survivor, true).exists());

    // Symlinked Desktop global state fails closed as well.
    let outside = f.temp.path().join("outside-state.json");
    fs::write(&outside, b"{}").unwrap();
    fs::remove_file(&global_state).unwrap();
    symlink(&outside, &global_state).unwrap();
    assert!(f.run(true).is_err());
    assert!(f.exists(&survivor) && f.path(&survivor, true).exists());
}

#[test]
fn paused_scheduled_run_tracks_restore_and_rearchive_without_deleting() {
    let mut f = Fixture::new();
    let id = f.add(8501, 1);
    f.age(&id);
    f.policy.paused = true;
    f.store.save(&f.policy).unwrap();
    let state = f.store.root.clone();
    let root = f.temp.path().to_path_buf();
    let path = f.path(&id, true);
    drop(f.store);

    // Restore the thread while policy is paused.
    f.c.execute(
        "UPDATE threads SET archived=0, archived_at=NULL WHERE id=?",
        [&id],
    )
    .unwrap();

    // Scheduled run while paused stays quiet and syncs sidecar tracker (read-only on Codex DB).
    command(&root, &state, true)
        .args(["run", "--scheduled"])
        .assert()
        .success()
        .stdout("");
    let read_epoch = |state: &Path, thread_id: &str| -> Option<i64> {
        use rusqlite::OptionalExtension;
        Connection::open(state.join("tracker.sqlite"))
            .unwrap()
            .query_row(
                "SELECT archived_since FROM tracked_epochs WHERE thread_id=?",
                [thread_id],
                |r| r.get(0),
            )
            .optional()
            .unwrap()
    };
    assert_eq!(read_epoch(&state, &id), None);

    // Re-archive even with an old timestamp (e.g. timestamp repair / older backup);
    // sidecar tracker must grant conservative fresh grace!
    let before = now();
    f.c.execute(
        "UPDATE threads SET archived=1, archived_at=1 WHERE id=?",
        [&id],
    )
    .unwrap();
    command(&root, &state, true)
        .args(["run", "--scheduled"])
        .assert()
        .success()
        .stdout("");
    assert!(read_epoch(&state, &id).unwrap() >= before);

    // Resuming and running must NOT delete the freshly re-archived thread.
    json_ok(command(&root, &state, true).arg("resume"));
    let run = json_ok(command(&root, &state, true).arg("run"));
    assert_eq!(run["deleted"], 0);
    let exists: bool =
        f.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM threads WHERE id=?)",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(exists && path.exists());
}

#[test]
fn seamless_upgrade_from_legacy_0_1_x_policy_migrates_epochs_and_cleans_triggers() {
    let f = Fixture::new();
    let aged = f.add(8601, 1);
    let recent = f.add(8602, 1);
    let aged_path = f.path(&aged, true);
    let recent_path = f.path(&recent, true);
    let recent_ts = now() - 60;
    f.c.execute(
        "UPDATE threads SET archived_at=? WHERE id=?",
        params![recent_ts, &recent],
    )
    .unwrap();

    // Remove tracker.sqlite and install legacy 0.1.x codex_retain_* tables + triggers in state_5.sqlite.
    tracker::remove_files(&f.store).unwrap();
    f.c.execute_batch(
        "CREATE TABLE codex_retain_owner (singleton INTEGER PRIMARY KEY CHECK(singleton=1), owner TEXT NOT NULL);
         CREATE TABLE codex_retain_epochs (thread_id TEXT PRIMARY KEY, archived_since INTEGER NOT NULL, codex_archived_at INTEGER);
         CREATE TRIGGER codex_retain_insert AFTER INSERT ON threads WHEN new.archived=1 BEGIN SELECT 1; END;
         CREATE TRIGGER codex_retain_update AFTER UPDATE ON threads WHEN new.archived=1 BEGIN SELECT 1; END;
         CREATE TRIGGER codex_retain_delete AFTER DELETE ON threads BEGIN DELETE FROM codex_retain_epochs WHERE thread_id=old.id; END;",
    )
    .unwrap();
    f.c.execute(
        "INSERT INTO codex_retain_owner(singleton, owner) VALUES(1, ?)",
        [&f.policy.owner],
    )
    .unwrap();
    f.c.execute(
        "INSERT INTO codex_retain_epochs(thread_id, archived_since, codex_archived_at) VALUES(?, ?, 1)",
        params![&aged, now() - 40 * 86400],
    )
    .unwrap();
    f.c.execute(
        "INSERT INTO codex_retain_epochs(thread_id, archived_since, codex_archived_at) VALUES(?, ?, ?)",
        params![&recent, recent_ts, recent_ts],
    )
    .unwrap();

    // Write a legacy 0.1.x policy.json (contains obsolete codex_bin path, lacks database_name).
    let state = f.store.root.clone();
    let root = f.temp.path().to_path_buf();
    fs::write(
        state.join("policy.json"),
        serde_json::to_vec(&json!({
            "schema": 1,
            "codex_home": &f.home,
            "codex_bin": "/nonexistent/legacy/codex-0.153.4",
            "database": f.policy.database,
            "retention_days": 30,
            "enabled": true,
            "paused": false,
            "automatic": false,
            "enabled_at": f.policy.enabled_at,
            "owner": &f.policy.owner,
            "exclusions": []
        }))
        .unwrap(),
    )
    .unwrap();
    drop(f.store);

    // Read-only status and preview succeed without creating tracker.sqlite or touching state_5.sqlite.
    let status = json_ok(command(&root, &state, true).arg("status"));
    assert!(status["compatibility_error"].is_null());
    assert!(!state.join("tracker.sqlite").exists());
    assert_eq!(legacy_schema_object_count(&f.c), 5);

    let preview = json_ok(command(&root, &state, true).arg("preview"));
    assert_eq!(preview["examined"], 2);
    assert_eq!(preview["eligible"], 1);
    assert!(!state.join("tracker.sqlite").exists());
    assert_eq!(legacy_schema_object_count(&f.c), 5);

    // First run migrates legacy epochs into tracker.sqlite, drops legacy triggers/tables
    // from state_5.sqlite, and cleans only the expired thread.
    let run = json_ok(command(&root, &state, true).arg("run"));
    assert_eq!(run["deleted"], 1);
    let row_exists = |id: &str| -> bool {
        f.c.query_row(
            "SELECT EXISTS(SELECT 1 FROM threads WHERE id=?)",
            [id],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(!aged_path.exists() && !row_exists(&aged));
    assert!(recent_path.exists() && row_exists(&recent));
    assert!(state.join("tracker.sqlite").exists());
    assert_eq!(legacy_schema_object_count(&f.c), 0);

    let tc = Connection::open(state.join("tracker.sqlite")).unwrap();
    let migrated_since: i64 = tc
        .query_row(
            "SELECT archived_since FROM tracked_epochs WHERE thread_id=?",
            [&recent],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(migrated_since, recent_ts);
}
