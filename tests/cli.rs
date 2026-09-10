#[cfg(target_os = "macos")]
#[allow(dead_code)] // The shared fixture also serves the larger lifecycle test target.
mod support;

use assert_cmd::Command;
#[cfg(target_os = "macos")]
use codex_retain::database;
use serde_json::Value;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
#[cfg(target_os = "macos")]
use support::Fixture;

fn command(state: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codex-retain"));
    cmd.arg("--state-dir")
        .arg(state)
        .arg("--json")
        .env_remove("CODEX_RETAIN_STATE_DIR");
    cmd.timeout(std::time::Duration::from_secs(10));
    cmd
}
fn json(cmd: &mut Command) -> Value {
    let output = cmd.assert().success().get_output().stdout.clone();
    serde_json::from_slice(&output).unwrap()
}
#[cfg(target_os = "macos")]
fn binary(root: &Path) -> PathBuf {
    let bin = root.join("codex-version");
    fs::write(&bin, "#!/bin/sh\nprintf '%s\\n' 'codex-cli 0.153.4'\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    bin
}

#[test]
fn help_version_completions_do_not_load_policy_or_create_state() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("missing");
    for args in [
        vec!["--help"],
        vec!["--version"],
        vec!["completions", "bash"],
    ] {
        command(&state)
            .env("HOME", temp.path().join("absent-home"))
            .args(args)
            .assert()
            .success();
        assert!(!state.exists());
    }
}

#[test]
fn unknown_arguments_and_invalid_retention_are_parser_errors() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("missing");
    for args in [
        vec!["enable", "--days", "0"],
        vec!["enable", "--days", "36501"],
        vec!["enable", "--days", "-1"],
        vec!["run", "--yes"],
        vec!["no-such-command"],
    ] {
        command(&state).args(args).assert().code(2);
        assert!(!state.exists());
    }
}

#[test]
fn status_without_policy_does_not_create_one() {
    let temp = tempfile::tempdir().unwrap();
    let state = temp.path().join("missing");
    let result = json(command(&state).arg("status").env("HOME", temp.path()));
    assert_eq!(result["configured"], false);
    assert!(!state.exists());
}

