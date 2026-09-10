//! A per-user, periodic launchd job; no resident scheduler or copied executable.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde::Serialize;

pub const LABEL: &str = "io.github.codex-retain";

/// Preserve the invoked installation link (for example Homebrew's bin link),
/// but only when it resolves to the executable that is actually running.
pub fn installation_executable() -> Result<std::path::PathBuf> {
    let current = std::env::current_exe()?.canonicalize()?;
    let invoked = std::env::args_os()
        .next()
        .context("missing executable name")?;
    installation_path(
        &current,
        Path::new(&invoked),
        std::env::var_os("PATH").as_deref(),
    )
}

fn installation_path(
    current: &Path,
    invoked: &Path,
    search_path: Option<&std::ffi::OsStr>,
) -> Result<std::path::PathBuf> {
    let matches = |candidate: &Path| candidate.canonicalize().is_ok_and(|path| path == current);
    if invoked.components().count() > 1 || invoked.is_absolute() {
        let absolute = std::path::absolute(invoked)?;
        if matches(&absolute) {
            return Ok(absolute);
        }
    } else if let Some(search_path) = search_path {
        for directory in std::env::split_paths(search_path) {
            let candidate = std::path::absolute(directory.join(invoked))?;
            if matches(&candidate) {
                return Ok(candidate);
            }
        }
    }
    // An altered argv[0] or PATH must never schedule a different executable.
    Ok(current.to_path_buf())
}

#[cfg(all(test, unix))]
mod installation_tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    #[test]
    fn scheduled_link_follows_package_upgrade_after_old_version_is_removed() -> Result<()> {
        let root = tempfile::tempdir()?;
        let root = root.path().canonicalize()?;
        let first = root.join("0.1.0");
        let second = root.join("0.2.0");
        let bin = root.join("bin");
        fs::create_dir(&bin)?;
        fs::write(&first, "first")?;
        fs::write(&second, "second")?;
        let link = bin.join("codex-retain");
        symlink(&first, &link)?;
        let selected = installation_path(&first, Path::new("codex-retain"), Some(bin.as_os_str()))?;
        assert_eq!(selected, link);
        assert_eq!(installation_path(&first, &link, None)?, link);
        let rendered = plist_with_path(&root, &selected, "/usr/bin")?;
        assert!(rendered.contains(&format!("<string>{}</string>", link.display())));
        fs::remove_file(&link)?;
        symlink(&second, &link)?;
        fs::remove_file(&first)?;
        assert_eq!(fs::read_to_string(selected)?, "second");
        Ok(())
    }

    #[test]
    fn different_or_missing_invoked_executable_falls_back_to_current() -> Result<()> {
        let root = tempfile::tempdir()?;
        let root = root.path().canonicalize()?;
        let current = root.join("current");
        let other = root.join("other");
        fs::write(&current, "current")?;
        fs::write(&other, "other")?;
        assert_eq!(installation_path(&current, &other, None)?, current);
        assert_eq!(
            installation_path(&current, Path::new("missing"), None)?,
            current
        );
        assert_eq!(
            installation_path(&current, Path::new("other"), Some(root.as_os_str()))?,
            current
        );
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Registration {
    Registered,
    Missing,
    Error,
    Unsupported,
}

#[derive(Debug, Serialize)]
pub struct SchedulerStatus {
    pub supported: bool,
    pub plist_present: bool,
    /// Registration is not proof that a job is currently running or succeeding.
    pub registration: Registration,
    pub detail: Option<String>,
}

/// Render without installing anything, including on unsupported platforms.
pub fn plist(state_dir: &Path, executable: &Path) -> Result<String> {
    let search_path = std::env::var("PATH")
        .context("PATH must be set and valid UTF-8 for scheduled Codex execution")?;
    plist_with_path(state_dir, executable, &search_path)
}

fn plist_with_path(state_dir: &Path, executable: &Path, search_path: &str) -> Result<String> {
    let state = xml_path(state_dir)?;
    let executable = xml_path(executable)?;
    let search_path = xml_text(search_path)?;
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{executable}</string>
    <string>--state-dir</string><string>{state}</string>
    <string>run</string><string>--scheduled</string>
  </array>
  <key>EnvironmentVariables</key>
  <dict><key>PATH</key><string>{search_path}</string></dict>
  <key>StartInterval</key><integer>3600</integer>
  <key>RunAtLoad</key><false/>
  <key>StandardOutPath</key><string>/dev/null</string>
  <key>StandardErrorPath</key><string>/dev/null</string>
</dict>
</plist>
"#
    ))
}

