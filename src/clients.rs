//! Read-only discovery of installed Codex clients (CLI, Desktop app, custom).
//!
//! Client discovery is purely diagnostic: retention safety depends on the local
//! storage contract in `CODEX_HOME`, not on pinning one Codex binary version.

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    collections::HashSet,
    ffi::OsStr,
    fs,
    io::{Read, Seek},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientKind {
    Custom,
    Cli,
    Desktop,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ClientInfo {
    pub kind: ClientKind,
    pub path: PathBuf,
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Probe `binary --version` in an isolated temporary HOME/CODEX_HOME so startup
/// side effects (such as Codex arg0 helpers) never touch any real profile.
/// Accepts any non-empty version output rather than pinning a single release.
pub fn probe_binary_version(binary: &Path) -> Result<String> {
    let environment = tempfile::tempdir()?;
    let codex_home = environment.path().join("codex");
    fs::create_dir(&codex_home)?;
    let output = tempfile::tempfile()?;
    let mut child = Command::new(binary)
        .arg("--version")
        .env("HOME", environment.path())
        .env("CODEX_HOME", &codex_home)
        .stdin(Stdio::null())
        .stdout(output.try_clone()?)
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("start Codex version probe for {}", binary.display()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut poll = Duration::from_micros(500);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("Codex version probe timed out for {}", binary.display());
        }
        std::thread::sleep(poll);
        if poll < Duration::from_millis(5) {
            poll = (poll * 2).min(Duration::from_millis(5));
        }
    };
    ensure!(
        status.success(),
        "Codex version probe failed for {}",
        binary.display()
    );
    let mut output = output;
    output.rewind()?;
    let mut version = String::new();
    output.take(1025).read_to_string(&mut version)?;
    let trimmed = version.trim();
    ensure!(
        !trimmed.is_empty(),
        "Codex binary returned empty version output: {}",
        binary.display()
    );
    Ok(trimmed.to_owned())
}

/// Discover installed Codex executables (optional custom path, CLI on PATH,
/// and macOS Codex Desktop embedded binaries). Never fails if none are found.
pub fn discover(explicit: Option<&Path>) -> Vec<ClientInfo> {
    #[cfg(target_os = "macos")]
    let desktop_candidates = {
        let mut candidates = vec![PathBuf::from(
            "/Applications/Codex.app/Contents/Resources/codex",
        )];
        if let Some(home) = std::env::var_os("HOME") {
            candidates
                .push(PathBuf::from(home).join("Applications/Codex.app/Contents/Resources/codex"));
        }
        candidates
    };
    #[cfg(not(target_os = "macos"))]
    let desktop_candidates: Vec<PathBuf> = Vec::new();
    discover_with_sources(
        explicit,
        std::env::var_os("PATH").as_deref(),
        &desktop_candidates,
    )
}

fn discover_with_sources(
    explicit: Option<&Path>,
    search_path: Option<&OsStr>,
    desktop_candidates: &[PathBuf],
) -> Vec<ClientInfo> {
    let mut candidates: Vec<(ClientKind, PathBuf)> = Vec::new();
    if let Some(path) = explicit {
        candidates.push((ClientKind::Custom, path.to_path_buf()));
    }
    if let Some(path_env) = search_path {
        for directory in std::env::split_paths(path_env) {
            let candidate = directory.join("codex");
            if candidate.is_file() {
                candidates.push((ClientKind::Cli, candidate));
                break;
            }
        }
    }
    for desktop in desktop_candidates {
        if desktop.is_file() {
            candidates.push((ClientKind::Desktop, desktop.clone()));
        }
    }

    let mut seen_canonical = HashSet::new();
    let mut discovered = Vec::new();
    for (kind, path) in candidates {
        let canonical = match fs::canonicalize(&path) {
            Ok(resolved) => resolved,
            Err(error) => {
                if kind == ClientKind::Custom {
                    discovered.push(ClientInfo {
                        kind,
                        path,
                        version: None,
                        error: Some(format!("{error:#}")),
                    });
                }
                continue;
            }
        };
        if !seen_canonical.insert(canonical.clone()) {
            continue;
        }
        match probe_binary_version(&canonical) {
            Ok(version) => discovered.push(ClientInfo {
                kind,
                path: canonical,
                version: Some(version),
                error: None,
            }),
            Err(error) => discovered.push(ClientInfo {
                kind,
                path: canonical,
                version: None,
                error: Some(format!("{error:#}")),
            }),
        }
    }
    discovered
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fake_codex(dir: &Path, name: &str, version: &str) -> PathBuf {
        let bin = dir.join(name);
        fs::write(&bin, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n")).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        bin
    }

    #[test]
    fn discovers_cli_and_desktop_clients_with_different_versions_and_deduplicates() {
        let temp = tempfile::tempdir().unwrap();
        let cli_dir = temp.path().join("bin");
        let desktop_dir = temp.path().join("Codex.app/Contents/Resources");
        fs::create_dir_all(&cli_dir).unwrap();
        fs::create_dir_all(&desktop_dir).unwrap();
        let cli_bin = fake_codex(&cli_dir, "codex", "codex-cli 0.140.0");
        let desktop_bin = fake_codex(&desktop_dir, "codex", "codex-cli 0.210.5");

        let found = discover_with_sources(
            None,
            Some(cli_dir.as_os_str()),
            std::slice::from_ref(&desktop_bin),
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].kind, ClientKind::Cli);
        assert_eq!(found[0].version.as_deref(), Some("codex-cli 0.140.0"));
        assert_eq!(found[0].path, fs::canonicalize(&cli_bin).unwrap());
        assert_eq!(found[1].kind, ClientKind::Desktop);
        assert_eq!(found[1].version.as_deref(), Some("codex-cli 0.210.5"));
        assert_eq!(found[1].path, fs::canonicalize(&desktop_bin).unwrap());

        // Passing the same binary via explicit custom path deduplicates cleanly.
        let dedup = discover_with_sources(
            Some(&desktop_bin),
            Some(cli_dir.as_os_str()),
            std::slice::from_ref(&desktop_bin),
        );
        assert_eq!(dedup.len(), 2);
        assert_eq!(dedup[0].kind, ClientKind::Custom);
        assert_eq!(dedup[1].kind, ClientKind::Cli);
    }

    #[test]
    fn empty_discovery_succeeds_without_any_installed_binary() {
        let temp = tempfile::tempdir().unwrap();
        let found = discover_with_sources(None, Some(temp.path().as_os_str()), &[]);
        assert!(found.is_empty());
    }

    #[test]
    fn version_probe_isolates_startup_side_effects_and_accepts_any_version() {
        let temp = tempfile::tempdir().unwrap();
        let bin = temp.path().join("codex-future");
        let receipt = temp.path().join("observed-home");
        let script = format!(
            "#!/bin/sh\nprintf '%s' \"$CODEX_HOME\" > '{}'\nprintf test > \"$CODEX_HOME/helper\"\nprintf '%s\\n' 'codex-cli 2.99.0-beta.1'\n",
            receipt.display()
        );
        fs::write(&bin, script).unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o700)).unwrap();
        let version = probe_binary_version(&bin).unwrap();
        assert_eq!(version, "codex-cli 2.99.0-beta.1");
        let isolated = PathBuf::from(fs::read_to_string(receipt).unwrap());
        assert!(!isolated.exists());
        assert_ne!(isolated, temp.path());
    }
}
