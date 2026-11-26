pub mod cli;
pub mod config;
pub mod database;
mod diagnostics;
pub mod engine;
pub mod fsutil;
mod lineage;
mod metadata;
mod owners;
mod relations;
pub mod scheduler;

#[cfg(test)]
extern crate self as codex_retain;

use anyhow::{Context, Result, ensure};
use clap::{CommandFactory, Parser};
use cli::{Action, Cli};
use config::{Policy, Store};
use database::DatabaseAccess;
use engine::ExecutionMode;
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{self, Write},
    path::Path,
    process::ExitCode,
};

pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(c) => c,
        Err(e) => {
            let code = e.exit_code();
            let _ = e.print();
            return ExitCode::from(code as u8);
        }
    };
    let scheduled = matches!(cli.command, Action::Run { scheduled: true });
    let state = cli
        .state_dir
        .clone()
        .or_else(|| config::default_state().ok());
    let outcome = {
        let mut out = io::BufWriter::with_capacity(64 * 1024, io::stdout().lock());
        execute(cli, &mut out).and_then(|code| {
            out.flush()?;
            Ok(code)
        })
    };
    match outcome {
        Ok(code) => ExitCode::from(code),
        Err(e)
            if e.downcast_ref::<io::Error>()
                .is_some_and(|e| e.kind() == io::ErrorKind::BrokenPipe)
                || e.downcast_ref::<serde_json::Error>()
                    .and_then(serde_json::Error::io_error_kind)
                    == Some(io::ErrorKind::BrokenPipe) =>
        {
            ExitCode::SUCCESS
        }
        Err(error) => {
            if scheduled {
                if let Some(state) = state {
                    // Never create an unbounded log. A failed acquisition must
                    // not overwrite the running owner's last-run receipt.
                    if let Ok(store) = Store::open(state) {
                        let _ = fsutil::atomic_json(
                            &store.root.join("last-run.json"),
                            &json!({"schema":1,"status":"error","at":config::now().ok(),"error":format!("{error:#}")}),
                        );
                    }
                }
            } else {
                let _ = writeln!(
                    io::stderr().lock(),
                    "error: {}",
                    safe_text(&format!("{error:#}"))
                );
            }
            ExitCode::FAILURE
        }
    }
}

fn safe_text(value: &str) -> String {
    let mut text = String::with_capacity(value.len());
    for c in value.chars() {
        if c.is_control() {
            text.extend(c.escape_default());
        } else {
            text.push(c);
        }
    }
    text
}
fn date(value: Option<i64>) -> String {
    value
        .and_then(|v| jiff::Timestamp::from_second(v).ok())
        .map(|v| v.to_string())
        .unwrap_or_else(|| "unknown".into())
}
fn emit<T: Serialize>(
    out: &mut impl Write,
    json_mode: bool,
    value: &T,
    message: &str,
) -> Result<()> {
    if json_mode {
        serde_json::to_writer(&mut *out, value)?;
        writeln!(out)?;
    } else {
        writeln!(out, "{message}")?;
    }
    out.flush()?;
    Ok(())
}
fn native_mutations() -> Result<()> {
    ensure!(
        cfg!(target_os = "macos"),
        "automatic and destructive operations are supported on macOS only in v0.1"
    );
    Ok(())
}
fn profile(
    home: Option<std::path::PathBuf>,
    binary: &Path,
) -> Result<(std::path::PathBuf, std::path::PathBuf)> {
    let home = fsutil::checked_absolute(&home.map(Ok).unwrap_or_else(config::default_home)?)?;
    let binary = config::resolve_executable(binary)?;
    database::verify_binary(&binary)?;
    Ok((home, binary))
}
fn enabled(store: &Store) -> Result<Policy> {
    let p = store.policy()?;
    ensure!(p.enabled, "policy is disabled; run enable first");
    Ok(p)
}

