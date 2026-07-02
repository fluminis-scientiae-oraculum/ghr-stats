//! Binary entry point and composition root: platform guard, global allocator,
//! tracing, then CLI dispatch to the verticals. Wires the layers together —
//! `tui` / `service` / `ops` over the `shared` kernel — and holds no domain logic
//! of its own.

mod cli;
mod ops;
mod service;
mod shared;
mod tui;

// Platform boundary (subtract > abstract). Host integration is Linux-only:
// runner liveness/cpu/mem come from procfs + cgroup v2, the host sampler reads
// /sys + statvfs, the collector's IPC uses AF_UNIX sockets, and `systemd`
// manages the service unit. Rather than ship a
// build that compiles elsewhere yet silently can't sample anything, state the
// boundary and fail fast. A thinner macOS build (launchd + Mac process
// introspection, TUI as a pure DB reader) is future work — see README "Platform".
#[cfg(not(target_os = "linux"))]
compile_error!(
    "ghr-stats currently supports Linux only (procfs / cgroup v2 / systemd). \
     A thinner macOS build is planned — see the README \"Platform\" section."
);

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Command, DbAction};

/// mimalloc: faster + lower-fragmentation allocation under the sampling loop's
/// steady churn; MUSL-clean for the static distribution build.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> Result<()> {
    let args = Cli::parse();
    let config_path = args.config;
    init_tracing(&args.command);

    // `config` bootstraps the config file, so it must not require one to already
    // exist; every other command loads config first. A small closure keeps that
    // load lazy and per-arm — so there is no unreachable arm to assert away.
    let load =
        || crate::shared::config::Config::load(config_path.as_deref()).context("loading config");
    match args.command {
        Some(Command::Config) => crate::ops::wizard::run(config_path.as_deref()),
        // Default (no subcommand) launches the TUI.
        None | Some(Command::Tui) => tui::run(&load()?, config_path.as_deref()),
        Some(Command::Serve) => crate::service::serve::run(&load()?),
        Some(Command::Systemd { action }) => crate::ops::systemd::run(action, &load()?),
        Some(Command::Db { action }) => run_db(action, &load()?),
        // Uninstall must work when the config is absent or being removed, so it
        // resolves paths itself rather than going through the lazy `load`.
        Some(Command::Uninstall(a)) => crate::ops::uninstall::run(&a, config_path.as_deref()),
    }
}

fn run_db(action: DbAction, cfg: &crate::shared::config::Config) -> Result<()> {
    match action {
        DbAction::Prune { days } => {
            let mut store = crate::service::store::Store::open(&cfg.db_path)
                .with_context(|| format!("opening db at {}", cfg.db_path.display()))?;
            let cutoff = crate::shared::util::now_epoch() - (days as i64) * 86_400;
            let removed = crate::service::store::writer::prune(store.conn_mut(), cutoff)?;
            println!("pruned {removed} sample rows older than {days}d");
            Ok(())
        }
    }
}

/// Install the tracing subscriber, with the sink chosen by command. The
/// interactive TUI owns the terminal (alternate screen), so ANY log line written
/// to stdout/stderr is visible noise that bleeds onto the dashboard — it gets NO
/// writer at all (events are dropped; `RUST_LOG` has no effect there — a
/// file/journal sink for TUI diagnostics is future work). Every other verb
/// (`serve`, `config`, `systemd`, `db`, `uninstall`) logs at `info`, honoring
/// `RUST_LOG`; `serve` runs under systemd, so its output lands in the journal.
fn init_tracing(command: &Option<Command>) {
    use tracing_subscriber::{EnvFilter, fmt};
    if matches!(command, None | Some(Command::Tui)) {
        return;
    }
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}
