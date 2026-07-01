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

    /// Sample the fleet into SQLite and expose Prometheus metrics. Runs as a service.
    #[command(
        long_about = "The always-on daemon and sole DB writer: it samples the fleet into \
        SQLite so the TUI's history and trends accrue even while the dashboard is closed, and — \
        when enabled in the config — exposes a Prometheus /metrics endpoint on loopback (scrape \
        it into Prometheus/Grafana) and/or pushes the metrics as JSON to an OpenObserve endpoint. \
        Install it as a background service with `ghr-stats systemd install`."
    )]
    Serve,

    /// Interactive first-run setup: runner root, per-org PATs, metrics, and hooks.
    #[command(
        long_about = "Consent-first interactive configuration. Four steps — discover \
        runners under a root you choose, add read-only fine-grained PATs per org (validated \
        before saving), optionally enable Prometheus metrics, and write a 0600 config — then \
        offers to install/repair each runner's job hooks, detect-first and never clobbering a \
        foreign hook (it chains after it or prints a snippet instead). Installing hooks edits \
        root-owned runner .env files, so re-run this with sudo to reach that step. The same \
        settings can be changed live from the TUI's Config tab ([a]/[h]/[m]/[o])."
    )]
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