#[test]
#[cfg(target_os = "macos")]
fn consent_is_required_before_profile_mutation() {
    let f = Fixture::new();
    let id = f.add(400, 1);
    let state = f.store.root.clone();
    let home = f.home.clone();
    let bin = binary(f.temp.path());
    database::uninstall_capture(&mut { f.c }, &f.policy).unwrap();
    drop(f.store);
    command(&state)
        .args(["enable", "--no-schedule", "--codex-home"])
        .arg(&home)
        .arg("--codex-bin")
        .arg(bin)
        .assert()
        .failure();
    let db = database::open(&home, database::DatabaseAccess::ReadOnly).unwrap();
    assert_eq!(
        db.query_row("SELECT count(*) FROM threads WHERE id=?", [id], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        db.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'codex_retain_%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
}

#[test]
#[cfg(target_os = "macos")]
fn enable_cleans_old_legacy_and_paginated_archives_and_preserves_protected_owners() {
    let mut f = Fixture::new();
    let legacy = f.add(410, 1);
    let paginated = f.add(411, 1);
    let recent = f.add(412, 1);
    let pinned = f.add(413, 1);
    let excluded = f.add(414, 1);
    let parent = f.add(415, 1);
    let active = f.add(416, 0);
    f.c.execute(
        "UPDATE threads SET archived_at=unixepoch() WHERE id=?",
        [&recent],
    )
    .unwrap();
    f.c.execute("UPDATE threads SET is_pinned=1 WHERE id=?", [&pinned])
        .unwrap();
    f.c.execute(
        "INSERT INTO thread_spawn_edges VALUES(?,?,'open')",
        [&parent, &active],
    )
    .unwrap();
    f.c.execute(
        "UPDATE threads SET history_mode='paginated' WHERE id=?",
        [&paginated],
    )
    .unwrap();
    let paginated_path = f.path(&paginated, true);
    let data = fs::read_to_string(&paginated_path)
        .unwrap()
        .replace("legacy", "paginated");
    fs::write(&paginated_path, data).unwrap();
    let legacy_path = f.path(&legacy, true);
    let preserved: Vec<_> = [&recent, &pinned, &excluded, &parent]
        .iter()
        .map(|id| {
            let path = f.path(id, true);
            let contents = fs::read(&path).unwrap();
            (path, contents)
        })
        .collect();
    let active_path = f.path(&active, false);
    let active_bytes = fs::read(&active_path).unwrap();
    // Explicit re-enable of a disabled older policy retains its exclusions.
    f.policy.enabled = false;
    f.policy.exclusions.insert(excluded.clone());
    f.store.save(&f.policy).unwrap();
    database::uninstall_capture(&mut f.c, &f.policy).unwrap();
    let bin = binary(f.temp.path());
    let state = f.store.root.clone();
    let home = f.home.clone();
    drop(f.store);
    let output = json(
        command(&state)
            .args([
                "enable",
                "--days",
                "30",
                "--yes",
                "--no-schedule",
                "--codex-home",
            ])
            .arg(&home)
            .arg("--codex-bin")
            .arg(bin),
    );
    assert_eq!(output["schema"], 2);
    assert_eq!(output["existing_archives_assessed"], 6);
    assert_eq!(output["initial_archive_clock"], "codex_archived_at");
    assert_eq!(output["initial_cleanup"]["deleted"], 2);
    assert_eq!(output["initial_cleanup"]["skipped"], 4);
    assert!(!legacy_path.exists() && !paginated_path.exists());
    assert_eq!(
        f.c.query_row(
            "SELECT count(*) FROM threads WHERE id IN (?,?)",
            [&legacy, &paginated],
            |row| row.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    for (path, bytes) in preserved {
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
    assert_eq!(fs::read(active_path).unwrap(), active_bytes);
    let receipt: Value =
        serde_json::from_slice(&fs::read(state.join("last-run.json")).unwrap()).unwrap();
    assert_eq!(receipt["deleted"], 2);
    assert_eq!(receipt["status"], "ok");
    command(&state)
        .args(["run", "--scheduled"])
        .assert()
        .success()
        .stdout("");
    let receipt: Value =
        serde_json::from_slice(&fs::read(state.join("last-run.json")).unwrap()).unwrap();
    assert_eq!(receipt["deleted"], 0);
    assert!(!state.join("pending.json").exists());
}

#[test]
#[cfg(target_os = "macos")]
fn initial_cleanup_reports_busy_and_invalid_artifacts_as_attention_then_retries() {
    for busy in [true, false] {
        let mut f = Fixture::new();
        let id = f.add(420, 1);
        let path = f.path(&id, true);
        let original = fs::read(&path).unwrap();
        let lock = busy.then(|| codex_retain::fsutil::ThreadLock::acquire(&f.home, &id).unwrap());
        if !busy {
            fs::write(&path, b"invalid metadata\n").unwrap();
        }
        database::uninstall_capture(&mut f.c, &f.policy).unwrap();
        let bin = binary(f.temp.path());
        let state = f.store.root.clone();
        let home = f.home.clone();
        drop(f.store);
        let assertion = command(&state)
            .args(["enable", "--yes", "--no-schedule", "--codex-home"])
            .arg(&home)
            .arg("--codex-bin")
            .arg(bin)
            .assert()
            .code(3);
        let output: Value = serde_json::from_slice(&assertion.get_output().stdout).unwrap();
        assert_eq!(output["enabled"], true);
        assert_eq!(output["initial_cleanup"]["deleted"], 0);
        let receipt: Value =
            serde_json::from_slice(&fs::read(state.join("last-run.json")).unwrap()).unwrap();
        assert_eq!(receipt["status"], "attention");
        assert_eq!(
            f.c.query_row("SELECT count(*) FROM threads WHERE id=?", [&id], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
            1
        );
        assert!(path.exists());
        drop(lock);
        fs::write(&path, original).unwrap();
        command(&state)
            .args(["run", "--scheduled"])
            .assert()
            .success()
            .stdout("");
        assert!(!path.exists());
        let receipt: Value =
            serde_json::from_slice(&fs::read(state.join("last-run.json")).unwrap()).unwrap();
        assert_eq!(receipt["deleted"], 1);
        assert_eq!(receipt["status"], "ok");
    }
}

#[test]
#[cfg(target_os = "macos")]
fn executable_lifecycle_is_local_predictable_and_confirmation_free_after_enable() {
    let mut f = Fixture::new();
    let id = f.add(401, 1);
    f.c.execute(
        "UPDATE threads SET archived_at=unixepoch() WHERE id=?",
        [&id],
    )
    .unwrap();
    let rowpath = f.path(&id, true);
    database::uninstall_capture(&mut f.c, &f.policy).unwrap();
    let bin = binary(f.temp.path());
    let state = f.store.root.clone();
    let home = f.home.clone();
    drop(f.c);
    drop(f.store);
    let enabled = json(
        command(&state)
            .args([
                "enable",
                "--days",
                "7",
                "--no-schedule",
                "--yes",
                "--codex-home",
            ])
            .arg(&home)
            .arg("--codex-bin")
            .arg(&bin),
    );
    assert_eq!(enabled["schema"], 2);
    assert_eq!(enabled["existing_archives_assessed"], 1);
    assert_eq!(enabled["initial_cleanup"]["deleted"], 0);
    assert!(enabled.get("existing_archives_given_grace").is_none());
    assert_eq!(enabled["automatic"], false);
    let preview = json(command(&state).arg("preview"));
    assert_eq!(preview["eligible"], 0);
    assert!(rowpath.exists());
    assert_eq!(json(command(&state).arg("pause"))["paused"], true);
    command(&state).arg("run").assert().failure();
    assert!(rowpath.exists());
    command(&state)
        .args(["run", "--scheduled"])
        .assert()
        .success()
        .stdout("");
    json(command(&state).arg("resume"));
    command(&state)
        .args(["policy", "--days", "1"])
        .assert()
        .failure();
    json(command(&state).args(["policy", "--days", "1", "--yes"]));
    json(command(&state).arg("exclude").arg(&id));
    let c = database::open(&home, database::DatabaseAccess::ReadWrite).unwrap();
    c.execute(
        "UPDATE codex_retain_epochs SET archived_since=unixepoch()-3*86400",
        [],
    )
    .unwrap();
    assert_eq!(json(command(&state).arg("run"))["deleted"], 0);
    assert!(rowpath.exists());
    command(&state).arg("include").arg(&id).assert().failure();
    json(command(&state).arg("include").arg(&id).arg("--yes"));
    let report = json(command(&state).arg("run"));
    assert_eq!(report["deleted"], 1);
    assert!(!rowpath.exists());
    assert!(report["actual_reclaimed_bytes"].is_null());
    assert!(report["logical_bytes_removed"].as_u64().unwrap() > 0);
    assert_eq!(json(command(&state).arg("run"))["deleted"], 0);
    let status = json(command(&state).arg("status").env("HOME", f.temp.path()));
    assert_eq!(status["policy"]["enabled"], true);
    assert_eq!(status["last_run"]["status"], "ok");
    assert_eq!(
        json(command(&state).arg("disable"))["capture_removed"],
        true
    );
    assert_eq!(
        c.query_row(
            "SELECT count(*) FROM sqlite_schema WHERE name LIKE 'codex_retain_%'",
            [],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        0
    );
    command(&state).arg("run").assert().failure();
    json(command(&state).arg("uninstall"));
    assert!(
        !home.join("archived_sessions").read_dir().unwrap().any(|e| e
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains("pending"))
    );
}

#[test]
#[cfg(target_os = "macos")]
fn scheduled_errors_replace_one_bounded_receipt_and_remain_quiet() {
    let mut f = Fixture::new();
    f.policy.codex_bin = binary(f.temp.path());
    f.store.save(&f.policy).unwrap();
    let state = f.store.root.clone();
    f.c.execute_batch("DROP TRIGGER codex_retain_update")
        .unwrap();
    drop(f.store);
    for _ in 0..2 {
        command(&state)
            .args(["run", "--scheduled"])
            .assert()
            .failure()
            .stdout("")
            .stderr("");
    }
    let value: Value =
        serde_json::from_slice(&fs::read(state.join("last-run.json")).unwrap()).unwrap();
    assert_eq!(value["status"], "error");
    assert!(value["error"].as_str().unwrap().contains("capture"));
    assert!(fs::metadata(state.join("last-run.json")).unwrap().len() < 4096);
}

#[test]
#[cfg(target_os = "macos")]
fn scheduled_artifact_failures_keep_attention_and_the_same_first_ten_diagnostics() {
    let mut f = Fixture::new();
    let mut ids = Vec::new();
    for number in 1..=16 {
        let id = f.add(number, 1);
        f.age(&id);
        fs::remove_file(f.path(&id, true)).unwrap();
        ids.push(id);
    }
    f.policy.codex_bin = binary(f.temp.path());
    f.store.save(&f.policy).unwrap();
    let state = f.store.root.clone();
    drop(f.store);
    command(&state)
        .args(["run", "--scheduled"])
        .assert()
        .code(3)
        .stdout("")
        .stderr("");
    let receipt: Value =
        serde_json::from_slice(&fs::read(state.join("last-run.json")).unwrap()).unwrap();
    assert_eq!(receipt["status"], "attention");
    assert_eq!(receipt["examined"], 16);
    assert_eq!(receipt["deleted"], 0);
    assert_eq!(receipt["skipped"], 16);
    assert_eq!(receipt["skips"].as_array().unwrap().len(), 10);
    assert_eq!(
        receipt["skips"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| entry["id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ids[..10].iter().map(String::as_str).collect::<Vec<_>>()
    );
    let output = command(&state)
        .arg("run")
        .assert()
        .code(3)
        .get_output()
        .stdout
        .clone();
    let full: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(full["entries"].as_array().unwrap().len(), 16);
    assert_eq!(
        receipt["skips"].as_array().unwrap(),
        &full["entries"].as_array().unwrap()[..10]
    );
    assert_eq!(
        f.c.query_row("SELECT count(*) FROM threads", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        16
    );
}

#[test]
#[cfg(target_os = "macos")]
fn large_json_preview_exits_successfully_when_the_reader_closes_early() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let mut f = Fixture::new();
    for number in 1..=1000 {
        f.add(number, 1);
    }
    f.policy.codex_bin = binary(f.temp.path());
    f.store.save(&f.policy).unwrap();
    let state = f.store.root.clone();
    drop(f.store);
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_codex-retain"))
        .args(["--state-dir", state.to_str().unwrap(), "--json", "preview"])
        .env("HOME", f.temp.path())
        .env_remove("CODEX_RETAIN_STATE_DIR")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(child.stdout.take());
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "closed stdout returned {status}");
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("preview did not finish after its reader closed");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        f.c.query_row("SELECT count(*) FROM threads", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        1000
    );
}

#[test]
#[cfg(target_os = "macos")]
fn incomplete_automatic_setup_cannot_become_a_manual_policy() {
    let mut f = Fixture::new();
    f.policy.enabled = false;
    f.policy.automatic = true;
    f.policy.codex_bin = binary(f.temp.path());
    f.store.save(&f.policy).unwrap();
    let state = f.store.root.clone();
    let before = fs::read(state.join("policy.json")).unwrap();
    let home = f.home.clone();
    let bin = f.policy.codex_bin.clone();
    drop(f.store);
    command(&state)
        .args(["enable", "--no-schedule", "--yes", "--codex-home"])
        .arg(home)
        .arg("--codex-bin")
        .arg(bin)
        .assert()
        .failure();
    assert_eq!(fs::read(state.join("policy.json")).unwrap(), before);
}

#[test]
#[cfg(target_os = "macos")]
fn pending_recovery_prevents_policy_replacement() {
    let mut f = Fixture::new();
    let id = f.add(420, 1);
    f.pending(&id, &f.path(&id, true));
    f.policy.enabled = false;
    f.policy.codex_bin = binary(f.temp.path());
    f.store.save(&f.policy).unwrap();
    let state = f.store.root.clone();
    let before = fs::read(state.join("policy.json")).unwrap();
    let pending = fs::read(state.join("pending.json")).unwrap();
    let home = f.home.clone();
    let bin = f.policy.codex_bin.clone();
    drop(f.store);
    command(&state)
        .args(["enable", "--no-schedule", "--yes", "--codex-home"])
        .arg(home)
        .arg("--codex-bin")
        .arg(bin)
        .assert()
        .failure();
    assert_eq!(fs::read(state.join("policy.json")).unwrap(), before);
    assert_eq!(fs::read(state.join("pending.json")).unwrap(), pending);
}

#[test]
#[cfg(target_os = "macos")]
fn version_probe_isolates_and_removes_codex_startup_side_effects() {
    let temp = tempfile::tempdir().unwrap();
    let bin = temp.path().join("codex-side-effect");
    let receipt = temp.path().join("observed-home");
    // This records the child's CODEX_HOME through its own explicit test receipt.
    // A real Codex creates analogous helpers while processing --version.
    let script = format!(
        "#!/bin/sh\nprintf '%s' \"$CODEX_HOME\" > '{}'\nprintf test > \"$CODEX_HOME/helper\"\nprintf '%s\\n' 'codex-cli 0.153.4'\n",
        receipt.display()
    );
    fs::write(&bin, script).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    database::verify_binary(&bin).unwrap();
    let isolated = PathBuf::from(fs::read_to_string(receipt).unwrap());
    assert!(
        !isolated.exists(),
        "isolated Codex startup files must be removed"
    );
    assert_ne!(isolated, temp.path());
}
