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

/// Filters + output shape for `ghr-stats status`.
#[derive(clap::Args, Debug)]
pub struct StatusArgs {
    /// Emit JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
    /// Only this org.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
    /// Only this runner (by name).
    #[arg(long, value_name = "NAME")]
    pub runner: Option<String>,
}

/// Output shape for `ghr-stats explain`.
#[derive(clap::Args, Debug)]
pub struct ExplainArgs {
    /// Emit JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

/// Window, filters and output shape for `ghr-stats timeline`.
#[derive(clap::Args, Debug)]
pub struct TimelineArgs {
    /// How far back to look: 90s, 30m, 6h, 2d. Capped at 7d.
    #[arg(long, value_name = "DURATION", default_value = "6h")]
    pub since: String,
    /// Maximum rows per section (transitions, and samples if requested).
    #[arg(long, value_name = "N", default_value_t = 500)]
    pub limit: usize,
    /// Only this org.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,
    /// Only this runner (by name).
    #[arg(long, value_name = "NAME")]
    pub runner: Option<String>,
    /// Also include the raw per-tick samples behind the transitions.
    #[arg(long)]
    pub samples: bool,
    /// Emit JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
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

    /// One-shot fleet status for scripts and agents (exit code = verdict).
    #[command(
        long_about = "Print the fleet's current state and an overall verdict, then exit with a         code that encodes it: 0 healthy, 1 degraded (a runner is offline, diverging from         GitHub's view, or its GitHub reading is stale), 2 cannot determine (no collector and no         readable runner root), 3 usage error. With --json the payload is machine-stable: no         colour, no localised time, both ISO-8601 and epoch, and a schema_version. Reads the         collector over its socket when one is running; otherwise falls back to a live local         scan, in which case the github_* fields are null rather than invented."
    )]
    Status(StatusArgs),

    /// Why the fleet is degraded — findings with the boundary to investigate.
    #[command(
        long_about = "Turn the fleet's current state into findings rather than numbers. Each \
        finding carries a claim and a `boundary` — local, github, network or config — naming which \
        side to investigate, which is the expensive half of diagnosing a fleet fault and the half \
        this tool is uniquely placed to shortcut: it holds the local process truth and GitHub's \
        opinion at the same instant. Exits with the same code as `status`: 0 healthy, 1 degraded, \
        2 cannot determine, 3 usage error. Without a collector the GitHub-side findings cannot be \
        assessed, and that limit is reported as a finding rather than left as silence."
    )]
    Explain(ExplainArgs),

    /// What changed over a window — edges only, so causality is readable.
    #[command(
        long_about = "Replay a window as the things that CHANGED in it: local liveness edges, \
        GitHub-online edges, and the per-org reconcile going bad or recovering. Reading those \
        three together is what separates \"GitHub says these runners are gone\" from \"we stopped \
        being able to ask\" — the distinction a raw sample dump buries under hundreds of \
        identical rows.\n\n\
        Output is bounded by construction: --since is capped at 7d, --limit applies to each \
        section, and when a section was cut the payload says so rather than looking complete. Raw \
        per-tick samples are omitted unless you ask for --samples; a window reaching past what \
        `db prune` has left reports truncated_at instead of a silently short series.\n\n\
        History lives only in the collector, so unlike `status` and `explain` this verb has no \
        local fallback: with no collector it exits 2 (cannot determine) and says why. Exits 0 \
        when the window was answered, 3 on a usage error."
    )]
    Timeline(TimelineArgs),

    /// Interactive first-run setup (run with sudo): runner root, per-org PATs,
    /// metrics, and hooks. Writes the system config at /etc.
    #[command(
        long_about = "Consent-first interactive configuration — run with sudo. Four steps — \
        discover runners under a root you choose, add read-only fine-grained PATs per org \
        (validated before saving; each needs Organization → Self-hosted runners: Read, plus \
        Repository → Actions: Read if you want job success/failure filled in the Jobs view), \
        optionally enable Prometheus metrics, and write the \
        root-owned 0600 system config at /etc/ghr-stats/config.toml (the collector's single \
        source of truth) — then offers to install/repair each runner's job hooks, detect-first \
        and never clobbering a foreign hook (it chains after it or prints a snippet instead). \
        Writing the system config and editing runner .env files both need root, hence sudo. \
        The same settings can be changed live from the TUI's Config tab ([a]/[h]/[m]/[o]) when \
        the dashboard is run as `sudo ghr-stats` — or, for [a]/[m], as an ordinary user in the \
        `ghr-stats` group, which lets the root collector apply the edit over its socket without \
        sudo (see `systemd install`)."
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
    #[command(
        long_about = "Copy the running binary to a stable absolute path, render + enable the \
        `serve` unit, and start it. A system install (root) also provisions the `ghr-stats` \
        group and adds the invoking operator ($SUDO_USER) to it: members (and root) may edit the \
        root-owned system config from a non-root TUI — the collector applies [a]/[m] edits over \
        its socket, authorized by the peer's kernel-reported uid. Membership takes effect \
        immediately (no re-login); add more operators with `sudo usermod -aG ghr-stats <user>`."
    )]
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
