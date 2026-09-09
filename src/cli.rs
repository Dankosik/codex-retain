use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Predictable retention for local archived Codex chats",
    long_about = "Keep archived local Codex chats for a chosen number of days. Start with enable --days 30 --yes. Every existing archive receives a full grace period. Only reviewed Codex versions are supported."
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
    /// Enable retention with a fresh grace period for the existing archive
    Enable {
        #[arg(long,default_value_t=30,value_parser=clap::value_parser!(u32).range(1..=36500))]
        days: u32,
        /// Profile to manage (default: CODEX_HOME or ~/.codex)
        #[arg(long)]
        codex_home: Option<PathBuf>,
        /// Executable used by the supported Codex client
        #[arg(long, default_value = "codex")]
        codex_bin: PathBuf,
        /// Enable manual runs without installing the macOS hourly LaunchAgent
        #[arg(long)]
        no_schedule: bool,
        /// Consent to permanent local deletion after retention; no later prompts
        #[arg(long)]
        yes: bool,
    },
    /// Show the current policy, compatibility, scheduler, and last run
    Status,
    /// Explain due and skipped archives without changing Codex data
    Preview,
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
