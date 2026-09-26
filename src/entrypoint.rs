//! Binary entry point: platform guard, allocator, tracing, CLI dispatch.

// `forbid`, not `deny`: an inner `#[allow]` cannot re-enable `unsafe`.
#![forbid(unsafe_code)]

mod cli;
mod ops;
mod service;
mod shared;
mod tui;

#[cfg(not(target_os = "linux"))]
compile_error!(
    "ghr-stats currently supports Linux only (procfs / cgroup v2 / systemd). \
     A thinner macOS build is planned — see the README \"Platform\" section."
);

use anyhow::{Context, Result};
use clap::Parser;

use crate::cli::{Cli, Command, DbAction};

/// Must stay MUSL-clean for the static distribution build.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:?}");
            // 3 = usage/config error in the `status` exit-code contract, shared by every verb.
            std::process::ExitCode::from(3)
        }
    }
}

fn run() -> Result<std::process::ExitCode> {
    // `try_parse`: clap exits 2 on usage errors, and `status` reserves 2 for "cannot determine".
    let args = match Cli::try_parse() {
        Ok(a) => a,
        Err(e) if e.use_stderr() => {
            let _ = e.print();
            return Ok(std::process::ExitCode::from(3));
        }
        Err(e) => {
            let _ = e.print();
            return Ok(std::process::ExitCode::SUCCESS);
        }
    };
    let config_path = args.config;
    init_tracing(&args.command);

    // Lazy, because `config` bootstraps the config file and must not require one.
    let load =
        || crate::shared::config::Config::load(config_path.as_deref()).context("loading config");
    // `status`, `explain` and `timeline` exit codes carry meaning; 2 is "cannot determine"
    // for all three.
    let ok = std::process::ExitCode::SUCCESS;
    match args.command {
        Some(Command::Config) => crate::ops::configure::run(config_path.as_deref()).map(|()| ok),
        None | Some(Command::Tui) => tui::run(&load()?, config_path.as_deref()).map(|()| ok),
        Some(Command::Status(a)) => {
            crate::ops::status::run(&a, &load()?).map(std::process::ExitCode::from)
        }
        Some(Command::Explain(a)) => {
            crate::ops::explain::run(&a, &load()?).map(std::process::ExitCode::from)
        }
        Some(Command::Timeline(a)) => {
            crate::ops::timeline::run(&a, &load()?).map(std::process::ExitCode::from)
        }
        // Takes the path: `load` substitutes defaults for an unreadable config, hiding a
        // broken install.
        Some(Command::Doctor(a)) => {
            crate::ops::doctor::run(&a, config_path.as_deref()).map(std::process::ExitCode::from)
        }
        Some(Command::Wait(a)) => {
            crate::ops::wait::run(&a, &load()?).map(std::process::ExitCode::from)
        }
        Some(Command::Tail(a)) => {
            crate::ops::tail::run(&a, &load()?).map(std::process::ExitCode::from)
        }
        Some(Command::Serve) => {
            crate::service::serve::run(&load()?, config_path.as_deref()).map(|()| ok)
        }
        Some(Command::Systemd { action }) => {
            crate::ops::systemd::run(action, &load()?).map(|()| ok)
        }
        Some(Command::Db { action }) => run_db(action, &load()?).map(|()| ok),
        // Must work with the config absent or being removed.
        Some(Command::Uninstall(a)) => {
            crate::ops::uninstall::run(&a, config_path.as_deref()).map(|()| ok)
        }
    }
}

fn run_db(action: DbAction, cfg: &crate::shared::config::Config) -> Result<()> {
    match action {
        DbAction::Prune { days } => {
            let mut db = crate::service::store::open_writer(&cfg.db_path)
                .with_context(|| format!("opening db at {}", cfg.db_path.display()))?;
            let cutoff = crate::shared::util::now_epoch() - (days as i64) * 86_400;
            let removed = crate::service::store::writer::prune(&mut db, cutoff)?;
            println!("pruned {removed} sample rows older than {days}d");
            Ok(())
        }
    }
}

fn init_tracing(command: &Option<Command>) {
    use tracing_subscriber::{EnvFilter, fmt};
    if !logs_to_stderr(command) {
        return;
    }
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

/// No `_` arm: a new verb must decide whether a log line would corrupt its output.
fn logs_to_stderr(command: &Option<Command>) -> bool {
    match command {
        // The dashboard owns the terminal.
        None | Some(Command::Tui) => false,
        // Machine-facing stdout.
        Some(Command::Status(_))
        | Some(Command::Explain(_))
        | Some(Command::Timeline(_))
        | Some(Command::Doctor(_))
        | Some(Command::Wait(_))
        | Some(Command::Tail(_)) => false,
        Some(Command::Serve) => true,
        Some(Command::Config) | Some(Command::Systemd { .. }) | Some(Command::Db { .. }) => true,
        Some(Command::Uninstall(_)) => true,
    }
}
