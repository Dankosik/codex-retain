#![cfg(unix)]

#[allow(dead_code)]
mod support;

use assert_cmd::Command;
use serde_json::Value;
use std::{fs, os::unix::fs::PermissionsExt, path::Path};
use support::{Fixture, now};

fn command(root: &Path, state: &Path, json: bool) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_codex-retain"));
    cmd.arg("--state-dir")
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
fn forecast_changes_retention_evaluation_without_changing_profile_or_receipts() {
    let mut f = Fixture::new();
    let id = f.add(601, 1);
    configure(&mut f);
    let state = f.store.root.clone();
    let policy_bytes = fs::read(state.join("policy.json")).unwrap();
    let rollout_path = f.path(&id, true);
    let rollout_bytes = fs::read(&rollout_path).unwrap();
    let epoch = f.epoch(&id).unwrap();
    let archived_before: (i64, i64, String) =
        f.c.query_row(
            "SELECT archived,archived_at,rollout_path FROM threads WHERE id=?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let evaluated_at = now() + 31 * 86400;
    let timestamp = jiff::Timestamp::from_second(evaluated_at)
        .unwrap()
        .to_string();
    drop(f.store);
    let ordinary = json(command(f.temp.path(), &state, true).arg("preview"));
    assert_eq!(ordinary["eligible"], 0);
    assert!(ordinary.get("evaluated_at").is_none());
    let started_before = now();
    let forecast = json(command(f.temp.path(), &state, true).args(["preview", "--at", &timestamp]));
    let started_after = now();
    assert_eq!(forecast["eligible"], 1);
    assert_eq!(forecast["deleted"], 0);
    assert_eq!(forecast["evaluated_at"], evaluated_at);
    let started_at = forecast["started_at"].as_i64().unwrap();
    assert!((started_before..=started_after).contains(&started_at));
    let output = command(f.temp.path(), &state, false)
        .args(["preview", "--at", &timestamp])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("Forecast for"));
    assert!(text.contains("current profile snapshot"));
    assert!(text.contains("No deletion"));
    assert!(text.contains("not a cleanup guarantee"));
    assert_eq!(fs::read(state.join("policy.json")).unwrap(), policy_bytes);
    assert_eq!(fs::read(&rollout_path).unwrap(), rollout_bytes);
    assert!(!state.join("last-run.json").exists());
    assert!(!state.join("pending.json").exists());
    let archived_after: (i64, i64, String) =
        f.c.query_row(
            "SELECT archived,archived_at,rollout_path FROM threads WHERE id=?",
            [&id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(archived_after, archived_before);
    assert_eq!(
        f.c.query_row(
            "SELECT archived_since FROM codex_retain_epochs WHERE thread_id=?",
            [&id],
            |r| r.get::<_, i64>(0)
        )
        .unwrap(),
        epoch
    );
    assert_eq!(
        json(command(f.temp.path(), &state, true).arg("preview"))["eligible"],
        0
    );
}

#[test]
fn forecast_timestamp_is_preview_only_and_parser_rejects_invalid_values() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("absent-state");
    for arguments in [
        ["run", "--at", "2030-01-01T00:00:00Z"],
        ["preview", "--at", "not-a-timestamp"],
        ["preview", "--at", "2030-01-01"],
    ] {
        command(root.path(), &state, true)
            .args(arguments)
            .assert()
            .code(2);
        assert!(!state.exists());
    }
}

#[test]
fn historical_timestamp_is_rejected_without_advancing_retention() {
    let mut f = Fixture::new();
    let id = f.add(602, 1);
    configure(&mut f);
    let state = f.store.root.clone();
    let path = f.path(&id, true);
    let before = fs::read(&path).unwrap();
    drop(f.store);
    let output = command(f.temp.path(), &state, true)
        .args(["preview", "--at", "2000-01-01T00:00:00Z"])
        .assert()
        .code(1)
        .get_output()
        .clone();
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("current time or a future")
    );
    assert_eq!(fs::read(path).unwrap(), before);
    assert!(!state.join("last-run.json").exists());
}
