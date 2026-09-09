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
    let db = database::open(&home, false).unwrap();
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
fn executable_lifecycle_is_local_predictable_and_confirmation_free_after_enable() {
    let mut f = Fixture::new();
    let id = f.add(401, 1);
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
    assert_eq!(enabled["existing_archives_given_grace"], 1);
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
    let c = database::open(&home, true).unwrap();
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