fn enable_policy(
    store: &Store,
    args: cli::EnableArgs,
    out: &mut impl Write,
    json_mode: bool,
) -> Result<u8> {
    let cli::EnableArgs {
        days,
        codex_home,
        codex_bin,
        no_schedule,
        yes,
    } = args;
    native_mutations()?;
    ensure!(
        yes,
        "enable immediately and permanently deletes eligible local archives recorded as archived more than {days} days ago, then applies the same retention without later prompts. Transition capture adds small SQLite tables and triggers. Review doctor first, then repeat with --yes"
    );
    let existing = if store.root.join("policy.json").exists() {
        Some(store.policy()?)
    } else {
        None
    };
    ensure!(
        existing.as_ref().is_none_or(|p| !p.enabled),
        "policy is already enabled; use policy --days, pause, resume, or disable first"
    );
    ensure!(
        existing.as_ref().is_none_or(|p| !p.automatic),
        "an earlier automatic installation is incomplete; run disable before re-enabling"
    );
    ensure!(
        !fsutil::path_exists(&store.root.join("pending.json"))?,
        "an interrupted deletion needs recovery; run disable before re-enabling"
    );
    let (home, binary) = profile(codex_home, &codex_bin)?;
    if let Some(previous) = existing.as_ref().filter(|p| p.codex_home != home) {
        let mut old_db = database::open(&previous.codex_home, DatabaseAccess::ReadWrite)
            .context("finish disabling the previous Codex profile before changing profiles")?;
        database::uninstall_capture(&mut old_db, previous)
            .context("finish removing the previous capture extension before changing profiles")?;
    }
    ensure!(
        !store.root.starts_with(&home) && !home.starts_with(&store.root),
        "policy directory and Codex home must be separate, non-nested directories"
    );
    let mut db = database::open(&home, DatabaseAccess::ReadWrite)?;
    let identity = database::identity(&home)?;
    let mut policy = Policy {
        schema: 1,
        codex_home: home,
        codex_bin: binary,
        database: identity,
        retention_days: days,
        enabled: false,
        automatic: !no_schedule,
        paused: false,
        enabled_at: config::now()?,
        owner: store
            .root
            .to_str()
            .context("state directory must be UTF-8")?
            .into(),
        exclusions: existing.map(|p| p.exclusions).unwrap_or_else(BTreeSet::new),
    };
    store.save(&policy)?;
    let count = database::install_capture(&mut db, &policy.owner)?;
    if !no_schedule {
        scheduler::install(&store.root, &scheduler::installation_executable()?)?;
    }
    policy.enabled = true;
    store.save(&policy)?;
    if json_mode {
        let report = cleanup_compact(&mut db, &policy, store, config::now()?).context(
            "policy is enabled, but initial cleanup failed; use run to retry or pause to stop cleanup",
        )?;
        let incomplete = report.requires_attention();
        write!(
            out,
            "{{\"automatic\":{},\"enabled\":true,\"existing_archives_assessed\":{count},\"initial_archive_clock\":\"codex_archived_at\",\"initial_cleanup\":",
            !no_schedule
        )?;
        report.write_json(out)?;
        out.write_all(b",\"policy\":")?;
        serde_json::to_writer(&mut *out, &store.root.join("policy.json"))?;
        writeln!(out, ",\"retention_days\":{days},\"schema\":2}}")?;
        out.flush()?;
        return Ok(if incomplete { 3 } else { 0 });
    }
    let report = cleanup(
        &mut db,
        &policy,
        store,
        config::now()?,
        CleanupDetail::Summary,
    )
    .context(
        "policy is enabled, but initial cleanup failed; use run to retry or pause to stop cleanup",
    )?;
    let incomplete = report.requires_attention();
    writeln!(
        out,
        "Enabled: keep archives for {days} days from their recorded archive date.\nInitial cleanup: {} examined, {} deleted, {} skipped.{}\n{}\nUse status or preview for details; pause or disable to stop cleanup.",
        report.examined,
        report.deleted,
        report.skipped,
        if incomplete {
            " Some archives need attention; inspect the last-run report."
        } else {
            ""
        },
        if no_schedule {
            "Manual runs enabled; automatic scheduling is off."
        } else {
            "Automatic cleanup: hourly macOS LaunchAgent; no resident process."
        }
    )?;
    out.flush()?;
    Ok(if incomplete { 3 } else { 0 })
}

#[derive(Clone, Copy)]
enum CleanupDetail {
    Text,
    Summary,
}

#[derive(Serialize)]
struct LastRunReceipt<'a> {
    actual_reclaimed_bytes: Option<u64>,
    allocated_bytes_unlinked: u64,
    deleted: u64,
    examined: u64,
    logical_bytes_removed: u64,
    observed_free_space_delta_bytes: Option<i64>,
    schema: u32,
    skipped: u64,
    skips: &'a [&'a engine::Entry],
    started_at: i64,
    status: &'static str,
    warnings: &'a [String],
}

fn check_last_run_clock(store: &Store, now: i64) -> Result<()> {
    if store.root.join("last-run.json").exists() {
        let last: Value = fsutil::read_json(&store.root.join("last-run.json"))?;
        ensure!(
            last["started_at"].as_i64().is_none_or(|t| now >= t),
            "system clock moved backwards since the last run"
        );
    }
    Ok(())
}

