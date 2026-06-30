mod cli;
mod collectors;
mod config;
mod config_wizard;
mod error;
mod github;
mod hooks;
mod metrics;
mod model;
mod paths;
mod privileged;
mod serve;
mod store;
mod systemd;
mod tui;
mod util;

// Platform boundary (subtract > abstract). Host integration is Linux-only:
// runner liveness/cpu/mem come from procfs + cgroup v2, the host sampler reads
// /sys + statvfs, and `serve`/`systemd` manage systemd units. Rather than ship a
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
    init_tracing();
    let args = Cli::parse();
    let config_path = args.config;

    // `config` bootstraps the config file, so it must not require one to already
    // exist; every other command loads config first. A small closure keeps that
    // load lazy and per-arm — so there is no unreachable arm to assert away.
    let load = || config::Config::load(config_path.as_deref()).context("loading config");
    match args.command {
        Some(Command::Config) => config_wizard::run(config_path.as_deref()),
        // Default (no subcommand) launches the TUI.
        None | Some(Command::Tui) => tui::run(&load()?),
        Some(Command::Serve) => serve::run(&load()?),
        Some(Command::Systemd { action }) => systemd::run(action, &load()?),
        Some(Command::Db { action }) => run_db(action, &load()?),
    }
}

fn run_db(action: DbAction, cfg: &config::Config) -> Result<()> {
    match action {
        DbAction::Prune { days } => {
            let mut store = store::Store::open(&cfg.db_path)
                .with_context(|| format!("opening db at {}", cfg.db_path.display()))?;
            let cutoff = util::now_epoch() - (days as i64) * 86_400;
            let removed = store::writer::prune(store.conn_mut(), cutoff)?;
            println!("pruned {removed} sample rows older than {days}d");
            Ok(())
        }
    }
}

fn init_tracing() {
    use tracing_subscriber::{EnvFilter, fmt};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}
