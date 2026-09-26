//! Command-line surface: clap types only.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "ghr-stats",
    version,
    about = "Live TUI + collector service (history, jobs, Prometheus) for self-hosted GitHub Actions runner fleets",
    long_about = "ghr-stats monitors a fleet of self-hosted GitHub Actions runners. Run it \
                  with no arguments for the TUI: an Ephemeral live dashboard standalone, or, \
                  once the collector service is installed (`ghr-stats systemd install`), a \
                  Persistent dashboard adding history, jobs, GitHub reconcile, and a Prometheus \
                  exporter. Runner identity comes from each runner's own .runner file.",
    styles = help_styles(),
)]
pub struct Cli {
    /// Path to a config file (overrides the default search paths).
    #[arg(long, global = true, value_name = "FILE")]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

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

#[derive(clap::Args, Debug)]
pub struct ExplainArgs {
    /// Emit JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Debug)]
pub struct TailArgs {
    /// Only follow this org's transitions.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// Only follow this runner's transitions, by display name.
    #[arg(long, value_name = "NAME")]
    pub runner: Option<String>,

    /// Emit this many seconds of history before following.
    #[arg(long, value_name = "SECONDS", default_value_t = 0)]
    pub backfill: u64,
}

impl TailArgs {
    /// How far back the first poll reaches.
    pub fn since_secs(&self) -> u64 {
        self.backfill
    }
}

#[derive(clap::Args, Debug)]
#[command(group(clap::ArgGroup::new("predicate").required(true).args(["github_online"])))]
pub struct WaitArgs {
    /// Block until every runner in scope is online to GitHub.
    #[arg(long)]
    pub github_online: bool,

    /// Only wait on this org's runners.
    #[arg(long, value_name = "ORG")]
    pub org: Option<String>,

    /// Give up after this many seconds. `0` evaluates once and exits.
    #[arg(long, value_name = "SECONDS", default_value_t = 600)]
    pub timeout: u64,

    /// Emit the final snapshot as JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

#[derive(clap::Args, Debug)]
pub struct DoctorArgs {
    /// Emit JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,

    /// Skip PAT validation, the only check that calls GitHub. It is reported
    /// as skipped, which holds the verdict at 2 (cannot determine).
    #[arg(long)]
    pub offline: bool,
}

#[derive(clap::Args, Debug)]
pub struct TimelineArgs {
    /// How far back to look: 90s, 30m, 6h, 2d. Capped at 7d.
    #[arg(long, value_name = "DURATION", default_value = "6h")]
    pub since: String,
    /// Maximum rows per section (transitions, jobs, and samples if requested).
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
        long_about = "The background collector and sole DB writer, run by systemd, not by \
        hand: it refuses to start on a terminal (GHR_STATS_ALLOW_TTY=1 overrides for dev/CI). \
        It samples the fleet into SQLite, serves history, jobs and the GitHub reconcile to the \
        TUI over a Unix socket, and, when enabled in the config, exposes Prometheus /metrics on \
        loopback and/or pushes JSON metrics to an OpenObserve endpoint. Install it with \
        `ghr-stats systemd install`."
    )]
    Serve,

