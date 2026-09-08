use std::{
    fs,
    io::{self, Read, Write},
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_rust-cli-template"));
    command
        .env_remove("RUST_CLI_TEMPLATE_CONFIG")
        .env_remove("RUST_CLI_TEMPLATE_FORMAT")
        .env("NO_COLOR", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

// Every process is bounded, stopped, and reaped, including on assertion failure.
struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn spawn(command: &mut Command) -> Self {
        Self(Some(command.spawn().expect("spawn test binary")))
    }

    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("live child")
    }

    fn finish(self) -> Output {
        thread::scope(move |scope| {
            // This guard is local to the closure. On a timeout or panic it kills
            // and reaps the binary BEFORE the scope joins its reader threads.
            // The template starts no descendants that could retain pipe handles.
            let mut guard = self;
            drop(guard.child().stdin.take());
            let stdout = guard
                .child()
                .stdout
                .take()
                .map(|reader| scope.spawn(move || capture(reader)));
            let stderr = guard
                .child()
                .stderr
                .take()
                .map(|reader| scope.spawn(move || capture(reader)));
            let deadline = Instant::now() + Duration::from_secs(10);
            while guard.child().try_wait().expect("query child").is_none() {
                assert!(Instant::now() < deadline, "test binary did not terminate");
                thread::sleep(Duration::from_millis(10));
            }
            let status = guard.child().wait().expect("reap child");
            let stdout = stdout
                .map(|reader| {
                    reader
                        .join()
                        .expect("stdout reader panicked")
                        .expect("capture stdout")
                })
                .unwrap_or_default();
            let stderr = stderr
                .map(|reader| {
                    reader
                        .join()
                        .expect("stderr reader panicked")
                        .expect("capture stderr")
                })
                .unwrap_or_default();
            drop(guard.0.take());
            Output {
                status,
                stdout,
                stderr,
            }
        })
    }
}

fn capture(reader: impl Read) -> io::Result<Vec<u8>> {
    const MAX_CAPTURE_BYTES: u64 = 256 * 1024;
    let mut bytes = Vec::new();
    reader.take(MAX_CAPTURE_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_CAPTURE_BYTES {
        return Err(io::Error::other(
            "test output exceeded the 256 KiB capture limit",
        ));
    }
    Ok(bytes)
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(child) = &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn run(command: &mut Command, stdin: &[u8]) -> Output {
    // Larger fixtures use files: don't make the harness's synchronous stdin
    // write itself block behind a faulty child that fails to consume its input.
    assert!(stdin.len() <= 1024);
    let mut child = ChildGuard::spawn(command);
    child
        .child()
        .stdin
        .take()
        .unwrap()
        .write_all(stdin)
        .unwrap();
    child.finish()
}

fn successful(output: &Output, expected: &[u8]) {
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, expected);
    assert_eq!(output.stderr, b"");
}

#[test]
fn stdin_json_preserves_binary_bytes_and_unterminated_tail() {
    let output = run(
        command().args(["stats", "--format", "json"]),
        b"a\r\n\xff\0tail",
    );
    successful(&output, b"{\"bytes\":9,\"lines\":1}\n");
}

#[test]
fn explicit_stdin_and_empty_input() {
    let output = run(command().args(["stats", "-"]), b"");
    successful(&output, b"bytes: 0\nlines: 0\n");
}

#[test]
fn a_large_file_does_not_need_newline_terminated_records() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("input.bin");
    let mut bytes = vec![b'x'; 4 * 64 * 1024 + 3];
    bytes[64 * 1024] = b'\n';
    fs::write(&path, bytes).unwrap();
    let output = run(command().args(["--format", "json", "stats"]).arg(path), b"");
    successful(&output, b"{\"bytes\":262147,\"lines\":1}\n");
}

#[test]
fn format_precedence_is_flag_environment_config_default() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "format = 'json'\n").unwrap();

    successful(
        &run(command().arg("stats"), b"x\n"),
        b"bytes: 2\nlines: 1\n",
    );
    successful(
        &run(command().arg("--config").arg(&path).arg("stats"), b"x\n"),
        b"{\"bytes\":2,\"lines\":1}\n",
    );
    successful(
        &run(
            command()
                .env("RUST_CLI_TEMPLATE_CONFIG", &path)
                .env("RUST_CLI_TEMPLATE_FORMAT", "text")
                .arg("stats"),
            b"x\n",
        ),
        b"bytes: 2\nlines: 1\n",
    );
    successful(
        &run(
            command()
                .env("RUST_CLI_TEMPLATE_CONFIG", &path)
                .env("RUST_CLI_TEMPLATE_FORMAT", "json")
                .args(["stats", "--format", "text"]),
            b"x\n",
        ),
        b"bytes: 2\nlines: 1\n",
    );
}

