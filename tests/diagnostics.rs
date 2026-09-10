#![cfg(unix)]

#[allow(dead_code)]
mod support;

use assert_cmd::Command;
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use support::Fixture;

fn command(root: &Path, state: &Path, json: bool) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codex-retain"));
    cmd.args(["--state-dir"])
        .arg(state)
        .env("HOME", root)
        .env_remove("CODEX_RETAIN_STATE_DIR")
        .timeout(std::time::Duration::from_secs(10));
    if json {
        cmd.arg("--json");
    }
    cmd
}

fn configure(f: &mut Fixture) {
    let bin = f.temp.path().join("codex-version");
    fs::write(&bin, "#!/bin/sh\nprintf '%s\\n' 'codex-cli 0.153.4'\n").unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
    f.policy.codex_bin = bin;
    f.store.save(&f.policy).unwrap();
}

fn doctor(f: &Fixture, json: bool) -> Command {
    let mut cmd = command(f.temp.path(), &f.store.root, json);
    cmd.args(["doctor", "--codex-home"])
        .arg(&f.home)
        .arg("--codex-bin")
        .arg(&f.policy.codex_bin);
    cmd
}

fn json(cmd: &mut Command) -> Value {
    let output = cmd
        .assert()
        .success()
        .stderr("")
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&output).unwrap()
}

#[test]
fn doctor_warns_when_every_archive_has_an_unsupported_format() {
    let mut f = Fixture::new();
    let id = f.add(501, 1);
    f.c.execute("UPDATE threads SET history_mode='future-format'", [])
        .unwrap();
    configure(&mut f);
    let result = json(&mut doctor(&f, true));
    assert_eq!(result["compatible"], true);
    assert_eq!(
        result["compatibility_scope"],
        "schema_and_selected_codex_binary"
    );
    assert_eq!(result["archived_threads"], 1);
    assert_eq!(result["history_coverage"]["supported_threads"], 0);
    assert_eq!(result["history_coverage"]["unsupported_threads"], 1);
    assert_eq!(result["history_coverage"]["formats"]["other"], 1);
    let output = doctor(&f, false)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Warning: no archived threads have a supported history format"));
    assert!(text.contains("Use preview"));
    assert!(f.exists(&id));
    assert!(f.path(&id, true).exists());
}

#[test]
fn doctor_aggregates_only_archived_rows_and_does_not_inspect_artifacts() {
    let mut f = Fixture::new();
    let legacy = f.add(502, 1);
    let paginated = f.add(503, 1);
    f.add(504, 1);
    f.add(505, 0);
    f.c.execute(
        "UPDATE threads SET history_mode='paginated' WHERE id=?",
        [&paginated],
    )
    .unwrap();
    f.c.execute(
        "UPDATE threads SET history_mode='future-format' WHERE id NOT IN (?,?)",
        [&legacy, &paginated],
    )
    .unwrap();
    // Coverage is a format inventory, so even missing content is not certified.
    fs::remove_file(f.path(&paginated, true)).unwrap();
    configure(&mut f);
    let result = json(&mut doctor(&f, true));
    assert_eq!(result["archived_threads"], 3);
    assert_eq!(result["history_coverage"]["supported_threads"], 2);
    assert_eq!(result["history_coverage"]["unsupported_threads"], 1);
    assert_eq!(
        result["history_coverage"]["formats"],
        serde_json::json!({"legacy":1,"paginated":1,"other":1})
    );
    let output = doctor(&f, false)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Warning: mixed history-format coverage"));
    assert!(text.contains("Format counts do not establish deletion eligibility"));
}

#[test]
fn doctor_distinguishes_empty_archive_from_unsupported_archive() {
    let mut f = Fixture::new();
    configure(&mut f);
    let result = json(&mut doctor(&f, true));
    assert_eq!(result["archived_threads"], 0);
    assert_eq!(result["history_coverage"]["supported_threads"], 0);
    assert_eq!(result["history_coverage"]["unsupported_threads"], 0);
    let output = doctor(&f, false)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("No archived threads to assess"));
    assert!(!text.contains("Warning:"));
}

#[test]
fn status_reports_supported_formats_without_claiming_retention_eligibility() {
    let mut f = Fixture::new();
    let id = f.add(506, 1);
    configure(&mut f);
    let state = f.store.root.clone();
    let path = f.path(&id, true);
    drop(f.store);
    let result = json(command(f.temp.path(), &state, true).arg("status"));
    assert!(result["compatibility_error"].is_null());
    assert_eq!(result["history_coverage"]["supported_threads"], 1);
    assert_eq!(result["history_coverage"]["unsupported_threads"], 0);
    let preview = json(command(f.temp.path(), &state, true).arg("preview"));
    assert_eq!(preview["eligible"], 0);
    let output = command(f.temp.path(), &state, false)
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Schema, selected Codex executable and enabled policy: verified"));
    assert!(text.contains("Format counts do not establish deletion eligibility"));
    assert!(path.exists());
}

#[test]
fn status_warns_about_mixed_formats_even_when_compatibility_is_verified() {
    let mut f = Fixture::new();
    f.add(508, 1);
    let unsupported = f.add(509, 1);
    f.c.execute(
        "UPDATE threads SET history_mode='future-format' WHERE id=?",
        [&unsupported],
    )
    .unwrap();
    configure(&mut f);
    let state = f.store.root.clone();
    drop(f.store);
    let result = json(command(f.temp.path(), &state, true).arg("status"));
    assert!(result["compatibility_error"].is_null());
    assert_eq!(result["history_coverage"]["supported_threads"], 1);
    assert_eq!(result["history_coverage"]["unsupported_threads"], 1);
    let output = command(f.temp.path(), &state, false)
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Warning: mixed history-format coverage"));
}

#[test]
fn status_keeps_compatibility_errors_and_marks_coverage_unavailable() {
    let mut f = Fixture::new();
    f.add(507, 1);
    configure(&mut f);
    f.c.execute_batch("DROP TRIGGER codex_retain_update")
        .unwrap();
    let state = f.store.root.clone();
    drop(f.store);
    let result = json(command(f.temp.path(), &state, true).arg("status"));
    assert!(
        result["compatibility_error"]
            .as_str()
            .unwrap()
            .contains("capture")
    );
    assert!(result["history_coverage"].is_null());
    let output = command(f.temp.path(), &state, false)
        .arg("status")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("unavailable because compatibility validation failed"));
    assert!(!text.contains(": verified"));
}