/// Initial and subsequent cleanup share the same clock guard, deletion engine
/// and durable result. Setup holds the Store lock through its first run too.
fn cleanup(
    db: &mut rusqlite::Connection,
    policy: &Policy,
    store: &Store,
    now: i64,
    detail: CleanupDetail,
) -> Result<engine::Report> {
    check_last_run_clock(store, now)?;
    let report = match detail {
        CleanupDetail::Summary => engine::execute_scheduled(db, policy, store, now)?,
        CleanupDetail::Text => engine::execute_text(db, policy, store, now, ExecutionMode::Run)?,
    };
    let incomplete = report.requires_attention();
    let warnings_len = report.warnings.len().min(10);
    let skips: Vec<&engine::Entry> = report
        .entries
        .iter()
        .filter(|e| e.detail.is_some())
        .take(10)
        .collect();
    let receipt = LastRunReceipt {
        actual_reclaimed_bytes: None,
        allocated_bytes_unlinked: report.allocated_bytes_unlinked,
        deleted: report.deleted,
        examined: report.examined,
        logical_bytes_removed: report.logical_bytes_removed,
        observed_free_space_delta_bytes: report.observed_free_space_delta_bytes,
        schema: 1,
        skipped: report.skipped,
        skips: &skips,
        started_at: now,
        status: if incomplete { "attention" } else { "ok" },
        warnings: &report.warnings[..warnings_len],
    };
    fsutil::atomic_json(&store.root.join("last-run.json"), &receipt)?;
    Ok(report)
}

fn cleanup_compact(
    db: &mut rusqlite::Connection,
    policy: &Policy,
    store: &Store,
    now: i64,
) -> Result<engine::CompactReport> {
    check_last_run_clock(store, now)?;
    let report = engine::execute_compact(db, policy, store, now, ExecutionMode::Run)?;
    let incomplete = report.requires_attention();
    let warnings_len = report.warnings.len().min(10);
    let owned_skips = report.first_skips(10);
    let skips: Vec<&engine::Entry> = owned_skips.iter().collect();
    let receipt = LastRunReceipt {
        actual_reclaimed_bytes: None,
        allocated_bytes_unlinked: report.allocated_bytes_unlinked,
        deleted: report.deleted,
        examined: report.examined,
        logical_bytes_removed: report.logical_bytes_removed,
        observed_free_space_delta_bytes: report.observed_free_space_delta_bytes,
        schema: 1,
        skipped: report.skipped,
        skips: &skips,
        started_at: now,
        status: if incomplete { "attention" } else { "ok" },
        warnings: &report.warnings[..warnings_len],
    };
    fsutil::atomic_json(&store.root.join("last-run.json"), &receipt)?;
    Ok(report)
}

