use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ghr-stats",
    version,
    about = "TUI dashboard for self-hosted GitHub Actions runners"
)]
pub struct Cli {
    /// Path to a config file (overrides the default search paths).
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Run the collector daemon: sample sources into SQLite. Intended as a systemd service.
    Collect,

    /// Launch the interactive TUI dashboard.
    Tui,

    /// Interactive, consent-first first-run configuration wizard.
    Setup,

    /// Validate the configured GitHub PAT(s) and list each org's runners.
    Github,

    /// Database maintenance.
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum DbAction {
    /// Create the database (if missing) and apply migrations.
    Init,

    /// Prune samples older than the retention window.
    Prune {
        /// Keep samples newer than this many days.
        #[arg(long, default_value_t = 14)]
        days: u64,
    },
}
