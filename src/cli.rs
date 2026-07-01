//! Command-line surface. Pure clap types — no logic lives here.
//!
//! Verbs: default → TUI, `serve` (the systemd-managed collector — not for
//! interactive use), `config`, `db`, `systemd`, `uninstall`.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

/// Top-level CLI. With no subcommand, launches the TUI.
#[derive(Parser, Debug)]
#[command(
    name = "ghr-stats",
    version,
    about = "Live TUI + collector service (history, jobs, Prometheus) for self-hosted GitHub Actions runner fleets",
    long_about = "ghr-stats monitors a fleet of self-hosted GitHub Actions runners. Run it \
                  with no arguments for the TUI: an Ephemeral live dashboard standalone, or — \
                  once the collector service is installed (`ghr-stats systemd install`) — a \
                  Persistent dashboard adding history, jobs, GitHub reconcile, and a Prometheus \
                  exporter. Runner identity comes from each runner's own .runner file — no host \
                  assumptions.",
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

    /// The background collector (systemd-managed). Not an interactive command.
    #[command(
        long_about = "The background collector and sole DB writer — installed and run by \
        systemd, NOT by hand: it refuses to start on a terminal (set GHR_STATS_ALLOW_TTY=1 to \
        override for dev/CI). It samples the fleet into SQLite so the TUI's Persistent mode has \
        history, jobs, and the GitHub reconcile, serves those to the TUI over a Unix socket, and \
        — when enabled in the config — exposes a Prometheus /metrics endpoint on loopback \
        (scrape into Prometheus/Grafana) and/or pushes metrics as JSON to an OpenObserve \
        endpoint. Install it with `ghr-stats systemd install`."
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

    /// Remove what ghr-stats installed — hooks, service, config, data, binary.
    #[command(
        long_about = "Reverse an install. With NO domain this prints a dry-run PLAN of \
        everything ghr-stats put on this host and removes nothing — a safe \"what's installed\" \
        preview. Name one or more domains (or `all`) to actually remove; you are asked to confirm \
        first unless --yes is given.\n\n\
        Domains: hooks · service · config · data · binary · all.\n\n\
        Hooks are reverted the way they were installed — detect-first, NEVER stranding a foreign \
        hook: a runner ghr-stats chained is restored to its original hook, a foreign or untouched \
        runner is left alone. Editing runner .env files needs root (same as install).\n\n\
        `config` deletes the file holding your GitHub PAT(s) (unlinked, not shredded — revoke the \
        token on GitHub to be sure). `all` also removes the SQLite history + event log. The \
        installed binary copy is removed; a `cargo install` build prints the `cargo uninstall` \
        command instead.\n\n\
        Examples:\n\
        \x20 ghr-stats uninstall                 # dry-run plan, removes nothing\n\
        \x20 ghr-stats uninstall hooks           # just revert the runner hooks\n\
        \x20 ghr-stats uninstall config data     # remove the PAT config + history\n\
        \x20 sudo ghr-stats uninstall all --yes  # everything, no prompt"
    )]
    Uninstall(UninstallArgs),
}

/// Which parts of an install to remove. No domain ⇒ dry-run plan of everything.
#[derive(Args, Debug)]
pub struct UninstallArgs {
    /// Domains to remove (space-separated). Omit for a dry-run plan of everything.
    #[arg(value_enum)]
    pub domains: Vec<UninstallDomain>,
    /// Execute without the interactive confirm (for scripts / headless).
    #[arg(long)]
    pub yes: bool,
    /// Force system scope (/etc, /var/lib, /usr/local/bin). Default: from euid.
    #[arg(long, conflicts_with = "user")]
    pub system: bool,
    /// Force user scope (XDG base dirs). Default: from euid.
    #[arg(long, conflicts_with = "system")]
    pub user: bool,
}

/// A removable install domain. `All` = every other domain at once.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum UninstallDomain {
    /// Runner job hooks (restore any chained foreign hook; needs root).
    Hooks,
    /// The systemd service unit.
    Service,
    /// The config file — holds your GitHub PAT(s).
    Config,
    /// The SQLite history database + event log.
    Data,
    /// The installed binary (or a `cargo uninstall` hint).
    Binary,
    /// Everything above.
    All,
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
