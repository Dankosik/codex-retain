//! CLI composition and a small reusable streaming core.

mod cli;
mod config;
mod error;
mod output;
pub mod stats;

use std::{fs::File, io, io::Write, path::Path, process::ExitCode};

use clap::{CommandFactory, Parser};

use crate::{cli::Cli, error::AppError};

/// Parse process arguments, perform the requested command, and report once.
pub fn run() -> ExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => return report_parser_error(error),
    };
    match execute(cli, &mut io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) if error.is_stdout_closed() => ExitCode::SUCCESS,
        Err(error) => {
            let mut stderr = io::stderr().lock();
            // Reporting failure cannot turn an already failed operation into success.
            let _ = writeln!(stderr, "error: {}", error::diagnostic(&error));
            let _ = stderr.flush();
            ExitCode::FAILURE
        }
    }
}

fn execute(cli: Cli, stdout: &mut impl Write) -> Result<(), AppError> {
    match cli.command {
        cli::Command::Completions { shell } => {
            let mut command = Cli::command();
            let name = command.get_name().to_owned();
            // The generator's writer API is infallible. Generate this bounded,
            // known command definition in memory, then handle stdout ourselves.
            let mut bytes = Vec::new();
            clap_complete::generate(shell, &mut command, name, &mut bytes);
            output::write_bytes(stdout, &bytes)
        }
        cli::Command::Stats { input } => {
            let format = config::resolve_format(cli.format, cli.config.as_deref())?;
            let stats = if input == Path::new("-") {
                stats::count(&mut io::stdin().lock()).map_err(AppError::Stdin)?
            } else {
                let read_error = |source| AppError::Input {
                    path: input.clone(),
                    source,
                };
                let mut file = File::open(&input).map_err(read_error)?;
                stats::count(&mut file).map_err(read_error)?
            };
            output::summary(stdout, &stats, format)
        }
    }
}

fn report_parser_error(mut error: clap::Error) -> ExitCode {
    let to_stderr = error.use_stderr();
    if to_stderr {
        use clap::error::{ContextKind, ContextValue};

        // Escape user-provided fragments before clap adds trusted styling and
        // layout. Sanitizing the finished message would also damage its help.
        let replacements: Vec<_> = error
            .context()
            .filter_map(|(kind, value)| {
                let sanitized = match value {
                    ContextValue::String(value) if value.chars().any(char::is_control) => {
                        ContextValue::String(error::diagnostic(value))
                    }
                    ContextValue::Strings(values)
                        if values
                            .iter()
                            .any(|value| value.chars().any(char::is_control)) =>
                    {
                        ContextValue::Strings(values.iter().map(error::diagnostic).collect())
                    }
                    _ => return None,
                };
                Some((kind, sanitized))
            })
            .collect();
        if !replacements.is_empty() {
            // clap's preformatted suggestions can repeat the original fragment.
            // Keep the escaped error and usage, without that unsafe duplicate.
            error.remove(ContextKind::Suggested);
            for (kind, value) in replacements {
                error.insert(kind, value);
            }
        }
    }
    let result = error.print().and_then(|()| {
        if to_stderr {
            io::stderr().lock().flush()
        } else {
            io::stdout().lock().flush()
        }
    });
    match result {
        Ok(()) => ExitCode::from(error.exit_code() as u8),
        Err(error) if !to_stderr && error.kind() == io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