#[test]
fn config_flag_overrides_environment_path() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("valid.toml");
    fs::write(&path, "format = 'json'\n").unwrap();
    let output = run(
        command()
            .env(
                "RUST_CLI_TEMPLATE_CONFIG",
                directory.path().join("absent.toml"),
            )
            .arg("--config")
            .arg(path)
            .arg("stats"),
        b"\n",
    );
    successful(&output, b"{\"bytes\":1,\"lines\":1}\n");
}

#[test]
fn no_config_is_discovered_from_working_directory() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("config.toml"), "broken = [").unwrap();
    let output = run(command().current_dir(directory.path()).arg("stats"), b"x");
    successful(&output, b"bytes: 1\nlines: 0\n");
}

#[test]
fn explicit_bad_configs_fail_without_summary_even_with_format_override() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    for config in ["format = [", "format = 'xml'", "unknown = true"] {
        fs::write(&path, config).unwrap();
        let output = run(
            command()
                .arg("--config")
                .arg(&path)
                .args(["stats", "--format", "text"]),
            b"",
        );
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8(output.stderr)
                .unwrap()
                .contains("invalid config")
        );
    }
}

#[test]
fn oversized_config_is_rejected() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, vec![b' '; 64 * 1024 + 1]).unwrap();
    let output = run(command().arg("--config").arg(path).arg("stats"), b"");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("65536-byte limit")
    );
}

#[test]
fn missing_input_and_config_are_operation_failures() {
    let directory = tempfile::tempdir().unwrap();
    let missing = directory.path().join("absent");
    for args in [vec!["stats"], vec!["--config"]] {
        let mut cmd = command();
        cmd.args(&args).arg(&missing);
        if args[0] == "--config" {
            cmd.arg("stats");
        }
        let output = run(&mut cmd, b"");
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn invalid_arguments_and_environment_have_usage_status() {
    for args in [
        vec!["stats", "--unknown"],
        vec!["stats", "--format", "yaml"],
    ] {
        let output = run(command().args(args), b"");
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
    let output = run(
        command()
            .env("RUST_CLI_TEMPLATE_FORMAT", "yaml")
            .arg("stats"),
        b"",
    );
    assert_eq!(output.status.code(), Some(2), "{output:?}");
}

#[test]
fn parser_diagnostics_escape_untrusted_controls_even_with_forced_color() {
    let invalid = "\u{1b}[2J\nFORGED";
    for source in ["flag", "environment", "unknown-argument"] {
        let mut cmd = command();
        cmd.env_remove("NO_COLOR").env("CLICOLOR_FORCE", "1");
        match source {
            "flag" => {
                cmd.args(["stats", "--format", invalid]);
            }
            "environment" => {
                cmd.env("RUST_CLI_TEMPLATE_FORMAT", invalid).arg("stats");
            }
            "unknown-argument" => {
                cmd.arg("stats").arg(format!("--bad{invalid}"));
            }
            _ => unreachable!(),
        }
        let output = run(&mut cmd, b"");
        assert_eq!(output.status.code(), Some(2), "{source}: {output:?}");
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!stderr.contains("\u{1b}[2J"), "{source}: {stderr:?}");
        assert!(!stderr.contains("\nFORGED"), "{source}: {stderr:?}");
        assert!(
            stderr.contains("\\u{1b}[2J\\nFORGED"),
            "{source}: {stderr:?}"
        );
        assert!(stderr.contains("error:"), "{source}: {stderr:?}");
        assert!(stderr.contains("--help"), "{source}: {stderr:?}");
    }
}

#[test]
fn help_keeps_trusted_layout_and_hides_untrusted_environment_values() {
    let output = run(
        command()
            .env_remove("NO_COLOR")
            .env("CLICOLOR_FORCE", "1")
            .env("RUST_CLI_TEMPLATE_FORMAT", "\u{1b}[2J\nFORGED_FORMAT")
            .env("RUST_CLI_TEMPLATE_CONFIG", "\u{1b}[2J\nFORGED_CONFIG")
            .arg("--help"),
        b"",
    );
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains('\n'));
    assert!(stdout.contains("RUST_CLI_TEMPLATE_FORMAT"));
    assert!(stdout.contains("RUST_CLI_TEMPLATE_CONFIG"));
    assert!(!stdout.contains("\u{1b}[2J"));
    assert!(!stdout.contains("FORGED_FORMAT"));
    assert!(!stdout.contains("FORGED_CONFIG"));
    assert!(!stdout.contains("\\u{1b}"));
}

#[test]
fn completion_output_is_drained_while_the_process_runs() {
    let output = run(command().args(["completions", "bash"]), b"");
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
    // This exceeds Windows' typical anonymous-pipe capacity, so waiting for
    // process exit before draining stdout would deadlock on the Windows CI job.
    assert!(
        output.stdout.len() > 4096,
        "completion fixture is too small"
    );
}

#[test]
fn informational_commands_do_not_open_config_or_read_stdin() {
    let directory = tempfile::tempdir().unwrap();
    for args in [
        vec!["--help"],
        vec!["--version"],
        vec!["stats", "--help"],
        vec!["completions", "bash"],
    ] {
        let mut child = ChildGuard::spawn(
            command()
                .env("RUST_CLI_TEMPLATE_CONFIG", directory.path().join("absent"))
                .args(args),
        );
        // Keep the pipe's writer alive through child termination. Reading stdin
        // would block, so this establishes that informational commands bypass it.
        let stdin = child.child().stdin.take().unwrap();
        let output = child.finish();
        drop(stdin);
        assert!(output.status.success(), "{output:?}");
        assert!(!output.stdout.is_empty());
        assert!(output.stderr.is_empty());
    }
}

#[test]
fn dash_prefixed_file_works_after_end_of_options() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("-input"), b"yes\n").unwrap();
    let output = run(
        command()
            .current_dir(directory.path())
            .args(["stats", "--", "-input"]),
        b"",
    );
    successful(&output, b"bytes: 4\nlines: 1\n");
}

