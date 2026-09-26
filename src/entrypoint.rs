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
    // The machine verbs' exit codes carry meaning: a config or usage error is 3 (via
    // `load()?`), anything that fails after that could not determine an answer (2).
    let ok = std::process::ExitCode::SUCCESS;
    match args.command {
        Some(Command::Config) => crate::ops::configure::run(config_path.as_deref()).map(|()| ok),
        None | Some(Command::Tui) => tui::run(&load()?, config_path.as_deref()).map(|()| ok),
        Some(Command::Status(a)) => {
            let cfg = load()?;
            Ok(determined(crate::ops::status::run(&a, &cfg)))
        }
        Some(Command::Explain(a)) => {
            let cfg = load()?;
            Ok(determined(crate::ops::explain::run(&a, &cfg)))
        }
        Some(Command::Timeline(a)) => Ok(determined(crate::ops::timeline::run(&a))),
        // Takes the path: `load` substitutes defaults for an unreadable config, hiding a
        // broken install.
        Some(Command::Doctor(a)) => Ok(determined(crate::ops::doctor::run(
            &a,
            config_path.as_deref(),
        ))),
        Some(Command::Wait(a)) => {
            let cfg = load()?;
            Ok(determined(crate::ops::wait::run(&a, &cfg)))
        }
        Some(Command::Tail(a)) => {
            let cfg = load()?;
            Ok(determined(crate::ops::tail::run(&a, &cfg)))
        }
        Some(Command::Serve) => {
            crate::service::serve::run(&load()?, config_path.as_deref()).map(|()| ok)
        }
        Some(Command::Systemd { action }) => crate::ops::systemd::run(action).map(|()| ok),
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
            cfg.require_readable()?;
            let mut db = crate::service::store::open_writer(&cfg.db_path)
                .with_context(|| format!("opening db at {}", cfg.db_path.display()))?;
            let cutoff = crate::shared::util::now_epoch() - i64::from(days) * 86_400;
            let mut removed = 0;
            loop {
                let n = crate::service::store::writer::prune_batch(&mut db, cutoff, 10_000)?;
                if n == 0 {
                    break;
                }
                removed += n;
            }
            println!("pruned {removed} sample rows older than {days}d");
            Ok(())
        }
    }
}

/// A machine verb that fails at runtime could not determine its answer.
fn determined<T: Into<std::process::ExitCode>>(r: Result<T>) -> std::process::ExitCode {
    r.map(Into::into).unwrap_or_else(|e| {
        eprintln!("Error: {e:?}");
        std::process::ExitCode::from(2)
    })
}

fn init_tracing(command: &Option<Command>) {
    use tracing_subscriber::{EnvFilter, fmt};
    if !logs_to_stderr(command) {
        return;
    }
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
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