fn xml_path(path: &Path) -> Result<String> {
    ensure!(
        path.is_absolute(),
        "scheduler paths must be absolute: {}",
        path.display()
    );
    let value = path.to_str().context("launchd paths must be valid UTF-8")?;
    xml_text(value)
}

fn xml_text(value: &str) -> Result<String> {
    ensure!(
        value.chars().all(|c| matches!(c, '\t' | '\n' | '\r')
            || c >= '\u{20}' && c != '\u{fffe}' && c != '\u{ffff}'),
        "scheduler value contains a character XML cannot represent"
    );
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

#[cfg(target_os = "macos")]
pub fn install(state_dir: &Path, executable: &Path) -> Result<()> {
    launchd::install_at(
        &launchd::home()?,
        state_dir,
        executable,
        &mut launchd::SystemRunner,
    )
}

#[cfg(not(target_os = "macos"))]
pub fn install(_state_dir: &Path, _executable: &Path) -> Result<()> {
    anyhow::bail!("automatic scheduling is supported on macOS only")
}

#[cfg(target_os = "macos")]
pub fn disable(state_dir: &Path) -> Result<()> {
    launchd::disable_at(&launchd::home()?, state_dir, &mut launchd::SystemRunner)
}

#[cfg(not(target_os = "macos"))]
pub fn disable(_state_dir: &Path) -> Result<()> {
    anyhow::bail!("automatic scheduling is supported on macOS only")
}

#[cfg(target_os = "macos")]
pub fn status() -> Result<SchedulerStatus> {
    launchd::status_at(&launchd::home()?, &mut launchd::SystemRunner)
}

#[cfg(not(target_os = "macos"))]
pub fn status() -> Result<SchedulerStatus> {
    Ok(SchedulerStatus {
        supported: false,
        plist_present: false,
        registration: Registration::Unsupported,
        detail: Some("automatic scheduling is supported on macOS only".into()),
    })
}

#[cfg(any(target_os = "macos", test))]
mod launchd {
    use std::ffi::OsString;
    use std::fs::{self, File, OpenOptions};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitStatus, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use anyhow::{Context, Result, bail, ensure};

    use super::{LABEL, Registration, SchedulerStatus, plist, plist_with_path, xml_path, xml_text};

    #[cfg(target_os = "macos")]
    const TIMEOUT: Duration = Duration::from_secs(5);
    const CAPTURE_LIMIT: usize = 16 * 1024;

    #[derive(Debug)]
    pub(super) struct CommandResult {
        status: ExitStatus,
        stdout: String,
        stderr: String,
    }

    impl CommandResult {
        fn checked(self, operation: &str) -> Result<Self> {
            ensure!(
                self.status.success(),
                "{operation} failed ({}): {}",
                match self.status.code() {
                    Some(code) => format!("exit Some({code})"),
                    None => self.status.to_string(),
                },
                self.stderr.trim()
            );
            Ok(self)
        }
    }

    pub(super) trait Runner {
        fn run(&mut self, program: &str, args: &[OsString]) -> Result<CommandResult>;
    }

    #[cfg(target_os = "macos")]
    pub(super) struct SystemRunner;

    #[cfg(target_os = "macos")]
    impl Runner for SystemRunner {
        fn run(&mut self, program: &str, args: &[OsString]) -> Result<CommandResult> {
            run_bounded(program, args, TIMEOUT)
        }
    }

    fn capture(mut stream: impl Read) -> std::io::Result<String> {
        let mut kept = Vec::new();
        let mut chunk = [0; 4096];
        loop {
            let count = stream.read(&mut chunk)?;
            if count == 0 {
                break;
            }
            let available = CAPTURE_LIMIT.saturating_sub(kept.len());
            kept.extend_from_slice(&chunk[..count.min(available)]);
        }
        Ok(String::from_utf8_lossy(&kept).into_owned())
    }

    fn run_bounded(program: &str, args: &[OsString], timeout: Duration) -> Result<CommandResult> {
        let mut child = Command::new(program)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("start {program}"))?;
        let (Some(stdout_pipe), Some(stderr_pipe)) = (child.stdout.take(), child.stderr.take())
        else {
            let _ = child.kill();
            let _ = child.wait();
            bail!("capture {program} output");
        };
        let stdout_reader = match thread::Builder::new().spawn(move || capture(stdout_pipe)) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("start stdout reader");
            }
        };
        let stderr_reader = match thread::Builder::new().spawn(move || capture(stderr_pipe)) {
            Ok(reader) => reader,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_reader.join();
                return Err(error).context("start stderr reader");
            }
        };
        let started = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(20)),
                Ok(None) => {
                    break Err(anyhow::anyhow!(
                        "{program} timed out after {} seconds",
                        timeout.as_secs()
                    ));
                }
                Err(error) => break Err(error.into()),
            }
        };
        if status.is_err() {
            let _ = child.kill();
            let _ = child.wait();
        }
        // Only fixed system utilities use this runner; they do not delegate pipe
        // ownership to long-lived descendants. Both streams are drained even
        // when the retained diagnostic prefix has reached its limit.
        let stdout = stdout_reader
            .join()
            .map_err(|_| anyhow::anyhow!("stdout reader failed"))?;
        let stderr = stderr_reader
            .join()
            .map_err(|_| anyhow::anyhow!("stderr reader failed"))?;
        Ok(CommandResult {
            status: status?,
            stdout: stdout?,
            stderr: stderr?,
        })
    }

    #[cfg(target_os = "macos")]
    pub(super) fn home() -> Result<PathBuf> {
        let home = PathBuf::from(std::env::var_os("HOME").context("HOME is unset")?);
        ensure!(home.is_absolute(), "HOME must be an absolute path");
        Ok(home)
    }

    fn agent_path(home: &Path) -> Result<PathBuf> {
        ensure!(home.is_absolute(), "HOME must be an absolute path");
        Ok(home
            .join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist")))
    }

    fn scheduler_lock(path: &Path) -> Result<File> {
        let directory = path.parent().context("LaunchAgent path has no parent")?;
        fs::create_dir_all(directory).context("create LaunchAgents directory")?;
        let lock_path = directory.join(format!(".{LABEL}.lock"));
        if let Ok(metadata) = fs::symlink_metadata(&lock_path) {
            ensure!(
                metadata.file_type().is_file(),
                "scheduler lock is not a regular file"
            );
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(lock_path).context("open scheduler lock")?;
        lock.try_lock()
            .context("another schedule installation or removal is in progress")?;
        Ok(lock)
    }

    /// Only the exact format generated by this version proves ownership. The
    /// executable may change during an update, but the state directory may not.
    fn verify_owned_plist(path: &Path, state: &Path) -> Result<bool> {
        let template = plist_with_path(
            state,
            Path::new("/__codex_retain_executable_placeholder__"),
            "__codex_retain_search_path_placeholder__",
        )?;
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error).context("inspect existing LaunchAgent ownership"),
        };
        ensure!(
            metadata.file_type().is_file(),
            "existing LaunchAgent is not a regular file"
        );
        let mut contents = String::new();
        File::open(path)?
            .take(64 * 1024 + 1)
            .read_to_string(&mut contents)?;
        ensure!(
            contents.len() <= 64 * 1024,
            "existing LaunchAgent is too large to establish ownership"
        );
        let (prefix, remainder) = template
            .split_once("/__codex_retain_executable_placeholder__")
            .context("scheduler template lacks executable placeholder")?;
        let (middle, suffix) = remainder
            .split_once("__codex_retain_search_path_placeholder__")
            .context("scheduler template lacks search path placeholder")?;
        let (encoded_executable, remainder) = contents.strip_prefix(prefix).and_then(|rest| rest.split_once(middle))
            .context("existing LaunchAgent belongs to another state directory or has an unknown format; disable it using its original state directory first")?;
        let encoded_search_path = remainder
            .strip_suffix(suffix)
            .context("existing LaunchAgent has an unknown environment or schedule format")?;
        let decoded_executable = decode_xml_text(encoded_executable);
        ensure!(
            xml_path(Path::new(&decoded_executable))? == encoded_executable,
            "existing LaunchAgent has an unknown executable encoding"
        );
        ensure!(
            xml_text(&decode_xml_text(encoded_search_path))? == encoded_search_path,
            "existing LaunchAgent has an unknown search path encoding"
        );
        Ok(true)
    }

    fn decode_xml_text(encoded: &str) -> String {
        encoded
            .replace("&apos;", "'")
            .replace("&quot;", "\"")
            .replace("&gt;", ">")
            .replace("&lt;", "<")
            .replace("&amp;", "&")
    }

    fn domain(runner: &mut impl Runner) -> Result<String> {
        let result = runner
            .run("/usr/bin/id", &["-u".into()])?
            .checked("read user ID")?;
        let uid = result.stdout.trim();
        ensure!(
            !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()),
            "id returned an invalid user ID"
        );
        Ok(format!("gui/{uid}"))
    }

    fn registration(domain: &str, runner: &mut impl Runner) -> Result<Registration> {
        let output = runner.run(
            "/bin/launchctl",
            &["print".into(), format!("{domain}/{LABEL}").into()],
        )?;
        if output.status.success() {
            return Ok(Registration::Registered);
        }
        // Other errors (including an unavailable GUI domain) are not absence.
        if output.status.code() == Some(113)
            && output.stderr.contains("Could not find service")
            && output.stderr.contains(LABEL)
        {
            return Ok(Registration::Missing);
        }
        output.checked("inspect launchd registration")?;
        unreachable!("a nonzero status is rejected by checked")
    }

    fn bootout(domain: &str, runner: &mut impl Runner) -> Result<()> {
        let result = runner.run(
            "/bin/launchctl",
            &["bootout".into(), format!("{domain}/{LABEL}").into()],
        )?;
        if result.status.success() {
            return Ok(());
        }
        // A concurrent removal, or a previously unregistered job, is harmless.
        // Verify absence; do not classify a generic bootout error as success.
        if registration(domain, runner)? == Registration::Missing {
            return Ok(());
        }
        result.checked("unregister launchd job")?;
        Ok(())
    }

    fn atomic_plist(path: &Path, contents: &str) -> Result<()> {
        let directory = path.parent().context("LaunchAgent path has no parent")?;
        fs::create_dir_all(directory).context("create LaunchAgents directory")?;
        if let Ok(metadata) = fs::symlink_metadata(path) {
            ensure!(
                metadata.file_type().is_file(),
                "refusing to replace a non-regular LaunchAgent file"
            );
        }
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).context("stage LaunchAgent plist")?;
        // NamedTempFile is private (0600 on Unix), even when replacing a file.
        temporary
            .write_all(contents.as_bytes())
            .context("write LaunchAgent plist")?;
        temporary
            .as_file()
            .sync_all()
            .context("sync LaunchAgent plist")?;
        temporary
            .persist(path)
            .context("replace LaunchAgent plist")?;
        File::open(directory)?
            .sync_all()
            .context("sync LaunchAgents directory")?;
        Ok(())
    }

    fn remove_plist(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => {
                File::open(path.parent().context("LaunchAgent path has no parent")?)?.sync_all()?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove LaunchAgent plist"),
        }
    }

    pub(super) fn install_at(
        home: &Path,
        state: &Path,
        executable: &Path,
        runner: &mut impl Runner,
    ) -> Result<()> {
        let contents = plist(state, executable)?;
        ensure!(state.is_dir(), "scheduler state directory does not exist");
        let metadata = fs::metadata(executable).context("inspect scheduled executable")?;
        ensure!(
            metadata.is_file(),
            "scheduled executable must be a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            ensure!(
                metadata.permissions().mode() & 0o111 != 0,
                "scheduled executable has no execute permission"
            );
        }
        let path = agent_path(home)?;
        let _lock = scheduler_lock(&path)?;
        let owned = verify_owned_plist(&path, state)?;
        let domain = domain(runner)?;
        if registration(&domain, runner)? == Registration::Registered {
            ensure!(
                owned,
                "a LaunchAgent is already registered without an owned plist; disable it using the original automatic policy first"
            );
            bootout(&domain, runner)?;
        }
        atomic_plist(&path, &contents)?;
        let bootstrap = runner
            .run(
                "/bin/launchctl",
                &[
                    "bootstrap".into(),
                    domain.clone().into(),
                    path.clone().into_os_string(),
                ],
            )
            .and_then(|result| result.checked("register launchd job").map(|_| ()))
            .and_then(|()| {
                ensure!(
                    registration(&domain, runner)? == Registration::Registered,
                    "launchd did not retain the registered job"
                );
                Ok(())
            });
        if let Err(error) = bootstrap {
            // Never leave a failed install silently set to start at next login.
            // A failed/ambiguous bootstrap may have registered the service.
            let unregister = bootout(&domain, runner);
            let remove = remove_plist(&path);
            return Err(error).with_context(|| format!("schedule installation failed; cleanup unregister={unregister:?}, plist removal={remove:?}; inspect status before retrying"));
        }
        Ok(())
    }

    pub(super) fn disable_at(home: &Path, state: &Path, runner: &mut impl Runner) -> Result<()> {
        let path = agent_path(home)?;
        let _lock = scheduler_lock(&path)?;
        verify_owned_plist(&path, state)?;
        let domain = domain(runner)?;
        bootout(&domain, runner)?;
        remove_plist(&path)
    }

    pub(super) fn status_at(home: &Path, runner: &mut impl Runner) -> Result<SchedulerStatus> {
        let path = agent_path(home)?;
        let plist_present = match fs::symlink_metadata(&path) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error).context("inspect LaunchAgent plist"),
        };
        let query = domain(runner).and_then(|domain| registration(&domain, runner));
        Ok(match query {
            Ok(registration) => SchedulerStatus {
                supported: true,
                plist_present,
                registration,
                detail: None,
            },
            Err(error) => SchedulerStatus {
                supported: true,
                plist_present,
                registration: Registration::Error,
                detail: Some(format!("{error:#}")),
            },
        })
    }

    #[cfg(test)]
    mod tests {
        use std::collections::VecDeque;
        use std::os::unix::process::ExitStatusExt;

        use super::*;

        #[derive(Default)]
        struct MockRunner {
            outcomes: VecDeque<CommandResult>,
            calls: Vec<(String, Vec<OsString>)>,
        }

        impl Runner for MockRunner {
            fn run(&mut self, program: &str, args: &[OsString]) -> Result<CommandResult> {
                self.calls.push((program.into(), args.into()));
                self.outcomes.pop_front().context("unexpected mock command")
            }
        }

        fn result(code: i32, stdout: &str, stderr: &str) -> CommandResult {
            CommandResult {
                status: ExitStatus::from_raw(code << 8),
                stdout: stdout.into(),
                stderr: stderr.into(),
            }
        }

        fn missing() -> CommandResult {
            result(
                113,
                "",
                &format!("Could not find service \"{LABEL}\" in domain for user gui: 501"),
            )
        }

        fn runner(outcomes: Vec<CommandResult>) -> MockRunner {
            MockRunner {
                outcomes: outcomes.into(),
                ..MockRunner::default()
            }
        }

        #[test]
        fn atomic_plist_replaces_privately() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let path = agent_path(temp.path())?;
            atomic_plist(&path, "old")?;
            atomic_plist(&path, "new")?;
            assert_eq!(fs::read_to_string(&path)?, "new");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
            }
            assert_eq!(fs::read_dir(path.parent().unwrap())?.count(), 1);
            Ok(())
        }

        #[test]
        #[cfg(unix)]
        fn refuses_symlink_plist() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let target = temp.path().join("target");
            let link = temp.path().join("job.plist");
            fs::write(&target, "preserved")?;
            std::os::unix::fs::symlink(&target, &link)?;
            assert!(atomic_plist(&link, "replacement").is_err());
            assert_eq!(fs::read_to_string(target)?, "preserved");
            Ok(())
        }

        #[test]
        fn failed_bootout_preserves_plist_and_reports_error() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let path = agent_path(temp.path())?;
            let contents = plist(temp.path(), Path::new("/tmp/bin"))?;
            atomic_plist(&path, &contents)?;
            let mut runner = runner(vec![
                result(0, "501\n", ""),
                result(1, "", "permission denied"),
                result(0, "service", ""),
            ]);
            assert!(disable_at(temp.path(), temp.path(), &mut runner).is_err());
            assert_eq!(fs::read_to_string(path)?, contents);
            assert_eq!(
                runner.calls[1].1,
                [
                    OsString::from("bootout"),
                    OsString::from(format!("gui/501/{LABEL}"))
                ]
            );
            Ok(())
        }

        #[test]
        fn already_missing_disable_is_idempotent() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut runner = runner(vec![
                result(0, "501", ""),
                result(3, "", "No such process"),
                missing(),
            ]);
            disable_at(temp.path(), temp.path(), &mut runner)?;
            assert!(runner.outcomes.is_empty());
            Ok(())
        }

        #[test]
        fn status_distinguishes_missing_from_query_error() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut absent = runner(vec![result(0, "501", ""), missing()]);
            assert_eq!(
                status_at(temp.path(), &mut absent)?.registration,
                Registration::Missing
            );
            let mut failed = runner(vec![
                result(0, "501", ""),
                result(125, "", "domain unavailable"),
            ]);
            let status = status_at(temp.path(), &mut failed)?;
            assert_eq!(status.registration, Registration::Error);
            assert!(status.detail.unwrap().contains("domain unavailable"));
            Ok(())
        }

        #[test]
        fn installation_uses_absolute_binary_without_starting_job() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let executable = std::env::current_exe()?;
            let mut runner = runner(vec![
                result(0, "501", ""),
                missing(),
                result(0, "", ""),
                result(0, "registered", ""),
            ]);
            install_at(temp.path(), temp.path(), &executable, &mut runner)?;
            let saved = fs::read_to_string(agent_path(temp.path())?)?;
            assert_eq!(saved, plist(temp.path(), &executable)?);
            assert_eq!(runner.calls[2].1[0], "bootstrap");
            assert!(
                !runner
                    .calls
                    .iter()
                    .any(|(_, args)| args.iter().any(|arg| arg == "kickstart"))
            );
            Ok(())
        }

        #[test]
        fn another_state_cannot_replace_or_disable_the_schedule() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let original_state = temp.path().join("original");
            let other_state = temp.path().join("other");
            fs::create_dir_all(&original_state)?;
            fs::create_dir_all(&other_state)?;
            let path = agent_path(temp.path())?;
            let contents = plist(&original_state, &std::env::current_exe()?)?;
            atomic_plist(&path, &contents)?;
            let mut runner = MockRunner::default();
            assert!(
                install_at(
                    temp.path(),
                    &other_state,
                    &std::env::current_exe()?,
                    &mut runner
                )
                .is_err()
            );
            assert!(disable_at(temp.path(), &other_state, &mut runner).is_err());
            assert!(runner.calls.is_empty());
            assert_eq!(fs::read_to_string(path)?, contents);
            Ok(())
        }

        #[test]
        fn reformatted_plist_cannot_prove_ownership() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let path = agent_path(temp.path())?;
            let contents =
                plist(temp.path(), Path::new("/tmp/bin"))?.replace("<array>", "<array> ");
            atomic_plist(&path, &contents)?;
            let mut runner = MockRunner::default();
            assert!(disable_at(temp.path(), temp.path(), &mut runner).is_err());
            assert!(runner.calls.is_empty());
            assert_eq!(fs::read_to_string(path)?, contents);
            Ok(())
        }

        #[test]
        fn ownership_allows_original_executable_to_move_and_escapes_state() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let state = temp.path().join("state & old <name>");
            let path = agent_path(temp.path())?;
            atomic_plist(&path, &plist(&state, Path::new("/tmp/old & <binary>\"'"))?)?;
            assert!(verify_owned_plist(&path, &state)?);
            Ok(())
        }

        #[test]
        fn old_search_path_is_owned_even_when_current_environment_differs() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let path = agent_path(temp.path())?;
            let saved = plist_with_path(
                temp.path(),
                Path::new("/tmp/bin"),
                "/old/node & <version>/bin:/usr/bin",
            )?;
            assert!(saved.contains("/old/node &amp; &lt;version&gt;/bin:/usr/bin"));
            atomic_plist(&path, &saved)?;
            assert!(verify_owned_plist(&path, temp.path())?);
            let mut runner = runner(vec![result(0, "501", ""), result(0, "", "")]);
            disable_at(temp.path(), temp.path(), &mut runner)?;
            assert!(!path.exists());
            Ok(())
        }

        #[test]
        fn registered_job_without_plist_cannot_be_taken_over() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let mut runner = runner(vec![result(0, "501", ""), result(0, "registered", "")]);
            assert!(
                install_at(
                    temp.path(),
                    temp.path(),
                    &std::env::current_exe()?,
                    &mut runner
                )
                .is_err()
            );
            assert_eq!(runner.calls.len(), 2);
            assert!(runner.calls.iter().all(|(_, args)| args[0] != "bootout"));
            assert!(!agent_path(temp.path())?.exists());
            Ok(())
        }

        #[test]
        fn scheduler_lock_serializes_different_policy_directories() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let path = agent_path(temp.path())?;
            let held = scheduler_lock(&path)?;
            assert!(scheduler_lock(&path).is_err());
            drop(held);
            let _reacquired = scheduler_lock(&path)?;
            Ok(())
        }

        #[test]
        fn failed_bootstrap_removes_future_login_configuration() -> Result<()> {
            let temp = tempfile::tempdir()?;
            let executable = std::env::current_exe()?;
            let mut runner = runner(vec![
                result(0, "501", ""),
                missing(),
                result(5, "", "bootstrap refused"),
                result(3, "", "No such process"),
                missing(),
            ]);
            assert!(install_at(temp.path(), temp.path(), &executable, &mut runner).is_err());
            assert!(!agent_path(temp.path())?.exists());
            Ok(())
        }

        #[test]
        fn bounded_capture_keeps_prefix_and_drains_remainder() -> Result<()> {
            let input = vec![b'a'; CAPTURE_LIMIT * 4];
            assert_eq!(capture(input.as_slice())?.len(), CAPTURE_LIMIT);
            Ok(())
        }

        #[test]
        fn signal_termination_retains_status_and_actionable_diagnostic() -> Result<()> {
            let outcome = run_bounded(
                "/bin/sh",
                &["-c".into(), "kill -TERM $$".into()],
                Duration::from_secs(2),
            )?;
            assert_eq!(outcome.status.signal(), Some(15));
            let error = outcome.checked("synthetic child").unwrap_err().to_string();
            assert!(error.contains("signal"), "{error}");
            assert!(error.contains("15"), "{error}");
            assert!(!error.contains("None"), "{error}");
            Ok(())
        }

        #[test]
        fn numeric_exit_diagnostic_is_preserved() {
            let error = result(5, "", "refused\n")
                .checked("synthetic child")
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "synthetic child failed (exit Some(5)): refused"
            );
        }

        #[test]
        #[cfg(unix)]
        fn timed_out_child_is_terminated_and_reaped() -> Result<()> {
            let started = Instant::now();
            let outcome = run_bounded("/bin/sleep", &["5".into()], Duration::from_millis(30));
            assert!(format!("{:#}", outcome.unwrap_err()).contains("timed out"));
            assert!(started.elapsed() < Duration::from_secs(2));
            Ok(())
        }

        #[test]
        fn uid_cannot_inject_a_launchctl_target() -> Result<()> {
            let mut runner = runner(vec![result(0, "501/another", "")]);
            assert!(domain(&mut runner).is_err());
            assert_eq!(runner.calls.len(), 1);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plist_is_periodic_quiet_and_escapes_each_path() -> Result<()> {
        let rendered = plist(Path::new("/tmp/a&b<c>\"'"), Path::new("/tmp/my executable"))?;
        assert!(rendered.contains("/tmp/a&amp;b&lt;c&gt;&quot;&apos;"));
        assert!(rendered.contains("<string>/tmp/my executable</string>"));
        assert!(rendered.contains("<key>StartInterval</key><integer>3600</integer>"));
        assert!(rendered.contains("<key>RunAtLoad</key><false/>"));
        // Throttled startup can exceed the Codex probe deadline and causes
        // priority inversion while a cleaner holds shared storage locks.
        assert!(!rendered.contains("<key>LowPriorityIO</key>"));
        assert!(!rendered.contains("<key>ProcessType</key>"));
        assert!(!rendered.contains("KeepAlive"));
        assert_eq!(rendered.matches("<string>/dev/null</string>").count(), 2);
        Ok(())
    }

    #[test]
    fn rejects_relative_and_invalid_xml_paths() {
        assert!(plist(Path::new("relative"), Path::new("/tmp/bin")).is_err());
        assert!(plist(Path::new("/tmp/state"), Path::new("relative")).is_err());
        assert!(plist(Path::new("/tmp/state\0"), Path::new("/tmp/bin")).is_err());
    }
}