    /// One-shot fleet status for scripts and agents (exit code = verdict).
    #[command(
        long_about = "Print the fleet's current state and a verdict, then exit with a code \
        that encodes it: 0 healthy, 1 degraded (a runner is offline, diverging from GitHub's \
        view, or its GitHub reading is stale), 2 cannot determine (no collector and no readable \
        runner root), 3 usage error.\n\n\
        --json is machine-stable: no colour, no localised time, ISO-8601 and epoch timestamps, \
        and a schema_version. Reads the collector over its socket when one is running; \
        otherwise falls back to a live local scan, with the github_* fields null."
    )]
    Status(StatusArgs),

    /// Why the fleet is degraded — findings with the boundary to investigate.
    #[command(
        long_about = "Turn the fleet's current state into findings. Each finding carries a \
        claim and a `boundary` (local, github, network or config) naming which side to \
        investigate. Exits like `status`: 0 healthy, 1 degraded, 2 cannot determine, 3 usage \
        error. Without a collector the GitHub-side findings cannot be assessed, and that is \
        reported as a finding."
    )]
    Explain(ExplainArgs),

    /// What changed over a window, as edges.
    #[command(
        long_about = "Replay a window as the edges that changed in it: local liveness, \
        GitHub-online, per-org reconcile failing or recovering, and job starts and \
        completions.\n\n\
        --since is capped at 7d, --limit applies to each section, and a section that was cut \
        says so. Raw per-tick samples are included only with --samples; a window reaching past \
        what `db prune` kept reports truncated_at.\n\n\
        History lives only in the collector: with no collector this exits 2 (cannot \
        determine). Exits 0 when the window was answered, 3 on a usage error."
    )]
    Timeline(TimelineArgs),

    /// Preflight the install itself: config, PATs, hooks, socket, database.
    #[command(
        long_about = "Check what every other verb assumes: the config parses, each org's PAT \
        can still list its runners, the hooks are installed, the collector is reachable and is \
        the same build as this binary, and where the retained record starts.\n\n\
        A check that could not run is reported as skipped, never as passing, and holds the \
        verdict at 2 (cannot determine). The system config is root-owned, so a non-root run \
        cannot inspect PATs: re-run with sudo. Every failure names its next action.\n\n\
        --offline skips PAT validation, the only check that calls GitHub. Exits 0 when every \
        check passed, 1 when one failed, 2 when any was skipped, 3 on a usage error."
    )]
    Doctor(DoctorArgs),

    /// Block until the fleet reaches a state.
    #[command(
        long_about = "Block until every runner in scope is online to GitHub, then exit 0.\n\n\
        Exits 2 (cannot determine), not 1, when the timeout hits while the GitHub view was \
        unreadable, when the filter matches no runners, and immediately when there is no \
        collector. Polls at the local sampling interval. Progress goes to stderr, only when it \
        changes; the final snapshot goes to stdout.\n\n\
        Exits 0 when the predicate held, 1 on a genuine timeout, 2 when it could not be \
        determined, 3 on a usage error."
    )]
    Wait(WaitArgs),

    /// Follow the fleet's transitions as they happen — NDJSON, one per line.
    #[command(
        long_about = "Print each state change as one JSON object on its own line, flushed \
        immediately: liveness edges, GitHub-online edges, per-org reconcile outcomes, and job \
        starts and completions.\n\n\
        Polls the collector. If more transitions occurred than one poll could carry, a \
        {\"type\":\"gap\"} line names the section and window it could not cover.\n\n\
        Starts from now; --backfill SECONDS replays a window first. Runs until stopped (exit \
        0); exits 2 immediately if there is no collector."
    )]
    Tail(TailArgs),

    /// Interactive first-run setup (run with sudo): runner root, per-org PATs,
    /// metrics, and hooks. Writes the system config at /etc.
    #[command(
        long_about = "Interactive configuration, run with sudo. Discovers runners under a root \
        you choose; adds read-only fine-grained PATs per org (validated before saving; each \
        needs Organization → Self-hosted runners: Read, plus Repository → Actions: Read for job \
        success/failure in the Jobs view); optionally enables Prometheus metrics; writes the \
        root-owned 0600 system config at /etc/ghr-stats/config.toml; then offers to \
        install/repair each runner's job hooks, never clobbering a foreign hook (it chains \
        after it or prints a snippet instead).\n\n\
        The same settings can be changed live from the TUI's Config tab ([a]/[h]/[m]/[o]) under \
        `sudo ghr-stats`, or, for [a]/[m], as a member of the `ghr-stats` group (see `systemd \
        install`)."
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
        long_about = "Reverse an install. With no domain this prints a dry-run plan of \
        everything ghr-stats put on this host and removes nothing. Name one or more domains (or \
        `all`) to remove them; you are asked to confirm first unless --yes is given.\n\n\
        Domains: hooks · service · config · data · binary · all.\n\n\
        Hooks are reverted detect-first, never stranding a foreign hook: a runner ghr-stats \
        chained is restored to its original hook; a foreign or untouched runner is left alone. \
        Editing runner .env files needs root.\n\n\
        `config` deletes the file holding your GitHub PAT(s) (unlinked, not shredded: revoke the \
        token on GitHub). `all` also removes the SQLite history + event log. A `cargo install` \
        build prints the `cargo uninstall` command instead of removing the binary.\n\n\
        Examples:\n\
        \x20 ghr-stats uninstall                 # dry-run plan, removes nothing\n\
        \x20 ghr-stats uninstall hooks           # just revert the runner hooks\n\
        \x20 ghr-stats uninstall config data     # remove the PAT config + history\n\
        \x20 sudo ghr-stats uninstall all --yes  # everything, no prompt"
    )]
    Uninstall(UninstallArgs),
}

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
        `serve` unit, and start it. A system install (root) also creates the `ghr-stats` group \
        and adds $SUDO_USER to it: members may edit the root-owned system config from a \
        non-root TUI, applied by the collector over its socket after checking the peer's \
        kernel-reported uid. Membership takes effect without re-login; add operators with \
        `sudo usermod -aG ghr-stats <user>`."
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
    /// Prune samples older than the retention window.
    Prune {
        /// Keep samples newer than this many days.
        #[arg(long, default_value_t = 14)]
        days: u64,
    },
}

fn help_styles() -> clap::builder::Styles {
    use clap::builder::styling::AnsiColor;
    clap::builder::Styles::styled()
        .header(AnsiColor::Green.on_default().bold())
        .usage(AnsiColor::Green.on_default().bold())
        .literal(AnsiColor::Cyan.on_default().bold())
        .placeholder(AnsiColor::Cyan.on_default())
}
