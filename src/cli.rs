use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum, ValueHint};
use clap_complete::Shell;
use serde::Deserialize;

#[derive(Debug, Parser)]
#[command(version, about, arg_required_else_help = true)]
pub(crate) struct Cli {
    /// Output format (precedence: flag, environment, config, text)
    #[arg(
        long,
        global = true,
        env = "RUST_CLI_TEMPLATE_FORMAT",
        hide_env_values = true,
        value_enum
    )]
    pub format: Option<Format>,

    /// Read this TOML configuration file; no files are loaded automatically
    #[arg(long, global = true, env = "RUST_CLI_TEMPLATE_CONFIG", hide_env_values = true, value_hint = ValueHint::FilePath)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Count bytes and LF newline terminators, using bounded memory
    #[command(
        long_about = "Count bytes and LF (0x0A) newline terminators, like wc -l.\nA final unterminated fragment adds bytes but no line. Input need not be UTF-8."
    )]
    Stats {
        /// Input file, or - for standard input
        #[arg(default_value = "-", value_hint = ValueHint::FilePath)]
        input: PathBuf,
    },
    /// Write a shell completion script to stdout
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, ValueEnum)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Format {
    #[default]
    Text,
    Json,
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::Cli;

    #[test]
    fn command_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[cfg(unix)]
    #[test]
    fn parser_preserves_non_utf8_paths_without_filesystem_assumptions() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        use clap::Parser;

        use super::Command;

        let path = OsString::from_vec(b"input-\xff".to_vec());
        let cli = Cli::try_parse_from([
            OsString::from("rust-cli-template"),
            OsString::from("--format"),
            OsString::from("text"),
            OsString::from("--config"),
            path.clone(),
            OsString::from("stats"),
            path.clone(),
        ])
        .unwrap();
        assert_eq!(cli.config.unwrap().into_os_string(), path);
        match cli.command {
            Command::Stats { input } => assert_eq!(input.into_os_string(), path),
            Command::Completions { .. } => panic!("expected stats command"),
        }
    }
}
