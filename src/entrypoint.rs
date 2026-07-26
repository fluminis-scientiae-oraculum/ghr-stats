//! Binary entry point and composition root: platform guard, global allocator,
//! tracing, then CLI dispatch to the verticals. Wires the layers together —
//! `tui` / `service` / `ops` over the `shared` kernel — and holds no domain logic
//! of its own.

// Zero `unsafe` is already true of this tree; `forbid` (not `deny`) makes it a
// property of the crate rather than a habit — an inner `#[allow]` cannot buy it
// back, so re-introducing `unsafe` is a build failure that has to be argued at
// this line. Host integration stays safe by construction: procfs/cgroup reads go
// through `std::fs`, `sysconf` through `nix`, and the allocator through
// `mimalloc`'s own `GlobalAlloc` impl.
#![forbid(unsafe_code)]

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

fn main() -> std::process::ExitCode {
    match run() {
        Ok(code) => code,
        Err(e) => {
            eprintln!("Error: {e:?}");
            // 3 = usage/config error, per the `status` exit-code contract. Every
            // verb shares it so a caller sees one meaning for "the invocation
            // itself failed" regardless of which verb it ran.
            std::process::ExitCode::from(3)
        }
    }
}

fn run() -> Result<std::process::ExitCode> {
    // `try_parse`, not `parse`: clap's default is to print and exit(2) itself,
    // and 2 already means "cannot determine" in the `status` verdict table. A
    // caller branching on the exit code must never confuse "you typed the flag
    // wrong" with "the fleet's state is unknowable", so usage errors are routed
    // to the dedicated 3. `--help`/`--version` are NOT errors and keep exiting 0.
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

    // `config` bootstraps the config file, so it must not require one to already
    // exist; every other command loads config first. A small closure keeps that
    // load lazy and per-arm — so there is no unreachable arm to assert away.
    let load =
        || crate::shared::config::Config::load(config_path.as_deref()).context("loading config");
    // `status`, `explain` and `timeline` are the verbs whose exit code carries
    // meaning beyond success/failure, so a caller can branch without parsing the
    // payload. For the first two it is the verdict; `timeline` makes no health
    // call, so its code reports availability instead — over the same numbers, so
    // 2 still means "cannot determine" whichever verb returned it. Every other
    // verb exits 0 on success.
    let ok = std::process::ExitCode::SUCCESS;
    match args.command {
        Some(Command::Config) => crate::ops::wizard::run(config_path.as_deref()).map(|()| ok),
        // Default (no subcommand) launches the TUI.
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
        // Takes the PATH, not the loaded config: `load` substitutes defaults for
        // an unreadable system config, and a preflight reasoning over a config
        // it never read would report a phantom install as merely empty.
        Some(Command::Doctor(a)) => {
            crate::ops::doctor::run(&a, config_path.as_deref()).map(std::process::ExitCode::from)
        }
        Some(Command::Wait(a)) => {
            crate::ops::wait::run(&a, &load()?).map(std::process::ExitCode::from)
        }
        Some(Command::Serve) => {
            crate::service::serve::run(&load()?, config_path.as_deref()).map(|()| ok)
        }
        Some(Command::Systemd { action }) => {
            crate::ops::systemd::run(action, &load()?).map(|()| ok)
        }
        Some(Command::Db { action }) => run_db(action, &load()?).map(|()| ok),
        // Uninstall must work when the config is absent or being removed, so it
        // resolves paths itself rather than going through the lazy `load`.
        Some(Command::Uninstall(a)) => {
            crate::ops::uninstall::run(&a, config_path.as_deref()).map(|()| ok)
        }
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
    if !logs_to_stderr(command) {
        return;
    }
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt().with_env_filter(filter).with_target(false).init();
}

/// Whether this verb may write log lines to the terminal.
///
/// An EXHAUSTIVE match with no `_` arm, deliberately: adding a verb must force a
/// decision here rather than inheriting a default. Two verbs already own their
/// output and would be corrupted by a stray log line — the TUI owns the
/// alternate screen, and `status --json` writes a payload something is parsing —
/// and the way that bug arrives is by omission, which is exactly what an
/// exhaustive match prevents. (Same class as the root gate `serve --system`
/// remembered and hook install forgot.)
fn logs_to_stderr(command: &Option<Command>) -> bool {
    match command {
        // The dashboard owns the terminal; any log line bleeds onto it.
        None | Some(Command::Tui) => false,
        // Machine-facing stdout — a log line would corrupt the payload.
        Some(Command::Status(_))
        | Some(Command::Explain(_))
        | Some(Command::Timeline(_))
        | Some(Command::Doctor(_))
        | Some(Command::Wait(_)) => false,
        // Runs under systemd, so its output lands in the journal.
        Some(Command::Serve) => true,
        Some(Command::Config) | Some(Command::Systemd { .. }) | Some(Command::Db { .. }) => true,
        Some(Command::Uninstall(_)) => true,
    }
}
