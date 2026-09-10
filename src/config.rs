use crate::fsutil::{self, Identity};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::File,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    pub schema: u32,
    pub codex_home: PathBuf,
    pub codex_bin: PathBuf,
    pub database: Identity,
    pub retention_days: u32,
    pub enabled: bool,
    /// Whether automatic scheduling was requested. Remains true during incomplete
    /// installation or removal until scheduler disable succeeds; actual launchd
    /// registration is reported separately by SchedulerStatus.
    pub automatic: bool,
    pub paused: bool,
    /// Policy activation time in whole seconds since the Unix epoch.
    pub enabled_at: i64,
    pub owner: String,
    pub exclusions: BTreeSet<String>,
}
impl Policy {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema == 1, "unsupported policy schema");
        ensure!(
            (1..=36500).contains(&self.retention_days),
            "retention must be 1..36500 days"
        );
        ensure!(
            self.codex_home.is_absolute() && self.codex_bin.is_absolute(),
            "policy paths must be absolute"
        );
        ensure!(self.enabled_at > 0, "invalid policy start time");
        Ok(())
    }
    /// Retention duration in seconds, with each day equal to 86,400 seconds.
    pub fn duration(&self) -> i64 {
        i64::from(self.retention_days) * 86400
    }
}

pub struct Store {
    pub root: PathBuf,
    _lock: File,
}
impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        fsutil::private_dir(&root)?;
        let root = fsutil::checked_absolute(&root)?;
        let lock = fsutil::try_lock(&root.join("operation.lock"))?;
        Ok(Self { root, _lock: lock })
    }
    pub fn policy(&self) -> Result<Policy> {
        let p: Policy = fsutil::read_json(&self.root.join("policy.json"))
            .context("no usable policy; run enable first")?;
        p.validate()?;
        Ok(p)
    }
    pub fn save(&self, p: &Policy) -> Result<()> {
        p.validate()?;
        fsutil::atomic_json(&self.root.join("policy.json"), p)
    }
}

/// Current wall-clock time in whole seconds since the Unix epoch.
pub fn now() -> Result<i64> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs()
        .try_into()?)
}
pub fn default_home() -> Result<PathBuf> {
    match std::env::var_os("CODEX_HOME") {
        Some(p) => Ok(p.into()),
        None => Ok(user_home()?.join(".codex")),
    }
}
pub fn user_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is unset; supply explicit paths")
}
pub fn default_state() -> Result<PathBuf> {
    if cfg!(target_os = "macos") {
        Ok(user_home()?.join("Library/Application Support/codex-retain"))
    } else {
        Ok(user_home()?.join(".local/state/codex-retain"))
    }
}
pub fn resolve_executable(path: &Path) -> Result<PathBuf> {
    if path.components().count() > 1 {
        return fsutil::checked_absolute(path);
    }
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let candidate = directory.join(path);
        if candidate.is_file() {
            return fsutil::checked_absolute(&candidate);
        }
    }
    anyhow::bail!("Codex executable not found; pass --codex-bin /absolute/path/to/codex")
}
