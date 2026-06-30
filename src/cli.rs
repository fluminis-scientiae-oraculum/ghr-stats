//! Command-line surface. Pure clap types — no logic lives here.
//!
//! Five orthogonal verbs (default → TUI, `serve`, `config`, `db`, `systemd`).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Top-level CLI. With no subcommand, launches the TUI.
#[derive(Parser, Debug)]
#[command(
    name = "ghr-stats",
    version,
    about = "TUI dashboard + Prometheus exporter for self-hosted GitHub Actions runners",
    long_about = "ghr-stats monitors a fleet of self-hosted GitHub Actions runners: a \
                  mouse-driven TUI over a local SQLite history, plus a `serve` daemon that \
                  samples the fleet and exposes Prometheus metrics. Runner identity comes \
                  from each runner's own .runner file — no host assumptions.",
    styles = help_styles(),
)]
pub struct Cli {
    /// Path to a config file (overrides the default search paths).
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch the interactive TUI dashboard (this is the default).
    #[command(hide = true)]
    Tui,

    /// Sample the fleet into SQLite and expose metrics. Runs as a systemd service.
    Serve,

    /// Consent-first interactive configuration wizard (orgs, PATs, hooks).
    Config,

    /// Manage the ghr-stats systemd service.
    Systemd {
        #[command(subcommand)]
        action: SystemdAction,
    },

    /// Database maintenance.
    Db {
        #[command(subcommand)]
        action: DbAction,
    },
}

#[derive(Subcommand, Debug)]
pub enum SystemdAction {
    /// Install + enable the service, copying the binary to a stable system path.
    Install {
        /// System-wide service under /etc + /var/lib (needs root).
        #[arg(long, conflicts_with = "user")]
        system: bool,
        /// Per-user service under the XDG base dirs.
        #[arg(long, conflicts_with = "system")]
        user: bool,
    },

    /// Disable + remove the service (leaves data in place).
    Uninstall,
}

#[derive(Subcommand, Debug)]
pub enum DbAction {
    /// Prune samples older than the retention window. (Opening the store
    /// already migrates it, so there is no `init`.)
    Prune {
        /// Keep samples newer than this many days.
        #[arg(long, default_value_t = 14)]
        days: u64,
    },
}

/// Colored help styling: green headers/usage, cyan literals/placeholders.
fn help_styles() -> clap::builder::Styles {
    use clap::builder::styling::AnsiColor;
    clap::builder::Styles::styled()
        .header(AnsiColor::Green.on_default().bold())
        .usage(AnsiColor::Green.on_default().bold())
        .literal(AnsiColor::Cyan.on_default().bold())
        .placeholder(AnsiColor::Cyan.on_default())
}