pub fn execute(cli: Cli, out: &mut impl Write) -> Result<u8> {
    if let Action::Completions { shell } = cli.command {
        let mut command = Cli::command();
        let mut bytes = Vec::new();
        clap_complete::generate(shell, &mut command, "codex-retain", &mut bytes);
        out.write_all(&bytes)?;
        out.flush()?;
        return Ok(0);
    }
    if let Action::Doctor {
        codex_home,
        codex_bin,
    } = cli.command
    {
        let (home, binary) = profile(codex_home, &codex_bin)?;
        let db = database::open(&home, DatabaseAccess::ReadOnly)?;
        let coverage = diagnostics::HistoryCoverage::read(&db)?;
        emit(
            out,
            cli.json,
            &json!({"schema":1,"compatible":true,"compatibility_scope":diagnostics::COMPATIBILITY_SCOPE,"codex_version":database::CODEX_VERSION,"codex_home":home,"codex_bin":binary,"archived_threads":coverage.archived_threads(),"history_coverage":coverage,"mutation_platform_supported":cfg!(target_os="macos")}),
            &format!(
                "Schema and selected Codex executable: verified for {}.\nThis checks the selected executable; every writer of this profile must use the supported version.\n{}",
                database::CODEX_VERSION,
                coverage.message()
            ),
        )?;
        return Ok(0);
    }
    let root = cli
        .state_dir
        .map(Ok)
        .unwrap_or_else(config::default_state)?;
    if matches!(cli.command, Action::Status) && !root.join("policy.json").exists() {
        emit(
            out,
            cli.json,
            &json!({"schema":1,"enabled":false,"configured":false,"scheduler":scheduler::status()?}),
            "No policy configured. Run codex-retain enable --days 30 --yes to clean eligible archives now and enable retention.",
        )?;
        return Ok(0);
    }
    let store = Store::open(root)?;
    match cli.command {
        Action::Enable(args) => return enable_policy(&store, args, out, cli.json),
        Action::Status => {
            let p = store.policy()?;
            let scheduler = scheduler::status()?;
            let compatibility = database::verify_binary(&p.codex_bin)
                .and_then(|()| database::open(&p.codex_home, DatabaseAccess::ReadOnly))
                .and_then(|c| {
                    if p.enabled {
                        database::verify_policy(&c, &p)?;
                    }
                    Ok(c)
                });
            let (error, coverage, coverage_error) = match compatibility {
                Ok(c) => match diagnostics::HistoryCoverage::read(&c) {
                    Ok(coverage) => (None, Some(coverage), None),
                    Err(error) => (None, None, Some(format!("{error:#}"))),
                },
                Err(error) => (Some(format!("{error:#}")), None, None),
            };
            let coverage_message = coverage.as_ref().map_or_else(
                || match &coverage_error {
                    Some(error) => format!("Archive history formats: unavailable: {}", safe_text(error)),
                    None => "Archive history formats: unavailable because compatibility validation failed.".into(),
                },
                diagnostics::HistoryCoverage::message,
            );
            let last: Option<Value> = if store.root.join("last-run.json").exists() {
                Some(fsutil::read_json(&store.root.join("last-run.json"))?)
            } else {
                None
            };
            let message = format!(
                "Policy: {}{}; {} days.\nScheduler: {:?}; plist {}.\nSchema, selected Codex executable and enabled policy: {}.\n{}\nExclusions: {}. Pending recovery: {}.\nLast run: {}",
                if p.enabled { "enabled" } else { "disabled" },
                if p.paused { " (paused)" } else { "" },
                p.retention_days,
                scheduler.registration,
                if scheduler.plist_present {
                    "present"
                } else {
                    "absent"
                },
                error
                    .as_deref()
                    .map(safe_text)
                    .unwrap_or_else(|| "verified".into()),
                coverage_message,
                p.exclusions.len(),
                store.root.join("pending.json").exists(),
                last.as_ref()
                    .map(|v| safe_text(&v.to_string()))
                    .unwrap_or_else(|| "not run yet".into())
            );
            emit(
                out,
                cli.json,
                &json!({"schema":1,"policy":p,"scheduler":scheduler,"compatibility_scope":"schema_selected_codex_binary_and_enabled_policy","compatibility_error":error,"history_coverage":coverage,"history_coverage_error":coverage_error,"last_run":last,"pending_recovery":store.root.join("pending.json").exists()}),
                &message,
            )?;
        }
        Action::Preview { .. } | Action::Run { .. } => {
            let mode = if matches!(cli.command, Action::Run { .. }) {
                ExecutionMode::Run
            } else {
                ExecutionMode::Preview
            };
            let apply = mode == ExecutionMode::Run;
            let quiet = matches!(cli.command, Action::Run { scheduled: true });
            let p = enabled(&store)?;
            if quiet && p.paused {
                return Ok(0);
            }
            if apply {
                native_mutations()?;
            }
            database::verify_binary(&p.codex_bin)?;
            let access = match mode {
                ExecutionMode::Preview => DatabaseAccess::ReadOnly,
                ExecutionMode::Run => DatabaseAccess::ReadWrite,
            };
            let mut db = database::open(&p.codex_home, access)?;
            let now = config::now()?;
            let forecast_at = match cli.command {
                Action::Preview { at } => at.map(|timestamp| timestamp.as_second()),
                _ => None,
            };
            ensure!(
                forecast_at.is_none_or(|at| at >= now),
                "preview --at must be the current time or a future RFC3339 timestamp"
            );
            if !quiet && cli.json {
                let mut report = if apply {
                    cleanup_compact(&mut db, &p, &store, now)?
                } else {
                    engine::execute_compact(&mut db, &p, &store, forecast_at.unwrap_or(now), mode)?
                };
                if let Some(at) = forecast_at {
                    report.set_forecast(now, at);
                }
                let incomplete = report.requires_attention();
                report.write_json(out)?;
                writeln!(out)?;
                out.flush()?;
                return Ok(if apply && incomplete { 3 } else { 0 });
            }
            let mut report = if apply {
                let detail = if quiet {
                    CleanupDetail::Summary
                } else {
                    CleanupDetail::Text
                };
                cleanup(&mut db, &p, &store, now, detail)?
            } else {
                engine::execute_text(&mut db, &p, &store, forecast_at.unwrap_or(now), mode)?
            };
            if let Some(at) = forecast_at {
                report.started_at = now;
                report.evaluated_at = Some(at);
            }
            let incomplete = report.requires_attention();
            if !quiet {
                if let Some(at) = forecast_at {
                    writeln!(
                        out,
                        "Forecast for {} using the current profile snapshot. No deletion. Future changes can alter eligibility; this is not a cleanup guarantee.",
                        date(Some(at))
                    )?;
                }
                writeln!(
                    out,
                    "{}: {} examined, {} eligible, {} deleted, {} skipped.",
                    report.mode, report.examined, report.eligible, report.deleted, report.skipped
                )?;
                for e in report.entries.iter().take(30) {
                    writeln!(
                        out,
                        "{}  {}  due={}  {}{}",
                        safe_text(&e.id),
                        e.reason,
                        date(e.eligible_at),
                        safe_text(&e.title),
                        e.detail
                            .as_ref()
                            .map(|d| format!("  {}", safe_text(d)))
                            .unwrap_or_default()
                    )?;
                }
                if report.examined > 30 {
                    writeln!(
                        out,
                        "Showing 30 entries; use --json for all {}.",
                        report.examined
                    )?;
                }
                if apply {
                    writeln!(
                        out,
                        "Removed {} logical bytes; unlinked {} allocated bytes (estimate).\nObserved volume free-space change: {} bytes. Exact reclaimed space is unknown (snapshots/shared blocks/other activity).",
                        report.logical_bytes_removed,
                        report.allocated_bytes_unlinked,
                        report
                            .observed_free_space_delta_bytes
                            .map(|v| v.to_string())
                            .unwrap_or_else(|| "unknown".into())
                    )?;
                }
                for warning in &report.warnings {
                    writeln!(out, "Note: {}", safe_text(warning))?;
                }
                out.flush()?;
            }
            return Ok(if apply && incomplete { 3 } else { 0 });
        }
        Action::Pause | Action::Resume => {
            let mut p = enabled(&store)?;
            let pause = matches!(cli.command, Action::Pause);
            p.paused = pause;
            store.save(&p)?;
            emit(
                out,
                cli.json,
                &json!({"schema":1,"paused":pause}),
                if pause {
                    "Paused. No deletion; archive transition capture continues."
                } else {
                    "Resumed. Time already spent in the archive still counts; inspect preview before the next run."
                },
            )?;
        }
        Action::Policy { days, yes } => {
            let mut p = enabled(&store)?;
            ensure!(
                days >= p.retention_days || yes,
                "a shorter policy can make archives immediately due; repeat with --yes after preview"
            );
            p.retention_days = days;
            store.save(&p)?;
            emit(
                out,
                cli.json,
                &json!({"schema":1,"retention_days":days}),
                &format!("Retention set to {days} days. Existing archive periods are preserved."),
            )?;
        }
        Action::Exclude { ref id } | Action::Include { ref id, .. } => {
            let include = matches!(cli.command, Action::Include { .. });
            if let Action::Include { yes, .. } = cli.command {
                ensure!(
                    yes,
                    "removing protection may make this chat immediately due; repeat with --yes"
                );
            }
            let id = uuid::Uuid::parse_str(id)
                .context("expected a UUID thread ID")?
                .to_string();
            let mut p = store.policy()?;
            if include {
                p.exclusions.remove(&id);
            } else {
                p.exclusions.insert(id.clone());
            }
            store.save(&p)?;
            emit(
                out,
                cli.json,
                &json!({"schema":1,"id":id,"excluded":!include}),
                if include {
                    "Protection removed; the existing archive period still applies."
                } else {
                    "Thread excluded from automatic and manual cleanup under this policy."
                },
            )?;
        }
        Action::Disable | Action::Uninstall => {
            native_mutations()?;
            let mut p = store.policy()?;
            p.enabled = false;
            store.save(&p)?; // fail closed even if launchctl or Codex is unavailable
            if p.automatic {
                scheduler::disable(&store.root)?;
                p.automatic = false;
                store.save(&p)?;
            }
            let mut db = database::open(&p.codex_home, DatabaseAccess::ReadWrite)?;
            if store.root.join("pending.json").exists() {
                engine::recover(&mut db, &p, &store)?;
            }
            database::uninstall_capture(&mut db, &p)?;
            emit(
                out,
                cli.json,
                &json!({"schema":1,"enabled":false,"scheduler_removed":true,"capture_removed":true}),
                "Disabled. LaunchAgent and transition capture removed.\nYou can now remove the executable (for Cargo: cargo uninstall codex-retain). Policy and last-run report remain for inspection.",
            )?;
        }
        Action::Doctor { .. } | Action::Completions { .. } => unreachable!("handled above"),
    }
    Ok(0)
}