#[test]
fn closed_stdout_is_a_quiet_success() {
    let mut child = ChildGuard::spawn(command().args(["stats", "--format", "json"]));
    // No output can be produced before EOF; close the real pipe's read end first.
    drop(child.child().stdout.take());
    let output = child.finish();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty());
}

// Linux filesystems used by CI accept arbitrary filename bytes. APFS may reject
// invalid UTF-8 at file creation, so Unix-wide parser coverage lives in cli.rs.
#[cfg(target_os = "linux")]
#[test]
fn non_utf8_input_path_is_preserved() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join(OsString::from_vec(b"input-\xff".to_vec()));
    fs::write(&path, b"abc\n").unwrap();
    let output = run(command().arg("stats").arg(path), b"");
    successful(&output, b"bytes: 4\nlines: 1\n");
}

#[cfg(unix)]
#[test]
fn nonexistent_non_utf8_path_is_an_operation_error_not_a_parser_error() {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};

    let directory = tempfile::tempdir().unwrap();
    let path = directory
        .path()
        .join(OsString::from_vec(b"absent-\xff".to_vec()));
    let output = run(command().arg("stats").arg(&path), b"");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("cannot read input"), "{stderr}");
    assert!(stderr.contains(&format!("{path:?}")), "{stderr}");
}

#[test]
fn config_diagnostics_do_not_emit_terminal_control_sequences() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("config.toml");
    fs::write(&path, "format = \"\\u001b[2J\"\n").unwrap();
    let output = run(command().arg("--config").arg(path).arg("stats"), b"");
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(output.stdout.is_empty());
    assert!(!output.stderr.contains(&0x1b));
    assert_eq!(
        output.stderr.iter().filter(|&&byte| byte == b'\n').count(),
        1
    );
}

#[cfg(target_os = "linux")]
#[test]
fn stdout_device_failure_is_not_a_clean_pipeline_end() {
    let output = run(
        command().arg("stats").stdout(
            fs::OpenOptions::new()
                .write(true)
                .open("/dev/full")
                .unwrap(),
        ),
        b"a\n",
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("cannot write standard output")
    );
}
