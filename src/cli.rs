use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Predictable retention for local archived Codex chats",
    long_about = "Keep archived local Codex chats for a chosen number of days. Start with enable --days 30 --yes. Enable immediately cleans eligible existing archives using Codex's recorded archive dates. Only reviewed Codex versions are supported."
)]
pub struct Cli {
    /// Directory containing this utility's policy and bounded last-run report
    #[arg(long, global = true, env = "CODEX_RETAIN_STATE_DIR")]
    pub state_dir: Option<PathBuf>,
    /// Emit versioned machine-readable JSON
    #[arg(long, global = true)]
    pub json: bool,
    #[command(subcommand)]
    pub command: Action,
}

#[derive(Debug, Subcommand)]
pub enum Action {
    /// Enable retention and immediately clean eligible existing archives
    Enable(EnableArgs),
    /// Show the current policy, compatibility, scheduler, and last run
    Status,
    /// Explain due and skipped archives without changing Codex data
    Preview {
        /// Forecast retention at a future RFC3339 timestamp using the current snapshot
        #[arg(long, value_name = "RFC3339")]
        at: Option<jiff::Timestamp>,
    },
    /// Perform one cleanup under the enabled policy
    Run {
        /// Quiet scheduler entry point; still saves the bounded last-run result
        #[arg(long, hide = true)]
        scheduled: bool,
    },
    /// Suspend deletion while preserving transition capture and scheduling
    Pause,
    /// Resume the existing policy (time in the archive continues during a pause)
    Resume,
    /// Change retention; shortening can make existing archives immediately due
    Policy {
        #[arg(long,value_parser=clap::value_parser!(u32).range(1..=36500))]
        days: u32,
        /// Acknowledge the effect of a shorter retention period
        #[arg(long)]
        yes: bool,
    },
    /// Permanently protect a thread ID from this policy
    Exclude { id: String },
    /// Remove an explicit protection; the original archive period still applies
    Include {
        id: String,
        #[arg(long)]
        yes: bool,
    },
    /// Disable deletion, unregister the LaunchAgent, and remove transition capture
    Disable,
    /// Disable safely before removing the executable with your package manager
    Uninstall,
    /// Inspect a Codex profile without enabling or modifying it
    Doctor {
        #[arg(long)]
        codex_home: Option<PathBuf>,
        #[arg(long, default_value = "codex")]
        codex_bin: PathBuf,
    },
    /// Generate shell completions
    Completions { shell: clap_complete::Shell },
}

#[derive(Debug, Args)]
pub struct EnableArgs {
    #[arg(long,default_value_t=30,value_parser=clap::value_parser!(u32).range(1..=36500))]
    pub days: u32,
    /// Profile to manage (default: CODEX_HOME or ~/.codex)
    #[arg(long)]
    pub codex_home: Option<PathBuf>,
    /// Executable used by the supported Codex client
    #[arg(long, default_value = "codex")]
    pub codex_bin: PathBuf,
    /// Perform initial cleanup and enable manual runs without the hourly LaunchAgent
    #[arg(long)]
    pub no_schedule: bool,
    /// Consent to immediate and future permanent deletion of eligible local archives
    #[arg(long)]
    pub yes: bool,
}
